//! The listening pipeline: one long-lived worker thread that owns the engine (built on that
//! thread from an [`EngineFactory`], loaded once, kept warm across listen start/stop) and,
//! while listening, pulls 16 kHz mono chunks from an audio channel, runs VAD → [`Segmenter`] →
//! transcription, writes each segment to the [`Store`] and reports [`PipelineEvent`]s.
//!
//! Two lanes: the microphone ("me") and system audio ("them", what the computer plays — a call,
//! a video). Chunks arrive on ONE channel tagged with their [`AudioSourceKind`]; each lane has
//! its own pending buffer, VAD state ([`SpeechEngine::vad_lane`]), [`Segmenter`] and sample
//! clock, and the closed segments of both are transcribed on this one thread, first come first
//! served (the engine is not `Send`). Segments carry their `source`; the store orders by
//! `started_at`, so the lanes interleave on one timeline. A microphone segment that only
//! repeats a system segment (speakers heard by the microphone) is stored as its echo
//! ([`crate::echo`]) while the echo rule is on ([`PipelineHandle::set_echo_dedup`]).
//!
//! Efficiency rules (the app runs all day on an M2 Air):
//! - The worker blocks on the audio channel; nothing spins. With no audio channel attached
//!   (not listening) it blocks on the command channel.
//! - VAD runs on every frame (cheap) — except all-zero system-audio frames (the tap delivers
//!   exact digital silence while nothing plays), which skip the engine; the model runs only on
//!   closed speech segments.
//! - Level events are rate-limited to ~4 Hz (and off while the UI says it has no use for them); the UI repaints only when an event arrives
//!   (`wake` is called after each event is queued).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::echo;
use crate::engine::{
    AudioChunk, AudioSourceKind, EnergyVad, EngineFactory, SAMPLE_RATE, SpeechEngine,
};
use crate::segmenter::{Segment, Segmenter, SegmenterConfig};
use crate::store::{Store, TranscriptRow};

/// How long the worker waits for audio before checking commands again.
const AUDIO_POLL: Duration = Duration::from_millis(100);
/// Minimum spacing of `Level` events (~4 Hz).
const LEVEL_INTERVAL: Duration = Duration::from_millis(250);
/// A lane that delivered nothing for this long reads as silent in `Level` events.
const LEVEL_STALE: Duration = Duration::from_secs(1);
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
    /// Input levels (RMS dBFS of each lane's latest chunk; −100 for a lane that is silent or
    /// delivers nothing) and whether a speech segment is open on each lane.
    Level {
        rms_db_mic: f32,
        rms_db_system: f32,
        in_speech_mic: bool,
        in_speech_system: bool,
    },
    /// A segment was transcribed and stored (the row as stored; `source` says which lane, and
    /// `echo_of` is set for a microphone echo of system audio, which the views hide). Empty
    /// transcriptions are not stored and not reported.
    Segment(TranscriptRow),
    /// An already reported microphone segment turned out to be the echo of a system segment
    /// stored after it: it is now hidden.
    EchoMarked {
        segment: i64,
        echo_of: i64,
    },
    /// The system-audio lane (when expected, [`ListenOptions::system`]) produced nothing but
    /// silence for [`ListenOptions::no_signal_after`] since the session started
    /// (`signal: false`: nothing is playing, or the System Audio Recording permission is
    /// missing — macOS then delivers zeros), or produced sound after such a report
    /// (`signal: true`).
    SystemAudio {
        signal: bool,
    },
    /// A new session row was opened / closed.
    SessionStarted(i64),
    SessionEnded(i64),
    /// `start` could not begin a session (the store refused the session row): listening did
    /// not start, and the audio channel was dropped. A `Status(Idle)` follows.
    ListenFailed(String),
    /// A non-fatal problem (one transcription failed, the store refused a write).
    Warning(String),
}

/// What a listening session expects from its audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenOptions {
    /// System audio was asked for (the "no signal" report only runs then).
    pub system: bool,
    /// Report [`PipelineEvent::SystemAudio`] `{ signal: false }` after this long without a
    /// single non-zero system sample.
    pub no_signal_after: Duration,
}

impl Default for ListenOptions {
    fn default() -> Self {
        Self {
            system: false,
            no_signal_after: Duration::from_secs(30),
        }
    }
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
    /// Whether the echo rule runs (see [`PipelineHandle::set_echo_dedup`]).
    echo_dedup: Arc<AtomicBool>,
}

