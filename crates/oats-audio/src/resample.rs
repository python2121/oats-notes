//! A streaming windowed-sinc FIR resampler for an arbitrary (integer-Hz) rate pair.
//!
//! The ratio `in_rate / out_rate` is reduced to `P / Q`. Output sample `k` sits at input
//! position `k · P / Q`, tracked exactly in integers (an integer index plus a numerator over
//! `Q`), so the fractional phase carries across calls without drift and chunk boundaries are
//! seamless. The kernel is a Blackman-windowed sinc whose width is [`HALF_WIDTH`] periods of the
//! *lower* of the two rates on each side (32 taps at the 16 kHz side; ~96 input taps at
//! 48 kHz), with its cutoff a little below the lower Nyquist. Coefficients are precomputed per
//! phase (polyphase table), each phase normalised to unit DC gain. When `Q` is large (an odd
//! device rate such as 44 101 Hz) the phase is quantised to [`MAX_PHASES`] steps; the position
//! itself is still tracked exactly.

use std::f64::consts::PI;

/// Kernel half-width, in periods of the lower of the two rates.
pub const HALF_WIDTH: f64 = 16.0;
/// Cutoff as a fraction of the lower Nyquist frequency (0.95 × 8 kHz = 7.6 kHz at 16 kHz out).
pub const CUTOFF: f64 = 0.95;
/// Upper bound on the polyphase table's phase count.
pub const MAX_PHASES: u64 = 1024;

#[derive(Clone, Debug)]
pub struct Resampler {
    kind: Kind,
}

#[derive(Clone, Debug)]
enum Kind {
    Passthrough,
    Fir(Box<Fir>),
}

#[derive(Clone, Debug)]
struct Fir {
    /// Input samples per output sample, as the reduced fraction `p / q`.
    p: u64,
    q: u64,
    /// Taps per output sample (`2·k`); output at `base + frac` uses input `base-k+1 ..= base+k`.
    k: usize,
    taps: usize,
    phases: u64,
    /// `phases × taps` coefficients, row-major.
    table: Vec<f32>,
    /// Pending input history. `hist[0]` is the oldest sample still needed.
    hist: Vec<f32>,
    /// Integer part of the next output's position, as an index into `hist`.
    base: usize,
    /// Fractional part of the next output's position, `num / q`.
    num: u64,
    /// Real (non-padding) input samples seen and outputs produced, for `flush`.
    total_in: u64,
    total_out: u64,
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// Blackman window over `u ∈ [-1, 1]` (0 outside).
fn blackman(u: f64) -> f64 {
    if u.abs() >= 1.0 {
        return 0.0;
    }
    let t = PI * (u + 1.0); // 0 … 2π
    0.42 - 0.5 * t.cos() + 0.08 * (2.0 * t).cos()
}

impl Resampler {
    /// A resampler from `in_rate` to `out_rate` Hz (both must be non-zero; a zero rate yields a
    /// passthrough).
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        if in_rate == out_rate || in_rate == 0 || out_rate == 0 {
            return Self {
                kind: Kind::Passthrough,
            };
        }
        let g = gcd(u64::from(in_rate), u64::from(out_rate));
        let p = u64::from(in_rate) / g;
        let q = u64::from(out_rate) / g;

        // Work in input-sample units: the kernel's scale relative to the input rate.
        let scale = (f64::from(out_rate) / f64::from(in_rate)).min(1.0); // lower rate / in rate
        let fc = CUTOFF * 0.5 * scale; // cycles per input sample
        let half_in = HALF_WIDTH / scale; // half-width in input samples
        let k = half_in.ceil() as usize;
        let taps = 2 * k;
        let phases = q.min(MAX_PHASES);

