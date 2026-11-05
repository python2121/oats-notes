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
//! Before that, while the audio-level check is on ([`EchoAudioMode`], `OATS_ECHO_AUDIO`), a
//! closing microphone segment is compared with the system audio played over the same span
//! (the session keeps the last [`echo_audio::ECHO_RING_SECS`] of it): a suspected echo
//! ([`echo_audio::analyse`]) is never transcribed. It is stored as an empty mic row with
//! `echo_of` set once its system segment is stored (at once when that is already in the store;
//! overlap ± [`echo_audio::LINK_SLACK_MS`]), or discarded when none arrives within the pending
//! timeout (the computer played something that wasn't speech). Counts per session:
//! [`EchoStats`].
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
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::echo;
use crate::echo_audio::{self, EchoAudioMode, SystemRing};
use crate::engine::{
    AudioChunk, AudioSourceKind, EnergyVad, EngineFactory, SAMPLE_RATE, SpeechEngine,
};
use crate::media::{FeedReader, MediaClass, MediaFeed, MediaPolicy, MediaStamp};
use crate::segmenter::{Segment, Segmenter, SegmenterConfig};
use crate::speakers::{self, SpeakerBook};
use crate::store::{Store, TranscriptRow};

/// How long the worker waits for audio before checking commands again.
const AUDIO_POLL: Duration = Duration::from_millis(100);
/// Minimum spacing of `Level` events (~4 Hz).
const LEVEL_INTERVAL: Duration = Duration::from_millis(250);
/// A failed transcription is retried once after this pause.
const TRANSCRIBE_RETRY_DELAY: Duration = Duration::from_millis(200);

/// A lane that delivered nothing for this long reads as silent in `Level` events.
const LEVEL_STALE: Duration = Duration::from_secs(1);
/// Both captures deliver a 100 ms chunk every 100 ms, silence included, into one channel:
/// this many system chunks in a row (3 s) without a microphone chunk means the microphone's
/// capture died (the system capture keeps the channel open, so the session goes on and nobody
/// would notice). Counted in chunks, not time, so a processing stall (chunks queued) can't
/// fake it.
const MIC_LOST_AFTER_CHUNKS: u32 = 30;
/// A media play paused longer than this is closed (a resume opens a new row).
const MEDIA_PAUSE_CLOSES_MS: i64 = 120_000;
/// An open play's end is written back at least this often (a crash loses little).
const MEDIA_FLUSH_MS: i64 = 30_000;
/// Plays shorter than this (a preview, a tab switch) are not kept.
const MEDIA_MIN_PLAY_MS: i64 = 3_000;
/// Speaker recognition runs only on segments at least this long.
const SPEAKER_MIN_SEGMENT_MS: u32 = 1_000;
/// Changed voiceprints are written back at most this often (and at session end).
const SPEAKER_FLUSH_EVERY: Duration = Duration::from_secs(60);
/// "On a call" turns itself off after this long of silence on the system lane.
pub const ON_CALL_QUIET_MS: u64 = 10 * 60 * 1000;
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
    /// "On a call" ([`PipelineHandle::set_on_call`]) turned itself off: the system lane was
    /// silent for [`ON_CALL_QUIET_MS`] (`after_silence`), or the session ended.
    OnCallEnded {
        after_silence: bool,
    },
    /// The stored voices changed (a new voice was heard, or one became the user's "me"):
    /// reload the speaker labels. Sent before the `Segment` that caused it.
    SpeakersChanged,
}

/// Per-session speaker-recognition counters (logged at info when the session ends; the last
/// ended session's are [`PipelineHandle::speaker_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpeakerStats {
    /// Segments that went through the speaker models.
    pub speaker_segments: u32,
    /// Wall time spent in them, ms.
    pub speaker_ms: u64,
    /// …and in transcribing those same segments, ms (the cost to compare with).
    pub transcribe_ms: u64,
    /// Segments stored with a voice.
    pub assigned: u32,
    /// New voices opened.
    pub speakers_created: u32,
    /// Speaker-model calls that failed (the segment is stored without a voice).
    pub failed: u32,
    /// Spans close enough to learn from that taught no voice: another voice was within
    /// [`speakers::UPDATE_MARGIN`] (tagged, not learned from).
    pub ambiguous: u32,
}

/// Per-session echo counters (logged at info when the session ends; the last ended session's
/// are [`PipelineHandle::echo_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EchoStats {
    /// Microphone segments that went through the model.
    pub mic_transcribed: u32,
    /// Microphone segments the audio-level check took for an echo: not transcribed.
    pub echo_skipped_by_audio: u32,
    /// …of which stored as an (empty) echo row of a system segment.
    pub echo_linked: u32,
    /// …of which dropped: no system segment came within the timeout.
    pub echo_discarded: u32,
    /// Transcribed microphone segments the text rule marked as echoes (at insert or later).
    pub echo_marked_by_text: u32,
    /// Segments whose transcription failed twice (first try and the retry): dropped.
    pub transcribe_failed: u32,
    /// Transcriptions that failed once and worked on the retry.
    pub transcribe_retried_ok: u32,
}

/// What a listening session expects from its audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenOptions {
    /// The microphone lane is open (off for system-audio-only listening: the "microphone
    /// stopped delivering" warning must not fire then).
    pub mic: bool,
    /// System audio was asked for (the "no signal" report only runs then).
    pub system: bool,
    /// Report [`PipelineEvent::SystemAudio`] `{ signal: false }` after this long without a
    /// single non-zero system sample.
    pub no_signal_after: Duration,
}

