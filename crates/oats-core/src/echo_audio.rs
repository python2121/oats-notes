//! Echo detection at the audio level, BEFORE the model runs: on speakers the microphone hears
//! the call, and transcribing that copy only to throw it away (the text rule, [`crate::echo`])
//! was most of the two-lane cost. The pipeline keeps the last [`ECHO_RING_SECS`] of system
//! audio ([`SystemRing`]); when a microphone segment closes it compares the segment's loudness
//! contour with what the computer played over the same span ([`analyse`]):
//!
//! - 10 ms RMS envelopes (linear amplitude) of both; the microphone's is cut to its *active*
//!   span (frames clearly above its own noise floor), so the on/off edges of a segment — which
//!   two unrelated voices speaking at the same time share — don't count, only the syllable
//!   rhythm inside does;
//! - the Pearson correlation of the two envelopes (each window mean-centred) at every lag in
//!   ± [`ECHO_MAX_LAG_MS`] (the lanes' clocks are anchored independently, and the room adds a
//!   few ms), best lag wins;
//! - three guards against the user's own voice, all of which must pass ([`Verdict::is_echo`]):
//!   peak ≥ [`ECHO_ENV_THRESHOLD`]; the microphone no louder than the system audio by more than
//!   [`ECHO_MAX_GAIN_DB`]; and at most [`ECHO_MAX_UNCOVERED_MS`] of active microphone audio
//!   where the computer played nothing (allowing [`REVERB_MS`] of room tail) — a segment that
//!   runs on into the user's reply is not an echo even when its first half correlates.
//!
//! Not decidable ([`analyse`] → `None`: too little active audio, a flat envelope such as a
//! steady tone, or system audio silent over the span) means "transcribe as before": every
//! failure of this check falls back to the model plus the text rule, never to dropping speech.
//!
//! [`EchoAudioMode`] (`OATS_ECHO_AUDIO=auto|always|never`, default auto: on exactly while the
//! text rule is on, i.e. on speakers) switches it.

use std::collections::VecDeque;

use crate::engine::SAMPLE_RATE;

/// How much system audio the pipeline keeps (16 kHz mono f32: ≈ 2.9 MB).
pub const ECHO_RING_SECS: u32 = 45;
/// The lag window each side (and how far the system span is widened around the mic segment).
pub const ECHO_MAX_LAG_MS: u32 = 500;
/// Envelope hop: 10 ms.
pub const ENVELOPE_HOP: usize = SAMPLE_RATE as usize / 100;
/// The envelope correlation at or above which a microphone segment with at least
/// [`FULL_THRESHOLD_MS`] of active audio is the speakers (shorter ones: [`required_peak`]).
///
/// Calibration (unit tests below, synthetic speech-like bursts): a clean echo (×0.3, 60 ms,
/// −45 dBFS room noise) correlates ≈ 1.0, the same through a 300 ms reverberant room at −14 dB
/// ≈ 0.98; an unrelated voice at the same time ≈ 0.15–0.35 for 3 s (99th percentile 0.53 over
/// 200 pairs), and the echo with the user talking over it at full level ≈ 0.2–0.36. 0.6 sits
/// above every chance correlation seen at 3 s and far below a real copy. Real rooms are
/// still to be measured (`docs/performance-2026-10-28.md`); the spike's 0.16
/// (`docs/verification-2026-10-28.md`) correlated the mic with a `say` rendering, not with
/// the system tap, and may have missed `say`'s start-up delay.
pub const ECHO_ENV_THRESHOLD: f32 = 0.6;
/// A microphone segment more than this many dB louder than the system audio it is compared
/// with is the user (an echo through the room is quieter than what was played digitally).
pub const ECHO_MAX_GAIN_DB: f32 = 6.0;
/// At most this much active microphone audio may fall where the system played nothing.
pub const ECHO_MAX_UNCOVERED_MS: u32 = 250;
/// A system frame "covers" microphone frames up to this long after it (the room's tail).
pub const REVERB_MS: u32 = 200;
/// Less active microphone audio than this: not decidable (transcribed).
pub const MIN_ACTIVE_MS: u32 = 1_000;
/// From this much active audio up the threshold is [`ECHO_ENV_THRESHOLD`]…
pub const FULL_THRESHOLD_MS: u32 = 3_000;
/// …and at [`MIN_ACTIVE_MS`] it is this ([`required_peak`]).
pub const SHORT_THRESHOLD: f32 = 0.9;
/// A suspected echo whose system segment hasn't been stored after this long is discarded.
pub const ECHO_PENDING_TIMEOUT_MS: u64 = 45_000;
/// Overlap slack when linking a suspected echo to a system segment.
pub const LINK_SLACK_MS: i64 = 2_000;

