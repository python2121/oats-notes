//! The macOS speech engine: Parakeet TDT v3 (ASR) and Silero VAD, both on CoreML / the Neural
//! Engine, through FluidAudio (`fluidaudio-rs`). On other platforms this crate still exports
//! [`factory`], whose engine is [`oats_core::EngineError::Unavailable`], so the app compiles everywhere.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use oats_core::EngineFactory;

#[cfg(target_os = "macos")]
pub use mac::FluidAudioEngine;

/// The factory the app hands to the pipeline. The engine is constructed on the pipeline's
/// worker thread (`FluidAudio` is not `Send`); models are loaded later by
/// [`oats_core::SpeechEngine::load`].
#[cfg(target_os = "macos")]
pub fn factory() -> EngineFactory {
    Box::new(|| {
        let engine = FluidAudioEngine::new()?;
        Ok(Box::new(engine) as Box<dyn oats_core::SpeechEngine>)
    })
}

/// The factory the app hands to the pipeline. No FluidAudio off macOS.
#[cfg(not(target_os = "macos"))]
pub fn factory() -> EngineFactory {
    Box::new(|| Err(oats_core::EngineError::Unavailable))
}

#[cfg(target_os = "macos")]
mod mac {
    use fluidaudio_rs::FluidAudio;
    use oats_core::{EngineError, SpeechEngine, Transcription, VadFrame};

    /// Silero VAD chunk in FluidAudio: `VadManager.chunkSize = 4096` (256 ms at 16 kHz), from
    /// `Sources/FluidAudio/VAD/VadManager.swift` in FluidAudio 0.14.8; `fluidaudio-rs` documents
    /// the same ("one VadFrame per 4096-sample chunk").
    pub const VAD_CHUNK: usize = 4096;

    /// VAD threshold. FluidAudio's default (0.85) misses quiet speech; 0.6 is a better
    /// balance for ambient capture, and the segmenter's hangover absorbs the odd false positive.
    const VAD_THRESHOLD: f32 = 0.6;

    /// FluidAudio's `VadManager.process(samples)` starts every call from a fresh LSTM state,
    /// so calling it one 256 ms chunk at a time would throw the recurrent context away. We
    /// re-feed this many previous chunks ahead of each call (and drop their results) so the
    /// state is warm when the new chunks arrive. Costs this many extra chunk inferences per
    /// call; Silero on the ANE takes ~1 ms per chunk.
    const WARMUP_CHUNKS: usize = 2;

    pub struct FluidAudioEngine {
        fluid: FluidAudio,
        chip: String,
        vad_loaded: bool,
        asr_loaded: bool,
        /// The last up-to-`WARMUP_CHUNKS` chunks fed to VAD, plus scratch for the next call.
        vad_context: Vec<f32>,
        vad_input: Vec<f32>,
    }

    impl std::fmt::Debug for FluidAudioEngine {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FluidAudioEngine")
                .field("chip", &self.chip)
                .field("vad_loaded", &self.vad_loaded)
                .field("asr_loaded", &self.asr_loaded)
                .finish_non_exhaustive()
        }
    }

    impl FluidAudioEngine {
        /// Create the bridge (cheap: no models are loaded or downloaded here).
        pub fn new() -> Result<Self, EngineError> {
            let fluid = FluidAudio::new().map_err(|e| EngineError::Load(e.to_string()))?;
            let chip = fluid.system_info().chip_name.trim().to_owned();
            Ok(Self {
                fluid,
                chip,
                vad_loaded: false,
                asr_loaded: false,
                vad_context: Vec::with_capacity(WARMUP_CHUNKS * VAD_CHUNK),
                vad_input: Vec::new(),
            })
        }
    }

    impl SpeechEngine for FluidAudioEngine {
        fn name(&self) -> String {
            let base = "Parakeet TDT v3 · CoreML (FluidAudio 0.14.8)";
            if self.chip.is_empty() {
                base.to_owned()
            } else {
                format!("{base} · {}", self.chip)
            }
        }

        fn load(&mut self, status: &mut dyn FnMut(String)) -> Result<(), EngineError> {
            if !self.vad_loaded {
                status("Loading voice activity model…".to_owned());
                self.fluid
                    .init_vad(VAD_THRESHOLD)
                    .map_err(|e| EngineError::Load(format!("VAD: {e}")))?;
                self.vad_loaded = true;
                log::info!("FluidAudio VAD loaded (threshold {VAD_THRESHOLD})");
            }
            if !self.asr_loaded {
                status("Loading Parakeet model (first run downloads ~600 MB)…".to_owned());
                self.fluid
                    .init_asr()
                    .map_err(|e| EngineError::Load(format!("ASR: {e}")))?;
                self.asr_loaded = true;
                log::info!("FluidAudio ASR (Parakeet TDT v3) loaded");
            }
            Ok(())
        }

        fn vad_frame_len(&self) -> Option<usize> {
            Some(VAD_CHUNK)
        }

        fn vad(&mut self, samples: &[f32]) -> Result<Vec<VadFrame>, EngineError> {
            if samples.is_empty() {
                return Ok(Vec::new());
            }
            let new_chunks = samples.len().div_ceil(VAD_CHUNK);

            self.vad_input.clear();
            self.vad_input.extend_from_slice(&self.vad_context);
            self.vad_input.extend_from_slice(samples);
            let frames = self
                .fluid
                .vad_process_samples(&self.vad_input)
                .map_err(|e| EngineError::Vad(e.to_string()))?;

            // Keep the tail as warm-up context for the next call (whole chunks only, so chunk
            // boundaries stay aligned with the caller's frames).
            let whole = self.vad_input.len() / VAD_CHUNK * VAD_CHUNK;
            let keep = (WARMUP_CHUNKS * VAD_CHUNK).min(whole);
            self.vad_context.clear();
            self.vad_context
                .extend_from_slice(&self.vad_input[whole - keep..whole]);

            let skip = frames.len().saturating_sub(new_chunks);
            Ok(frames[skip..]
                .iter()
                .map(|f| VadFrame {
                    probability: f.probability,
                    is_speech: f.is_voice_active,
                })
                .collect())
        }

        fn transcribe(&mut self, samples: &[f32]) -> Result<Transcription, EngineError> {
            let r = self
                .fluid
                .transcribe_samples(samples)
                .map_err(|e| EngineError::Transcribe(e.to_string()))?;
            let confidence = r
                .confidence
                .is_finite()
                .then(|| r.confidence.clamp(0.0, 1.0));
            Ok(Transcription {
                text: r.text.trim().to_owned(),
                confidence,
                processing_ms: r.processing_time * 1000.0,
            })
        }
    }
}
