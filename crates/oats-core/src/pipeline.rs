//! The listening pipeline: one long-lived worker thread that owns the engine (built on that
//! thread from an [`EngineFactory`], loaded once, kept warm across listen start/stop) and,
//! while listening, pulls 16 kHz mono chunks from an audio channel, runs VAD → [`Segmenter`] →
//! transcription, writes each segment to the [`Store`] and reports [`PipelineEvent`]s.
//!
//! Efficiency rules (the app runs all day on an M2 Air):
//! - The worker blocks on the audio channel; nothing spins. With no audio channel attached
//!   (not listening) it blocks on the command channel.
//! - VAD runs on every frame (cheap); the model runs only on closed speech segments.
//! - Level events are rate-limited to ~10 Hz; the UI repaints only when an event arrives
//!   (`wake` is called after each event is queued).

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::engine::EngineFactory;
use crate::segmenter::SegmenterConfig;
use crate::store::{Store, TranscriptRow};

/// Coarse state for the status line and the Listen toggle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PipelineStatus {
    /// The engine is being built / its models loaded; the string is the latest progress line.
    Loading(String),
    /// Engine ready, not listening.
    Idle,
    Listening,
    /// The engine failed to load; listening is impossible until the app restarts.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PipelineEvent {
    Status(PipelineStatus),
    /// Engine name, once known.
    Engine(String),
    /// Input level (RMS dBFS of the latest chunk) and whether a speech segment is open.
    Level { rms_db: f32, in_speech: bool },
    /// A segment was transcribed and stored (the row as stored). Empty transcriptions are
    /// not stored and not reported.
    Segment(TranscriptRow),
    /// A new session row was opened / closed.
    SessionStarted(i64),
    SessionEnded(i64),
    /// A non-fatal problem (one transcription failed, the store refused a write).
    Warning(String),
}

/// The UI's handle: start/stop listening, receive events. Dropping it shuts the worker down.
pub struct PipelineHandle {
    cmd_tx: Sender<Command>,
    events: Receiver<PipelineEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub(crate) enum Command {
    /// Start listening: consume 16 kHz mono chunks from this channel until it closes or
    /// `Stop` arrives.
    Listen(Receiver<Vec<f32>>),
    Stop,
    Shutdown,
}

/// Construction parameters.
pub struct Pipeline {
    pub engine: EngineFactory,
    /// Opened on the worker thread (a `Store` is not `Send`).
    pub store_path: std::path::PathBuf,
    pub segmenter: SegmenterConfig,
    /// Called after every event is queued; the UI passes `ctx.request_repaint`.
    pub wake: Box<dyn Fn() + Send + Sync>,
}

impl Pipeline {
    /// Spawn the worker. The engine is built and loaded immediately (status events follow);
    /// listening starts only on [`PipelineHandle::start`].
    pub fn spawn(self) -> PipelineHandle {
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel();
        let (_event_tx, events) = std::sync::mpsc::channel();
        let _ = (self.engine, self.store_path, self.segmenter, self.wake);
        PipelineHandle {
            cmd_tx,
            events,
            thread: None,
        }
    }
}

impl PipelineHandle {
    /// Begin a listening session over `audio` (16 kHz mono f32 chunks, any chunk size).
    pub fn start(&self, audio: Receiver<Vec<f32>>) {
        let _ = self.cmd_tx.send(Command::Listen(audio));
    }

    /// End the listening session: the open segment is flushed and transcribed, the session row
    /// closed.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(Command::Stop);
    }

    /// Drain pending events (call once per UI frame).
    pub fn poll(&self) -> Vec<PipelineEvent> {
        self.events.try_iter().collect()
    }

    /// Block for one event (tests).
    pub fn recv_timeout(&self, timeout: Duration) -> Option<PipelineEvent> {
        self.events.recv_timeout(timeout).ok()
    }
}

impl Drop for PipelineHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Keep `Store` referenced so the worker's use is obvious to readers of this stub.
#[allow(dead_code)]
fn _uses_store(_: &Store) {}