/// The audio a session reads: the plain microphone-only stream (every chunk is the
/// microphone lane — the file source and older callers) or tagged chunks from both lanes.
pub(crate) enum AudioIn {
    Mono(Receiver<Vec<f32>>),
    Lanes(Receiver<AudioChunk>),
}

impl AudioIn {
    fn recv_timeout(&self, timeout: Duration) -> Result<AudioChunk, RecvTimeoutError> {
        match self {
            AudioIn::Mono(rx) => rx.recv_timeout(timeout).map(AudioChunk::mic),
            AudioIn::Lanes(rx) => rx.recv_timeout(timeout),
        }
    }
}

pub(crate) struct Listen {
    audio: AudioIn,
    options: ListenOptions,
}

pub(crate) enum Command {
    /// Start listening: consume chunks from this channel until it closes or `Stop` arrives.
    Listen(Listen),
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
        let echo_dedup = Arc::new(AtomicBool::new(true));
        let out = Outbox {
            tx: event_tx,
            wake,
            levels_wanted: levels_wanted.clone(),
            echo_dedup: echo_dedup.clone(),
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
            echo_dedup,
        }
    }
}

impl PipelineHandle {
    /// Begin a microphone-only listening session over `audio` (16 kHz mono f32 chunks, any
    /// chunk size; every chunk is the microphone lane).
    pub fn start(&self, audio: Receiver<Vec<f32>>) {
        let _ = self.cmd_tx.send(Command::Listen(Listen {
            audio: AudioIn::Mono(audio),
            options: ListenOptions::default(),
        }));
    }

    /// Begin a listening session over tagged chunks from both lanes (the microphone and the
    /// system-audio capture share the sender). A lane whose capture stops (its sender
    /// dropped) simply goes quiet; the session ends when every sender is gone.
    pub fn start_lanes(&self, audio: Receiver<AudioChunk>, options: ListenOptions) {
        let _ = self.cmd_tx.send(Command::Listen(Listen {
            audio: AudioIn::Lanes(audio),
            options,
        }));
    }