/// Active microphone frames: above the segment's peak − 20 dB and its noise floor (10th
/// percentile) + 9 dB.
const ACTIVE_BELOW_PEAK: f32 = 0.1;
const ACTIVE_OVER_FLOOR: f32 = 2.8;
/// System frames this far below the window's peak (−30 dB), or below −80 dBFS, are silence.
const SYSTEM_SILENT_BELOW_PEAK: f32 = 0.031_6;
const SYSTEM_SILENT_ABS: f32 = 1e-4;
/// The active microphone envelope's coefficient of variation must reach this: speech is
/// strongly modulated (≈ 0.5–1); a steady tone isn't, and correlating its edges means nothing.
const MIN_MODULATION: f32 = 0.1;

/// Whether the audio-level check runs (`OATS_ECHO_AUDIO`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EchoAudioMode {
    /// Exactly while the text rule runs (`DedupMode`, i.e. on speakers).
    #[default]
    Auto,
    Always,
    /// Off: every microphone segment is transcribed (the text rule alone).
    Never,
}

impl EchoAudioMode {
    /// `auto` | `always` | `never` (case-insensitive; `on` / `off` too).
    pub fn parse(s: &str) -> Option<EchoAudioMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(EchoAudioMode::Auto),
            "always" | "on" => Some(EchoAudioMode::Always),
            "never" | "off" => Some(EchoAudioMode::Never),
            _ => None,
        }
    }

    /// From `OATS_ECHO_AUDIO` (unset or unknown → auto; unknown is logged).
    pub fn from_env() -> EchoAudioMode {
        match std::env::var("OATS_ECHO_AUDIO") {
            Ok(v) => EchoAudioMode::parse(&v).unwrap_or_else(|| {
                log::warn!("OATS_ECHO_AUDIO={v:?} is not auto|always|never; using auto");
                EchoAudioMode::Auto
            }),
            Err(_) => EchoAudioMode::Auto,
        }
    }

    /// Whether the check runs, given whether the text rule is on.
    pub fn active(self, dedup_on: bool) -> bool {
        match self {
            EchoAudioMode::Auto => dedup_on,
            EchoAudioMode::Always => true,
            EchoAudioMode::Never => false,
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        match self {
            EchoAudioMode::Auto => 0,
            EchoAudioMode::Always => 1,
            EchoAudioMode::Never => 2,
        }
    }

    pub(crate) fn from_u8(v: u8) -> EchoAudioMode {
        match v {
            1 => EchoAudioMode::Always,
            2 => EchoAudioMode::Never,
            _ => EchoAudioMode::Auto,
        }
    }
}

/// The RMS envelope of `samples`: one value per `hop` samples (a last partial hop dropped).
pub fn envelope(samples: &[f32], hop: usize) -> Vec<f32> {
    let hop = hop.max(1);
    samples
        .chunks_exact(hop)
        .map(|c| (c.iter().map(|&s| s * s).sum::<f32>() / hop as f32).sqrt())
        .collect()
}

