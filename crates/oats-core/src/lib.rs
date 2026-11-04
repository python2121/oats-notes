//! OatsNotes core — everything that is the same on every platform:
//!
//! - [`engine`]: the [`SpeechEngine`] contract the on-device model sits behind (VAD +
//!   transcription over 16 kHz mono f32), the [`NullEngine`] used when no model is wired in,
//!   and the energy VAD fallback.
//! - [`segmenter`]: turns a stream of VAD-labelled frames into speech segments
//!   (pre-roll, hangover, min/max length) — pure, no I/O.
//! - [`store`]: the SQLite transcript store (sessions, segments, categories).
//! - [`pipeline`]: the worker thread that owns the engine, consumes 16 kHz audio, runs VAD →
//!   segmentation → transcription, writes to the store and reports events to the UI.
//! - [`echo`]: the text-level rule that marks a microphone segment as the echo of a
//!   system-audio segment (speakers heard by the microphone) — pure.
//! - [`echo_audio`]: the audio-level pre-check that recognises such an echo by correlating
//!   loudness envelopes BEFORE the model runs, and the system-audio ring it reads — pure.
//! - [`media`]: what plays on the system lane (YouTube, music, a call, …), the stamp its
//!   segments carry, the YouTube / music policy and the feed the media resolver writes.
//! - [`speakers`]: the voiceprint book — matches a speaker embedding to a known voice (cosine
//!   distance to running centroids) or opens a new one — pure.
//! - [`paths`]: where the database and logs live.
//!
//! The engine implementation (`oats-engine-fluidaudio` on macOS) and the audio capture
//! (`oats-audio`) are the only crates that touch a platform; this one never does.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod echo;
pub mod echo_audio;
pub mod engine;
pub mod media;
pub mod paths;
pub mod pipeline;
pub mod segmenter;
pub mod speakers;
pub mod store;
pub mod when;

pub use echo::DedupMode;
pub use echo_audio::EchoAudioMode;
pub use engine::{
    AudioChunk, AudioSourceKind, EngineError, EngineFactory, NullEngine, SAMPLE_RATE, SpeakerSpan,
    SpeechEngine, Transcription, VadFrame,
};
pub use media::{
    MediaClass, MediaFeed, MediaPlay, MediaPolicy, MediaStamp, ProducerClass, ProducerOverride,
    ProducerSeen,
};
pub use pipeline::{Pipeline, PipelineEvent, PipelineHandle, PipelineStatus, SpeakerStats};
pub use segmenter::{Segment, Segmenter, SegmenterConfig};
pub use speakers::{Assignment, NameSource, Speaker, SpeakerBook, SpeakerEntry, speaker_label};
pub use store::{
    ActionItem, BrainRun, Change, Decision, Episode, EpisodeContent, Event, NewEvent, NewTodo,
    Node, NodeKind, Note, NoteItem, Origin, RunOutcome, SearchResults, Session, Store, StoreError,
    Todo, TodoFilter, TodoState, TranscriptPurge, TranscriptRow, View,
};
