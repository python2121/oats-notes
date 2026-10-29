//! The listening pipeline: one long-lived worker thread that owns the engine (built on that
//! thread from an [`EngineFactory`], loaded once, kept warm across listen start/stop) and,
//! while listening, pulls 16 kHz mono chunks from an audio channel, runs VAD → [`Segmenter`] →
//! transcription, writes each segment to the [`Store`] and reports [`PipelineEvent`]s.
//!
//! Efficiency rules (the app runs all day on an M2 Air):
//! - The worker blocks on the audio channel; nothing spins. With no audio channel attached
//!   (not listening) it blocks on the command channel.
//! - VAD runs on every frame (cheap); the model runs only on closed speech segments.
//! - Level events are rate-limited to ~4 Hz (and off while the UI says it has no use for them); the UI repaints only when an event arrives
//!   (`wake` is called after each event is queued).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::engine::{EnergyVad, EngineFactory, SAMPLE_RATE, SpeechEngine};
use crate::segmenter::{Segment, Segmenter, SegmenterConfig};
use crate::store::{Store, TranscriptRow};

/// How long the worker waits for audio before checking commands again.
const AUDIO_POLL: Duration = Duration::from_millis(100);
/// Minimum spacing of `Level` events (~4 Hz).
const LEVEL_INTERVAL: Duration = Duration::from_millis(250);
/// How long dropping the handle waits for the worker before detaching it (a model load can
/// take minutes and can't be interrupted; quitting must not wait for it).
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// `start` could not begin a session (the store refused the session row): listening did
    /// not start, and the audio channel was dropped. A `Status(Idle)` follows.
    ListenFailed(String),
    /// A non-fatal problem (one transcription failed, the store refused a write).
    Warning(String),
}

/// The UI's handle: start/stop listening, receive events. Dropping it shuts the worker down.
pub struct PipelineHandle {
    cmd_tx: Sender<Command>,
    events: Receiver<PipelineEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Set on drop: the worker gives up between engine-load steps.
    cancel: Arc<AtomicBool>,
    /// Whether the worker sends `Level` events (the UI turns them off while hidden).
    levels_wanted: Arc<AtomicBool>,
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
        let levels_wanted = Arc::new(AtomicBool::new(true));
        let out = Outbox {
            tx: event_tx,
            wake,
            levels_wanted: levels_wanted.clone(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("oats-pipeline".to_owned())
            .spawn(move || {
                worker(
                    engine,
                    &store_path,
                    segmenter,
                    &cmd_rx,
                    &out,
                    &worker_cancel,
                )
            });
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
            cancel,
            levels_wanted,
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

    /// Turn `Level` events on or off (default on). Off sends none and wakes nothing for them;
    /// all other events are unaffected.
    pub fn set_levels_wanted(&self, wanted: bool) {
        self.levels_wanted.store(wanted, Ordering::Relaxed);
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
    /// Shut the worker down: a bounded join. Normally the worker is back on its command
    /// channel within a poll interval; while the engine is still loading (which can't be
    /// interrupted) it is detached after [`JOIN_TIMEOUT`] and exits by itself at the next
    /// load step, so quitting never hangs on a model download.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + JOIN_TIMEOUT;
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                log::warn!(
                    "the pipeline worker is still busy (loading the speech engine?) after {:?}; \
                     not waiting for it",
                    JOIN_TIMEOUT
                );
            }
        }
    }
}

/// The worker's event sender: queue an event, then wake the UI.
struct Outbox {
    tx: Sender<PipelineEvent>,
    wake: Box<dyn Fn() + Send + Sync>,
    levels_wanted: Arc<AtomicBool>,
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

/// Maps a sample position of the session's audio to wall-clock Unix ms. Positions count
/// samples, so the clock is exact while audio flows — but the machine can sleep mid-session
/// (lid closed) and the audio simply stops, so the sample count falls behind the wall clock by
/// the length of the nap. The monotonic clock (`Instant`) doesn't advance during sleep on
/// macOS or Linux while the wall clock does, so their drift measures the sleep; when it grows
/// by more than [`SampleClock::RESYNC_MS`], the gap is inserted before the chunk that arrived
/// after the wake. (A processing stall moves both clocks alike and changes nothing.)
#[derive(Clone, Copy, Debug)]
struct SampleClock {
    anchor_ms: i64,
    anchor_sample: u64,
    start_wall_ms: i64,
    start_mono: Instant,
    /// Sleep already accounted for.
    slept_ms: i64,
}

impl SampleClock {
    const RESYNC_MS: i64 = 2_000;

    fn new(start_wall_ms: i64, start_mono: Instant) -> Self {
        Self {
            anchor_ms: start_wall_ms,
            anchor_sample: 0,
            start_wall_ms,
            start_mono,
            slept_ms: 0,
        }
    }

    fn ms_at(&self, sample: u64) -> i64 {
        let rate = i64::from(SAMPLE_RATE);
        let delta = sample as i64 - self.anchor_sample as i64;
        self.anchor_ms + delta * 1000 / rate
    }