/// The correlation a segment with `active_ms` of active audio must reach: a short envelope has
/// few syllables, so two unrelated voices correlate by chance far more often (synthetic
/// voices, 200 pairs each: 99th percentile 0.84 at 1 s, 0.73 at 1.5 s, 0.61 at 2 s, 0.53 at
/// 3 s). [`ECHO_ENV_THRESHOLD`] from [`FULL_THRESHOLD_MS`] up, rising linearly to
/// [`SHORT_THRESHOLD`] at [`MIN_ACTIVE_MS`].
pub fn required_peak(active_ms: u32) -> f32 {
    if active_ms >= FULL_THRESHOLD_MS {
        return ECHO_ENV_THRESHOLD;
    }
    let short = active_ms.max(MIN_ACTIVE_MS);
    let t = (FULL_THRESHOLD_MS - short) as f32 / (FULL_THRESHOLD_MS - MIN_ACTIVE_MS) as f32;
    ECHO_ENV_THRESHOLD + t * (SHORT_THRESHOLD - ECHO_ENV_THRESHOLD)
}

/// What [`analyse`] measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Verdict {
    /// Best envelope correlation, −1…1.
    pub peak: f32,
    /// Lag of the best correlation, ms (positive: the microphone hears it later).
    pub lag_ms: i32,
    /// Microphone RMS over its active span minus the system RMS over the aligned span, dB.
    pub gain_db: f32,
    /// Active microphone audio where the system played nothing (after the reverb allowance).
    pub uncovered_ms: u32,
    /// Active microphone audio compared.
    pub active_ms: u32,
}

impl Verdict {
    /// The decision (see the module docs).
    pub fn is_echo(&self) -> bool {
        self.peak >= required_peak(self.active_ms)
            && self.gain_db <= ECHO_MAX_GAIN_DB
            && self.uncovered_ms <= ECHO_MAX_UNCOVERED_MS
    }
}

/// Compare a microphone segment with the system audio over the same span widened by
/// [`ECHO_MAX_LAG_MS`] on each side (`system[k]` is simultaneous with `mic[k - lag samples]`).
/// `None` when nothing can be decided (see the module docs).
pub fn analyse(mic: &[f32], system: &[f32]) -> Option<Verdict> {
    let hop = ENVELOPE_HOP;
    let frame_ms = (hop * 1000 / SAMPLE_RATE as usize) as u32;
    let max_lag = (ECHO_MAX_LAG_MS / frame_ms) as usize;
    let m = envelope(mic, hop);
    let s = envelope(system, hop);
    if m.is_empty() || s.is_empty() {
        return None;
    }

    // the microphone's active span
    let mut sorted = m.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[sorted.len() / 10];
    let peak = sorted[sorted.len() - 1];
    let active_at = (peak * ACTIVE_BELOW_PEAK).max(floor * ACTIVE_OVER_FLOOR);
    let active: Vec<bool> = m.iter().map(|&v| v > active_at && v > 0.0).collect();
    let first = active.iter().position(|&a| a)?;
    let last = active.iter().rposition(|&a| a)?;
    let r = &m[first..=last];
    let active_frames = active.iter().filter(|&&a| a).count();
    if (active_frames as u32) * frame_ms < MIN_ACTIVE_MS || r.len() < 2 {
        return None;
    }
    let r_mean = r.iter().sum::<f32>() / r.len() as f32;
    let r_c: Vec<f64> = r.iter().map(|&v| f64::from(v - r_mean)).collect();
    let rr: f64 = r_c.iter().map(|v| v * v).sum();
    let r_std = (rr / r.len() as f64).sqrt() as f32;
    if r_mean <= 0.0 || r_std / r_mean < MIN_MODULATION {
        return None;
    }

    let s_peak = s.iter().fold(0.0f32, |a, &v| a.max(v));
    if s_peak <= SYSTEM_SILENT_ABS {
        return None;
    }

    // Pearson correlation per lag; system frame `first + lag` lines up with mic frame `first`
    // (lag `max_lag` = simultaneous)
    let mut best: Option<(f64, usize)> = None;
    for lag in 0..=2 * max_lag {
        let at = first + lag;
        if at + r.len() > s.len() {
            break;
        }
        let w = &s[at..at + r.len()];
        let w_mean = w.iter().map(|&v| f64::from(v)).sum::<f64>() / w.len() as f64;
        let (mut cr, mut cc) = (0.0f64, 0.0f64);
        for (&c, &rv) in w.iter().zip(&r_c) {
            let c = f64::from(c) - w_mean;
            cr += c * rv;
            cc += c * c;
        }
        if cc <= 0.0 || rr <= 0.0 {
            continue;
        }
        let v = cr / (cc * rr).sqrt();
        if best.is_none_or(|(p, _)| v > p) {
            best = Some((v, lag));
        }
    }
    let (peak_corr, lag) = best?;

    // the guards, at the best lag
    let silent_below = (s_peak * SYSTEM_SILENT_BELOW_PEAK).max(SYSTEM_SILENT_ABS);
    let reverb = (REVERB_MS / frame_ms) as usize;
    let mut uncovered = 0u32;
    let (mut mic_sq, mut sys_sq) = (0.0f64, 0.0f64);
    for (i, &is_active) in active.iter().enumerate().take(last + 1).skip(first) {
        let j = i + lag; // system frame aligned with mic frame i
        let lo = j.saturating_sub(reverb);
        let covered = s
            .get(lo..=j.min(s.len().saturating_sub(1)))
            .is_some_and(|w| w.iter().any(|&v| v >= silent_below));
        if is_active && !covered {
            uncovered += 1;
        }
        mic_sq += f64::from(m[i]).powi(2);
        sys_sq += f64::from(s.get(j).copied().unwrap_or(0.0)).powi(2);
    }
    let gain_db = if sys_sq <= 0.0 {
        f32::INFINITY
    } else {
        (10.0 * (mic_sq / sys_sq).log10()) as f32
    };
    Some(Verdict {
        peak: peak_corr as f32,
        lag_ms: (max_lag as i32 - lag as i32) * frame_ms as i32,
        gain_db,
        uncovered_ms: uncovered * frame_ms,
        active_ms: active_frames as u32 * frame_ms,
    })
}