    /// End the listening session: the open segments are flushed and transcribed, the session
    /// row closed.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(Command::Stop);
    }

    /// Turn `Level` events on or off (default on). Off sends none and wakes nothing for them;
    /// all other events are unaffected.
    pub fn set_levels_wanted(&self, wanted: bool) {
        self.levels_wanted.store(wanted, Ordering::Relaxed);
    }

    /// Turn the echo rule on or off (default on): while on, a microphone segment that repeats
    /// an overlapping system segment is stored as its echo. The audio side decides
    /// ([`crate::DedupMode`]: off while the output is headphones).
    pub fn set_echo_dedup(&self, on: bool) {
        self.echo_dedup.store(on, Ordering::Relaxed);
    }

    /// The echo rule's switch, for a capture thread that follows the output device.
    pub fn echo_dedup_flag(&self) -> Arc<AtomicBool> {
        self.echo_dedup.clone()
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
    echo_dedup: Arc<AtomicBool>,
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
            Command::Listen(listen) => {
                let mut session = Session {
                    engine: engine.as_mut(),
                    engine_name: &engine_name,
                    store: &store,
                    out,
                };
                match session.run(config, &listen, cmd_rx) {
                    AfterListen::Idle => next = cmd_rx.recv().ok(),
                    AfterListen::Listen(l) => next = Some(Command::Listen(l)),
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
    Listen(Listen),
    Shutdown,
}

/// One lane's state within a session.
struct Lane {
    kind: AudioSourceKind,
    segmenter: Segmenter,
    /// Samples not yet making up a whole VAD frame.
    pending: Vec<f32>,
    /// Anchored when the lane's first chunk arrives (a capture can start later than the
    /// session, e.g. the system-audio tap).
    clock: Option<SampleClock>,
    /// Samples received so far.
    received: u64,
    /// `Some` when the engine has no VAD of its own.
    energy_vad: Option<EnergyVad>,
    /// RMS dBFS of the latest chunk, and when it arrived.
    last_db: f32,
    last_chunk: Option<Instant>,
    /// Any non-zero sample seen this session.
    signal: bool,
}

impl Lane {
    fn new(kind: AudioSourceKind, config: SegmenterConfig, energy: bool) -> Self {
        Self {
            kind,
            segmenter: Segmenter::new(config),
            pending: Vec::new(),
            clock: None,
            received: 0,
            energy_vad: energy.then(EnergyVad::new),
            last_db: -100.0,
            last_chunk: None,
            signal: false,
        }
    }

    /// The level to report: the latest chunk's, or silence when the lane went quiet.
    fn level(&self) -> (f32, bool) {
        match self.last_chunk {
            Some(t) if t.elapsed() < LEVEL_STALE => (self.last_db, self.segmenter.in_speech()),
            _ => (-100.0, self.segmenter.in_speech()),
        }
    }
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
        listen: &Listen,
        cmd_rx: &Receiver<Command>,
    ) -> AfterListen {
        let started_ms = now_ms();
        let started = Instant::now();
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
        let energy = engine_frame.is_none();
        let mut lanes = [
            Lane::new(AudioSourceKind::Mic, config, energy),
            Lane::new(AudioSourceKind::System, config, energy),
        ];
        let mut last_level: Option<Instant> = None;
        let mut vad_warned = false;
        let mut no_signal_reported = false;

        let after = loop {
            match listen.audio.recv_timeout(AUDIO_POLL) {
                Ok(chunk) => {
                    let lane = &mut lanes[chunk.source.index()];
                    let silent = chunk.samples.iter().all(|s| *s == 0.0);
                    if lane.kind == AudioSourceKind::System && !silent && !lane.signal {
                        log::info!("system audio: first signal");
                        if no_signal_reported {
                            no_signal_reported = false;
                            self.out.send(PipelineEvent::SystemAudio { signal: true });
                        }
                    }
                    lane.signal |= !silent;
                    self.feed(
                        lane,
                        &chunk.samples,
                        started_ms,
                        session_id,
                        frame_len,
                        &mut vad_warned,
                    );
                    lane.last_db = if silent {
                        -100.0
                    } else {
                        EnergyVad::rms_db(&chunk.samples)
                    };
                    lane.last_chunk = Some(Instant::now());

                    // an all-zero system chunk (nothing playing) wakes nobody
                    let report = chunk.source == AudioSourceKind::Mic || !silent;
                    if report
                        && self.out.levels_wanted.load(Ordering::Relaxed)
                        && last_level.is_none_or(|t| t.elapsed() >= LEVEL_INTERVAL)
                    {
                        last_level = Some(Instant::now());
                        let (rms_db_mic, in_speech_mic) = lanes[0].level();
                        let (rms_db_system, in_speech_system) = lanes[1].level();
                        self.out.send(PipelineEvent::Level {
                            rms_db_mic,
                            rms_db_system,
                            in_speech_mic,
                            in_speech_system,
                        });
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break AfterListen::Idle,
            }
            if listen.options.system
                && !no_signal_reported
                && !lanes[1].signal
                && started.elapsed() >= listen.options.no_signal_after
            {
                no_signal_reported = true;
                log::warn!(
                    "system audio: no signal {:?} after the start (nothing playing, or no \
                     System Audio Recording permission)",
                    listen.options.no_signal_after
                );
                self.out.send(PipelineEvent::SystemAudio { signal: false });
            }
            match cmd_rx.try_recv() {
                Ok(Command::Stop) => break AfterListen::Idle,
                Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                    break AfterListen::Shutdown;
                }
                Ok(Command::Listen(l)) => break AfterListen::Listen(l),
                Err(TryRecvError::Empty) => {}
            }
        };

        // system first: a microphone echo flushed after it finds it
        for lane in lanes.iter_mut().rev() {
            if let (Some(seg), Some(clock)) = (lane.segmenter.flush(), lane.clock) {
                self.finish_segment(lane.kind, session_id, &clock, &seg);
            }
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

    /// Append a chunk to its lane and run every whole VAD frame through VAD → segmenter,
    /// finishing the segments that close.
    fn feed(
        &mut self,
        lane: &mut Lane,
        samples: &[f32],
        session_started_ms: i64,
        session_id: i64,
        frame_len: usize,
        vad_warned: &mut bool,
    ) {
        let wall = now_ms();
        let mono = Instant::now();
        let chunk_ms = samples.len() as i64 * 1000 / i64::from(SAMPLE_RATE);
        let clock = lane.clock.get_or_insert_with(|| {
            // the chunk's first sample was captured about one chunk ago
            let anchor = (wall - chunk_ms).max(session_started_ms);
            let back = Duration::from_millis((wall - anchor).max(0) as u64);
            SampleClock::new(anchor, mono.checked_sub(back).unwrap_or(mono))
        });
        clock.observe(lane.received, wall, mono);
        let clock = *clock;
        lane.received += samples.len() as u64;
        lane.pending.extend_from_slice(samples);
        let mut offset = 0;
        while lane.pending.len() - offset >= frame_len {
            let frame = &lane.pending[offset..offset + frame_len];
            offset += frame_len;
            let is_speech =
                if lane.kind == AudioSourceKind::System && frame.iter().all(|s| *s == 0.0) {
                    // digital silence from the tap: nothing to classify
                    false
                } else {
                    match lane.energy_vad.as_mut() {
                        Some(vad) => vad.classify(frame).is_speech,
                        None => match self.engine.vad_lane(lane.kind, frame) {
                            Ok(v) => v.first().is_some_and(|f| f.is_speech),
                            Err(e) => {
                                if !*vad_warned {
                                    *vad_warned = true;
                                    self.out.send(PipelineEvent::Warning(e.to_string()));
                                }
                                false
                            }
                        },
                    }
                };
            if let Some(seg) = lane.segmenter.push(frame, is_speech) {
                self.finish_segment(lane.kind, session_id, &clock, &seg);
            }
        }
        lane.pending.drain(..offset);
    }

    /// Transcribe a closed segment, store it (as an echo when it repeats system audio) and
    /// report it.
    fn finish_segment(
        &mut self,
        source: AudioSourceKind,
        session_id: i64,
        clock: &SampleClock,
        seg: &Segment,
    ) {
        let started_at = clock.ms_at(seg.start_sample);
        let ended_at = clock.ms_at(seg.start_sample + seg.samples.len() as u64);
        let t = match self.engine.transcribe(&seg.samples) {
            Ok(t) => t,
            Err(e) => return self.out.send(PipelineEvent::Warning(e.to_string())),
        };
        let text = t.text.trim();
        if text.is_empty() {
            return;
        }
        log::debug!(
            "{} segment {} ms transcribed in {:.0} ms",
            source.as_str(),
            seg.duration_ms(),
            t.processing_ms
        );
        let dedup = self.out.echo_dedup.load(Ordering::Relaxed);
        let echo_of = if dedup && source == AudioSourceKind::Mic {
            self.echo_target(started_at, ended_at, text)
        } else {
            None
        };
        let row = match self.store.insert_segment_from(
            session_id,
            started_at,
            ended_at,
            text,
            t.confidence,
            source,
            echo_of,
        ) {
            Ok(row) => row,
            Err(e) => {
                return self.out.send(PipelineEvent::Warning(format!(
                    "could not save a segment: {e}"
                )));
            }
        };
        if let Some(sys) = echo_of {
            log::info!(
                "mic segment {} stored as the echo of system segment {sys}",
                row.id
            );
        }
        self.out.send(PipelineEvent::Segment(row.clone()));
        if dedup && source == AudioSourceKind::System {
            self.claim_echoes(&row);
        }
    }

    /// The system segment a new microphone segment repeats, if any.
    fn echo_target(&self, started_at: i64, ended_at: i64, text: &str) -> Option<i64> {
        match self.store.segments_near(
            AudioSourceKind::System,
            started_at - echo::ECHO_WINDOW_MS,
            ended_at + echo::ECHO_WINDOW_MS,
        ) {
            Ok(near) => echo::find_echo_of(started_at, ended_at, text, &near),
            Err(e) => {
                log::warn!("echo check: {e}");
                None
            }
        }
    }

    /// A system segment stored after the microphone heard it: the microphone rows that repeat
    /// it become its echoes (and the UI drops them).
    fn claim_echoes(&mut self, system: &TranscriptRow) {
        let near = match self.store.segments_near(
            AudioSourceKind::Mic,
            system.started_at - echo::ECHO_WINDOW_MS,
            system.ended_at + echo::ECHO_WINDOW_MS,
        ) {
            Ok(n) => n,
            Err(e) => return log::warn!("echo check: {e}"),
        };
        for mic in near {
            if echo::find_echo_of(
                mic.started_at,
                mic.ended_at,
                &mic.text,
                std::slice::from_ref(system),
            )
            .is_none()
            {
                continue;
            }
            match self.store.set_segment_echo(mic.id, Some(system.id)) {
                Ok(()) => {
                    log::info!(
                        "mic segment {} marked as the echo of system segment {}",
                        mic.id,
                        system.id
                    );
                    self.out.send(PipelineEvent::EchoMarked {
                        segment: mic.id,
                        echo_of: system.id,
                    });
                }
                Err(e) => log::warn!("echo mark: {e}"),
            }
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