        let mut table = vec![0.0f32; phases as usize * taps];
        for ph in 0..phases as usize {
            let frac = ph as f64 / phases as f64;
            let row = &mut table[ph * taps..(ph + 1) * taps];
            let mut sum = 0.0f64;
            let mut coeffs = vec![0.0f64; taps];
            for (m, c) in coeffs.iter_mut().enumerate() {
                // tap m reads input base - k + 1 + m; its distance from the output position:
                let x = (m as f64 - k as f64 + 1.0) - frac;
                *c = 2.0 * fc * sinc(2.0 * fc * x) * blackman(x / half_in);
                sum += *c;
            }
            for (r, c) in row.iter_mut().zip(&coeffs) {
                *r = (c / sum) as f32;
            }
        }

        // Prime the history with k-1 zeros so the first output lands on input sample 0.
        let hist = vec![0.0f32; k - 1];
        Self {
            kind: Kind::Fir(Box::new(Fir {
                p,
                q,
                k,
                taps,
                phases,
                table,
                hist,
                base: k - 1,
                num: 0,
                total_in: 0,
                total_out: 0,
            })),
        }
    }

    pub fn is_passthrough(&self) -> bool {
        matches!(self.kind, Kind::Passthrough)
    }

    /// Resample `input`, appending every output sample that is now fully determined to `out`.
    /// The output lags the input by about [`HALF_WIDTH`] periods (filter look-ahead).
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        match &mut self.kind {
            Kind::Passthrough => out.extend_from_slice(input),
            Kind::Fir(f) => {
                f.total_in += input.len() as u64;
                f.hist.extend_from_slice(input);
                f.run(out, u64::MAX);
            }
        }
    }

    /// Emit the outputs still held back by the look-ahead (padding with silence), so that the
    /// total output length is `ceil(total_in · out_rate / in_rate)`. Then resets to a fresh
    /// state.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        if let Kind::Fir(f) = &mut self.kind {
            let expected = (f.total_in * f.q).div_ceil(f.p);
            let pad = f.k + 1;
            f.hist.extend(std::iter::repeat_n(0.0, pad));
            f.run(out, expected);
            f.hist.clear();
            f.hist.resize(f.k - 1, 0.0);
            f.base = f.k - 1;
            f.num = 0;
            f.total_in = 0;
            f.total_out = 0;
        }
    }
}

