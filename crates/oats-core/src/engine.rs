//! The speech-engine contract. An engine is the on-device model stack behind two operations on
//! 16 kHz mono f32 audio: voice-activity detection (cheap, runs all day) and transcription
//! (runs only on speech segments). `oats-engine-fluidaudio` implements it with Parakeet TDT +
//! Silero VAD on CoreML; a Linux engine implements the same trait over a different runtime and
//! nothing else in the app changes.
//!
//! Engines are NOT required to be `Send`: the pipeline constructs the engine on its own worker
//! thread through an [`EngineFactory`] and never moves it.

/// The one sample rate the whole app speaks. Audio capture resamples to it; engines consume it.
pub const SAMPLE_RATE: u32 = 16_000;

/// One VAD frame's verdict.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VadFrame {
    /// Speech probability, 0…1.
    pub probability: f32,
    pub is_speech: bool,
}

/// The result of transcribing one speech segment.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcription {
    /// The text, already trimmed; may be empty (the engine heard nothing it could write down).
    pub text: String,
    /// 0…1 when the engine reports one, else `None`.
    pub confidence: Option<f32>,
    /// Wall time the engine spent, for the status line.
    pub processing_ms: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// Creating the engine or loading / downloading / compiling its models failed.
    #[error("engine load failed: {0}")]
    Load(String),
    #[error("VAD failed: {0}")]
    Vad(String),
    #[error("transcription failed: {0}")]
    Transcribe(String),
    /// This build has no engine for this platform.
    #[error("no speech engine is available on this platform")]
    Unavailable,
}

/// The on-device model stack. See the module docs.
pub trait SpeechEngine {
    /// Short human-readable name for the status line ("Parakeet TDT v3 · CoreML").
    fn name(&self) -> String;

    /// Load (and on first run download and compile) the models. May take tens of seconds the
    /// first time; `status` receives progress lines for the UI ("Downloading model…").
    fn load(&mut self, status: &mut dyn FnMut(String)) -> Result<(), EngineError>;

    /// The number of 16 kHz samples in one VAD frame, or `None` when the engine has no VAD of
    /// its own — the pipeline then uses [`EnergyVad`] and never calls [`SpeechEngine::vad`].
    fn vad_frame_len(&self) -> Option<usize>;

    /// Classify `samples` (a whole number of VAD frames) frame by frame, in order.
    fn vad(&mut self, samples: &[f32]) -> Result<Vec<VadFrame>, EngineError>;

    /// Transcribe one speech segment (≤ ~30 s). Each call is independent: no state carries
    /// over between segments.
    fn transcribe(&mut self, samples: &[f32]) -> Result<Transcription, EngineError>;
}

/// Builds the engine on the pipeline's worker thread.
pub type EngineFactory = Box<dyn FnOnce() -> Result<Box<dyn SpeechEngine>, EngineError> + Send>;

/// A placeholder engine: no model, energy-based VAD only, and "transcriptions" that just
/// record how long the speech was. Lets the whole app run without a model (UI work, the
/// pipeline's tests, a platform with no engine yet).
#[derive(Debug, Default)]
pub struct NullEngine;

impl SpeechEngine for NullEngine {
    fn name(&self) -> String {
        "No engine (energy VAD, no transcription)".to_owned()
    }

    fn load(&mut self, status: &mut dyn FnMut(String)) -> Result<(), EngineError> {
        status("No speech model: speech is detected but not transcribed".to_owned());
        Ok(())
    }

    fn vad_frame_len(&self) -> Option<usize> {
        None
    }

    fn vad(&mut self, _samples: &[f32]) -> Result<Vec<VadFrame>, EngineError> {
        Ok(Vec::new())
    }

    fn transcribe(&mut self, samples: &[f32]) -> Result<Transcription, EngineError> {
        let secs = samples.len() as f64 / f64::from(SAMPLE_RATE);
        Ok(Transcription {
            text: format!("[speech, {secs:.1} s — no engine]"),
            confidence: None,
            processing_ms: 0.0,
        })
    }
}

/// Energy VAD fallback: 32 ms frames (512 samples), an RMS level in dBFS against a slowly
/// adapting noise floor. Used when the engine reports no VAD of its own.
#[derive(Clone, Debug)]
pub struct EnergyVad {
    /// Noise floor estimate in dBFS, adapted during non-speech frames.
    floor_db: f32,
    /// A frame is speech when it exceeds the floor by this much (dB)…
    margin_db: f32,
    /// …and is at least this loud in absolute terms (dBFS).
    min_db: f32,
}

impl EnergyVad {
    /// 32 ms at 16 kHz.
    pub const FRAME_LEN: usize = 512;

    pub fn new() -> Self {
        Self {
            floor_db: -60.0,
            margin_db: 9.0,
            min_db: -48.0,
        }
    }

    /// RMS of `frame` in dBFS (−100 for silence).
    pub fn rms_db(frame: &[f32]) -> f32 {
        if frame.is_empty() {
            return -100.0;
        }
        let mean_sq = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
        (20.0 * mean_sq.sqrt().max(1e-5).log10()).max(-100.0)
    }

    /// Classify one frame (any length; [`Self::FRAME_LEN`] is the intended one) and adapt the
    /// floor.
    pub fn classify(&mut self, frame: &[f32]) -> VadFrame {
        let db = Self::rms_db(frame);
        let is_speech = db > self.min_db && db > self.floor_db + self.margin_db;
        if !is_speech {
            // fast down, slow up: the floor tracks the quiet between words, not the words
            let rate = if db < self.floor_db { 0.2 } else { 0.02 };
            self.floor_db += (db - self.floor_db) * rate;
        }
        let probability = ((db - self.floor_db) / (2.0 * self.margin_db)).clamp(0.0, 1.0);
        VadFrame {
            probability,
            is_speech,
        }
    }
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_vad_separates_tone_from_silence() {
        let mut vad = EnergyVad::new();
        let silence = vec![0.0f32; EnergyVad::FRAME_LEN];
        for _ in 0..20 {
            assert!(!vad.classify(&silence).is_speech);
        }
        let tone: Vec<f32> = (0..EnergyVad::FRAME_LEN)
            .map(|i| 0.3 * (i as f32 * 0.1).sin())
            .collect();
        assert!(vad.classify(&tone).is_speech);
    }

    #[test]
    fn null_engine_reports_duration() {
        let mut e = NullEngine;
        let t = e.transcribe(&vec![0.0; SAMPLE_RATE as usize * 2]);
        assert!(t.is_ok_and(|t| t.text.contains("2.0 s")));
    }
}
