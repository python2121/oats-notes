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
//! - [`paths`]: where the database and logs live.
//!
//! The engine implementation (`oats-engine-fluidaudio` on macOS) and the audio capture
//! (`oats-audio`) are the only crates that touch a platform; this one never does.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod engine;
pub mod paths;
pub mod pipeline;
pub mod segmenter;
pub mod store;

pub use engine::{
    EngineError, EngineFactory, NullEngine, SAMPLE_RATE, SpeechEngine, Transcription, VadFrame,
};
pub use pipeline::{Pipeline, PipelineEvent, PipelineHandle, PipelineStatus};
pub use segmenter::{Segment, Segmenter, SegmenterConfig};
pub use store::{
    ActionItem, BrainRun, Change, Episode, EpisodeContent, Event, NewEvent, NewReminder, Node,
    NodeKind, Note, Origin, Reminder, RunOutcome, Session, Store, StoreError, TranscriptRow, View,
};