impl Fir {
    fn run(&mut self, out: &mut Vec<f32>, max_total_out: u64) {
        while self.base + self.k < self.hist.len() && self.total_out < max_total_out {
            let ph = if self.phases == self.q {
                self.num as usize
            } else {
                (self.num * self.phases / self.q) as usize
            };
            let row = &self.table[ph * self.taps..(ph + 1) * self.taps];
            let start = self.base + 1 - self.k;
            let window = &self.hist[start..start + self.taps];
            let y: f32 = window.iter().zip(row).map(|(a, b)| a * b).sum();
            out.push(y);
            self.total_out += 1;

            self.num += self.p;
            self.base += (self.num / self.q) as usize;
            self.num %= self.q;
        }
        // Drop input no output will need again (memmove, no allocation).
        let keep_from = (self.base + 1).saturating_sub(self.k);
        if keep_from > 0 {
            let n = keep_from.min(self.hist.len());
            self.hist.drain(..n);
            self.base -= n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, len: usize, amp: f64) -> Vec<f32> {
        (0..len)
            .map(|i| (amp * (2.0 * PI * freq * i as f64 / f64::from(rate)).sin()) as f32)
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// Resample in uneven chunks (to exercise the cross-call phase), then flush.
    fn resample_chunked(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
        let mut r = Resampler::new(in_rate, out_rate);
        let mut out = Vec::new();
        let sizes = [441usize, 512, 37, 1000, 4096, 1];
        let mut i = 0;
        let mut s = 0;
        while i < input.len() {
            let n = sizes[s % sizes.len()].min(input.len() - i);
            r.process(&input[i..i + n], &mut out);
            i += n;
            s += 1;
        }
        r.flush(&mut out);
        out
    }

    /// Steady-state slice: skip the filter's edge transients.
    fn middle(x: &[f32]) -> &[f32] {
        &x[200..x.len() - 200]
    }

    #[test]
    fn passthrough_at_16k() {
        let mut r = Resampler::new(16_000, 16_000);
        assert!(r.is_passthrough());
        let mut out = Vec::new();
        r.process(&[0.1, 0.2, 0.3], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn length_is_right() {
        for (rate, len) in [
            (48_000u32, 48_000usize),
            (44_100, 44_100),
            (44_100, 12_345),
            (8_000, 8_000),
            (22_050, 7_777),
        ] {
            let input = sine(440.0, rate, len, 0.5);
            let out = resample_chunked(&input, rate, 16_000);
            let expected = (len as f64 * 16_000.0 / f64::from(rate)).ceil() as i64;
            assert!(
                (out.len() as i64 - expected).abs() <= 1,
                "{rate}: got {} want {expected}",
                out.len()
            );
        }
    }

    #[test]
    fn one_khz_sine_48k_to_16k() {
        let input = sine(1000.0, 48_000, 48_000, 0.5);
        let out = resample_chunked(&input, 48_000, 16_000);
        assert!((out.len() as i64 - 16_000).abs() <= 1);

        // Correlate against the ideal 16 kHz sine (no delay: the filter is centred).
        let ideal = sine(1000.0, 16_000, out.len(), 0.5);
        let m = middle(&out);
        let mi = middle(&ideal);
        let err: f64 = m
            .iter()
            .zip(mi)
            .map(|(a, b)| f64::from(a - b).powi(2))
            .sum::<f64>()
            / m.len() as f64;
        assert!(err.sqrt() < 0.005, "rms error {}", err.sqrt());

        // Amplitude: RMS of a 0.5 sine is 0.3536.
        assert!((rms(m) - 0.5 / 2f64.sqrt()).abs() < 0.005);

        // Frequency: zero crossings ≈ 2 per cycle → ~2000 over a second.
        let zc = out
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        assert!((1995..=2001).contains(&zc), "zero crossings {zc}");
    }

    #[test]
    fn one_khz_sine_44k1_to_16k_matches_ideal() {
        let input = sine(1000.0, 44_100, 44_100, 0.5);
        let out = resample_chunked(&input, 44_100, 16_000);
        let ideal = sine(1000.0, 16_000, out.len(), 0.5);
        let max_err = middle(&out)
            .iter()
            .zip(middle(&ideal))
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err < 0.01, "max error {max_err}");
    }

    fn gain_db(freq: f64, in_rate: u32) -> f64 {
        let input = sine(freq, in_rate, in_rate as usize, 1.0);
        let out = resample_chunked(&input, in_rate, 16_000);
        20.0 * (rms(middle(&out)) / (1.0 / 2f64.sqrt())).log10()
    }

    #[test]
    fn passband_and_stopband() {
        for rate in [48_000u32, 44_100] {
            let pass = gain_db(7_000.0, rate);
            assert!(pass > -1.0, "{rate}: 7 kHz at {pass:.1} dB");
            let edge = gain_db(7_500.0, rate);
            assert!(edge > -6.0, "{rate}: 7.5 kHz at {edge:.1} dB");
            let stop = gain_db(15_000.0, rate);
            assert!(stop < -20.0, "{rate}: 15 kHz at {stop:.1} dB");
            let alias = gain_db(9_000.0, rate);
            assert!(alias < -20.0, "{rate}: 9 kHz at {alias:.1} dB");
        }
    }

    #[test]
    fn chunking_does_not_change_output() {
        let input = sine(1234.0, 44_100, 20_000, 0.7);
        let chunked = resample_chunked(&input, 44_100, 16_000);
        let mut r = Resampler::new(44_100, 16_000);
        let mut whole = Vec::new();
        r.process(&input, &mut whole);
        r.flush(&mut whole);
        assert_eq!(chunked.len(), whole.len());
        for (a, b) in chunked.iter().zip(&whole) {
            assert!((a - b).abs() < 1e-6);
        }
    }
}
