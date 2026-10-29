//! Pure signal measurements for the echo-cancellation spike (`examples/aec_spike.rs`): level in
//! dBFS, and how strongly a known reference (the sentence `say` played) shows up in a capture.

/// The floor [`rms_dbfs`] reports for silence (all zeros, or nothing to measure).
pub const FLOOR_DBFS: f32 = -120.0;

/// RMS level of `samples` in dBFS (full scale = 1.0), at least [`FLOOR_DBFS`].
pub fn rms_dbfs(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return FLOOR_DBFS;
    }
    let mean_sq = samples
        .iter()
        .map(|&s| f64::from(s) * f64::from(s))
        .sum::<f64>()
        / samples.len() as f64;
    if mean_sq <= 0.0 {
        return FLOOR_DBFS;
    }
    ((10.0 * mean_sq.log10()) as f32).max(FLOOR_DBFS)
}

/// `samples[from..to]`, clamped to the slice (empty when the range is out of bounds).
pub fn span(samples: &[f32], from: usize, to: usize) -> &[f32] {
    let to = to.min(samples.len());
    let from = from.min(to);
    &samples[from..to]
}

/// The best normalised cross-correlation between `reference` and `capture`, with the reference
/// placed at `capture[start + lag..]` for every `lag` in `0..=max_lag`: `|Σ c·r| / √(Σc² Σr²)`
/// over the overlap, in 0..=1 (the sign is ignored — a speaker/microphone path may invert).
/// Returns `(peak, lag)`; `None` when no lag has any overlap or either side is silent.
/// 1.0 = the reference is in the capture unchanged; a few hundredths = not there.
pub fn ncc_peak(
    capture: &[f32],
    reference: &[f32],
    start: usize,
    max_lag: usize,
) -> Option<(f32, usize)> {
    let mut best: Option<(f32, usize)> = None;
    for lag in 0..=max_lag {
        let at = start + lag;
        if at >= capture.len() {
            break;
        }
        let n = reference.len().min(capture.len() - at);
        if n == 0 {
            break;
        }
        let c = &capture[at..at + n];
        let r = &reference[..n];
        let (mut cr, mut cc, mut rr) = (0.0f64, 0.0f64, 0.0f64);
        for (&a, &b) in c.iter().zip(r) {
            let (a, b) = (f64::from(a), f64::from(b));
            cr += a * b;
            cc += a * a;
            rr += b * b;
        }
        if cc <= 0.0 || rr <= 0.0 {
            continue;
        }
        let v = (cr.abs() / (cc * rr).sqrt()) as f32;
        if best.is_none_or(|(p, _)| v > p) {
            best = Some((v, lag));
        }
    }
    best
}

/// The RMS envelope of `samples`: one value per `hop` samples (the last partial hop dropped).
/// Correlating envelopes ([`ncc_peak`] on two envelopes, mean removed by [`centred`]) is
/// robust to the room's filtering, which smears the raw waveform.
pub fn envelope(samples: &[f32], hop: usize) -> Vec<f32> {
    let hop = hop.max(1);
    samples
        .chunks_exact(hop)
        .map(|c| (c.iter().map(|&s| s * s).sum::<f32>() / hop as f32).sqrt())
        .collect()
}

/// `v` minus its mean.
pub fn centred(v: &[f32]) -> Vec<f32> {
    if v.is_empty() {
        return Vec::new();
    }
    let mean = v.iter().sum::<f32>() / v.len() as f32;
    v.iter().map(|&x| x - mean).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: f32, period: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (i as f32 * std::f32::consts::TAU / period as f32).sin())
            .collect()
    }

    /// A deterministic noise-like signal (an LCG), so the correlation has one clear peak.
    fn noise(n: usize, seed: u32, amp: f32) -> Vec<f32> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                amp * ((x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
            })
            .collect()
    }

    #[test]
    fn dbfs_of_known_levels() {
        // a full-scale sine is −3.01 dBFS RMS
        assert!((rms_dbfs(&tone(16_000, 1.0, 40)) + 3.01).abs() < 0.05);
        assert!((rms_dbfs(&tone(16_000, 0.1, 40)) + 23.01).abs() < 0.05);
        assert!((rms_dbfs(&[0.5; 100]) + 6.02).abs() < 0.01);
        assert_eq!(rms_dbfs(&[]), FLOOR_DBFS);
        assert_eq!(rms_dbfs(&[0.0; 10]), FLOOR_DBFS);
    }

    #[test]
    fn span_clamps() {
        let v = [1.0, 2.0, 3.0];
        assert_eq!(span(&v, 1, 2), &[2.0]);
        assert_eq!(span(&v, 2, 10), &[3.0]);
        assert!(span(&v, 5, 10).is_empty());
        assert!(span(&v, 2, 1).is_empty());
    }

    #[test]
    fn correlation_finds_an_attenuated_delayed_copy() {
        let reference = noise(4_000, 7, 0.5);
        // room noise, then the reference 1234 samples after `start`, at −20 dB, inverted
        let start = 1_000;
        let mut capture = noise(12_000, 99, 0.005);
        for (i, &r) in reference.iter().enumerate() {
            capture[start + 1_234 + i] -= 0.1 * r;
        }
        let found = ncc_peak(&capture, &reference, start, 2_000);
        assert!(
            matches!(found, Some((peak, 1_234)) if peak > 0.95),
            "{found:?}"
        );
    }

    #[test]
    fn correlation_of_unrelated_signals_is_low() {
        let reference = noise(4_000, 7, 0.5);
        let capture = noise(12_000, 12_345, 0.3);
        let found = ncc_peak(&capture, &reference, 0, 1_000);
        assert!(matches!(found, Some((peak, _)) if peak < 0.1), "{found:?}");
        // silence: nothing to correlate
        assert_eq!(ncc_peak(&[0.0; 100], &reference, 0, 10), None);
        // start beyond the capture
        assert_eq!(ncc_peak(&capture, &reference, 20_000, 10), None);
    }

    #[test]
    fn envelope_and_centring() {
        let mut v = vec![0.0f32; 160];
        v.extend(vec![0.5f32; 160]);
        v.extend(vec![0.0f32; 50]); // partial hop dropped
        let e = envelope(&v, 160);
        assert_eq!(e.len(), 2);
        assert!(e[0].abs() < 1e-6 && (e[1] - 0.5).abs() < 1e-6);
        let c = centred(&e);
        assert!((c[0] + 0.25).abs() < 1e-6 && (c[1] - 0.25).abs() < 1e-6);
        assert!(centred(&[]).is_empty());
    }
}
