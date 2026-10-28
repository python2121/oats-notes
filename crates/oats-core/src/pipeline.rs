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

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::engine::{EnergyVad, EngineFactory, SAMPLE_RATE, SpeechEngine};
use crate::segmenter::{Segment, Segmenter, SegmenterConfig};
use crate::store::{Store, TranscriptRow};

/// How long the worker waits for audio before checking commands again.
const AUDIO_POLL: Duration = Duration::from_millis(100);
/// Minimum spacing of `Level` events (~10 Hz).
const LEVEL_INTERVAL: Duration = Duration::from_millis(100);

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
    Level {
        rms_db: f32,
        in_speech: bool,
    },
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
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (event_tx, events) = std::sync::mpsc::channel();
        let Pipeline {
            engine,
            store_path,
            segmenter,
            wake,
        } = self;
        let out = Outbox { tx: event_tx, wake };
        let thread = std::thread::Builder::new()
            .name("oats-pipeline".to_owned())
            .spawn(move || worker(engine, &store_path, segmenter, &cmd_rx, &out));
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                log::error!("could not spawn the pipeline thread: {e}");
                None
            }
        };
        PipelineHandle {
            cmd_tx,
            events,
            thread,
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

/// The worker's event sender: queue an event, then wake the UI.
struct Outbox {
    tx: Sender<PipelineEvent>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Outbox {
    fn send(&self, event: PipelineEvent) {
        if self.tx.send(event).is_ok() {
            (self.wake)();
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// After a fatal error: keep draining commands (dropping audio channels) until shutdown, so
/// the handle's `Drop` never hangs.
fn idle_until_shutdown(cmd_rx: &Receiver<Command>) {
    while let Ok(cmd) = cmd_rx.recv() {
        if matches!(cmd, Command::Shutdown) {
            return;
        }
    }
}

fn worker(
    factory: EngineFactory,
    store_path: &std::path::Path,
    config: SegmenterConfig,
    cmd_rx: &Receiver<Command>,
    out: &Outbox,
) {
    let store = match Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("could not open the transcript store: {e}");
            log::error!("{msg}");
            out.send(PipelineEvent::Status(PipelineStatus::Failed(msg)));
            return idle_until_shutdown(cmd_rx);
        }
    };

    out.send(PipelineEvent::Status(PipelineStatus::Loading(
        "Loading speech engine…".to_owned(),
    )));
    let loaded = factory().and_then(|mut engine| {
        engine.load(&mut |line| {
            out.send(PipelineEvent::Status(PipelineStatus::Loading(line)));
        })?;
        Ok(engine)
    });
    let mut engine = match loaded {
        Ok(e) => e,
        Err(e) => {
            let msg = e.to_string();
            log::error!("{msg}");
            out.send(PipelineEvent::Status(PipelineStatus::Failed(msg)));
            return idle_until_shutdown(cmd_rx);
        }
    };
    let engine_name = engine.name();
    log::info!("speech engine ready: {engine_name}");
    out.send(PipelineEvent::Engine(engine_name.clone()));
    out.send(PipelineEvent::Status(PipelineStatus::Idle));

    let mut next = cmd_rx.recv().ok();
    while let Some(cmd) = next.take() {
        match cmd {
            Command::Listen(audio_rx) => {
                let mut session = Session {
                    engine: engine.as_mut(),
                    engine_name: &engine_name,
                    store: &store,
                    out,
                };
                match session.run(config, &audio_rx, cmd_rx) {
                    AfterListen::Idle => next = cmd_rx.recv().ok(),
                    AfterListen::Listen(rx) => next = Some(Command::Listen(rx)),
                    AfterListen::Shutdown => return,
                }
            }
            Command::Stop => next = cmd_rx.recv().ok(),
            Command::Shutdown => return,
        }
    }
}

/// What the worker does once a listening session ends.
enum AfterListen {
    Idle,
    /// A new `Listen` arrived mid-session: the old session was closed, start this one.
    Listen(Receiver<Vec<f32>>),
    Shutdown,
}

/// One listening session's borrowed context.
struct Session<'a> {
    engine: &'a mut dyn SpeechEngine,
    engine_name: &'a str,
    store: &'a Store,
    out: &'a Outbox,
}

impl Session<'_> {
    fn run(
        &mut self,
        config: SegmenterConfig,
        audio_rx: &Receiver<Vec<f32>>,
        cmd_rx: &Receiver<Command>,
    ) -> AfterListen {
        let started_ms = now_ms();
        let session_id = match self.store.begin_session(started_ms, self.engine_name) {
            Ok(id) => id,
            Err(e) => {
                self.out.send(PipelineEvent::Warning(format!(
                    "could not start a session: {e}"
                )));
                self.out.send(PipelineEvent::Status(PipelineStatus::Idle));
                return AfterListen::Idle;
            }
        };
        self.out.send(PipelineEvent::SessionStarted(session_id));
        self.out
            .send(PipelineEvent::Status(PipelineStatus::Listening));

        let engine_frame = self.engine.vad_frame_len().filter(|&n| n > 0);
        let frame_len = engine_frame.unwrap_or(EnergyVad::FRAME_LEN);
        let mut energy_vad = engine_frame.is_none().then(EnergyVad::new);
        let mut segmenter = Segmenter::new(config);
        let mut pending: Vec<f32> = Vec::new();
        let mut last_level: Option<Instant> = None;
        let mut vad_warned = false;

        let after = loop {
            match audio_rx.recv_timeout(AUDIO_POLL) {
                Ok(chunk) => {
                    pending.extend_from_slice(&chunk);
                    let mut offset = 0;
                    while pending.len() - offset >= frame_len {
                        let frame = &pending[offset..offset + frame_len];
                        offset += frame_len;
                        let is_speech = match energy_vad.as_mut() {
                            Some(vad) => vad.classify(frame).is_speech,
                            None => match self.engine.vad(frame) {
                                Ok(v) => v.first().is_some_and(|f| f.is_speech),
                                Err(e) => {
                                    if !vad_warned {
                                        vad_warned = true;
                                        self.out.send(PipelineEvent::Warning(e.to_string()));
                                    }
                                    false
                                }
                            },
                        };
                        if let Some(seg) = segmenter.push(frame, is_speech) {
                            self.finish_segment(session_id, started_ms, &seg);
                        }
                    }
                    pending.drain(..offset);

                    if last_level.is_none_or(|t| t.elapsed() >= LEVEL_INTERVAL) {
                        last_level = Some(Instant::now());
                        self.out.send(PipelineEvent::Level {
                            rms_db: EnergyVad::rms_db(&chunk),
                            in_speech: segmenter.in_speech(),
                        });
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break AfterListen::Idle,
            }
            match cmd_rx.try_recv() {
                Ok(Command::Stop) => break AfterListen::Idle,
                Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                    break AfterListen::Shutdown;
                }
                Ok(Command::Listen(rx)) => break AfterListen::Listen(rx),
                Err(TryRecvError::Empty) => {}
            }
        };

        if let Some(seg) = segmenter.flush() {
            self.finish_segment(session_id, started_ms, &seg);
        }
        if let Err(e) = self.store.end_session(session_id, now_ms()) {
            self.out.send(PipelineEvent::Warning(format!(
                "could not close the session: {e}"
            )));
        }
        self.out.send(PipelineEvent::SessionEnded(session_id));
        self.out.send(PipelineEvent::Status(PipelineStatus::Idle));
        after
    }

    /// Transcribe a closed segment, store it and report it.
    fn finish_segment(&mut self, session_id: i64, session_start_ms: i64, seg: &Segment) {
        let rate = u64::from(SAMPLE_RATE);
        let started_at = session_start_ms + (seg.start_sample * 1000 / rate) as i64;
        let ended_at =
            session_start_ms + ((seg.start_sample + seg.samples.len() as u64) * 1000 / rate) as i64;
        match self.engine.transcribe(&seg.samples) {
            Ok(t) => {
                let text = t.text.trim();
                if text.is_empty() {
                    return;
                }
                log::debug!(
                    "segment {} ms transcribed in {:.0} ms",
                    seg.duration_ms(),
                    t.processing_ms
                );
                match self.store.insert_segment(
                    session_id,
                    started_at,
                    ended_at,
                    text,
                    t.confidence,
                ) {
                    Ok(row) => self.out.send(PipelineEvent::Segment(row)),
                    Err(e) => self.out.send(PipelineEvent::Warning(format!(
                        "could not save a segment: {e}"
                    ))),
                }
            }
            Err(e) => self.out.send(PipelineEvent::Warning(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::NullEngine;
    use crate::store::View;

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir()
            .join(format!(
                "oats-pipeline-{tag}-{}-{nanos}",
                std::process::id()
            ))
            .join("oats.sqlite")
    }

    fn spawn_null(path: &std::path::Path) -> PipelineHandle {
        Pipeline {
            engine: Box::new(|| Ok(Box::new(NullEngine) as Box<dyn SpeechEngine>)),
            store_path: path.to_path_buf(),
            segmenter: SegmenterConfig::default(),
            wake: Box::new(|| {}),
        }
        .spawn()
    }

    /// Wait for the first event matching `pred`, failing the test on timeout.
    fn wait_for(
        h: &PipelineHandle,
        what: &str,
        pred: impl Fn(&PipelineEvent) -> bool,
    ) -> PipelineEvent {
        let deadline = Instant::now() + TIMEOUT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match h.recv_timeout(left) {
                Some(ev) if pred(&ev) => return ev,
                Some(_) => {}
                None => break,
            }
        }
        unreachable!("timed out waiting for {what}");
    }

    #[test]
    fn transcribes_speech_into_store() {
        let path = temp_db("speech");
        let h = spawn_null(&path);
        wait_for(&h, "engine name", |e| matches!(e, PipelineEvent::Engine(_)));
        wait_for(&h, "idle", |e| {
            matches!(e, PipelineEvent::Status(PipelineStatus::Idle))
        });

        let (audio_tx, audio_rx) = std::sync::mpsc::channel();
        h.start(audio_rx);
        let started = wait_for(&h, "session start", |e| {
            matches!(e, PipelineEvent::SessionStarted(_))
        });

        let mut n = 0usize;
        let mut chunk = |loud: bool| -> Vec<f32> {
            (0..1600)
                .map(|_| {
                    n += 1;
                    if loud {
                        0.3 * (n as f32 * 2.0 * std::f32::consts::PI * 440.0 / 16_000.0).sin()
                    } else {
                        0.0
                    }
                })
                .collect()
        };
        for _ in 0..20 {
            let _ = audio_tx.send(chunk(true));
        }
        for _ in 0..15 {
            let _ = audio_tx.send(chunk(false));
        }

        let seg = wait_for(&h, "segment", |e| matches!(e, PipelineEvent::Segment(_)));
        let PipelineEvent::Segment(row) = seg else {
            unreachable!()
        };
        assert!(row.text.contains("no engine"), "{}", row.text);
        assert!(row.ended_at > row.started_at);

        h.stop();
        let ended = wait_for(&h, "session end", |e| {
            matches!(e, PipelineEvent::SessionEnded(_))
        });
        match (started, ended) {
            (PipelineEvent::SessionStarted(a), PipelineEvent::SessionEnded(b)) => {
                assert_eq!(a, b);
                assert_eq!(row.session_id, a);
            }
            _ => unreachable!(),
        }
        drop(audio_tx);
        drop(h);

        let rows = Store::open(&path).and_then(|s| s.segments(View::All, 0));
        assert!(rows.is_ok_and(|r| r.len() == 1 && r[0].text.contains("no engine")));
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn closing_audio_ends_session_and_engine_stays_loaded() {
        let path = temp_db("reuse");
        let h = spawn_null(&path);
        for _ in 0..2 {
            let (audio_tx, audio_rx) = std::sync::mpsc::channel::<Vec<f32>>();
            h.start(audio_rx);
            wait_for(&h, "session start", |e| {
                matches!(e, PipelineEvent::SessionStarted(_))
            });
            let _ = audio_tx.send(vec![0.0; 1600]);
            drop(audio_tx);
            wait_for(&h, "session end", |e| {
                matches!(e, PipelineEvent::SessionEnded(_))
            });
        }
        drop(h);
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn store_failure_reports_and_shuts_down() {
        // a path under a regular file cannot be created
        let blocker = temp_db("blocker");
        let Some(dir) = blocker.parent() else {
            unreachable!()
        };
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&blocker, b"not a directory");
        let h = spawn_null(&blocker.join("sub").join("oats.sqlite"));
        wait_for(&h, "failure", |e| {
            matches!(e, PipelineEvent::Status(PipelineStatus::Failed(_)))
        });
        let (_tx, rx) = std::sync::mpsc::channel();
        h.start(rx);
        h.stop();
        drop(h); // must not hang
        let _ = std::fs::remove_dir_all(dir);
    }
}
