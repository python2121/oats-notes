//! The macOS speech engine: Parakeet TDT v3 (ASR), Silero VAD and per-segment speaker
//! embeddings (pyannote segmentation + WeSpeaker), all on CoreML / the Neural
//! Engine, through FluidAudio (`fluidaudio-rs`, vendored in `vendor/fluidaudio-rs` with a
//! stateful VAD stream added; see its `VENDORED.md`). On other platforms this crate still exports
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
    use fluidaudio_rs::{FluidAudio, VadStream};
    use oats_core::{
        AudioSourceKind, EngineError, SpeakerSpan, SpeechEngine, Transcription, VadFrame,
    };

    /// Silero VAD chunk in FluidAudio: `VadManager.chunkSize = 4096` (256 ms at 16 kHz), from
    /// `Sources/FluidAudio/VAD/VadManager.swift` in FluidAudio 0.14.8; the vendored bridge's
    /// [`VadStream::CHUNK`] takes exactly one such chunk per call.
    pub const VAD_CHUNK: usize = VadStream::CHUNK;

    /// VAD threshold. FluidAudio's default (0.85) misses quiet speech; 0.6 is a better
    /// balance for ambient capture, and the segmenter's hangover absorbs the odd false positive.
    const VAD_THRESHOLD: f32 = 0.6;

    pub struct FluidAudioEngine {
        /// Stateful Silero streams, one per lane (microphone, system audio; indexed by
        /// [`AudioSourceKind::index`]): the LSTM state is carried from chunk to chunk, one
        /// inference per 256 ms chunk, and the lanes never share context. Same CoreML model.
        /// The microphone's is created by `load`, the system lane's on its first chunk. The
        /// `SpeechEngine` contract has no session boundary, so the state also carries across
        /// a listen stop/start; that is harmless (the model re-converges within a few chunks
        /// of new audio).
        vad_streams: [Option<VadStream>; 2],
        fluid: FluidAudio,
        chip: String,
        vad_loaded: bool,
        asr_loaded: bool,
        /// Speaker segmentation + embedding models: `None` = not tried yet, `Some(false)` =
        /// failed to load (a warning; transcription carries on without speakers).
        speakers_loaded: Option<bool>,
    }

    impl std::fmt::Debug for FluidAudioEngine {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FluidAudioEngine")
                .field("chip", &self.chip)
                .field("vad_loaded", &self.vad_loaded)
                .field("asr_loaded", &self.asr_loaded)
                .field("speakers_loaded", &self.speakers_loaded)
                .finish_non_exhaustive()
        }
    }

    impl FluidAudioEngine {
        /// Create the bridge (cheap: no models are loaded or downloaded here).
        pub fn new() -> Result<Self, EngineError> {
            let fluid = FluidAudio::new().map_err(|e| EngineError::Load(e.to_string()))?;
            let chip = fluid.system_info().chip_name.trim().to_owned();
            if !chip.is_empty() {
                log::info!("FluidAudio engine on {chip}");
            }
            Ok(Self {
                vad_streams: [None, None],
                fluid,
                chip,
                vad_loaded: false,
                asr_loaded: false,
                speakers_loaded: None,
            })
        }

        /// A lane's VAD stream, created on first use (needs `init_vad`, done by `load`).
        fn vad_stream(&mut self, lane: AudioSourceKind) -> Result<&mut VadStream, EngineError> {
            let slot = &mut self.vad_streams[lane.index()];
            if slot.is_none() {
                let stream = self
                    .fluid
                    .vad_stream_start()
                    .map_err(|e| EngineError::Vad(bridge_reason(&e)))?;
                log::info!("FluidAudio VAD stream for the {} lane", lane.as_str());
                *slot = Some(stream);
            }
            slot.as_mut()
                .ok_or_else(|| EngineError::Vad("no VAD stream".to_owned()))
        }
    }

    /// The bridge's error text without the layers' own prefixes ("Swift bridge error: ",
    /// "transcription failed: ", …), so the pipeline's "transcription failed: …" reads once.
    fn bridge_reason(e: &fluidaudio_rs::FluidAudioError) -> String {
        let mut s = e.to_string();
        for prefix in [
            "Swift bridge error: ",
            "transcription failed: ",
            "VAD process samples failed: ",
            "speaker embedding failed: ",
        ] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.to_owned();
            }
        }
        s
    }

    impl SpeechEngine for FluidAudioEngine {
        fn name(&self) -> String {
            // the chip (`self.chip`, logged at load) is not part of the name: the status strip
            // has no room for what the user already knows
            "Parakeet TDT v3 · CoreML (FluidAudio 0.14.8)".to_owned()
        }

        fn load(&mut self, status: &mut dyn FnMut(String)) -> Result<(), EngineError> {
            if !self.vad_loaded {
                status("Loading voice activity model…".to_owned());
                self.fluid
                    .init_vad(VAD_THRESHOLD)
                    .map_err(|e| EngineError::Load(format!("VAD: {e}")))?;
                self.vad_loaded = true;
                self.vad_stream(AudioSourceKind::Mic)
                    .map_err(|e| EngineError::Load(format!("VAD stream: {e}")))?;
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
            if self.speakers_loaded.is_none() {
                // never an engine failure: transcription must not depend on speaker models
                status("Loading speaker models…".to_owned());
                let t = std::time::Instant::now();
                match self.fluid.init_speaker_embed() {
                    Ok(()) => {
                        self.speakers_loaded = Some(true);
                        log::info!(
                            "FluidAudio speaker models (pyannote segmentation + WeSpeaker) loaded in {} ms",
                            t.elapsed().as_millis()
                        );
                    }
                    Err(e) => {
                        self.speakers_loaded = Some(false);
                        log::warn!("speaker models failed to load, speakers are off: {e}");
                    }
                }
            }
            Ok(())
        }

        fn vad_frame_len(&self) -> Option<usize> {
            Some(VAD_CHUNK)
        }

        fn vad(&mut self, samples: &[f32]) -> Result<Vec<VadFrame>, EngineError> {
            self.vad_lane(AudioSourceKind::Mic, samples)
        }

        fn vad_lane(
            &mut self,
            lane: AudioSourceKind,
            samples: &[f32],
        ) -> Result<Vec<VadFrame>, EngineError> {
            if samples.is_empty() {
                return Ok(Vec::new());
            }
            let stream = self.vad_stream(lane)?;
            let mut frames = Vec::with_capacity(samples.len().div_ceil(VAD_CHUNK));
            for chunk in samples.chunks(VAD_CHUNK) {
                let frame = if chunk.len() == VAD_CHUNK {
                    stream.process_chunk(chunk)
                } else {
                    // a final partial chunk is zero-padded
                    let mut padded = chunk.to_vec();
                    padded.resize(VAD_CHUNK, 0.0);
                    stream.process_chunk(&padded)
                }
                .map_err(|e| EngineError::Vad(bridge_reason(&e)))?;
                frames.push(VadFrame {
                    probability: frame.probability,
                    is_speech: frame.is_voice_active,
                });
            }
            Ok(frames)
        }

        fn transcribe(&mut self, samples: &[f32]) -> Result<Transcription, EngineError> {
            let r = self
                .fluid
                .transcribe_samples(samples)
                .map_err(|e| EngineError::Transcribe(bridge_reason(&e)))?;
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

        fn speaker_spans(&mut self, samples: &[f32]) -> Result<Vec<SpeakerSpan>, EngineError> {
            if self.speakers_loaded != Some(true) {
                return Ok(Vec::new());
            }
            let spans = self
                .fluid
                .speaker_spans(samples)
                .map_err(|e| EngineError::Speakers(bridge_reason(&e)))?;
            let ms = |s: f32| (f64::from(s) * 1000.0).round() as i64;
            Ok(spans
                .into_iter()
                .map(|s| SpeakerSpan {
                    start_ms: ms(s.start),
                    end_ms: ms(s.end),
                    speech_ms: ms(s.speech),
                    embedding: s.embedding,
                })
                .collect())
        }
    }
}