impl Default for ListenOptions {
    fn default() -> Self {
        Self {
            mic: true,
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
    echo_audio: Arc<AtomicU8>,
    echo_pending_timeout_ms: Arc<AtomicU64>,
    echo_stats: Arc<Mutex<EchoStats>>,
    /// What plays on the system lane (written by the media resolver).
    media_feed: MediaFeed,
    media_policy: Arc<AtomicU8>,
    /// "On a call": every system-audio producer is a meeting (session-only).
    on_call: Arc<AtomicBool>,
    on_call_quiet_ms: Arc<AtomicU64>,
    /// Speaker recognition on (`OATS_SPEAKERS`, the Preferences switch).
    speakers: Arc<AtomicBool>,
    /// The stored voices were changed by someone else (the user): reload the book.
    speakers_reload: Arc<AtomicBool>,
    speaker_stats: Arc<Mutex<SpeakerStats>>,
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
        let echo_mode = EchoAudioMode::from_env();
        log::info!("audio-level echo check (OATS_ECHO_AUDIO): {echo_mode:?}");
        let echo_audio = Arc::new(AtomicU8::new(echo_mode.to_u8()));
        let echo_pending_timeout_ms = Arc::new(AtomicU64::new(echo_audio::ECHO_PENDING_TIMEOUT_MS));
        let echo_stats = Arc::new(Mutex::new(EchoStats::default()));
        let media_feed = MediaFeed::new();
        let media_policy = Arc::new(AtomicU8::new(MediaPolicy::default().to_u8()));
        let on_call = Arc::new(AtomicBool::new(false));
        let on_call_quiet_ms = Arc::new(AtomicU64::new(ON_CALL_QUIET_MS));
        let speakers_on = speakers::speakers_env().unwrap_or(true);
        log::info!(
            "speaker recognition: {}",
            if speakers_on { "on" } else { "off" }
        );
        let speakers = Arc::new(AtomicBool::new(speakers_on));
        let speakers_reload = Arc::new(AtomicBool::new(false));
        let speaker_stats = Arc::new(Mutex::new(SpeakerStats::default()));
        let out = Outbox {
            tx: event_tx,
            wake,
            levels_wanted: levels_wanted.clone(),
            echo_dedup: echo_dedup.clone(),
            echo_audio: echo_audio.clone(),
            echo_pending_timeout_ms: echo_pending_timeout_ms.clone(),
            echo_stats: echo_stats.clone(),
            media_feed: media_feed.clone(),
            media_policy: media_policy.clone(),
            on_call: on_call.clone(),
            on_call_quiet_ms: on_call_quiet_ms.clone(),
            speakers: speakers.clone(),
            speakers_reload: speakers_reload.clone(),
            speaker_stats: speaker_stats.clone(),
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
            echo_audio,
            echo_pending_timeout_ms,
            echo_stats,
            media_feed,
            media_policy,
            on_call,
            on_call_quiet_ms,
            speakers,
            speakers_reload,
            speaker_stats,
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

    /// The audio-level echo check's mode (default: `OATS_ECHO_AUDIO`, else auto = while the
    /// echo rule is on). Takes effect at the next closing microphone segment.
    pub fn set_echo_audio(&self, mode: EchoAudioMode) {
        self.echo_audio.store(mode.to_u8(), Ordering::Relaxed);
    }

    /// How long (of audio) a suspected echo waits for its system segment before it is
    /// discarded (default [`echo_audio::ECHO_PENDING_TIMEOUT_MS`]).
    pub fn set_echo_pending_timeout(&self, timeout: Duration) {
        self.echo_pending_timeout_ms
            .store(timeout.as_millis() as u64, Ordering::Relaxed);
    }

    /// The echo counters of the current session (updated as they change), or of the last one
    /// once it ended.
    pub fn echo_stats(&self) -> EchoStats {
        self.echo_stats.lock().map(|s| *s).unwrap_or_default()
    }

    /// The echo rule's switch, for a capture thread that follows the output device.
    pub fn echo_dedup_flag(&self) -> Arc<AtomicBool> {
        self.echo_dedup.clone()
    }

    /// The slot the media resolver writes what plays on the system lane into (system
    /// segments are stamped with it; YouTube / music follow the media policy).
    pub fn media_feed(&self) -> MediaFeed {
        self.media_feed.clone()
    }

    /// Change the YouTube / music policy now (each session also starts from the stored one:
    /// `meta.media_policy`, `OATS_MEDIA_POLICY` overriding — so the caller stores it too).
    pub fn set_media_policy(&self, policy: MediaPolicy) {
        self.media_policy.store(policy.to_u8(), Ordering::Relaxed);
    }

    /// The policy in force.
    pub fn media_policy(&self) -> MediaPolicy {
        MediaPolicy::from_u8(self.media_policy.load(Ordering::Relaxed))
    }

    /// "On a call" (session-only, nothing persisted): while on, whatever plays on the system
    /// lane is treated as a meeting — transcribed as `them`, stamped `meeting` (the brain's
    /// meeting hint), never media, whatever the app's class and the media policy. Turns
    /// itself off after [`ON_CALL_QUIET_MS`] of silence on the system lane and when the
    /// session ends ([`PipelineEvent::OnCallEnded`]).
    pub fn set_on_call(&self, on: bool) {
        self.on_call.store(on, Ordering::Relaxed);
    }

    pub fn on_call(&self) -> bool {
        self.on_call.load(Ordering::Relaxed)
    }

    /// How long of system-lane silence ends "On a call" (default [`ON_CALL_QUIET_MS`]; tests).
    pub fn set_on_call_timeout(&self, quiet: Duration) {
        self.on_call_quiet_ms
            .store(quiet.as_millis() as u64, Ordering::Relaxed);
    }

    /// Turn speaker recognition on or off (default: `OATS_SPEAKERS`, else on). Takes effect
    /// at the next segment; off = no speaker model calls and no voice on new segments.
    pub fn set_speakers(&self, on: bool) {
        self.speakers.store(on, Ordering::Relaxed);
    }

    pub fn speakers_enabled(&self) -> bool {
        self.speakers.load(Ordering::Relaxed)
    }

    /// The stored voices were changed outside the pipeline (renamed, flagged as me, merged,
    /// forgotten): the worker writes back its pending voiceprint updates and reloads them
    /// before the next segment.
    pub fn reload_speakers(&self) {
        self.speakers_reload.store(true, Ordering::Relaxed);
    }

    /// The speaker counters of the current session (as of its last segment), or of the last
    /// one once it ended.
    pub fn speaker_stats(&self) -> SpeakerStats {
        self.speaker_stats.lock().map(|s| *s).unwrap_or_default()
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
    echo_audio: Arc<AtomicU8>,
    echo_pending_timeout_ms: Arc<AtomicU64>,
    echo_stats: Arc<Mutex<EchoStats>>,
    media_feed: MediaFeed,
    media_policy: Arc<AtomicU8>,
    on_call: Arc<AtomicBool>,
    on_call_quiet_ms: Arc<AtomicU64>,
    speakers: Arc<AtomicBool>,
    speakers_reload: Arc<AtomicBool>,
    speaker_stats: Arc<Mutex<SpeakerStats>>,
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

    /// The inverse of [`SampleClock::ms_at`]: the sample position heard at `ms` (may be
    /// negative, before the lane started).
    fn sample_at(&self, ms: i64) -> i64 {
        self.anchor_sample as i64 + (ms - self.anchor_ms) * i64::from(SAMPLE_RATE) / 1000
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
                    echo: EchoState::default(),
                    media: MediaState::new(out.media_feed.clone()),
                    speakers: SpeakerState::default(),
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

/// A microphone segment the audio-level check took for an echo, waiting for its system
/// segment to be stored.
#[derive(Clone, Copy, Debug)]
struct PendingEcho {
    session_id: i64,
    started_at: i64,
    ended_at: i64,
    speech_ms: u32,
    peak: f32,
    lag_ms: i32,
}

/// The audio-level echo check's per-session state.
#[derive(Default)]
struct EchoState {
    /// The last 45 s of system audio, by the system lane's sample position.
    ring: SystemRing,
    /// The system lane's clock (a copy, updated with every system chunk).
    sys_clock: Option<SampleClock>,
    pending: Vec<PendingEcho>,
    stats: EchoStats,
    /// What [`PipelineHandle::echo_stats`] last saw.
    published: EchoStats,
}

/// A media play being logged (`media_plays` row `id`).
#[derive(Debug)]
struct OpenPlay {
    id: i64,
    key: String,
    title: String,
    started_ms: i64,
    /// The end of the last chunk with sound.
    last_signal_ms: i64,
    flushed_ms: i64,
    segments: i64,
}

/// The session's view of what plays on the system lane.
#[derive(Debug)]
struct MediaState {
    reader: FeedReader,
    /// YouTube / music plays and the policy skips it: the system lane runs no VAD, closes no
    /// segment.
    skip_system: bool,
    open: Option<OpenPlay>,
    /// While "On a call" is on: the system lane's time (ms) of its last sound (or of the
    /// moment the switch was first seen on).
    on_call_quiet_since: Option<i64>,
}

impl MediaState {
    fn new(feed: MediaFeed) -> Self {
        Self {
            reader: FeedReader::new(feed),
            skip_system: false,
            open: None,
            on_call_quiet_since: None,
        }
    }
}

/// The session's voiceprints ([`crate::speakers`]).
#[derive(Default)]
struct SpeakerState {
    /// Loaded from the store on the first segment that needs it (and after a reload).
    book: Option<SpeakerBook>,
    last_flush: Option<Instant>,
    stats: SpeakerStats,
}

/// One listening session's borrowed context.
struct Session<'a> {
    engine: &'a mut dyn SpeechEngine,
    engine_name: &'a str,
    store: &'a Store,
    out: &'a Outbox,
    echo: EchoState,
    media: MediaState,
    speakers: SpeakerState,
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
        // a new session counts from zero
        self.publish_echo_stats();
        self.publish_speaker_stats();
        let stored = self.store.meta(MediaPolicy::META_KEY).ok().flatten();
        let policy = MediaPolicy::effective(stored.as_deref());
        self.out
            .media_policy
            .store(policy.to_u8(), Ordering::Relaxed);
        log::info!(
            "media policy (YouTube / music on system audio): {}",
            policy.as_str()
        );
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
        let mut mic_lost_reported = false;
        let mut system_since_mic: u32 = 0;

        let after = loop {
            match listen.audio.recv_timeout(AUDIO_POLL) {
                Ok(chunk) => {
                    system_since_mic = match chunk.source {
                        AudioSourceKind::Mic => 0,
                        AudioSourceKind::System => system_since_mic.saturating_add(1),
                    };
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
                    if lane.kind == AudioSourceKind::System {
                        if !silent {
                            // the media watcher's "the call's audio goes on"
                            self.out.media_feed.note_system_signal(now_ms());
                        }
                        self.media.skip_system = self.media_skips();
                    }
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
                    if lane.kind == AudioSourceKind::System
                        && let Some(clock) = lane.clock
                    {
                        let at = clock.ms_at(lane.received);
                        self.watch_on_call(at, !silent);
                        self.track_media(at, !silent);
                    }

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
            // the microphone went quiet while system audio flows: its device is gone
            if listen.options.mic && !mic_lost_reported && system_since_mic >= MIC_LOST_AFTER_CHUNKS
            {
                mic_lost_reported = true;
                log::warn!("the microphone stopped delivering audio; system audio continues");
                self.out.send(PipelineEvent::Warning(
                    "Microphone: no audio from the input device (lost?) — only system audio \
                     is being transcribed; stop and start listening to reopen it"
                        .to_owned(),
                ));
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
        self.expire_pending(i64::MAX);
        self.close_play();
        if self.out.on_call.swap(false, Ordering::Relaxed) {
            log::info!("on a call: off (the session ended)");
            self.out.send(PipelineEvent::OnCallEnded {
                after_silence: false,
            });
        }
        self.end_echo_stats(session_id);
        self.end_speakers(session_id);
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
        if lane.kind == AudioSourceKind::System && self.echo_mode() != EchoAudioMode::Never {
            self.echo.ring.push(lane.received, samples);
            self.echo.sys_clock = Some(clock);
        }
        lane.received += samples.len() as u64;
        lane.pending.extend_from_slice(samples);
        let mut offset = 0;
        while lane.pending.len() - offset >= frame_len {
            let frame = &lane.pending[offset..offset + frame_len];
            offset += frame_len;
            let is_speech = if lane.kind == AudioSourceKind::System
                && (self.media.skip_system || frame.iter().all(|s| *s == 0.0))
            {
                // digital silence from the tap, or media the policy skips: nothing to classify
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
        if !self.echo.pending.is_empty() {
            self.expire_pending(clock.ms_at(lane.received));
        }
        if self.echo.stats != self.echo.published {
            self.publish_echo_stats();
        }
    }

    fn publish_echo_stats(&mut self) {
        self.echo.published = self.echo.stats;
        if let Ok(mut shared) = self.out.echo_stats.lock() {
            *shared = self.echo.stats;
        }
    }

    fn echo_mode(&self) -> EchoAudioMode {
        EchoAudioMode::from_u8(self.out.echo_audio.load(Ordering::Relaxed))
    }

    /// Compare a closed microphone segment with the system audio over its span (widened by
    /// the lag window): `None` when there is nothing to compare or nothing can be decided.
    fn audio_echo_check(&self, started_at: i64, seg: &Segment) -> Option<echo_audio::Verdict> {
        let clock = self.echo.sys_clock?;
        if self.echo.ring.is_empty() {
            return None;
        }
        let widen = i64::from(echo_audio::ECHO_MAX_LAG_MS) * i64::from(SAMPLE_RATE) / 1000;
        let from = clock.sample_at(started_at) - widen;
        let to = from + seg.samples.len() as i64 + 2 * widen;
        let system = self.echo.ring.slice(from, to);
        echo_audio::analyse(&seg.samples, &system)
    }

    /// A suspected echo: linked at once to a system segment already stored over its span,
    /// else kept until one is.
    fn suspect_echo(&mut self, echo: PendingEcho) {
        let near = match self.store.segments_near(
            AudioSourceKind::System,
            echo.started_at - echo_audio::LINK_SLACK_MS,
            echo.ended_at + echo_audio::LINK_SLACK_MS,
        ) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("echo check: {e}");
                Vec::new()
            }
        };
        let overlap =
            |r: &TranscriptRow| r.ended_at.min(echo.ended_at) - r.started_at.max(echo.started_at);
        match near.iter().max_by_key(|r| overlap(r)) {
            Some(system) => self.link_echo(&echo, system.id),
            None => self.echo.pending.push(echo),
        }
    }

    /// Store a suspected echo as the (empty) echo row of `system_id`. The UI hears nothing:
    /// echoes are never shown.
    fn link_echo(&mut self, echo: &PendingEcho, system_id: i64) {
        match self.store.insert_segment_from(
            echo.session_id,
            echo.started_at,
            echo.ended_at,
            "",
            None,
            AudioSourceKind::Mic,
            Some(system_id),
        ) {
            Ok(row) => {
                self.echo.stats.echo_linked += 1;
                log::debug!(
                    "mic segment {} ({} ms of speech, envelope peak {:.2} at {} ms) stored \
                     untranscribed as the echo of system segment {system_id}",
                    row.id,
                    echo.speech_ms,
                    echo.peak,
                    echo.lag_ms
                );
            }
            Err(e) => self.out.send(PipelineEvent::Warning(format!(
                "could not save a segment: {e}"
            ))),
        }
    }

    /// A system segment was stored: the suspected echoes overlapping it are its echoes.
    fn link_pending(&mut self, system: &TranscriptRow) {
        let slack = echo_audio::LINK_SLACK_MS;
        let (hits, rest): (Vec<PendingEcho>, Vec<PendingEcho>) =
            std::mem::take(&mut self.echo.pending)
                .into_iter()
                .partition(|p| {
                    p.ended_at >= system.started_at - slack
                        && p.started_at <= system.ended_at + slack
                });
        self.echo.pending = rest;
        for p in hits {
            self.link_echo(&p, system.id);
        }
    }

    /// Drop the suspected echoes that ended more than the timeout before `now_ms` (audio
    /// time; `i64::MAX` drops them all).
    fn expire_pending(&mut self, now_ms: i64) {
        let timeout = self.out.echo_pending_timeout_ms.load(Ordering::Relaxed) as i64;
        let before = self.echo.pending.len();
        self.echo
            .pending
            .retain(|p| now_ms.saturating_sub(p.ended_at) <= timeout);
        let dropped = (before - self.echo.pending.len()) as u32;
        if dropped > 0 {
            self.echo.stats.echo_discarded += dropped;
            log::debug!(
                "{dropped} suspected mic echo(es) discarded: no system segment within {timeout} \
                 ms ({:?})",
                self.echo.stats
            );
        }
    }

    /// Log the session's echo counters and publish them for [`PipelineHandle::echo_stats`].
    fn end_echo_stats(&mut self, session_id: i64) {
        let s = self.echo.stats;
        if s.transcribe_failed > 0 || s.transcribe_retried_ok > 0 {
            log::info!(
                "session {session_id} transcription: {} segments failed twice (dropped), {} \
                 recovered on the retry",
                s.transcribe_failed,
                s.transcribe_retried_ok
            );
        }
        if s != EchoStats::default() {
            log::info!(
                "session {session_id} echoes: {} mic segments transcribed, {} skipped by the \
                 audio check ({} linked, {} discarded), {} marked by the text rule",
                s.mic_transcribed,
                s.echo_skipped_by_audio,
                s.echo_linked,
                s.echo_discarded,
                s.echo_marked_by_text
            );
        }
        self.publish_echo_stats();
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
        if source == AudioSourceKind::System && self.media.skip_system {
            // media the policy doesn't transcribe (a segment open when it started playing)
            log::debug!("system segment {} ms skipped: media", seg.duration_ms());
            return;
        }
        if source == AudioSourceKind::Mic
            && self
                .echo_mode()
                .active(self.out.echo_dedup.load(Ordering::Relaxed))
            && let Some(v) = self.audio_echo_check(started_at, seg)
        {
            log::debug!(
                "mic segment {} ms: envelope peak {:.2} at {} ms, {:+.1} dB vs system, {} ms \
                 uncovered of {} ms → {}",
                seg.duration_ms(),
                v.peak,
                v.lag_ms,
                v.gain_db,
                v.uncovered_ms,
                v.active_ms,
                if v.is_echo() { "echo" } else { "transcribe" }
            );
            if v.is_echo() {
                self.echo.stats.echo_skipped_by_audio += 1;
                return self.suspect_echo(PendingEcho {
                    session_id,
                    started_at,
                    ended_at,
                    speech_ms: seg.speech_ms,
                    peak: v.peak,
                    lag_ms: v.lag_ms,
                });
            }
        }
        if source == AudioSourceKind::Mic {
            self.echo.stats.mic_transcribed += 1;
        }
        let t = match self.engine.transcribe(&seg.samples) {
            Ok(t) => t,
            Err(first) => {
                log::warn!(
                    "{} lane: transcription of a {} ms segment failed, retrying once: {first}",
                    source.as_str(),
                    seg.duration_ms()
                );
                std::thread::sleep(TRANSCRIBE_RETRY_DELAY);
                match self.engine.transcribe(&seg.samples) {
                    Ok(t) => {
                        self.echo.stats.transcribe_retried_ok += 1;
                        t
                    }
                    Err(e) => {
                        self.echo.stats.transcribe_failed += 1;
                        log::warn!(
                            "{} lane: transcription of a {} ms segment failed again, segment \
                             dropped: {e}",
                            source.as_str(),
                            seg.duration_ms()
                        );
                        return self.out.send(PipelineEvent::Warning(e.to_string()));
                    }
                }
            }
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
        let (speaker_id, voice, speaker_sure) = if echo_of.is_none() {
            self.identify_speaker(source, seg, ended_at, t.processing_ms)
        } else {
            (None, None, None)
        };
        let mut row = match self.store.insert_segment_full(
            session_id,
            started_at,
            ended_at,
            text,
            t.confidence,
            source,
            echo_of,
            speaker_id,
            speaker_sure,
        ) {
            Ok(row) => row,
            Err(e) => {
                return self.out.send(PipelineEvent::Warning(format!(
                    "could not save a segment: {e}"
                )));
            }
        };
        // the line's own embedding, kept for the user's corrections ("This line was ▸")
        if let Some((embedding, speech_ms)) = voice
            && let Err(e) = self
                .store
                .insert_segment_voice(row.id, &embedding, speech_ms, ended_at)
        {
            log::warn!("speakers: could not keep a line's voiceprint: {e}");
        }
        if let Some(sys) = echo_of {
            self.echo.stats.echo_marked_by_text += 1;
            log::info!(
                "mic segment {} stored as the echo of system segment {sys}",
                row.id
            );
        }
        if source == AudioSourceKind::System {
            self.stamp_media(&mut row);
        }
        self.out.send(PipelineEvent::Segment(row.clone()));
        if source == AudioSourceKind::System {
            if !self.echo.pending.is_empty() {
                self.link_pending(&row);
            }
            if dedup {
                self.claim_echoes(&row);
            }
        }
    }

    fn media_policy(&self) -> MediaPolicy {
        MediaPolicy::from_u8(self.out.media_policy.load(Ordering::Relaxed))
    }

    /// The dominant known voice of a transcribed, non-echo segment (see [`crate::speakers`]):
    /// only while recognition is on, for segments of ≥ [`SPEAKER_MIN_SEGMENT_MS`], and never
    /// for media on the system lane (a video's voices are nobody's voiceprint). New voices
    /// are stored at once; changed voiceprints in batches ([`SPEAKER_FLUSH_EVERY`]). With the
    /// voice, the line's own embedding and its speech (ms) to keep: the span that became the
    /// line's voice (the longest of them), else the longest span.
    fn identify_speaker(
        &mut self,
        source: AudioSourceKind,
        seg: &Segment,
        ended_at: i64,
        transcribe_ms: f64,
    ) -> (Option<i64>, Option<(Vec<f32>, i64)>, Option<bool>) {
        if !self.out.speakers.load(Ordering::Relaxed) || seg.duration_ms() < SPEAKER_MIN_SEGMENT_MS
        {
            return (None, None, None);
        }
        // system audio: media (a video, music) gets no speaker pass at all, and only a CALL
        // (a meeting app / page, or "On a call") may create NEW voices — a film clip in a
        // browser tab or an unknown app is matched against the voices already known, but its
        // speakers never become voiceprints
        let may_create = match source {
            AudioSourceKind::Mic => true,
            AudioSourceKind::System => match self.stamp_now() {
                Some(m) if m.is_media() => return (None, None, None),
                Some(m) => m.class == MediaClass::Meeting,
                None => false,
            },
        };
        if self.out.speakers_reload.swap(false, Ordering::Relaxed) {
            self.flush_speakers();
            self.speakers.book = None;
        }
        if self.speakers.book.is_none() {
            match self.store.speakers() {
                Ok(all) => {
                    log::debug!("speakers: {} known voices loaded", all.len());
                    self.speakers.book = Some(SpeakerBook::new(
                        all.iter().map(crate::speakers::Speaker::entry).collect(),
                    ));
                }
                Err(e) => {
                    log::warn!("speakers: could not load the voices: {e}");
                    return (None, None, None);
                }
            }
        }
        let t = Instant::now();
        let spans = match self.engine.speaker_spans(&seg.samples) {
            Ok(s) => s,
            Err(e) => {
                self.speakers.stats.failed += 1;
                log::warn!(
                    "{} lane: speaker recognition of a {} ms segment failed: {e}",
                    source.as_str(),
                    seg.duration_ms()
                );
                return (None, None, None);
            }
        };
        let model_ms = t.elapsed().as_millis() as u64;
        if spans.is_empty() && model_ms == 0 {
            // an engine without speaker models: nothing ran, nothing to count
            return (None, None, None);
        }
        self.speakers.stats.speaker_segments += 1;
        self.speakers.stats.speaker_ms += model_ms;
        self.speakers.stats.transcribe_ms += transcribe_ms.max(0.0) as u64;
        let mic = source == AudioSourceKind::Mic;
        let store = self.store;
        let mut assigned = Vec::with_capacity(spans.len());
        let mut created = 0u32;
        let Some(book) = self.speakers.book.as_mut() else {
            return (None, None, None);
        };
        // per span: the voice it was assigned to (index into `spans`)
        let mut span_voice: Vec<Option<i64>> = Vec::with_capacity(spans.len());
        let mut ambiguous = 0u32;
        let mut sure: Vec<i64> = Vec::new();
        for span in &spans {
            let secs = span.speech_ms as f64 / 1000.0;
            let a = book.assign(&span.embedding, secs, mic, ended_at, |centroid, secs| {
                if !may_create {
                    return None;
                }
                match store.insert_speaker(centroid, secs, if mic { secs } else { 0.0 }, ended_at) {
                    Ok(id) => Some(id),
                    Err(e) => {
                        log::warn!("speakers: could not store a new voice: {e}");
                        None
                    }
                }
            });
            span_voice.push(a.map(|a| a.speaker_id));
            if let Some(a) = a {
                if a.ambiguous {
                    ambiguous += 1;
                }
                // a sure match: the span taught the voice (or opened it)
                if a.updated || a.created {
                    sure.push(a.speaker_id);
                }
                if a.created {
                    created += 1;
                    log::info!(
                        "speakers: new voice {} ({secs:.1} s of speech)",
                        a.speaker_id
                    );
                }
                assigned.push((a.speaker_id, secs));
            }
        }
        let mut changed = created > 0;
        // a centroid that moved may now sit on top of another voice's: one person, merged
        let moved: Vec<i64> = assigned.iter().map(|(id, _)| *id).collect();
        for id in moved {
            let Some(m) = book.merge_if_close(id) else {
                continue;
            };
            match store.merge_speakers(m.from, m.into) {
                Ok(_) => {
                    for (id, _) in &mut assigned {
                        if *id == m.from {
                            *id = m.into;
                        }
                    }
                    for id in span_voice.iter_mut().flatten() {
                        if *id == m.from {
                            *id = m.into;
                        }
                    }
                    changed = true;
                    log::info!(
                        "speakers: voice {} merged into {} (centroids {:.2} apart)",
                        m.from,
                        m.into,
                        m.distance
                    );
                }
                Err(e) => log::warn!(
                    "speakers: could not merge voice {} into {}: {e}",
                    m.from,
                    m.into
                ),
            }
        }
        if mic && let Some(me) = book.auto_me() {
            match store.set_speaker_me(me) {
                Ok(true) => {
                    book.set_me(me);
                    changed = true;
                    log::info!(
                        "speakers: voice {me} is the user (the first with {} s of microphone \
                         speech)",
                        speakers::AUTO_ME_MIC_SECS
                    );
                }
                Ok(false) => {}
                Err(e) => log::warn!("speakers: could not flag voice {me} as the user: {e}"),
            }
        }
        let dominant = speakers::dominant(&assigned);
        log::debug!(
            "speakers: {} spans in {model_ms} ms ({} ms segment, transcribed in {transcribe_ms:.0} \
             ms) → {dominant:?}",
            spans.len(),
            seg.duration_ms()
        );
        self.speakers.stats.speakers_created += created;
        self.speakers.stats.ambiguous += ambiguous;
        // the line's own embedding: the dominant voice's longest span, else the longest span
        let longest = |only: Option<i64>| {
            spans
                .iter()
                .zip(&span_voice)
                .filter(|(_, v)| only.is_none() || **v == only)
                .max_by_key(|(s, _)| s.speech_ms)
                .map(|(s, _)| (s.embedding.clone(), s.speech_ms))
        };
        let voice = dominant
            .and_then(|d| longest(Some(d)))
            .or_else(|| longest(None));
        if dominant.is_some() {
            self.speakers.stats.assigned += 1;
        }
        if changed {
            self.out.send(PipelineEvent::SpeakersChanged);
        }
        if self
            .speakers
            .last_flush
            .is_none_or(|t| t.elapsed() >= SPEAKER_FLUSH_EVERY)
        {
            self.flush_speakers();
        }
        self.publish_speaker_stats();
        // sure when the dominant voice learned from (or was opened by) one of its spans
        let speaker_sure = dominant.map(|d| sure.contains(&d));
        (dominant, voice, speaker_sure)
    }

    /// Write the changed voiceprints back (one transaction).
    fn flush_speakers(&mut self) {
        self.speakers.last_flush = Some(Instant::now());
        let Some(book) = self.speakers.book.as_mut() else {
            return;
        };
        let dirty = book.take_dirty();
        if dirty.is_empty() {
            return;
        }
        match self.store.flush_speakers(&dirty) {
            Ok(()) => log::debug!("speakers: {} voiceprints written back", dirty.len()),
            Err(e) => log::warn!("speakers: could not write back the voiceprints: {e}"),
        }
    }

    fn publish_speaker_stats(&mut self) {
        if let Ok(mut shared) = self.out.speaker_stats.lock() {
            *shared = self.speakers.stats;
        }
    }

    /// Flush the voiceprints and log the session's speaker counters.
    fn end_speakers(&mut self, session_id: i64) {
        self.flush_speakers();
        // the lines' own embeddings are kept for a while only
        match self.store.prune_segment_voices(now_ms()) {
            Ok(0) => {}
            Ok(n) => log::info!("speakers: {n} line voiceprints past retention deleted"),
            Err(e) => log::warn!("speakers: could not prune the line voiceprints: {e}"),
        }
        let s = self.speakers.stats;
        if s.speaker_segments > 0 || s.failed > 0 {
            log::info!(
                "session {session_id} speakers: {} segments in {} ms (transcription of the same \
                 segments: {} ms), {} with a voice, {} new voices, {} ambiguous (not learned \
                 from), {} failed",
                s.speaker_segments,
                s.speaker_ms,
                s.transcribe_ms,
                s.assigned,
                s.speakers_created,
                s.ambiguous,
                s.failed
            );
        }
        self.publish_speaker_stats();
    }

    /// What plays on the system lane as the pipeline treats it: the resolver's stamp, or —
    /// while "On a call" is on — that stamp as a call (a bare call stamp when nothing is
    /// known).
    fn stamp_now(&mut self) -> Option<MediaStamp> {
        let stamp = self.media.reader.get().cloned();
        if !self.out.on_call.load(Ordering::Relaxed) {
            return stamp;
        }
        Some(match stamp {
            Some(s) => s.as_call(),
            None => MediaStamp::new(MediaClass::Meeting, None, "Call", ""),
        })
    }

    /// Media plays and its policy (the app's own, else the global one) doesn't transcribe it.
    fn media_skips(&mut self) -> bool {
        let policy = self.media_policy();
        self.stamp_now()
            .is_some_and(|m| m.skips_transcription(policy))
    }

    /// "On a call" ends after [`ON_CALL_QUIET_MS`] of silence on the system lane (`at_ms`:
    /// the end of the system chunk just fed; `signal`: it had sound).
    fn watch_on_call(&mut self, at_ms: i64, signal: bool) {
        if !self.out.on_call.load(Ordering::Relaxed) {
            self.media.on_call_quiet_since = None;
            return;
        }
        let quiet = self.media.on_call_quiet_since.get_or_insert(at_ms);
        if signal {
            *quiet = at_ms;
        }
        let limit = self.out.on_call_quiet_ms.load(Ordering::Relaxed) as i64;
        if at_ms - *quiet >= limit {
            self.media.on_call_quiet_since = None;
            if self.out.on_call.swap(false, Ordering::Relaxed) {
                log::info!(
                    "on a call: off after {} s of silence on system audio",
                    limit / 1000
                );
                self.out.send(PipelineEvent::OnCallEnded {
                    after_silence: true,
                });
            }
        }
    }

    /// Stamp a stored system segment with what was playing when it closed (and count it
    /// toward the open play).
    fn stamp_media(&mut self, row: &mut TranscriptRow) {
        let Some(stamp) = self.stamp_now() else {
            return;
        };
        if let Err(e) = self.store.stamp_segment_media(row.id, &stamp) {
            return log::warn!("could not stamp segment {} with its media: {e}", row.id);
        }
        row.media_kind = Some(stamp.class);
        row.media_ref = stamp.url.clone();
        if let Some(open) = self.media.open.as_mut()
            && open.key == stamp.play_key()
        {
            open.segments += 1;
        }
    }

    /// Follow the media plays on the system lane (`at_ms`: the end of the chunk just fed;
    /// `signal`: it had sound). While the policy logs plays, a YouTube / music stamp with
    /// sound opens a `media_plays` row, which is extended while it sounds and closed when
    /// the media changes, stops being media, pauses longer than [`MEDIA_PAUSE_CLOSES_MS`]
    /// or the session ends.
    fn track_media(&mut self, at_ms: i64, signal: bool) {
        let policy = self.media_policy();
        let stamp = self
            .stamp_now()
            .filter(|m| m.is_media() && m.policy_under(policy).logs_plays());
        let Some(stamp) = stamp else {
            return self.close_play();
        };
        let same = self
            .media
            .open
            .as_ref()
            .is_some_and(|o| o.key == stamp.play_key());
        if !same {
            self.close_play();
        }
        if let Some(open) = self.media.open.as_mut() {
            if signal {
                open.last_signal_ms = at_ms;
            }
            if !stamp.title.is_empty() && stamp.title != open.title {
                open.title = stamp.title.clone();
                if let Err(e) = self.store.set_media_play_title(open.id, &open.title) {
                    log::warn!("media play: {e}");
                }
            }
            if at_ms - open.last_signal_ms > MEDIA_PAUSE_CLOSES_MS {
                return self.close_play();
            }
            if open.last_signal_ms - open.flushed_ms >= MEDIA_FLUSH_MS {
                open.flushed_ms = open.last_signal_ms;
                if let Err(e) =
                    self.store
                        .update_media_play(open.id, open.last_signal_ms, open.segments)
                {
                    log::warn!("media play: {e}");
                }
            }
            return;
        }
        if !signal {
            return;
        }
        // the chunk's sound started about one chunk before its end
        let started = at_ms - 100;
        match self.store.begin_media_play(started, &stamp) {
            Ok(id) => {
                log::info!("media play {id} started ({})", stamp.class.as_str());
                self.media.open = Some(OpenPlay {
                    id,
                    key: stamp.play_key(),
                    title: stamp.title.clone(),
                    started_ms: started,
                    last_signal_ms: at_ms,
                    flushed_ms: started,
                    segments: 0,
                });
            }
            Err(e) => log::warn!("could not log a media play: {e}"),
        }
    }

    /// Close the open play (its end = its last sound); a blip shorter than
    /// [`MEDIA_MIN_PLAY_MS`] is dropped.
    fn close_play(&mut self) {
        let Some(open) = self.media.open.take() else {
            return;
        };
        let result =
            if open.last_signal_ms - open.started_ms < MEDIA_MIN_PLAY_MS && open.segments == 0 {
                self.store.delete_media_play(open.id)
            } else {
                log::info!(
                    "media play {} ended: {} s, {} segments",
                    open.id,
                    (open.last_signal_ms - open.started_ms) / 1000,
                    open.segments
                );
                self.store
                    .update_media_play(open.id, open.last_signal_ms, open.segments)
            };
        if let Err(e) = result {
            log::warn!("media play: {e}");
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
                    self.echo.stats.echo_marked_by_text += 1;
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

    /// Wraps the null engine; the first `failures` transcriptions fail.
    struct Flaky {
        failures: u32,
        calls: std::sync::Arc<std::sync::atomic::AtomicU32>,
    }

    impl SpeechEngine for Flaky {
        fn name(&self) -> String {
            "flaky".into()
        }
        fn load(&mut self, s: &mut dyn FnMut(String)) -> Result<(), crate::EngineError> {
            NullEngine.load(s)
        }
        fn vad_frame_len(&self) -> Option<usize> {
            None
        }
        fn vad(&mut self, _s: &[f32]) -> Result<Vec<crate::VadFrame>, crate::EngineError> {
            Ok(Vec::new())
        }
        fn transcribe(&mut self, s: &[f32]) -> Result<crate::Transcription, crate::EngineError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.failures {
                return Err(crate::EngineError::Transcribe("scripted failure".into()));
            }
            NullEngine.transcribe(s)
        }
    }

    /// Speak one segment into a pipeline whose engine fails `failures` times; returns the
    /// events seen up to the session end and the engine's call count.
    fn run_flaky(failures: u32, tag: &str) -> (Vec<PipelineEvent>, u32, Vec<String>) {
        let path = temp_db(tag);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = calls.clone();
        let h = Pipeline {
            engine: Box::new(move || {
                Ok(Box::new(Flaky {
                    failures,
                    calls: c2.clone(),
                }) as Box<dyn SpeechEngine>)
            }),
            store_path: path.clone(),
            segmenter: SegmenterConfig::default(),
            wake: Box::new(|| {}),
        }
        .spawn();
        wait_for(&h, "idle", |e| {
            matches!(e, PipelineEvent::Status(PipelineStatus::Idle))
        });
        let (tx, rx) = std::sync::mpsc::channel();
        h.start(rx);
        wait_for(&h, "session start", |e| {
            matches!(e, PipelineEvent::SessionStarted(_))
        });
        let mut n = 0usize;
        for i in 0..35 {
            let loud = i < 20;
            let chunk: Vec<f32> = (0..1600)
                .map(|_| {
                    n += 1;
                    if loud {
                        0.3 * (n as f32 * 2.0 * std::f32::consts::PI * 440.0 / 16_000.0).sin()
                    } else {
                        0.0
                    }
                })
                .collect();
            let _ = tx.send(chunk);
        }
        // The trailing 1.5 s of silence closes the segment on its own; wait for its outcome
        // (a stored segment, or the warning after the retry) BEFORE stopping, otherwise a
        // loaded machine can deliver `Stop` before the audio was consumed.
        let mut events = Vec::new();
        let deadline = Instant::now() + TIMEOUT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match h.recv_timeout(left) {
                Some(e @ (PipelineEvent::Segment(_) | PipelineEvent::Warning(_))) => {
                    events.push(e);
                    break;
                }
                Some(e) => events.push(e),
                None => break,
            }
        }
        h.stop();
        let deadline = Instant::now() + TIMEOUT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match h.recv_timeout(left) {
                Some(PipelineEvent::SessionEnded(_)) => break,
                Some(e) => events.push(e),
                None => break,
            }
        }
        drop(tx);
        drop(h);
        let texts = Store::open(&path)
            .and_then(|s| s.segments(View::Transcripts))
            .map(|r| r.into_iter().map(|x| x.text).collect())
            .unwrap_or_default();
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
        (events, calls.load(Ordering::SeqCst), texts)
    }

    #[test]
    fn a_transcription_that_fails_once_is_retried_without_a_warning() {
        let (events, calls, texts) = run_flaky(1, "flaky1");
        assert_eq!(calls, 2);
        assert_eq!(texts.len(), 1);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PipelineEvent::Segment(_)))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PipelineEvent::Warning(_)))
        );
    }

    #[test]
    fn a_transcription_that_fails_twice_warns_and_drops_the_segment() {
        let (events, calls, texts) = run_flaky(2, "flaky2");
        assert_eq!(calls, 2);
        assert!(texts.is_empty());
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, PipelineEvent::Segment(_)))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PipelineEvent::Warning(_)))
        );
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

        let rows = Store::open(&path).and_then(|s| s.segments(View::Transcripts));
        assert!(rows.is_ok_and(|r| r.len() == 1 && r[0].text.contains("no engine")));
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// NullEngine plus one speaker span per segment whose "voice" is the tone's pitch
    /// (zero crossings): 440 Hz and 880 Hz are two people.
    struct PitchVoices;

    impl SpeechEngine for PitchVoices {
        fn name(&self) -> String {
            "pitch voices".into()
        }
        fn load(&mut self, s: &mut dyn FnMut(String)) -> Result<(), crate::EngineError> {
            NullEngine.load(s)
        }
        fn vad_frame_len(&self) -> Option<usize> {
            None
        }
        fn vad(&mut self, _s: &[f32]) -> Result<Vec<crate::VadFrame>, crate::EngineError> {
            Ok(Vec::new())
        }
        fn transcribe(&mut self, s: &[f32]) -> Result<crate::Transcription, crate::EngineError> {
            NullEngine.transcribe(s)
        }
        fn speaker_spans(
            &mut self,
            s: &[f32],
        ) -> Result<Vec<crate::SpeakerSpan>, crate::EngineError> {
            let crossings = s
                .windows(2)
                .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
                .count();
            let voiced = s.iter().filter(|x| x.abs() > 1e-4).count().max(1);
            let hz = crossings as f32 / 2.0 / (voiced as f32 / 16_000.0);
            let mut embedding = vec![0.0f32; 8];
            embedding[if hz > 660.0 { 1 } else { 0 }] = 1.0;
            embedding[7] = 0.1;
            Ok(vec![crate::SpeakerSpan {
                start_ms: 0,
                end_ms: s.len() as i64 / 16,
                speech_ms: s.len() as i64 / 16,
                embedding,
            }])
        }
    }

    #[test]
    fn segments_carry_their_voice_and_voices_persist() {
        let path = temp_db("speakers");
        let h = Pipeline {
            engine: Box::new(|| Ok(Box::new(PitchVoices) as Box<dyn SpeechEngine>)),
            store_path: path.clone(),
            segmenter: SegmenterConfig::default(),
            wake: Box::new(|| {}),
        }
        .spawn();
        h.set_speakers(true);
        wait_for(&h, "idle", |e| {
            matches!(e, PipelineEvent::Status(PipelineStatus::Idle))
        });
        let (audio_tx, audio_rx) = std::sync::mpsc::channel();
        h.start(audio_rx);
        wait_for(&h, "session start", |e| {
            matches!(e, PipelineEvent::SessionStarted(_))
        });
        let mut n = 0usize;
        let mut chunk = |hz: f32| -> Vec<f32> {
            (0..1600)
                .map(|_| {
                    n += 1;
                    if hz > 0.0 {
                        0.3 * (n as f32 * 2.0 * std::f32::consts::PI * hz / 16_000.0).sin()
                    } else {
                        0.0
                    }
                })
                .collect()
        };
        let mut voices = Vec::new();
        let mut lines = Vec::new();
        for hz in [440.0, 880.0, 440.0] {
            // 6 s of tone: a NEW voice needs ≥ MIN_NEW_SPEAKER_SECS (5 s) of speech
            for _ in 0..60 {
                let _ = audio_tx.send(chunk(hz));
            }
            for _ in 0..15 {
                let _ = audio_tx.send(chunk(0.0));
            }
            let PipelineEvent::Segment(row) =
                wait_for(&h, "segment", |e| matches!(e, PipelineEvent::Segment(_)))
            else {
                unreachable!()
            };
            voices.push(row.speaker_id);
            lines.push(row.id);
        }
        assert!(voices.iter().all(Option::is_some), "{voices:?}");
        assert_eq!(voices[0], voices[2]);
        assert_ne!(voices[0], voices[1]);
        // off: no voice
        h.set_speakers(false);
        for _ in 0..20 {
            let _ = audio_tx.send(chunk(440.0));
        }
        for _ in 0..15 {
            let _ = audio_tx.send(chunk(0.0));
        }
        let PipelineEvent::Segment(row) =
            wait_for(&h, "segment", |e| matches!(e, PipelineEvent::Segment(_)))
        else {
            unreachable!()
        };
        assert_eq!(row.speaker_id, None);
        let unvoiced = row.id;
        h.stop();
        wait_for(&h, "session end", |e| {
            matches!(e, PipelineEvent::SessionEnded(_))
        });
        let stats = h.speaker_stats();
        assert_eq!(stats.speaker_segments, 3);
        assert_eq!(stats.speakers_created, 2);
        drop(audio_tx);
        drop(h);
        let store = Store::open(&path);
        let speakers = store.as_ref().map(|s| s.speakers());
        let speakers = match speakers {
            Ok(Ok(s)) => s,
            _ => unreachable!("store"),
        };
        assert_eq!(speakers.len(), 2);
        // each assigned line keeps its own embedding (for corrections); none while off
        let kept = |id| match store.as_ref().map(|s| s.segment_voice(id)) {
            Ok(Ok(v)) => v,
            _ => unreachable!("store"),
        };
        for id in &lines {
            let v = kept(*id);
            assert!(
                v.as_ref()
                    .is_some_and(|(e, ms)| e.len() == 8 && *ms >= 3_000),
                "line {id}: {v:?}"
            );
        }
        assert_eq!(
            kept(lines[1]).map(|(e, _)| e[1]),
            Some(1.0),
            "the 880 Hz voice"
        );
        assert!(kept(unvoiced).is_none(), "speakers off: nothing kept");
        // the first voice spoke twice: its seconds were flushed at session end
        let first = speakers.iter().find(|s| Some(s.id) == voices[0]);
        assert!(first.is_some_and(|s| s.speech_secs > 3.0), "{speakers:?}");
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
        // either the worker saw the cancel flag before it entered the slow load (at once)
        // or it was inside it and the join gave up at JOIN_TIMEOUT; it never hangs
        assert!(took <= JOIN_TIMEOUT + Duration::from_secs(1), "{took:?}");
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