/// The last [`ECHO_RING_SECS`] of system audio, indexed by the system lane's sample count.
#[derive(Debug, Default)]
pub struct SystemRing {
    samples: VecDeque<f32>,
    /// Lane sample index of `samples[0]`.
    start: u64,
}

impl SystemRing {
    const CAPACITY: usize = ECHO_RING_SECS as usize * SAMPLE_RATE as usize;

    pub fn new() -> Self {
        Self::default()
    }

    /// Append the chunk whose first sample is lane sample `at` (chunks arrive in order; a jump
    /// forward is filled with silence, a jump back restarts the ring).
    pub fn push(&mut self, at: u64, chunk: &[f32]) {
        let end = self.start + self.samples.len() as u64;
        if self.samples.is_empty() || at < end {
            self.samples.clear();
            self.start = at;
        } else if at > end {
            let gap = (at - end).min(Self::CAPACITY as u64) as usize;
            self.samples.extend(std::iter::repeat_n(0.0, gap));
            self.start = at - self.samples.len() as u64;
        }
        self.samples.extend(chunk.iter().copied());
        let excess = self.samples.len().saturating_sub(Self::CAPACITY);
        if excess > 0 {
            self.samples.drain(..excess);
            self.start += excess as u64;
        }
    }

    /// Lane samples `from..to` (may be negative / beyond what arrived): what the ring doesn't
    /// hold reads as silence, so the result is always `to - from` long and aligned.
    pub fn slice(&self, from: i64, to: i64) -> Vec<f32> {
        let len = (to - from).max(0) as usize;
        let mut out = vec![0.0f32; len];
        let start = self.start as i64;
        let end = start + self.samples.len() as i64;
        let lo = from.max(start);
        let hi = to.min(end);
        for k in lo..hi {
            if let (Some(dst), Some(src)) = (
                out.get_mut((k - from) as usize),
                self.samples.get((k - start) as usize),
            ) {
                *dst = *src;
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Synthetic speech-like signals for the tests (here and in the pipeline's integration tests).
#[doc(hidden)]
pub mod synth {
    use super::SAMPLE_RATE;

    /// A deterministic LCG in −1…1.
    pub struct Lcg(u32);

    impl Lcg {
        pub fn new(seed: u32) -> Self {
            Lcg(seed.wrapping_mul(2_654_435_761).wrapping_add(1))
        }
        pub fn sample(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (self.0 >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        }
    }

    /// `ms` of a speech-like burst: noise shaped into "syllables" of 120–300 ms at random
    /// levels (0.3–1.0 of `amp`) with 30–120 ms dips (to 5 %) between them, the rhythm and the
    /// carrier both from `seed`.
    pub fn burst(seed: u32, ms: u32, amp: f32) -> Vec<f32> {
        let n = (u64::from(ms) * u64::from(SAMPLE_RATE) / 1000) as usize;
        let mut rng = Lcg::new(seed);
        let per_ms = SAMPLE_RATE as usize / 1000;
        let mut gains = Vec::with_capacity(n);
        while gains.len() < n {
            let syl = (120.0 + 90.0 * (rng.sample() + 1.0)) as usize * per_ms;
            let level = 0.65 + 0.35 * rng.sample();
            let dip = (30.0 + 45.0 * (rng.sample() + 1.0)) as usize * per_ms;
            gains.extend(std::iter::repeat_n(level, syl));
            gains.extend(std::iter::repeat_n(0.05, dip));
        }
        gains.truncate(n);
        gains.into_iter().map(|g| amp * g * rng.sample()).collect()
    }

    /// White noise at `rms_db` dBFS.
    pub fn noise(seed: u32, n: usize, rms_db: f32) -> Vec<f32> {
        // uniform −1…1 has an RMS of 1/√3
        let amp = 10f32.powf(rms_db / 20.0) * 3f32.sqrt();
        let mut rng = Lcg::new(seed ^ 0x5eed);
        (0..n).map(|_| amp * rng.sample()).collect()
    }

    /// `signal` delayed by `ms` (zeros in front, same length) and scaled.
    pub fn delayed(signal: &[f32], ms: u32, gain: f32) -> Vec<f32> {
        let d = (u64::from(ms) * u64::from(SAMPLE_RATE) / 1000) as usize;
        (0..signal.len())
            .map(|i| if i < d { 0.0 } else { gain * signal[i - d] })
            .collect()
    }

    /// A crude reverberant room: `signal` convolved with exponentially decaying noise
    /// (`rt60_ms` to −60 dB), normalised to the direct path.
    pub fn reverb(signal: &[f32], rt60_ms: u32, seed: u32) -> Vec<f32> {
        let len = (u64::from(rt60_ms) * u64::from(SAMPLE_RATE) / 1000) as usize;
        let mut rng = Lcg::new(seed);
        // sparse taps keep the convolution cheap
        let taps: Vec<(usize, f32)> = (0..len)
            .step_by(37)
            .map(|k| {
                let decay = 10f32.powf(-3.0 * k as f32 / len as f32);
                (
                    k,
                    if k == 0 {
                        1.0
                    } else {
                        0.25 * decay * rng.sample()
                    },
                )
            })
            .collect();
        (0..signal.len())
            .map(|i| {
                taps.iter()
                    .filter(|(k, _)| *k <= i)
                    .map(|(k, g)| g * signal[i - k])
                    .sum()
            })
            .collect()
    }

    /// Element-wise sum (the longer length).
    pub fn mix(a: &[f32], b: &[f32]) -> Vec<f32> {
        (0..a.len().max(b.len()))
            .map(|i| a.get(i).copied().unwrap_or(0.0) + b.get(i).copied().unwrap_or(0.0))
            .collect()
    }

    /// `ms` of zeros.
    pub fn silence(ms: u32) -> Vec<f32> {
        vec![0.0; (u64::from(ms) * u64::from(SAMPLE_RATE) / 1000) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::synth::*;
    use super::*;

    const LAG: u32 = ECHO_MAX_LAG_MS;

    /// A mic segment (300 ms pre-roll, the burst, 800 ms hangover, room noise throughout) and
    /// the system audio over the same span widened by the lag window, the system playing
    /// `played` where the mic segment has its burst.
    fn scene(mic_burst: &[f32], played: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mic_clean = [silence(300), mic_burst.to_vec(), silence(800)].concat();
        let mic = mix(&mic_clean, &noise(3, mic_clean.len(), -45.0));
        let system = [
            silence(LAG + 300),
            played.to_vec(),
            silence(800 + LAG + 1_000),
        ]
        .concat();
        (mic, system)
    }

    fn verdict(mic_burst: &[f32], played: &[f32]) -> Verdict {
        let (mic, system) = scene(mic_burst, played);
        let v = analyse(&mic, &system);
        assert!(v.is_some(), "undecidable");
        v.unwrap_or(Verdict {
            peak: 0.0,
            lag_ms: 0,
            gain_db: 0.0,
            uncovered_ms: 0,
            active_ms: 0,
        })
    }

    #[test]
    fn an_attenuated_delayed_copy_is_an_echo() {
        let played = burst(1, 3_000, 0.3);
        // the mic hears it 60 ms later at ×0.3 (−10.5 dB)
        let heard = delayed(&played, 60, 0.3);
        let v = verdict(&heard, &played);
        assert!(v.peak >= 0.95, "{v:?}");
        assert!((50..=70).contains(&v.lag_ms), "{v:?}");
        assert!(v.gain_db < -8.0, "{v:?}");
        assert!(v.is_echo(), "{v:?}");
    }

    #[test]
    fn a_reverberant_room_still_correlates() {
        let played = burst(2, 3_000, 0.3);
        let heard = delayed(&reverb(&played, 300, 9), 40, 0.2);
        let v = verdict(&heard, &played);
        assert!(v.peak >= 0.8, "{v:?}");
        assert!(v.is_echo(), "{v:?}");
    }

    #[test]
    fn an_unrelated_voice_at_the_same_time_is_not() {
        let played = burst(1, 3_000, 0.3);
        for seed in [4, 5, 6, 7, 8] {
            let other = burst(seed, 3_000, 0.3);
            let v = verdict(&other, &played);
            assert!(v.peak < 0.35, "seed {seed}: {v:?}");
            assert!(!v.is_echo(), "seed {seed}: {v:?}");
        }
    }

    /// The bound on false positives while the user talks over the call: the echo at ×0.3 and
    /// the user at full level.
    #[test]
    fn talking_over_the_echo_is_not_an_echo() {
        let played = burst(1, 3_000, 0.3);
        for seed in [4, 5, 6, 7, 8] {
            let own = burst(seed, 3_000, 0.3);
            let heard = mix(&delayed(&played, 60, 0.3), &own);
            let v = verdict(&heard, &played);
            assert!(v.peak < 0.5, "seed {seed}: {v:?}");
            assert!(!v.is_echo(), "seed {seed}: {v:?}");
        }
    }

    /// The segment carries the echo and then runs on into the user's reply: the first half
    /// correlates, but the reply falls where the computer played nothing.
    #[test]
    fn an_echo_followed_by_the_users_reply_is_not_an_echo() {
        let played = burst(1, 3_000, 0.3);
        let heard = [delayed(&played, 60, 0.3), burst(11, 1_000, 0.1)].concat();
        let (mic, _) = scene(&heard, &[]);
        let system = [
            silence(LAG + 300),
            played,
            silence(1_000 + 800 + LAG + 1_000),
        ]
        .concat();
        let v = analyse(&mic, &system);
        assert!(
            v.is_some_and(|v| !v.is_echo() && v.uncovered_ms > ECHO_MAX_UNCOVERED_MS),
            "{v:?}"
        );
    }

    #[test]
    fn a_mic_much_louder_than_the_system_audio_is_the_user() {
        let played = burst(1, 3_000, 0.05);
        let heard = delayed(&played, 60, 6.0); // +15.6 dB
        let v = verdict(&heard, &played);
        assert!(v.peak >= 0.95 && v.gain_db > ECHO_MAX_GAIN_DB, "{v:?}");
        assert!(!v.is_echo());
    }

    #[test]
    fn steady_tones_and_silent_system_audio_are_undecidable() {
        let tone = vec![0.3f32; 48_000];
        let (mic, system) = scene(&tone, &tone);
        assert_eq!(analyse(&mic, &system), None);
        let played = burst(1, 3_000, 0.3);
        let (mic, _) = scene(&delayed(&played, 60, 0.3), &played);
        assert_eq!(analyse(&mic, &vec![0.0; mic.len() + 16_000]), None);
        // nothing but room noise on the mic
        let (mic, system) = scene(&silence(1_000), &played);
        assert_eq!(analyse(&mic, &system), None);
        assert_eq!(analyse(&[], &system), None);
    }

    #[test]
    fn the_ring_keeps_45_s_aligned_and_pads_the_rest_with_silence() {
        let mut ring = SystemRing::new();
        ring.push(100, &[1.0, 2.0, 3.0]);
        assert_eq!(ring.slice(98, 105), vec![0.0, 0.0, 1.0, 2.0, 3.0, 0.0, 0.0]);
        // a gap is silence
        ring.push(105, &[5.0]);
        assert_eq!(ring.slice(102, 106), vec![3.0, 0.0, 0.0, 5.0]);
        // capacity
        let cap = SystemRing::CAPACITY as i64;
        ring.push(106, &vec![7.0; SystemRing::CAPACITY]);
        assert_eq!(ring.len(), SystemRing::CAPACITY);
        assert_eq!(ring.slice(105, 107), vec![0.0, 7.0]);
        assert_eq!(ring.slice(106 + cap - 1, 106 + cap + 1), vec![7.0, 0.0]);
        // going backwards restarts
        ring.push(0, &[9.0]);
        assert_eq!(ring.slice(0, 2), vec![9.0, 0.0]);
        assert!(ring.slice(5, 3).is_empty());
    }

    #[test]
    fn mode_parses_and_decides() {
        assert_eq!(EchoAudioMode::parse("Never"), Some(EchoAudioMode::Never));
        assert_eq!(EchoAudioMode::parse("on"), Some(EchoAudioMode::Always));
        assert_eq!(EchoAudioMode::parse(""), Some(EchoAudioMode::Auto));
        assert_eq!(EchoAudioMode::parse("maybe"), None);
        assert!(EchoAudioMode::Auto.active(true) && !EchoAudioMode::Auto.active(false));
        assert!(EchoAudioMode::Always.active(false));
        assert!(!EchoAudioMode::Never.active(true));
        for m in [
            EchoAudioMode::Auto,
            EchoAudioMode::Always,
            EchoAudioMode::Never,
        ] {
            assert_eq!(EchoAudioMode::from_u8(m.to_u8()), m);
        }
    }

    /// The calibration behind the thresholds: many pairs of unrelated synthetic voices at the
    /// same time never pass (at any length), and echoes through a 300 ms reverberant room at
    /// −14 dB (only ≈ 16 dB over the room noise) mostly do from 2 s of speech up.
    #[test]
    fn calibration_unrelated_voices_never_pass_reverberant_echoes_mostly_do() {
        for (ms, min_hits) in [(1_500u32, 0usize), (2_000, 35), (3_000, 40)] {
            let mut hits = 0;
            for seed in 0..40u32 {
                let played = burst(seed * 7 + 1, ms, 0.3);
                let other = burst(seed * 13 + 5_000, ms, 0.3);
                let (mic, system) = scene(&other, &played);
                let v = analyse(&mic, &system);
                assert!(
                    v.is_none_or(|v| !v.is_echo()),
                    "{ms} ms, seed {seed}: {v:?}"
                );
                let heard = delayed(&reverb(&played, 300, seed), 40, 0.2);
                let (mic, system) = scene(&heard, &played);
                if analyse(&mic, &system).is_some_and(|v| v.is_echo()) {
                    hits += 1;
                }
            }
            assert!(hits >= min_hits, "{ms} ms: {hits}/40 echoes found");
        }
    }

    #[test]
    fn the_threshold_rises_for_short_segments() {
        assert_eq!(required_peak(FULL_THRESHOLD_MS), ECHO_ENV_THRESHOLD);
        assert_eq!(required_peak(30_000), ECHO_ENV_THRESHOLD);
        assert!((required_peak(MIN_ACTIVE_MS) - SHORT_THRESHOLD).abs() < 1e-6);
        assert!((required_peak(2_000) - 0.75).abs() < 1e-6);
    }
}