    /// A chunk arrived at (`wall_ms`, `mono`); `before` samples had arrived before it.
    fn observe(&mut self, before: u64, wall_ms: i64, mono: Instant) {
        let mono_ms = mono.saturating_duration_since(self.start_mono).as_millis() as i64;
        let slept = (wall_ms - self.start_wall_ms) - mono_ms;
        let extra = slept - self.slept_ms;
        if extra > Self::RESYNC_MS {
            self.anchor_ms = self.ms_at(before) + extra;
            self.anchor_sample = before;
            self.slept_ms = slept;
        } else if extra < -Self::RESYNC_MS {
            // the wall clock was set back: re-baseline, keep the sample times
            self.slept_ms = slept;
        }
    }
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
    cancel: &AtomicBool,
) {
    let cancelled = || {
        let c = cancel.load(Ordering::Relaxed);
        if c {
            log::info!("pipeline: shut down while loading");
        }
        c
    };
    let store = match Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("could not open the transcript store: {e}");
            log::error!("{msg}");
            out.send(PipelineEvent::Status(PipelineStatus::Failed(msg)));
            return idle_until_shutdown(cmd_rx);
        }
    };

    if cancelled() {
        return;
    }
    out.send(PipelineEvent::Status(PipelineStatus::Loading(
        "Loading speech engine…".to_owned(),
    )));
    let built = factory();
    if cancelled() {
        return;
    }
    let loaded = built.and_then(|mut engine| {
        engine.load(&mut |line| {
            out.send(PipelineEvent::Status(PipelineStatus::Loading(line)));
        })?;
        Ok(engine)
    });
    if cancelled() {
        return;
    }
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
                log::warn!("could not start a listening session: {e}");
                self.out.send(PipelineEvent::ListenFailed(format!(
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
        let mut clock = SampleClock::new(started_ms, Instant::now());
        let mut received: u64 = 0;

        let after = loop {
            match audio_rx.recv_timeout(AUDIO_POLL) {
                Ok(chunk) => {
                    clock.observe(received, now_ms(), Instant::now());
                    received += chunk.len() as u64;
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
                            self.finish_segment(session_id, &clock, &seg);
                        }
                    }
                    pending.drain(..offset);

                    if self.out.levels_wanted.load(Ordering::Relaxed)
                        && last_level.is_none_or(|t| t.elapsed() >= LEVEL_INTERVAL)
                    {
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
            self.finish_segment(session_id, &clock, &seg);
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
    fn finish_segment(&mut self, session_id: i64, clock: &SampleClock, seg: &Segment) {
        let started_at = clock.ms_at(seg.start_sample);
        let ended_at = clock.ms_at(seg.start_sample + seg.samples.len() as u64);
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
    fn sample_clock_follows_audio_and_resyncs_after_a_sleep() {
        let t0 = Instant::now();
        let mut c = SampleClock::new(1_000_000, t0);
        // 10 s of audio, delivered late by a 5 s processing stall: sample time stays exact
        c.observe(160_000, 1_015_000, t0 + Duration::from_secs(15));
        assert_eq!(c.ms_at(160_000), 1_010_000);
        assert_eq!(c.ms_at(16_000), 1_001_000);
        // the machine slept exactly an hour: the wall clock moved, the monotonic one didn't
        let hour = 3_600_000;
        c.observe(
            161_600,
            1_015_100 + hour,
            t0 + Duration::from_millis(15_100),
        );
        // the chunk after the wake (samples 161 600…) is an hour later than without the nap
        assert_eq!(c.ms_at(161_600), 1_010_100 + hour);
        assert_eq!(c.ms_at(161_600 + 16_000), 1_011_100 + hour);
        let woke = 1_010_100 + hour;
        assert!(c.ms_at(160_000) < woke);
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

    /// An engine whose load blocks far longer than any quit may wait.
    struct SlowLoad;

    impl SpeechEngine for SlowLoad {
        fn name(&self) -> String {
            "slow".into()
        }
        fn load(&mut self, _status: &mut dyn FnMut(String)) -> Result<(), crate::EngineError> {
            std::thread::sleep(Duration::from_secs(8));
            Ok(())
        }
        fn vad_frame_len(&self) -> Option<usize> {
            None
        }
        fn vad(&mut self, _samples: &[f32]) -> Result<Vec<crate::VadFrame>, crate::EngineError> {
            Ok(Vec::new())
        }
        fn transcribe(
            &mut self,
            _samples: &[f32],
        ) -> Result<crate::Transcription, crate::EngineError> {
            Ok(crate::Transcription {
                text: String::new(),
                confidence: None,
                processing_ms: 0.0,
            })
        }
    }

    #[test]
    fn dropping_the_handle_while_the_engine_loads_does_not_hang() {
        let path = temp_db("slowload");
        let h = Pipeline {
            engine: Box::new(|| Ok(Box::new(SlowLoad) as Box<dyn SpeechEngine>)),
            store_path: path.clone(),
            segmenter: SegmenterConfig::default(),
            wake: Box::new(|| {}),
        }
        .spawn();
        wait_for(&h, "loading", |e| {
            matches!(e, PipelineEvent::Status(PipelineStatus::Loading(_)))
        });
        let t = Instant::now();
        drop(h);
        let took = t.elapsed();
        assert!(
            took >= JOIN_TIMEOUT - Duration::from_millis(100),
            "{took:?}"
        );
        assert!(took < JOIN_TIMEOUT + Duration::from_secs(2), "{took:?}");
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
