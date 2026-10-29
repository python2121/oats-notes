//! The brain's worker thread — same shape as `oats_core::pipeline`: owns its own [`Store`]
//! connection, takes [`BrainCommand`]s over a channel, reports [`BrainEvent`]s and calls `wake`
//! after each so the UI repaints. One job runs at a time, never concurrently with itself, and
//! the thread blocks (`recv_timeout` until the next due job) rather than polling.
//!
//! The tiered job model (the user wants the note-taking fairly autonomous, cheap models for the
//! basics, rare expensive passes for reorganisation):
//!
//! | tier | job | model | when |
//! |---|---|---|---|
//! | 0 reactive | hotword command | haiku | immediately when a segment with the hotword arrives |
//! | 0 reactive | categorise new segments | haiku | a few minutes after the first new segment, in batches |
//! | 1 rolling | interpret episodes | sonnet | every 15 min while uninterpreted segments exist, or on demand; an episode still "open" (speech within the gap) is **revised** on the next run with the fuller transcript |
//! | 2 review | coherence review of the day | sonnet | every 4 h, or after N new episodes, or on demand |
//! | 3 reorganise | tidy the node tree | opus | once a day in the idle window (02:00–05:00 local), or on demand |
//!
//! Budget: a daily USD cap summed from `brain_runs` (tiers 1–3 skip when hit), a
//! max-runs-per-hour guard for tiers 1–3, and `--max-budget-usd` on every CLI call. Tier 0
//! (commands, categorisation) counts toward the daily total but has its own limits so voice
//! commands keep working while interpretation pauses: at most `tier0_per_hour` runs an hour
//! and a hard ceiling of twice the daily budget. Every run is recorded with model, cost and
//! duration; structural changes land in `changes`.
//!
//! Offline resilience: listening never depends on any of this. When `claude` is missing, not
//! logged in, offline or rate limited, the brain marks itself unavailable, keeps every new
//! segment and command waiting (reporting [`BrainEvent::Backlog`]), and re-probes with
//! backoff (1 min, doubling to 15 min) with `claude --version` only — free; the first real
//! call afterwards is the probe for login and rate limits. When it is back, the backlog is
//! worked oldest-first: queued hotword commands first (dated from when they were spoken),
//! then the rolling interpretation in chronological chunks within the budget, pausing
//! between chunks. A panic inside a job is caught, recorded as a failed run and backed off;
//! the thread itself restarts after a pause if anything outside a job panics.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeZone, Timelike};
use oats_core::{BrainRun, EpisodeContent, RunOutcome, Store, TranscriptRow};

use crate::claude_cli::{ClaudeCli, CliError, Request, resolve_binary};
use crate::episodes::{self, Chunk, EpisodeRule, cut};
use crate::hotword::{self, HotwordConfig};
use crate::interpret;

/// A job tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Reactive,
    Rolling,
    Review,
    Reorganise,
}

impl Tier {
    pub fn number(self) -> i64 {
        match self {
            Tier::Reactive => 0,
            Tier::Rolling => 1,
            Tier::Review => 2,
            Tier::Reorganise => 3,
        }
    }
}

/// Model alias per tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierModels {
    pub reactive: String,
    pub rolling: String,
    pub review: String,
    pub reorganise: String,
}

impl Default for TierModels {
    fn default() -> Self {
        TierModels {
            reactive: "haiku".into(),
            rolling: "sonnet".into(),
            review: "sonnet".into(),
            reorganise: "opus".into(),
        }
    }
}

impl TierModels {
    pub fn for_tier(&self, tier: Tier) -> &str {
        match tier {
            Tier::Reactive => &self.reactive,
            Tier::Rolling => &self.rolling,
            Tier::Review => &self.review,
            Tier::Reorganise => &self.reorganise,
        }
    }
}

/// Everything configurable. Environment overrides (see [`BrainConfig::from_env`]):
/// `OATS_BRAIN=off`, `OATS_CLAUDE_BIN`, `OATS_CLAUDE_MODEL` (every tier), `OATS_BRAIN_MODEL_0..3`
/// (per tier), `OATS_INTERPRET_EVERY_MIN`, `OATS_BRAIN_BUDGET_USD`,
/// `OATS_BRAIN_TIER0_PER_HOUR`, `OATS_HOTWORDS`.
#[derive(Clone, Debug)]
pub struct BrainConfig {
    pub enabled: bool,
    pub cli: ClaudeCli,
    pub models: TierModels,
    /// Categorise new segments this long after the first one arrives.
    pub categorise_after: Duration,
    pub categorise_batch: usize,
    /// The rolling-window interpretation cadence.
    pub interpret_every: Duration,
    /// How many chunks one interpretation run interprets at most (the rest wait a minute).
    pub interpret_chunks_per_run: usize,
    /// Chunks shorter than this (characters) are filed as low-signal fragments without a call.
    pub min_chunk_chars: usize,
    pub review_every: Duration,
    /// A review also runs once this many episodes were created/revised since the last one.
    pub review_after_episodes: usize,
    pub reorganise_every: Duration,
    /// Local hours `[start, end)` of the idle window for reorganisation.
    pub reorganise_window: (u32, u32),
    pub daily_budget_usd: f64,
    /// Runs per rolling hour (all tiers counted) above which tiers 1–3 wait.
    pub max_runs_per_hour: i64,
    /// Tier 0 runs (commands + categorisation) per rolling hour; tier 0 is otherwise only
    /// stopped by the hard ceiling of twice `daily_budget_usd`.
    pub tier0_per_hour: i64,
    pub episode_rule: EpisodeRule,
    pub hotword: HotwordConfig,
    pub max_new_nodes_per_run: usize,
    /// Re-probe delay after `claude` became unavailable; doubles per failed probe up to
    /// `probe_backoff_max`, and resets after a successful call.
    pub probe_backoff_min: Duration,
    pub probe_backoff_max: Duration,
    /// Pause between interpreted chunks while working off a backlog.
    pub catchup_pause: Duration,
    /// Tries of one hotword command (failures other than unavailability) before it is given
    /// up and recorded as a warning in the change log.
    pub command_tries: u32,
    /// Delay before a failed command's first retry (doubles per try).
    pub command_retry: Duration,
    /// The user's own name, for the prompts. Refreshed from the store (`meta.user_name`) at
    /// the start of every job, so a change in the preferences applies without a restart;
    /// `OATS_USER_NAME` overrides the store.
    pub user_name: Option<String>,
}

impl Default for BrainConfig {
    fn default() -> Self {
        BrainConfig {
            enabled: true,
            cli: ClaudeCli::default(),
            models: TierModels::default(),
            categorise_after: Duration::from_secs(3 * 60),
            categorise_batch: 40,
            interpret_every: Duration::from_secs(15 * 60),
            interpret_chunks_per_run: 6,
            min_chunk_chars: 80,
            review_every: Duration::from_secs(4 * 60 * 60),
            review_after_episodes: 8,
            reorganise_every: Duration::from_secs(20 * 60 * 60),
            reorganise_window: (2, 5),
            daily_budget_usd: 2.0,
            max_runs_per_hour: 20,
            tier0_per_hour: 30,
            episode_rule: EpisodeRule::default(),
            hotword: HotwordConfig::default(),
            max_new_nodes_per_run: interpret::DEFAULT_MAX_NEW_NODES,
            probe_backoff_min: Duration::from_secs(60),
            probe_backoff_max: Duration::from_secs(15 * 60),
            catchup_pause: Duration::from_secs(2),
            command_tries: 3,
            command_retry: Duration::from_secs(60),
            user_name: None,
        }
    }
}

impl BrainConfig {
    /// Defaults with the environment applied.
    pub fn from_env() -> Self {
        let mut c = BrainConfig::default();
        if std::env::var("OATS_BRAIN").is_ok_and(|v| v.eq_ignore_ascii_case("off") || v == "0") {
            c.enabled = false;
        }
        if let Some(m) = c.cli.model.clone() {
            c.models = TierModels {
                reactive: m.clone(),
                rolling: m.clone(),
                review: m.clone(),
                reorganise: m,
            };
        }
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        if let Some(m) = env("OATS_BRAIN_MODEL_0") {
            c.models.reactive = m;
        }
        if let Some(m) = env("OATS_BRAIN_MODEL_1") {
            c.models.rolling = m;
        }
        if let Some(m) = env("OATS_BRAIN_MODEL_2") {
            c.models.review = m;
        }
        if let Some(m) = env("OATS_BRAIN_MODEL_3") {
            c.models.reorganise = m;
        }
        if let Some(min) = env("OATS_INTERPRET_EVERY_MIN").and_then(|v| v.parse::<u64>().ok()) {
            c.interpret_every = Duration::from_secs(min.max(1) * 60);
        }
        if let Some(b) = env("OATS_BRAIN_BUDGET_USD").and_then(|v| v.parse::<f64>().ok()) {
            c.daily_budget_usd = b.max(0.0);
        }
        if let Some(n) = env("OATS_BRAIN_TIER0_PER_HOUR").and_then(|v| v.parse::<i64>().ok()) {
            c.tier0_per_hour = n.max(0);
        }
        c.hotword = HotwordConfig::from_env();
        c
    }
}

/// What the UI can ask for.
#[derive(Clone, Debug)]
pub enum BrainCommand {
    /// A segment was just stored (the UI forwards `PipelineEvent::Segment`).
    SegmentArrived(TranscriptRow),
    InterpretNow,
    ReviewNow,
    ReorganiseNow,
    /// Forget an episode's interpretation and redo it. After "Mark as a call" / "Mark as
    /// media" the episode's segments carry the new stamps, so the redo follows the new kind
    /// (a call: the conversation prompt with the meeting hint; media: filed without a call).
    Reinterpret(i64),
    /// Summarise a media episode now: one interpretation with the media prompt (summary
    /// only; no decisions, action items or events). Media is otherwise filed without a model
    /// call.
    Summarise(i64),
    UndoChange(i64),
    /// "Move to…" with text the local resolver could not settle: haiku picks an existing node
    /// or proposes one new project, and the item is moved there. `current_node` is where the
    /// item is filed now (the default category for a new project).
    ResolveMove {
        target: crate::moveto::MoveTarget,
        text: String,
        current_node: Option<i64>,
    },
    Shutdown,
}

/// What the brain reports.
#[derive(Clone, Debug, PartialEq)]
pub enum BrainEvent {
    /// The CLI's version, or why it can't be used right now ("not installed", "not logged
    /// in", "offline", "rate limited", "not working"); the brain re-probes by itself.
    Availability(Result<String, String>),
    /// What is waiting for `claude`: uninterpreted segments and queued voice commands. Sent
    /// while it is unavailable (as the counts change) and while the backlog is worked off
    /// afterwards; `0, 0` when the catch-up is done.
    Backlog {
        waiting_segments: i64,
        waiting_commands: usize,
    },
    /// An on-demand command was received (`kind`: "interpret", "review", "reorganise",
    /// "reinterpret", "summarise"; `id` = the episode for the last two). It may wait behind
    /// another job; the UI shows it as pending until the matching `Settled`.
    Queued {
        kind: String,
        id: Option<i64>,
    },
    /// An on-demand command is finished with, whether or not it produced a run (nothing to
    /// do, budget hit, failure). `kind` as in `Queued`; a re-interpretation settles as
    /// "interpret" (id `None`) once the interpret job after it has run.
    Settled {
        kind: String,
        id: Option<i64>,
    },
    /// Where a job is, at phase boundaries only: `step` of `total` (1-based), `label` a short
    /// phrase ("chunk", "gathering", "asking claude", "applying").
    Progress {
        kind: String,
        step: usize,
        total: usize,
        label: String,
    },
    RunStarted {
        tier: Tier,
        kind: String,
    },
    RunFinished(BrainRun),
    /// A hotword command was executed; one line per action, and one confirmation pop-up
    /// text per thing it created (see `Executed::confirmation`).
    Command {
        segment_id: i64,
        lines: Vec<String>,
        confirmations: Vec<String>,
    },
    /// Episodes, nodes, events or changes were written: the UI should reload.
    Changed(Vec<String>),
    /// Today's spend after a run.
    Spend {
        today_usd: f64,
    },
    Warning(String),
}

/// Construction parameters.
pub struct Brain {
    pub config: BrainConfig,
    pub store_path: PathBuf,
    /// Called after every event is queued.
    pub wake: Box<dyn Fn() + Send + Sync>,
}

/// The UI's handle. Dropping it shuts the worker down.
pub struct BrainHandle {
    cmd_tx: Sender<BrainCommand>,
    events: Receiver<BrainEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Set on drop: kills an in-flight `claude` call so shutdown never waits it out.
    cancel: Arc<AtomicBool>,
}

impl Brain {
    pub fn spawn(self) -> BrainHandle {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (event_tx, events) = std::sync::mpsc::channel();
        let Brain {
            config,
            store_path,
            wake,
        } = self;
        let out = Outbox {
            tx: event_tx,
            wake: Arc::from(wake),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("oats-brain".to_owned())
            .spawn(move || supervise(config, &store_path, &cmd_rx, &out, &worker_cancel));
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                log::error!("could not spawn the brain thread: {e}");
                None
            }
        };
        BrainHandle {
            cmd_tx,
            events,
            thread,
            cancel,
        }
    }
}

impl BrainHandle {
    pub fn send(&self, cmd: BrainCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn segment_arrived(&self, row: TranscriptRow) {
        self.send(BrainCommand::SegmentArrived(row));
    }

    /// Drain pending events (once per UI frame).
    pub fn poll(&self) -> Vec<BrainEvent> {
        self.events.try_iter().collect()
    }

    /// Block for one event (tests).
    pub fn recv_timeout(&self, timeout: Duration) -> Option<BrainEvent> {
        self.events.recv_timeout(timeout).ok()
    }
}

impl Drop for BrainHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(BrainCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Clone)]
struct Outbox {
    tx: Sender<BrainEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl Outbox {
    fn send(&self, event: BrainEvent) {
        if self.tx.send(event).is_ok() {
            (self.wake)();
        }
    }
}

/// How long a job that failed (or hit a rate limit) waits before it is tried again.
const FAILURE_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// How long a command or categorisation held back by tier 0's own limits waits.
const TIER0_WAIT: Duration = Duration::from_secs(5 * 60);
/// How long the thread waits before restarting after a panic outside a job.
const RESTART_PAUSE: Duration = Duration::from_secs(60);

/// What one chunk of an interpretation run came to.
enum ChunkStep {
    /// Too short and still open: left for a later run.
    Waiting,
    /// Too short and closed: filed as a low-signal "Fragments" episode without a call.
    Fragment,
    RateLimited(String),
    Interpreted {
        resp: Box<crate::claude_cli::Response>,
        applied: Box<interpret::Applied>,
        revised: bool,
    },
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The next time the idle window `[start, end)` (local hours) is open, from `now`: `now` if
/// inside it, else today's or tomorrow's `start`.
pub fn next_window_start(now: &DateTime<Local>, window: (u32, u32)) -> DateTime<Local> {
    let (start, end) = window;
    let h = now.hour();
    if start <= h && h < end {
        return *now;
    }
    let date = if h < start {
        now.date_naive()
    } else {
        now.date_naive() + chrono::Duration::days(1)
    };
    date.and_hms_opt(start, 0, 0)
        .and_then(|t| Local.from_local_datetime(&t).earliest())
        .unwrap_or_else(|| *now + chrono::Duration::hours(24))
}

/// `Instant` for a wall-clock target (clamped to now).
fn instant_at(target_ms: i64) -> Instant {
    let delta = (target_ms - now_ms()).max(0) as u64;
    Instant::now() + Duration::from_millis(delta)
}

/// A local `DateTime` for a stored timestamp (a segment's), else now.
fn local_at(ms: i64) -> DateTime<Local> {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Local::now)
}

/// A failed job: why, and what it had already spent (earlier calls of a multi-call job
/// succeeded and were paid for; the daily budget must still see that).
struct JobError {
    msg: String,
    cost_usd: Option<f64>,
    /// A short class for logs that must not carry CLI output or model text.
    class: &'static str,
    /// The failure was `claude` being unavailable (not this job's fault): retry when it is
    /// back, don't count it against the job.
    unavailable: bool,
}

impl From<String> for JobError {
    fn from(msg: String) -> Self {
        JobError {
            msg,
            cost_usd: None,
            class: "error",
            unavailable: false,
        }
    }
}

impl From<CliError> for JobError {
    fn from(e: CliError) -> Self {
        JobError {
            msg: e.to_string(),
            cost_usd: None,
            class: e.kind(),
            unavailable: e.unavailable_reason().is_some(),
        }
    }
}

/// One job's result.
struct JobOutput {
    summary: String,
    cost_usd: Option<f64>,
    duration_ms: Option<i64>,
    model: Option<String>,
    lines: Vec<String>,
    changed: bool,
}

/// Whether `claude` can be used.
#[derive(Clone, Debug, PartialEq)]
enum Avail {
    /// Not probed yet.
    Unknown,
    Up,
    /// Unavailable for `reason`; probed again at `retry_at`.
    Down {
        reason: String,
        retry_at: Instant,
    },
}

/// A hotword segment waiting to be run as a command.
#[derive(Clone, Debug)]
struct PendingCommand {
    row: TranscriptRow,
    /// Failed tries so far (unavailability doesn't count).
    tries: u32,
    /// Not before this (a retry's backoff, tier 0's limits).
    not_before: Option<Instant>,
}

struct Worker {
    cfg: BrainConfig,
    store: Store,
    out: Outbox,
    avail: Avail,
    /// The configured binary (`OATS_CLAUDE_BIN` or the default), re-resolved on every probe;
    /// `cfg.cli.binary` holds the resolved absolute path.
    configured_binary: PathBuf,
    /// The next re-probe delay.
    probe_backoff: Duration,
    /// Working off a backlog after `claude` came back.
    catching_up: bool,
    /// The last [`BrainEvent::Backlog`] sent.
    last_backlog: Option<(i64, usize)>,
    pending_commands: VecDeque<PendingCommand>,
    reinterpret: VecDeque<i64>,
    summarise: VecDeque<i64>,
    moves: VecDeque<(crate::moveto::MoveTarget, String, Option<i64>)>,
    undo: VecDeque<i64>,
    categorise_due: Option<Instant>,
    interpret_due: Option<Instant>,
    interpret_force: bool,
    review_due: Option<Instant>,
    review_force: bool,
    reorganise_due: Option<Instant>,
    reorganise_force: bool,
    episodes_since_review: usize,
    last_budget_warning: Option<Instant>,
    cancel: Arc<AtomicBool>,
}

/// The thread body: the worker, restarted after a pause if it ever panics outside a job (a
/// job's own panic is caught in [`Worker::run_job`]). The brain thread never dies silently.
fn supervise(
    cfg: BrainConfig,
    store_path: &std::path::Path,
    cmd_rx: &Receiver<BrainCommand>,
    out: &Outbox,
    cancel: &Arc<AtomicBool>,
) {
    loop {
        let run = std::panic::catch_unwind(AssertUnwindSafe(|| {
            worker(cfg.clone(), store_path, cmd_rx, out, cancel.clone())
        }));
        let Err(panic) = run else { return };
        let what = panic_message(panic.as_ref());
        log::error!("the brain thread panicked ({what}); restarting in {RESTART_PAUSE:?}");
        out.send(BrainEvent::Warning(format!(
            "brain: internal error ({what}); restarting in a minute"
        )));
        let deadline = Instant::now() + RESTART_PAUSE;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match cmd_rx.recv_timeout(left) {
                Ok(BrainCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            }
            if cancel.load(Ordering::Relaxed) {
                return;
            }
        }
    }
}

/// The text of a panic payload.
fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".to_owned())
}

fn worker(
    cfg: BrainConfig,
    store_path: &std::path::Path,
    cmd_rx: &Receiver<BrainCommand>,
    out: &Outbox,
    cancel: Arc<AtomicBool>,
) {
    let wait_for_shutdown = || {
        while let Ok(cmd) = cmd_rx.recv() {
            if matches!(cmd, BrainCommand::Shutdown) {
                return;
            }
        }
    };
    let store = match Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            out.send(BrainEvent::Warning(format!(
                "brain: could not open the store: {e}"
            )));
            return wait_for_shutdown();
        }
    };
    if !cfg.enabled {
        out.send(BrainEvent::Availability(Err("OATS_BRAIN=off".into())));
        return wait_for_shutdown();
    }
    let mut w = Worker::new(cfg, store, out.clone(), cancel);
    w.probe();
    w.schedule_initial();
    w.run_loop(cmd_rx);
}

impl Worker {
    fn new(cfg: BrainConfig, store: Store, out: Outbox, cancel: Arc<AtomicBool>) -> Self {
        Worker {
            configured_binary: cfg.cli.binary.clone(),
            probe_backoff: cfg.probe_backoff_min,
            cfg,
            store,
            out,
            avail: Avail::Unknown,
            catching_up: false,
            last_backlog: None,
            pending_commands: VecDeque::new(),
            reinterpret: VecDeque::new(),
            summarise: VecDeque::new(),
            moves: VecDeque::new(),
            undo: VecDeque::new(),
            categorise_due: None,
            interpret_due: None,
            interpret_force: false,
            review_due: None,
            review_force: false,
            reorganise_due: None,
            reorganise_force: false,
            episodes_since_review: 0,
            last_budget_warning: None,
            cancel,
        }
    }

    // ---- availability -----------------------------------------------------------------------

    /// Resolve the binary (absolute, canonical) and run `claude --version` — free; login and
    /// rate limits only show on a real call, which is the next probe. `true` when it answered.
    fn probe(&mut self) -> bool {
        let checked = resolve_binary(&self.configured_binary).and_then(|bin| {
            if bin != self.cfg.cli.binary || self.avail == Avail::Unknown {
                log::info!("claude CLI resolved to {}", bin.display());
                self.cfg.cli.binary = bin;
            }
            self.cfg.cli.available()
        });
        match checked {
            Ok(v) => {
                self.mark_up(&v.raw);
                true
            }
            Err(e) => {
                self.mark_down(&e);
                false
            }
        }
    }

    fn cli_ready(&self) -> bool {
        self.avail == Avail::Up
    }

    /// When the next probe is due (only while unavailable).
    fn probe_due(&self) -> Option<Instant> {
        match &self.avail {
            Avail::Down { retry_at, .. } => Some(*retry_at),
            _ => None,
        }
    }

    fn mark_up(&mut self, version: &str) {
        let was = std::mem::replace(&mut self.avail, Avail::Up);
        if was == Avail::Up {
            return;
        }
        log::info!("claude CLI {version} available");
        self.out
            .send(BrainEvent::Availability(Ok(version.to_owned())));
        if matches!(was, Avail::Down { .. }) {
            // back after an outage: work the backlog off, commands first (the loop runs them
            // before any job), then interpretation, oldest first
            self.catching_up = true;
            let now = Instant::now();
            if self.store.uninterpreted_count().unwrap_or(0) > 0 {
                self.interpret_due = Some(now);
            }
            if self
                .store
                .uncategorised_segments(1)
                .is_ok_and(|v| !v.is_empty())
            {
                let at = now + self.cfg.categorise_after;
                self.categorise_due = Some(self.categorise_due.map_or(at, |d| d.min(at)));
            }
            self.report_backlog();
        }
    }

    fn mark_down(&mut self, e: &CliError) {
        let reason = e.unavailable_reason().unwrap_or("not working").to_owned();
        let wait = self.probe_backoff;
        self.probe_backoff = (wait * 2).min(self.cfg.probe_backoff_max);
        let changed = !matches!(&self.avail, Avail::Down { reason: r, .. } if *r == reason);
        self.avail = Avail::Down {
            reason: reason.clone(),
            retry_at: Instant::now() + wait,
        };
        if changed {
            log::warn!("claude unavailable ({reason}); listening continues, the brain waits");
            self.out.send(BrainEvent::Availability(Err(reason)));
        }
        log::info!("claude: next availability check in {} s", wait.as_secs());
        log::debug!("claude unavailable: {e}");
        self.report_backlog();
    }

    /// Send [`BrainEvent::Backlog`] when the counts changed — only while unavailable or
    /// catching up (in normal operation segments wait for the next rolling run by design).
    fn report_backlog(&mut self) {
        if self.cli_ready() && !self.catching_up {
            return;
        }
        let commands = self.pending_commands.len();
        let segments = (self.store.uninterpreted_count().unwrap_or(0) - commands as i64).max(0);
        self.send_backlog(segments, commands);
    }

    fn send_backlog(&mut self, segments: i64, commands: usize) {
        if self.last_backlog != Some((segments, commands)) {
            self.last_backlog = Some((segments, commands));
            self.out.send(BrainEvent::Backlog {
                waiting_segments: segments,
                waiting_commands: commands,
            });
        }
    }

    /// The catch-up ran dry (or only open fragments are left, waiting as usual).
    fn finish_catch_up(&mut self) {
        if self.catching_up && self.pending_commands.is_empty() {
            self.catching_up = false;
            log::info!("brain: backlog worked off");
            self.send_backlog(0, 0);
        }
    }

    /// Work left from before (uninterpreted segments, a reorganisation due) is scheduled at
    /// startup so the brain catches up without waiting for new speech.
    fn schedule_initial(&mut self) {
        if self
            .store
            .uncategorised_segments(1)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.categorise_due = Some(Instant::now() + self.cfg.categorise_after);
        }
        if self.store.uninterpreted_count().unwrap_or(0) > 0 {
            self.interpret_due = Some(Instant::now() + Duration::from_secs(60));
        }
        self.schedule_reorganise();
        self.schedule_review_from_store();
    }

    fn schedule_reorganise(&mut self) {
        let last = self
            .store
            .last_run_of("reorganise")
            .ok()
            .flatten()
            .map(|r| r.started_at)
            .unwrap_or(0);
        let earliest = last + self.cfg.reorganise_every.as_millis() as i64;
        let from = Local
            .timestamp_millis_opt(earliest.max(now_ms()))
            .single()
            .unwrap_or_else(Local::now);
        let at = next_window_start(&from, self.cfg.reorganise_window);
        self.reorganise_due = Some(instant_at(at.timestamp_millis()));
    }

    fn schedule_review_from_store(&mut self) {
        let last = self
            .store
            .last_run_of("review")
            .ok()
            .flatten()
            .map(|r| r.started_at)
            .unwrap_or(0);
        let next = last + self.cfg.review_every.as_millis() as i64;
        if self.review_due.is_none() {
            self.review_due = Some(instant_at(next.max(now_ms() + 60_000)));
        }
    }

    fn run_loop(&mut self, cmd_rx: &Receiver<BrainCommand>) {
        loop {
            // take what arrived while the last job ran before picking the next one, so a
            // Shutdown (or a hotword command) never waits behind a queue of due jobs
            loop {
                match cmd_rx.try_recv() {
                    Ok(cmd) => {
                        if !self.handle(cmd) {
                            return;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            let now = Instant::now();
            let due = |t: Option<Instant>| t.is_some_and(|t| t <= now);
            let up = self.cli_ready();
            if up
                && let Some(i) = self
                    .pending_commands
                    .iter()
                    .position(|p| p.not_before.is_none_or(|t| t <= now))
                && let Some(cmd) = self.pending_commands.remove(i)
            {
                self.run_command(cmd);
                continue;
            }
            if let Some(id) = self.undo.pop_front() {
                match interpret::undo_change(&self.store, id) {
                    Ok(true) => self
                        .out
                        .send(BrainEvent::Changed(vec![format!("Undid change {id}")])),
                    Ok(false) => self
                        .out
                        .send(BrainEvent::Warning(format!("change {id} cannot be undone"))),
                    Err(e) => self.out.send(BrainEvent::Warning(format!("undo: {e}"))),
                }
                continue;
            }
            if let Some(id) = self.reinterpret.pop_front() {
                if let Err(e) = self.store.reset_episode(id) {
                    self.out
                        .send(BrainEvent::Warning(format!("re-interpret: {e}")));
                    self.settled("reinterpret", Some(id));
                } else {
                    self.interpret_force = true;
                    self.interpret_due = Some(now);
                    self.out.send(BrainEvent::Changed(vec![format!(
                        "Episode {id} queued for re-interpretation"
                    )]));
                }
                continue;
            }
            if up && let Some((target, text, current)) = self.moves.pop_front() {
                self.run_move(target, &text, current);
                let (kind, id) = target.kind_and_id();
                self.settled(kind, Some(id));
                continue;
            }
            if !up {
                // unavailable: nothing runs but the (free) re-probe; due jobs simply wait
                if due(self.probe_due()) {
                    self.probe();
                    continue;
                }
            } else {
                if let Some(id) = self.summarise.pop_front() {
                    self.run_summarise(id);
                    self.settled("summarise", Some(id));
                    continue;
                }
                if due(self.categorise_due) {
                    self.categorise_due = None;
                    self.run_categorise();
                    continue;
                }
                if due(self.interpret_due) {
                    self.interpret_due = None;
                    let force = std::mem::take(&mut self.interpret_force);
                    self.run_interpret(force);
                    self.settled("interpret", None);
                    continue;
                }
                if due(self.review_due) {
                    self.review_due = None;
                    let force = std::mem::take(&mut self.review_force);
                    self.run_review(force);
                    self.settled("review", None);
                    continue;
                }
                if due(self.reorganise_due) {
                    self.reorganise_due = None;
                    let force = std::mem::take(&mut self.reorganise_force);
                    self.run_reorganise(force);
                    self.settled("reorganise", None);
                    continue;
                }
            }
            let next = if up {
                [
                    self.categorise_due,
                    self.interpret_due,
                    self.review_due,
                    self.reorganise_due,
                ]
                .into_iter()
                .chain(self.pending_commands.iter().map(|p| p.not_before))
                .flatten()
                .min()
            } else {
                self.probe_due()
            };
            let cmd = match next {
                Some(t) => match cmd_rx.recv_timeout(t.saturating_duration_since(Instant::now())) {
                    Ok(c) => c,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match cmd_rx.recv() {
                    Ok(c) => c,
                    Err(_) => return,
                },
            };
            if !self.handle(cmd) {
                return;
            }
        }
    }

    /// Apply one command; `false` = shut down.
    fn handle(&mut self, cmd: BrainCommand) -> bool {
        match cmd {
            BrainCommand::SegmentArrived(row) => self.on_segment(row),
            BrainCommand::InterpretNow => {
                self.queued("interpret", None);
                self.interpret_force = true;
                self.interpret_due = Some(Instant::now());
            }
            BrainCommand::ReviewNow => {
                self.queued("review", None);
                self.review_force = true;
                self.review_due = Some(Instant::now());
            }
            BrainCommand::ReorganiseNow => {
                self.queued("reorganise", None);
                self.reorganise_force = true;
                self.reorganise_due = Some(Instant::now());
            }
            BrainCommand::Reinterpret(id) => {
                self.queued("reinterpret", Some(id));
                self.reinterpret.push_back(id);
            }
            BrainCommand::Summarise(id) => {
                self.queued("summarise", Some(id));
                self.summarise.push_back(id);
            }
            BrainCommand::ResolveMove {
                target,
                text,
                current_node,
            } => {
                let (kind, id) = target.kind_and_id();
                self.queued(kind, Some(id));
                self.moves.push_back((target, text, current_node));
            }
            BrainCommand::UndoChange(id) => self.undo.push_back(id),
            BrainCommand::Shutdown => return false,
        }
        true
    }

    fn progress(&self, kind: &str, label: &str, step: usize, total: usize) {
        self.out.send(BrainEvent::Progress {
            kind: kind.to_owned(),
            step,
            total,
            label: label.to_owned(),
        });
    }

    fn queued(&self, kind: &str, id: Option<i64>) {
        self.out.send(BrainEvent::Queued {
            kind: kind.to_owned(),
            id,
        });
    }

    fn settled(&self, kind: &str, id: Option<i64>) {
        self.out.send(BrainEvent::Settled {
            kind: kind.to_owned(),
            id,
        });
    }

    /// Queue a hotword segment as a command (once).
    fn queue_command(&mut self, row: TranscriptRow) -> bool {
        if self.pending_commands.iter().any(|p| p.row.id == row.id) {
            return false;
        }
        self.pending_commands.push_back(PendingCommand {
            row,
            tries: 0,
            not_before: None,
        });
        true
    }

    fn on_segment(&mut self, row: TranscriptRow) {
        if hotword::can_command(&row) && hotword::detect(&row.text, &self.cfg.hotword).is_some() {
            self.queue_command(row);
            self.report_backlog();
            return;
        }
        let now = Instant::now();
        if self.categorise_due.is_none() {
            self.categorise_due = Some(now + self.cfg.categorise_after);
        }
        if self.interpret_due.is_none() {
            self.interpret_due = Some(now + self.cfg.interpret_every);
        }
        self.report_backlog();
    }

    // ---- guards ---------------------------------------------------------------------------

    /// `false` (with a warning, at most hourly) when a budget guard says wait. Tiers 1–3: the
    /// daily cap or the hourly run cap. Tier 0: its own hourly cap, or the hard ceiling of
    /// twice the daily budget (so commands keep working while interpretation pauses).
    fn within_budget(&mut self, tier: Tier) -> bool {
        let midnight = crate::dates::local_midnight_ms(now_ms());
        let spent = self.store.cost_since(midnight).unwrap_or(0.0);
        let hour_ago = now_ms() - 3_600_000;
        let over = if tier == Tier::Reactive {
            let ceiling = 2.0 * self.cfg.daily_budget_usd;
            let tier0 = self.store.run_count_of_tier_since(0, hour_ago).unwrap_or(0);
            if spent >= ceiling {
                Some(format!(
                    "brain spend ${spent:.2} reached the hard ceiling (${ceiling:.2}, twice the daily budget); voice commands paused until midnight"
                ))
            } else if tier0 >= self.cfg.tier0_per_hour {
                Some(format!(
                    "{tier0} voice commands / categorisations in the last hour; waiting"
                ))
            } else {
                None
            }
        } else {
            let runs = self.store.run_count_since(hour_ago).unwrap_or(0);
            if spent >= self.cfg.daily_budget_usd {
                Some(format!(
                    "daily brain budget reached (${spent:.2} of ${:.2}); tiers 1–3 paused until midnight",
                    self.cfg.daily_budget_usd
                ))
            } else if runs >= self.cfg.max_runs_per_hour {
                Some(format!("{runs} brain runs in the last hour; waiting"))
            } else {
                None
            }
        };
        match over {
            None => true,
            Some(msg) => {
                if self
                    .last_budget_warning
                    .is_none_or(|t| t.elapsed() > Duration::from_secs(3600))
                {
                    self.last_budget_warning = Some(Instant::now());
                    log::warn!("{msg}");
                    self.out.send(BrainEvent::Warning(msg));
                }
                false
            }
        }
    }

    /// Run one CLI request. Unavailability (missing, logged out, offline, rate limited) marks
    /// the brain unavailable and starts the re-probe backoff; a success resets the backoff.
    fn call(&mut self, req: &Request) -> Result<crate::claude_cli::Response, CliError> {
        let req = interpret::personalise(req.clone(), self.cfg.user_name.as_deref());
        let r = self.cfg.cli.run_cancellable(&req, &self.cancel);
        match &r {
            Ok(_) => self.probe_backoff = self.cfg.probe_backoff_min,
            Err(e) if e.unavailable_reason().is_some() => self.mark_down(e),
            Err(_) => {}
        }
        r
    }

    /// Re-read the user's name: `OATS_USER_NAME`, else the store's `meta.user_name`.
    fn refresh_user_name(&mut self) {
        let env = std::env::var("OATS_USER_NAME")
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        self.cfg.user_name = env.or_else(|| self.store.user_name().ok().flatten());
    }

    /// Bookkeeping around a job: the `brain_runs` row, events, spend. A panic inside the job
    /// is caught and recorded as a failed run (the caller backs off as for any failure).
    fn run_job(
        &mut self,
        tier: Tier,
        kind: &str,
        job: impl FnOnce(&mut Worker, i64) -> Result<JobOutput, JobError>,
    ) -> Result<(), JobError> {
        self.refresh_user_name();
        let model = self.cfg.models.for_tier(tier).to_owned();
        let started = now_ms();
        let run_id = match self.store.begin_run(tier.number(), kind, &model, started) {
            Ok(id) => id,
            Err(e) => {
                self.out.send(BrainEvent::Warning(format!("brain: {e}")));
                return Err(JobError::from(e.to_string()));
            }
        };
        self.out.send(BrainEvent::RunStarted {
            tier,
            kind: kind.to_owned(),
        });
        let t = Instant::now();
        let result = match std::panic::catch_unwind(AssertUnwindSafe(|| job(self, run_id))) {
            Ok(r) => r,
            Err(panic) => {
                let what = panic_message(panic.as_ref());
                log::error!("brain {kind} panicked: {what}");
                Err(JobError {
                    msg: format!("internal error (panic): {what}"),
                    cost_usd: None,
                    class: "panic",
                    unavailable: false,
                })
            }
        };
        let outcome = match &result {
            Ok(o) => RunOutcome {
                finished_at: now_ms(),
                model: o.model.clone(),
                cost_usd: o.cost_usd,
                duration_ms: o.duration_ms.or(Some(t.elapsed().as_millis() as i64)),
                ok: true,
                error: None,
                summary: o.summary.clone(),
            },
            Err(e) => RunOutcome {
                finished_at: now_ms(),
                model: None,
                cost_usd: e.cost_usd,
                duration_ms: Some(t.elapsed().as_millis() as i64),
                ok: false,
                error: Some(e.msg.clone()),
                summary: String::new(),
            },
        };
        if let Err(e) = self.store.finish_run(run_id, &outcome) {
            log::warn!("brain: could not record the run: {e}");
        }
        // info carries counts and costs only; summaries (dictated text, titles, node names)
        // and CLI output stay at debug — the full detail is in `brain_runs`
        match &result {
            Ok(o) => {
                log::info!(
                    "brain {kind} ({model}): ok, {} change line(s) — ${:.4}, {} ms",
                    o.lines.len(),
                    o.cost_usd.unwrap_or(0.0),
                    outcome.duration_ms.unwrap_or(0)
                );
                log::debug!("brain {kind}: {}", o.summary);
                if o.changed {
                    self.out.send(BrainEvent::Changed(o.lines.clone()));
                }
            }
            Err(e) => {
                log::warn!("brain {kind} failed ({})", e.class);
                log::debug!("brain {kind} failed: {}", e.msg);
                self.out
                    .send(BrainEvent::Warning(format!("{kind}: {}", e.msg)));
            }
        }
        if let Ok(runs) = self.store.runs(1)
            && let Some(run) = runs.into_iter().next()
        {
            self.out.send(BrainEvent::RunFinished(run));
        }
        let midnight = crate::dates::local_midnight_ms(now_ms());
        if let Ok(spent) = self.store.cost_since(midnight) {
            self.out.send(BrainEvent::Spend { today_usd: spent });
        }
        result.map(|_| ())
    }

    // ---- tier 0: hotword command ----------------------------------------------------------

    fn run_command(&mut self, mut cmd: PendingCommand) {
        let seg = cmd.row.clone();
        let Some(text) = hotword::detect(&seg.text, &self.cfg.hotword) else {
            return;
        };
        if !hotword::can_command(&seg) {
            return;
        }
        // already executed (the segment arrived twice, or an interpret run re-queued it after
        // it ran): notes and projects aren't deduplicated, so never run a command twice
        if let Ok(Some(stored)) = self.store.segment(seg.id)
            && stored.interpreted_at.is_some()
        {
            return;
        }
        // stored since as the echo of a system segment (the speakers said it; the system
        // segment closed after the microphone's): not the user's command
        if let Ok(Some(stored)) = self.store.segment(seg.id)
            && !hotword::can_command(&stored)
        {
            return;
        }
        if !self.within_budget(Tier::Reactive) {
            cmd.not_before = Some(Instant::now() + TIER0_WAIT);
            self.pending_commands.push_back(cmd);
            return;
        }
        let model = self.cfg.models.reactive.clone();
        // relative dates ("tomorrow") resolve against when the command was SPOKEN — which,
        // after an outage, can be long before this run
        let spoken = local_at(seg.started_at);
        let max_new_nodes = self.cfg.max_new_nodes_per_run;
        let job_seg = seg.clone();
        let r = self.run_job(Tier::Reactive, "command", move |w, _run_id| {
            let seg = job_seg;
            let tree = w.store.nodes().map_err(|e| e.to_string())?;
            let req = hotword::command_request(&tree, &text, &spoken, Some(model));
            let resp = w.call(&req).map_err(JobError::from)?;
            let result = resp
                .structured
                .as_ref()
                .ok_or_else(|| "no structured output".to_owned())
                .and_then(|v| hotword::parse_command(v).map_err(|e| e.to_string()))
                .map_err(|msg| JobError {
                    cost_usd: resp.cost_usd,
                    ..JobError::from(msg)
                })?;
            // the system segment this echoes can land while the CLI runs (the lanes close
            // their segments a moment apart): check again before anything is written
            if w.store
                .segment(seg.id)
                .ok()
                .flatten()
                .is_some_and(|r| !hotword::can_command(&r))
            {
                return Ok(JobOutput {
                    summary: "not run: an echo of system audio".to_owned(),
                    cost_usd: resp.cost_usd,
                    duration_ms: resp.duration_ms,
                    model: resp.model,
                    lines: Vec::new(),
                    changed: false,
                });
            }
            let done = w
                .store
                .with_savepoint(|s| {
                    let done = hotword::execute_capped(
                        s,
                        &tree,
                        &seg,
                        &result,
                        now_ms(),
                        &Local,
                        max_new_nodes,
                    )?;
                    s.mark_segment_consumed(seg.id, now_ms())?;
                    Ok(done)
                })
                .map_err(|e| JobError {
                    cost_usd: resp.cost_usd,
                    ..JobError::from(e.to_string())
                })?;
            let lines: Vec<String> = done.iter().map(|d| d.describe(&Local)).collect();
            w.out.send(BrainEvent::Command {
                segment_id: seg.id,
                lines: lines.clone(),
                confirmations: done.iter().filter_map(|d| d.confirmation(&Local)).collect(),
            });
            Ok(JobOutput {
                summary: lines.join("; "),
                cost_usd: resp.cost_usd,
                duration_ms: resp.duration_ms,
                model: resp.model,
                lines,
                changed: true,
            })
        });
        match r {
            Ok(()) => {}
            // claude went away: the command waits for it, untried
            Err(e) if e.unavailable => self.pending_commands.push_front(cmd),
            Err(e) => {
                cmd.tries += 1;
                if cmd.tries >= self.cfg.command_tries {
                    self.give_up_command(&seg, cmd.tries, e.class);
                } else {
                    let wait = self.cfg.command_retry * 2u32.saturating_pow(cmd.tries - 1);
                    cmd.not_before = Some(Instant::now() + wait);
                    self.pending_commands.push_back(cmd);
                }
            }
        }
        self.report_backlog();
    }

    /// "Move to…": one tier-0 call picks an existing node or proposes one new project for
    /// the typed text; the item is moved there and the result reported like a voice command
    /// (a confirmation pop-up). A failure is a warning; nothing is retried (the user's field
    /// reopens with the text).
    fn run_move(&mut self, target: crate::moveto::MoveTarget, text: &str, current: Option<i64>) {
        use crate::moveto;
        let item = match target {
            moveto::MoveTarget::Episode(id) => match self.store.episode(id) {
                Ok(Some(e)) => format!("{}\n{}", e.title, e.summary),
                _ => {
                    return self.out.send(BrainEvent::Warning(
                        "move: the note no longer exists".into(),
                    ));
                }
            },
            moveto::MoveTarget::Segment(id) => match self.store.segment(id) {
                Ok(Some(r)) => r.text,
                _ => {
                    return self.out.send(BrainEvent::Warning(
                        "move: the segment no longer exists".into(),
                    ));
                }
            },
        };
        if !self.within_budget(Tier::Reactive) {
            return self.out.send(BrainEvent::Warning(
                "move: the brain's tier-0 limit is reached; try again later".into(),
            ));
        }
        let model = self.cfg.models.reactive.clone();
        let text = text.to_owned();
        let _ = self.run_job(Tier::Reactive, "move", move |w, _run_id| {
            let tree = w.store.nodes().map_err(|e| e.to_string())?;
            let req = moveto::move_request(&tree, target, &item, current, &text, Some(model));
            let resp = w.call(&req).map_err(JobError::from)?;
            let fail = |msg: String| JobError {
                cost_usd: resp.cost_usd,
                ..JobError::from(msg)
            };
            let decision = resp
                .structured
                .as_ref()
                .ok_or_else(|| "no structured output".to_owned())
                .and_then(|v| moveto::parse_move(v).map_err(|e| e.to_string()))
                .map_err(fail)?;
            let done = moveto::execute_move(&w.store, target, &decision, now_ms()).map_err(fail)?;
            w.out.send(BrainEvent::Command {
                segment_id: match target {
                    moveto::MoveTarget::Segment(id) => id,
                    moveto::MoveTarget::Episode(_) => 0,
                },
                lines: vec![done.confirmation.clone()],
                confirmations: vec![done.confirmation.clone()],
            });
            Ok(JobOutput {
                summary: done.confirmation.clone(),
                cost_usd: resp.cost_usd,
                duration_ms: resp.duration_ms,
                model: resp.model,
                lines: vec![done.confirmation],
                changed: true,
            })
        });
    }

    /// A command that kept failing: consumed (no later pass retries it, and it never joins
    /// an episode), recorded as a warning in the change log.
    fn give_up_command(&mut self, seg: &TranscriptRow, tries: u32, class: &str) {
        let at = now_ms();
        let msg = format!("Voice command not carried out after {tries} tries ({class})");
        let recorded = self.store.with_savepoint(|s| {
            s.mark_segment_consumed(seg.id, at)?;
            s.record_change(
                None,
                "command_failed",
                &format!("Warning: {msg}"),
                &serde_json::json!({"segment_id": seg.id, "spoken_at": seg.started_at}),
                &serde_json::Value::Null,
                at,
            )
        });
        if let Err(e) = recorded {
            log::warn!("brain: could not record the failed command: {e}");
        }
        log::warn!("brain: {msg}");
        self.out.send(BrainEvent::Warning(msg));
        self.out.send(BrainEvent::Changed(vec![
            "A voice command failed".to_owned(),
        ]));
    }

    // ---- tier 0: categorisation -----------------------------------------------------------

    fn run_categorise(&mut self) {
        let batch = match self
            .store
            .uncategorised_segments(self.cfg.categorise_batch as i64)
        {
            Ok(b) => b,
            Err(e) => {
                self.out
                    .send(BrainEvent::Warning(format!("categorise: {e}")));
                return;
            }
        };
        if batch.is_empty() {
            return;
        }
        let more = batch.len() >= self.cfg.categorise_batch;
        // media (YouTube, music) is filed under the Media category without a call
        let (media, batch): (Vec<TranscriptRow>, Vec<TranscriptRow>) =
            batch.into_iter().partition(episodes::is_media_row);
        if !media.is_empty() {
            let filed = self.store.media_category().and_then(|node| {
                let filed: Vec<(i64, Option<i64>)> =
                    media.iter().map(|r| (r.id, Some(node.id))).collect();
                self.store.apply_categorisation(&filed)
            });
            match filed {
                Ok(()) => self.out.send(BrainEvent::Changed(Vec::new())),
                Err(e) => self
                    .out
                    .send(BrainEvent::Warning(format!("categorise media: {e}"))),
            }
        }
        if batch.is_empty() {
            if more {
                self.categorise_due = Some(Instant::now());
            }
            return;
        }
        if !self.cli_ready() {
            self.categorise_due = Some(Instant::now() + Duration::from_secs(30 * 60));
            return;
        }
        if !self.within_budget(Tier::Reactive) {
            self.categorise_due = Some(Instant::now() + TIER0_WAIT);
            return;
        }
        let model = self.cfg.models.reactive.clone();
        let ok = self
            .run_job(Tier::Reactive, "categorise", move |w, _| {
                let tree = w.store.nodes().map_err(|e| e.to_string())?;
                let req = interpret::categorise_request(&tree, &batch, &Local::now(), Some(model));
                let resp = w.call(&req).map_err(JobError::from)?;
                let cat: interpret::Categorisation = resp
                    .structured
                    .as_ref()
                    .ok_or_else(|| "no structured output".to_owned())
                    .and_then(|v| serde_json::from_value(v.clone()).map_err(|e| e.to_string()))?;
                let filed = interpret::apply_categorisation(&w.store, &tree, &batch, &cat)
                    .map_err(|e| e.to_string())?;
                Ok(JobOutput {
                    summary: format!("{filed} of {} segments filed", batch.len()),
                    cost_usd: resp.cost_usd,
                    duration_ms: resp.duration_ms,
                    model: resp.model,
                    lines: Vec::new(),
                    changed: filed > 0,
                })
            })
            .is_ok();
        if !ok {
            // tier 0 has no budget guard: a failing batch (bad output, refusals) must not be
            // retried — and paid for — every 20 s; try again after a pause
            self.categorise_due = Some(Instant::now() + FAILURE_BACKOFF);
        } else if more {
            self.categorise_due = Some(Instant::now() + Duration::from_secs(20));
        }
    }

    // ---- tier 1: rolling-window interpretation --------------------------------------------

    /// "Summarise" on a media episode: one interpretation of its transcript with the media
    /// prompt, revising it in place (the media rules apply: title, Media, nothing actionable).
    fn run_summarise(&mut self, id: i64) {
        let ep = match self.store.episode(id) {
            Ok(Some(ep)) if ep.is_media() => ep,
            Ok(_) => {
                return self.out.send(BrainEvent::Warning(format!(
                    "summarise: episode {id} is not a media note"
                )));
            }
            Err(e) => {
                return self
                    .out
                    .send(BrainEvent::Warning(format!("summarise: {e}")));
            }
        };
        if ep.user_edited_at.is_some() {
            return self.out.send(BrainEvent::Warning(
                "summarise: you edited this note; its text is left alone".to_owned(),
            ));
        }
        let segments = match self.store.segments_of_episode(id) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                return self.out.send(BrainEvent::Warning(
                    "summarise: no transcript was kept for this media".to_owned(),
                ));
            }
            Err(e) => {
                return self
                    .out
                    .send(BrainEvent::Warning(format!("summarise: {e}")));
            }
        };
        if !self.within_budget(Tier::Rolling) {
            return;
        }
        let chunk = Chunk {
            started_at: ep.started_at,
            ended_at: ep.ended_at,
            segments,
        };
        let model = self.cfg.models.rolling.clone();
        let _ = self.run_job(Tier::Rolling, "summarise", |w, run_id| {
            let now = now_ms();
            let tree = w.store.nodes().map_err(|e| e.to_string())?;
            let titles = interpret::media_titles(&w.store, chunk.started_at, chunk.ended_at);
            let series = interpret::series_hint(&w.store, &tree, &chunk);
            let req = interpret::interpret_request_full(
                &tree,
                &chunk,
                Some(&ep),
                &Local::now(),
                Some(model.clone()),
                &titles,
                &series,
            );
            let resp = w.call(&req).map_err(JobError::from)?;
            let interp = resp
                .structured
                .as_ref()
                .ok_or_else(|| "no structured output".to_owned())
                .and_then(|v| interpret::parse_interpretation(v).map_err(|e| e.to_string()))?;
            let model_name = resp.model.clone().unwrap_or_else(|| model.clone());
            let applied = interpret::apply_interpretation(
                &w.store,
                &tree,
                &chunk,
                &interp,
                Some(&ep),
                &model_name,
                resp.cost_usd,
                Some(run_id),
                0,
                now,
            )
            .map_err(|e| e.to_string())?;
            Ok(JobOutput {
                summary: format!("summarised '{}'", applied.episode.title),
                cost_usd: resp.cost_usd,
                duration_ms: resp.duration_ms,
                model: resp.model.clone(),
                lines: vec![format!("Summarised '{}'", applied.episode.title)],
                changed: true,
            })
        });
    }

    fn run_interpret(&mut self, force: bool) {
        let rows = match self.store.uninterpreted_segments(2_000) {
            Ok(r) => r,
            Err(e) => {
                self.out
                    .send(BrainEvent::Warning(format!("interpret: {e}")));
                return;
            }
        };
        // segments with a hotword (stored while the brain wasn't running) are commands, not
        // conversation: route them to tier 0 and keep them out of every episode
        let (commands, rows): (Vec<TranscriptRow>, Vec<TranscriptRow>) =
            rows.into_iter().partition(|r| {
                hotword::can_command(r) && hotword::detect(&r.text, &self.cfg.hotword).is_some()
            });
        let mut queued = 0usize;
        for c in commands {
            if self.queue_command(c) {
                queued += 1;
            }
        }
        if queued > 0 {
            // commands found in the backlog go first (the loop runs them before any job):
            // come back to the interpretation right after them
            self.interpret_force |= force;
            self.interpret_due = Some(Instant::now());
            self.report_backlog();
            return;
        }
        if rows.is_empty() {
            self.finish_catch_up();
            return;
        }
        if !self.cli_ready() || !self.within_budget(Tier::Rolling) {
            self.interpret_due = Some(Instant::now() + Duration::from_secs(30 * 60));
            return;
        }
        let chunks = cut(&rows, &self.cfg.episode_rule);
        let per_run = self.cfg.interpret_chunks_per_run;
        let model = self.cfg.models.rolling.clone();
        let mut leftover = false;
        let mut waiting = false;
        let mut rate_limited = false;
        let mut episodes_touched = 0usize;
        let catching_up = self.catching_up;
        let pause = self.cfg.catchup_pause;
        let ok = self
            .run_job(Tier::Rolling, "interpret", |w, run_id| {
                let mut cost = 0.0;
                let mut any_cost = false;
                let mut duration = 0;
                let mut used_model = None;
                let mut lines = Vec::new();
                let mut done = 0usize;
                let mut new_nodes = 0usize;
                for chunk in &chunks {
                    if done >= per_run || rate_limited {
                        leftover = true;
                        break;
                    }
                    w.progress(
                        "interpret",
                        "chunk",
                        (done + 1).min(chunks.len().min(per_run)),
                        chunks.len().min(per_run),
                    );
                    let step = w.interpret_chunk(chunk, force, run_id, &model, new_nodes);
                    let step = match step {
                        Ok(s) => s,
                        Err(mut e) => {
                            // what the earlier chunks cost was spent all the same
                            e.cost_usd = any_cost.then_some(cost);
                            return Err(e);
                        }
                    };
                    match step {
                        ChunkStep::Waiting => waiting = true,
                        ChunkStep::Fragment => done += 1,
                        ChunkStep::RateLimited(m) => {
                            rate_limited = true;
                            lines.push(format!("rate limited: {m}"));
                        }
                        ChunkStep::Interpreted {
                            resp,
                            applied,
                            revised,
                        } => {
                            if let Some(c) = resp.cost_usd {
                                cost += c;
                                any_cost = true;
                            }
                            duration += resp.duration_ms.unwrap_or(0);
                            used_model = resp.model.or(used_model);
                            lines.push(format!(
                                "{} '{}'{}",
                                if revised { "Revised" } else { "Interpreted" },
                                applied.episode.title,
                                if applied.episode.low_signal {
                                    " (low signal)"
                                } else {
                                    ""
                                }
                            ));
                            for n in &applied.new_nodes {
                                lines.push(format!("New {} '{}'", n.kind.as_str(), n.name));
                            }
                            new_nodes += applied.new_nodes.len();
                            episodes_touched += 1;
                            done += 1;
                            if catching_up {
                                // working off a backlog: show each chunk as it lands, then
                                // breathe before the next call
                                w.out.send(BrainEvent::Changed(Vec::new()));
                                w.report_backlog();
                                w.pause(pause);
                            }
                        }
                    }
                }
                Ok(JobOutput {
                    summary: format!("{done} of {} chunks", chunks.len()),
                    cost_usd: any_cost.then_some(cost),
                    duration_ms: Some(duration),
                    model: used_model,
                    changed: done > 0,
                    lines,
                })
            })
            .is_ok();
        self.episodes_since_review += episodes_touched;
        if !ok || rate_limited {
            // don't hammer a failing or rate-limited CLI; the segments stay uninterpreted
            // (after an outage, its end schedules the catch-up at once)
            self.interpret_due = Some(Instant::now() + FAILURE_BACKOFF);
        } else if leftover {
            self.interpret_due = Some(
                Instant::now()
                    + if self.catching_up {
                        self.cfg.catchup_pause
                    } else {
                        Duration::from_secs(60)
                    },
            );
        } else {
            self.finish_catch_up();
            if waiting && self.interpret_due.is_none() {
                // a short chunk still in progress was left for later: make sure "later"
                // comes even if no new speech arrives to schedule it
                self.interpret_due = Some(Instant::now() + self.cfg.interpret_every);
            }
        }
        if episodes_touched > 0 && self.episodes_since_review >= self.cfg.review_after_episodes {
            self.review_due = Some(Instant::now() + Duration::from_secs(60));
        }
    }

    /// Interpret one chunk of an interpretation run: file a tiny closed fragment without a
    /// call, leave a tiny open one for later, else call the model (revising the episode the
    /// chunk continues, when that episode still has room) and apply the result.
    fn interpret_chunk(
        &mut self,
        chunk: &Chunk,
        force: bool,
        run_id: i64,
        model: &str,
        new_nodes_so_far: usize,
    ) -> Result<ChunkStep, JobError> {
        let rule = self.cfg.episode_rule;
        let now = now_ms();
        // an episode that ended within a gap before this chunk continues it: revise — unless
        // it is already at the episode caps (the chunk was split off by a cap, not by a gap):
        // revising then would slide the window and overwrite the earlier part's summary
        let gap = rule.gap.as_millis() as i64;
        let max_len = rule.max_len.as_millis() as i64;
        let candidate = self
            .store
            .episodes_between(chunk.started_at - gap, chunk.started_at + 1)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|e| chunk.started_at - e.ended_at <= gap)
            // a note the user edited by hand is never content-revised: what follows it becomes
            // an episode of its own
            .filter(|e| e.user_edited_at.is_none())
            .max_by_key(|e| e.ended_at);
        let mut prior = None;
        let mut full = chunk.clone();
        if let Some(p) = candidate {
            let mut segs = self
                .store
                .segments_of_episode(p.id)
                .map_err(|e| e.to_string())?;
            // media continues only the same media, a conversation only a conversation
            let same_kind = episodes::media_key_of(&segs) == chunk.media_key();
            // a call is revised only by the same call: the joined run must still cut as one
            // episode (speech after the call ended, or another call, starts its own)
            let joined_one = {
                let mut all = segs.clone();
                all.extend(chunk.segments.iter().cloned());
                cut(&all, &rule).len() == 1
            };
            if same_kind
                && joined_one
                && segs.len() + chunk.segments.len() <= rule.max_segments
                && chunk.ended_at - p.started_at.min(chunk.started_at) <= max_len
            {
                segs.extend(chunk.segments.iter().cloned());
                full = Chunk {
                    started_at: segs
                        .first()
                        .map(|s| s.started_at)
                        .unwrap_or(chunk.started_at),
                    ended_at: chunk.ended_at,
                    segments: segs,
                };
                prior = Some(p);
            }
        }
        // media is filed under Media by its title / URL without a model call (continuing the
        // same media's episode); "Summarise" on its note runs one on demand
        if let Some(media) = interpret::media_of(&self.store, chunk) {
            interpret::file_media_without_call(&self.store, chunk, &media, prior.as_ref(), now)
                .map_err(|e| e.to_string())?;
            return Ok(ChunkStep::Fragment);
        }
        // tiny fragments aren't worth a call: a closed one is filed as low-signal, an open one
        // waits for more speech (or for a forced run)
        if prior.is_none() && full.text_len() < self.cfg.min_chunk_chars {
            if chunk.is_open(now, &rule) && !force {
                return Ok(ChunkStep::Waiting);
            }
            let content = EpisodeContent {
                title: "Fragments".into(),
                summary: "A few words, too short to interpret.".into(),
                low_signal: true,
                model: "none".into(),
                ..Default::default()
            };
            let ep = self
                .store
                .insert_episode(chunk.started_at, chunk.ended_at, &content, now)
                .map_err(|e| e.to_string())?;
            self.store
                .mark_segments_interpreted(&chunk.segment_ids(), ep.id, now)
                .map_err(|e| e.to_string())?;
            return Ok(ChunkStep::Fragment);
        }
        let tree = self.store.nodes().map_err(|e| e.to_string())?;
        let titles = interpret::media_titles(&self.store, full.started_at, full.ended_at);
        let series = interpret::series_hint(&self.store, &tree, &full);
        let req = interpret::interpret_request_full(
            &tree,
            &full,
            prior.as_ref(),
            &Local::now(),
            Some(model.to_owned()),
            &titles,
            &series,
        );
        let resp = match self.call(&req) {
            Ok(r) => r,
            Err(CliError::RateLimited(m)) => return Ok(ChunkStep::RateLimited(m)),
            Err(e) => return Err(JobError::from(e)),
        };
        let interp = resp
            .structured
            .as_ref()
            .ok_or_else(|| "no structured output".to_owned())
            .and_then(|v| interpret::parse_interpretation(v).map_err(|e| e.to_string()))?;
        let model_name = resp.model.clone().unwrap_or_else(|| model.to_owned());
        let applied = interpret::apply_interpretation(
            &self.store,
            &tree,
            &full,
            &interp,
            prior.as_ref(),
            &model_name,
            resp.cost_usd,
            Some(run_id),
            // the cap is per run, not per chunk
            self.cfg
                .max_new_nodes_per_run
                .saturating_sub(new_nodes_so_far),
            now,
        )
        .map_err(|e| e.to_string())?;
        Ok(ChunkStep::Interpreted {
            resp: Box::new(resp),
            applied: Box::new(applied),
            revised: prior.is_some(),
        })
    }

    /// Sleep `d` in short slices, giving up early when the app quits.
    fn pause(&self, d: Duration) {
        let deadline = Instant::now() + d;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }

    // ---- tier 2: coherence review -----------------------------------------------------------

    fn run_review(&mut self, force: bool) {
        let midnight = crate::dates::local_midnight_ms(now_ms());
        let episodes = self
            .store
            .episodes_between(midnight, i64::MAX)
            .unwrap_or_default();
        self.review_due = Some(Instant::now() + self.cfg.review_every);
        if !force && (episodes.len() < 2 || self.episodes_since_review == 0) {
            return;
        }
        if episodes.is_empty() {
            return;
        }
        if !self.cli_ready() || !self.within_budget(Tier::Review) {
            return;
        }
        self.episodes_since_review = 0;
        let model = self.cfg.models.review.clone();
        let _ = self.run_job(Tier::Review, "review", move |w, run_id| {
            w.progress("review", "gathering", 1, 3);
            let tree = w.store.nodes().map_err(|e| e.to_string())?;
            let events = w
                .store
                .events_from(midnight, 200)
                .map_err(|e| e.to_string())?;
            let req =
                interpret::review_request(&tree, &episodes, &events, &Local::now(), Some(model));
            w.progress("review", "asking claude", 2, 3);
            let resp = w.call(&req).map_err(JobError::from)?;
            let actions: interpret::Actions = resp
                .structured
                .as_ref()
                .ok_or_else(|| "no structured output".to_owned())
                .and_then(|v| serde_json::from_value(v.clone()).map_err(|e| e.to_string()))?;
            w.progress("review", "applying", 3, 3);
            let lines = interpret::apply_review(
                &w.store,
                &tree,
                &episodes,
                &actions,
                Some(run_id),
                now_ms(),
            )
            .map_err(|e| e.to_string())?;
            Ok(JobOutput {
                summary: format!("{} changes over {} episodes", lines.len(), episodes.len()),
                cost_usd: resp.cost_usd,
                duration_ms: resp.duration_ms,
                model: resp.model,
                changed: !lines.is_empty(),
                lines,
            })
        });
    }

    // ---- tier 3: reorganisation -------------------------------------------------------------

    fn run_reorganise(&mut self, force: bool) {
        // the next attempt is a full period from THIS one, whatever happens below. (Scheduling
        // from the last recorded run, as at startup, would find no newer run when this one is
        // skipped — no projects yet, budget, CLI — and make it due again at once: a busy loop
        // through the whole idle window; and when it does run, a second run right after.)
        let every = chrono::Duration::milliseconds(self.cfg.reorganise_every.as_millis() as i64);
        let next = next_window_start(&(Local::now() + every), self.cfg.reorganise_window);
        self.reorganise_due = Some(instant_at(next.timestamp_millis()));
        let nodes = self.store.nodes().unwrap_or_default();
        let has_projects = nodes.iter().any(|n| n.parent_id.is_some());
        if !has_projects && !force {
            return;
        }
        if !self.cli_ready() || !self.within_budget(Tier::Reorganise) {
            return;
        }
        let model = self.cfg.models.reorganise.clone();
        let _ = self.run_job(Tier::Reorganise, "reorganise", move |w, run_id| {
            w.progress("reorganise", "gathering", 1, 3);
            let usage = w.store.node_usage().map_err(|e| e.to_string())?;
            let episodes = w.store.episodes(60).map_err(|e| e.to_string())?;
            let req = interpret::reorganise_request(
                &nodes,
                &usage,
                &episodes,
                &Local::now(),
                Some(model),
            );
            w.progress("reorganise", "asking claude", 2, 3);
            let resp = w.call(&req).map_err(JobError::from)?;
            let actions: interpret::Actions = resp
                .structured
                .as_ref()
                .ok_or_else(|| "no structured output".to_owned())
                .and_then(|v| serde_json::from_value(v.clone()).map_err(|e| e.to_string()))?;
            w.progress("reorganise", "applying", 3, 3);
            let lines =
                interpret::apply_reorganisation(&w.store, &nodes, &actions, Some(run_id), now_ms())
                    .map_err(|e| e.to_string())?;
            Ok(JobOutput {
                summary: if lines.is_empty() {
                    "nothing to reorganise".to_owned()
                } else {
                    format!("Reorganised: {}", lines.join(", "))
                },
                cost_usd: resp.cost_usd,
                duration_ms: resp.duration_ms,
                model: resp.model,
                changed: !lines.is_empty(),
                lines,
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oats_core::{RunOutcome, View};

    #[test]
    fn idle_window_is_today_or_tomorrow() {
        let at = |h: u32| {
            Local
                .with_ymd_and_hms(2026, 10, 28, h, 30, 0)
                .single()
                .unwrap_or_else(Local::now)
        };
        let w = (2, 5);
        assert_eq!(next_window_start(&at(3), w), at(3));
        let early = next_window_start(&at(1), w);
        assert_eq!(
            (early.day_of_month(), early.hour(), early.minute()),
            (28, 2, 0)
        );
        let late = next_window_start(&at(14), w);
        assert_eq!((late.day_of_month(), late.hour()), (29, 2));
    }

    trait Day {
        fn day_of_month(&self) -> u32;
    }
    impl Day for DateTime<Local> {
        fn day_of_month(&self) -> u32 {
            chrono::Datelike::day(self)
        }
    }

    #[test]
    fn config_from_env_defaults() {
        let c = BrainConfig::default();
        assert_eq!(c.models.for_tier(Tier::Reactive), "haiku");
        assert_eq!(c.models.for_tier(Tier::Rolling), "sonnet");
        assert_eq!(c.models.for_tier(Tier::Reorganise), "opus");
        assert_eq!(Tier::Review.number(), 2);
        assert_eq!(c.interpret_every, Duration::from_secs(900));
    }

    /// A fake `claude` that answers by schema: an interpretation, a categorisation or a
    /// command, so one script serves every job.
    fn fake_cli(dir: &std::path::Path) -> ClaudeCli {
        let script = dir.join("claude");
        let body = r#"#!/bin/sh
ARGS="$*"
cat > /dev/null
case "$ARGS" in
  *--version*) echo "2.1.312 (Claude Code)"; exit 0;;
  *low_signal*) echo '{"type":"result","is_error":false,"result":"","total_cost_usd":0.011,"duration_ms":900,"structured_output":{"title":"Exporter planning","summary":"- ship Friday","low_signal":false,"node_id":1,"new_project":{"parent_id":1,"name":"Exporter","description":"csv"},"new_topic":null,"decisions":[],"action_items":[{"text":"tests","owner":"Sam","due":null}],"topics":["exporter"],"events":[]}}';;
  *intent*) echo '{"type":"result","is_error":false,"result":"","total_cost_usd":0.002,"duration_ms":300,"structured_output":{"actions":[{"intent":"add_event","title":"Property taxes due","date":"2026-12-12","time":null,"all_day":true,"relative_to_event":false,"offset_days":null,"text":null,"details":"","node_name":null,"category_name":null},{"intent":"add_reminder","title":null,"date":null,"time":null,"all_day":true,"relative_to_event":true,"offset_days":-7,"text":null,"details":null,"node_name":null,"category_name":null}]}}';;
  *segment_id*) echo '{"type":"result","is_error":false,"result":"","total_cost_usd":0.001,"duration_ms":200,"structured_output":{"items":[]}}';;
  *) echo '{"type":"result","is_error":true,"result":"unexpected request"}';;
esac
"#;
        let _ = std::fs::write(&script, body);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
        ClaudeCli {
            binary: script,
            model: None,
            timeout: Duration::from_secs(10),
            max_budget_usd: None,
            work_dir: Some(dir.join("work")),
            isolate: true,
        }
    }

    fn wait_for(h: &BrainHandle, what: &str, pred: impl Fn(&BrainEvent) -> bool) -> BrainEvent {
        let deadline = Instant::now() + Duration::from_secs(15);
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
    fn worker_runs_interpret_now_and_hotword_commands() {
        let dir = std::env::temp_dir().join(format!(
            "oats-brain-sched-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("oats.sqlite");
        let (seg_hot, seg_plain) = {
            let s = Store::open(&db).unwrap();
            let sid = s.begin_session(0, "e").unwrap();
            let t = now_ms() - 60 * 60 * 1000;
            let a = s.insert_segment(sid, t, t + 5_000, "let's ship the exporter on Friday, Sam writes the tests and we review on Monday morning before the customer call", None).unwrap();
            let hot = s.insert_segment(sid, t + 10_000, t + 15_000, "hey computer property taxes are due on 12/12/2026 also set a reminder one week ahead", None).unwrap();
            (hot, a)
        };
        let cfg = BrainConfig {
            cli: fake_cli(&dir),
            ..BrainConfig::default()
        };
        let h = Brain {
            config: cfg,
            store_path: db.clone(),
            wake: Box::new(|| {}),
        }
        .spawn();
        wait_for(&h, "availability", |e| {
            matches!(e, BrainEvent::Availability(Ok(_)))
        });

        // the hotword segment was in the store already: the interpret run routes it to tier 0
        // by itself, and the command runs BEFORE the interpretation goes on
        h.send(BrainCommand::InterpretNow);
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !seen
            .iter()
            .any(|e| matches!(e, BrainEvent::RunFinished(r) if r.kind == "interpret"))
        {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            match h.recv_timeout(left) {
                Some(ev) => seen.push(ev),
                None => unreachable!("timed out: {seen:?}"),
            }
        }
        let kinds: Vec<&str> = seen
            .iter()
            .filter_map(|e| match e {
                BrainEvent::RunFinished(r) => Some(r.kind.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec!["command", "interpret"]);
        let Some(BrainEvent::RunFinished(run)) = seen
            .iter()
            .find(|e| matches!(e, BrainEvent::RunFinished(r) if r.kind == "interpret"))
        else {
            unreachable!()
        };
        assert!(run.ok, "{run:?}");
        assert_eq!(run.tier, 1);
        assert!((run.cost_usd.unwrap_or(0.0) - 0.011).abs() < 1e-9);
        // a second arrival of the same segment must not run it twice
        h.segment_arrived(seg_hot.clone());
        let Some(BrainEvent::Command {
            segment_id,
            lines,
            confirmations,
        }) = seen
            .iter()
            .find(|e| matches!(e, BrainEvent::Command { .. }))
            .cloned()
        else {
            unreachable!("{seen:?}")
        };
        assert_eq!(segment_id, seg_hot.id);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("Event: Property taxes due on"));
        assert!(
            lines[1].starts_with("Todo: Property taxes due in one week · due"),
            "{lines:?}"
        );
        assert!(
            confirmations[0].starts_with("Event added: Property taxes due — "),
            "{confirmations:?}"
        );
        assert!(
            confirmations[1].starts_with("Reminder set: Property taxes due in one week — "),
            "{confirmations:?}"
        );
        wait_for(
            &h,
            "spend",
            |e| matches!(e, BrainEvent::Spend { today_usd } if *today_usd > 0.012),
        );
        std::thread::sleep(Duration::from_millis(300));
        drop(h);

        let s = Store::open(&db).unwrap();
        let eps = s.episodes(10).unwrap();
        assert!(
            eps.iter().any(|e| e.title == "Exporter planning"),
            "{eps:?}"
        );
        assert!(s.nodes().unwrap().iter().any(|n| n.name == "Exporter"));
        assert_eq!(s.events_from(0, 10).unwrap().len(), 1);
        // the "one week ahead" reminder and the event's own todo for the day
        assert_eq!(s.todos(oats_core::TodoFilter::Open).unwrap().len(), 2);
        assert_eq!(
            s.segments(View::Transcripts)
                .unwrap()
                .iter()
                .filter(|r| r.interpreted_at.is_some())
                .count(),
            2
        );
        assert!(
            s.segment(seg_plain.id)
                .unwrap()
                .unwrap()
                .episode_id
                .is_some()
        );
        assert_eq!(
            s.segment(seg_hot.id).unwrap().unwrap().episode_id,
            None,
            "the command segment joins no episode"
        );
        assert!(s.runs(10).unwrap().iter().all(|r| r.ok));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oats-brain-sched-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// A fake `claude` from a shell body (`--version` always answers).
    fn script_cli(dir: &std::path::Path, body: &str) -> ClaudeCli {
        let script = dir.join("claude");
        let _ = std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$*\" in *--version*) echo '2.1.312 (Claude Code)'; exit 0;; esac\ncat > /dev/null\n{body}\n"
            ),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
        }
        ClaudeCli {
            binary: script,
            model: None,
            timeout: Duration::from_secs(20),
            max_budget_usd: None,
            work_dir: Some(dir.join("work")),
            isolate: true,
        }
    }

    /// A worker on an in-memory store, outside any thread (its events are dropped).
    fn bare_worker(cli: ClaudeCli) -> Worker {
        bare_worker_with_events(cli).0
    }

    /// [`bare_worker`], keeping its events.
    fn bare_worker_with_events(cli: ClaudeCli) -> (Worker, Receiver<BrainEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut w = Worker::new(
            BrainConfig {
                cli,
                ..BrainConfig::default()
            },
            Store::open_in_memory().unwrap(),
            Outbox {
                tx,
                wake: Arc::new(|| {}),
            },
            Arc::new(AtomicBool::new(false)),
        );
        w.avail = Avail::Up;
        (w, rx)
    }

    const LONG_TEXT: &str =
        "let's ship the exporter on Friday, Sam writes the tests and we review on Monday morning";

    #[test]
    fn a_skipped_reorganisation_is_not_due_again_at_once() {
        let dir = temp_dir("reorg");
        let mut w = bare_worker(script_cli(&dir, "exit 9"));
        // only the default categories: nothing to reorganise, the run is skipped
        let before = Instant::now();
        w.run_reorganise(false);
        assert!(
            w.reorganise_due
                .is_some_and(|d| d >= before + w.cfg.reorganise_every - Duration::from_secs(5)),
            "{:?}",
            w.reorganise_due
                .map(|d| d.saturating_duration_since(before))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_open_short_chunk_waits_without_a_call() {
        let dir = temp_dir("wait");
        // any model call fails the run
        let mut w = bare_worker(script_cli(&dir, "exit 9"));
        let sid = w.store.begin_session(0, "e").unwrap();
        let t = now_ms() - 60_000;
        w.store
            .insert_segment(sid, t, t + 2_000, "uh huh", None)
            .unwrap();
        w.run_interpret(false);
        let runs = w.store.runs(10).unwrap();
        assert!(runs.iter().all(|r| r.ok), "{runs:?}");
        assert_eq!(w.store.uninterpreted_count().unwrap(), 1);
        assert!(w.interpret_due.is_some(), "a later run must be scheduled");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_run_keeps_what_earlier_chunks_cost_and_backs_off() {
        let dir = temp_dir("partial");
        let counter = dir.join("n");
        // first call succeeds ($0.011), every later call fails
        let ok = r#"{"type":"result","is_error":false,"result":"","total_cost_usd":0.011,"structured_output":{"title":"A","summary":"s","low_signal":false,"node_id":null,"new_project":null,"new_topic":null,"decisions":[],"action_items":[],"topics":[],"events":[]}}"#;
        let body = format!(
            "if [ -f '{c}' ]; then echo boom >&2; exit 3; fi\ntouch '{c}'\necho '{ok}'",
            c = counter.display()
        );
        let mut w = bare_worker(script_cli(&dir, &body));
        let sid = w.store.begin_session(0, "e").unwrap();
        // two chunks a day apart, both closed and long enough to need a call
        let t = now_ms() - 2 * 86_400_000;
        w.store
            .insert_segment(sid, t, t + 5_000, LONG_TEXT, None)
            .unwrap();
        w.store
            .insert_segment(sid, t + 86_400_000, t + 86_405_000, LONG_TEXT, None)
            .unwrap();
        let before = Instant::now();
        w.run_interpret(false);
        let runs = w.store.runs(10).unwrap();
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].ok);
        assert!(
            (runs[0].cost_usd.unwrap_or(0.0) - 0.011).abs() < 1e-9,
            "{runs:?}"
        );
        assert!(
            w.interpret_due
                .is_some_and(|d| d >= before + FAILURE_BACKOFF - Duration::from_secs(1))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_categorisation_is_not_retried_every_20_seconds() {
        let dir = temp_dir("catfail");
        let mut w = bare_worker(script_cli(&dir, "echo 'not json'"));
        w.cfg.categorise_batch = 2;
        let sid = w.store.begin_session(0, "e").unwrap();
        for i in 0..3 {
            w.store
                .insert_segment(sid, i * 1000, i * 1000 + 500, LONG_TEXT, None)
                .unwrap();
        }
        let before = Instant::now();
        w.run_categorise();
        assert!(
            w.categorise_due
                .is_some_and(|d| d >= before + FAILURE_BACKOFF - Duration::from_secs(1))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_episode_at_the_segment_cap_is_not_revised_by_its_continuation() {
        let dir = temp_dir("cap");
        let ok = r#"{"type":"result","is_error":false,"result":"","total_cost_usd":0.01,"structured_output":{"title":"Next part","summary":"s","low_signal":false,"node_id":null,"new_project":null,"new_topic":null,"decisions":[],"action_items":[],"topics":[],"events":[]}}"#;
        let mut w = bare_worker(script_cli(&dir, &format!("echo '{ok}'")));
        w.cfg.episode_rule.max_segments = 2;
        let sid = w.store.begin_session(0, "e").unwrap();
        let t = now_ms() - 86_400_000;
        let a = w
            .store
            .insert_segment(sid, t, t + 5_000, LONG_TEXT, None)
            .unwrap();
        let b = w
            .store
            .insert_segment(sid, t + 6_000, t + 11_000, LONG_TEXT, None)
            .unwrap();
        let first = w
            .store
            .insert_episode(
                t,
                t + 11_000,
                &EpisodeContent {
                    title: "First part".into(),
                    ..Default::default()
                },
                t,
            )
            .unwrap();
        w.store
            .mark_segments_interpreted(&[a.id, b.id], first.id, t)
            .unwrap();
        w.store
            .insert_segment(sid, t + 12_000, t + 17_000, LONG_TEXT, None)
            .unwrap();
        w.run_interpret(false);
        assert_eq!(
            w.store.episode(first.id).unwrap().unwrap().title,
            "First part"
        );
        assert_eq!(w.store.episodes(10).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hand_edited_episode_is_not_revised_by_its_continuation() {
        let dir = temp_dir("edited");
        let ok = r#"{"type":"result","is_error":false,"result":"","total_cost_usd":0.01,"structured_output":{"title":"Model title","summary":"model summary","low_signal":false,"node_id":null,"new_project":null,"new_topic":null,"decisions":[],"action_items":[],"topics":[],"events":[]}}"#;
        let mut w = bare_worker(script_cli(&dir, &format!("echo '{ok}'")));
        let sid = w.store.begin_session(0, "e").unwrap();
        let t = now_ms() - 86_400_000;
        let a = w
            .store
            .insert_segment(sid, t, t + 5_000, LONG_TEXT, None)
            .unwrap();
        let mine = w
            .store
            .insert_episode(
                t,
                t + 5_000,
                &EpisodeContent {
                    title: "Mine".into(),
                    summary: "my careful summary".into(),
                    ..Default::default()
                },
                t,
            )
            .unwrap();
        w.store
            .mark_segments_interpreted(&[a.id], mine.id, t)
            .unwrap();
        w.store
            .update_episode_content(
                mine.id,
                &EpisodeContent {
                    title: "Mine".into(),
                    summary: "my careful summary".into(),
                    ..Default::default()
                },
                t + 1,
            )
            .unwrap();
        // speech right after it: without the edit this would REVISE `mine`
        w.store
            .insert_segment(sid, t + 6_000, t + 11_000, LONG_TEXT, None)
            .unwrap();
        w.run_interpret(false);
        let kept = w.store.episode(mine.id).unwrap().unwrap();
        assert_eq!(
            (kept.title.as_str(), kept.summary.as_str(), kept.revision),
            ("Mine", "my careful summary", 1)
        );
        let all = w.store.episodes(10).unwrap();
        assert_eq!(all.len(), 2, "the continuation became its own episode");
        assert!(all.iter().any(|e| e.summary == "model summary"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_the_handle_cancels_an_in_flight_call() {
        let dir = temp_dir("cancel");
        let db = dir.join("oats.sqlite");
        {
            let s = Store::open(&db).unwrap();
            let sid = s.begin_session(0, "e").unwrap();
            let t = now_ms() - 86_400_000;
            s.insert_segment(sid, t, t + 5_000, LONG_TEXT, None)
                .unwrap();
        }
        let mut cli = script_cli(&dir, "sleep 30");
        cli.timeout = Duration::from_secs(60);
        let h = Brain {
            config: BrainConfig {
                cli,
                ..BrainConfig::default()
            },
            store_path: db,
            wake: Box::new(|| {}),
        }
        .spawn();
        h.send(BrainCommand::InterpretNow);
        wait_for(&h, "run start", |e| {
            matches!(e, BrainEvent::RunStarted { .. })
        });
        std::thread::sleep(Duration::from_millis(200));
        let t = Instant::now();
        drop(h);
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_script(path: &std::path::Path, body: &str) {
        // written next to it and renamed over it: a running copy is never half-written
        let tmp = path.with_extension("new");
        std::fs::write(&tmp, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::rename(&tmp, path).unwrap();
    }

    /// The roadmap's offline scenario end to end: claude answers `--version` but every real
    /// call fails as when logged out; segments and a voice command arrive and wait; then
    /// claude works again and the backlog drains in order — the command first (its "tomorrow"
    /// resolved against the day it was spoken, three days ago), then the conversation, oldest
    /// chunk first.
    #[test]
    fn a_logged_out_cli_queues_everything_and_the_backlog_drains_in_order_when_it_returns() {
        let dir = temp_dir("offline");
        let db = dir.join("oats.sqlite");
        let calls = dir.join("calls");
        let script = dir.join("claude");
        write_script(
            &script,
            "#!/bin/sh\ncase \"$*\" in *--version*) echo '2.1.312 (Claude Code)'; exit 0;; esac\ncat > /dev/null\necho 'Not logged in · Please run /login' >&2\nexit 1\n",
        );
        let working = format!(
            r#"#!/bin/sh
case "$*" in *--version*) echo '2.1.312 (Claude Code)'; exit 0;; esac
IN=$(cat)
case "$*" in
  *intent*)
    D=$(printf '%s\n' "$IN" | sed -n 's/^Now (when the command was spoken): \([0-9-]*\).*/\1/p' | head -n 1)
    echo "command $D" >> '{calls}'
    echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.001,"structured_output":{{"actions":[{{"intent":"add_event","title":"Dentist","date":"'"$D"'","time":null,"all_day":true,"relative_to_event":false,"offset_days":null,"text":null,"details":"","node_name":null,"category_name":null}}]}}}}';;
  *low_signal*)
    C=$(printf '%s' "$IN" | grep -o 'chunk-[0-9]*' | head -n 1)
    echo "interpret $C" >> '{calls}'
    echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.01,"structured_output":{{"title":"'"$C"'","summary":"s","low_signal":false,"node_id":null,"new_project":null,"new_topic":null,"decisions":[],"action_items":[],"topics":[],"events":[]}}}}';;
  *) echo '{{"type":"result","is_error":true,"result":"unexpected request"}}';;
esac
"#,
            calls = calls.display()
        );
        // three days ago at noon, local: two conversations two hours apart, a command between
        let day = (Local::now() - chrono::Duration::days(3)).date_naive();
        let noon = day
            .and_hms_opt(12, 0, 0)
            .and_then(|t| Local.from_local_datetime(&t).earliest())
            .unwrap()
            .timestamp_millis();
        let rows = {
            let s = Store::open(&db).unwrap();
            let sid = s.begin_session(noon, "e").unwrap();
            let talk = |n: u32| {
                format!("chunk-{n}: {LONG_TEXT}, and then we went through the open questions")
            };
            vec![
                s.insert_segment(sid, noon, noon + 5_000, &talk(1), None)
                    .unwrap(),
                s.insert_segment(
                    sid,
                    noon + 1_800_000,
                    noon + 1_805_000,
                    "hey computer the dentist is tomorrow",
                    None,
                )
                .unwrap(),
                s.insert_segment(sid, noon + 7_200_000, noon + 7_205_000, &talk(2), None)
                    .unwrap(),
            ]
        };
        let cli = ClaudeCli {
            binary: script.clone(),
            model: None,
            timeout: Duration::from_secs(20),
            max_budget_usd: None,
            work_dir: Some(dir.join("work")),
            isolate: true,
        };
        let h = Brain {
            config: BrainConfig {
                cli,
                probe_backoff_min: Duration::from_millis(100),
                probe_backoff_max: Duration::from_millis(300),
                catchup_pause: Duration::from_millis(10),
                ..BrainConfig::default()
            },
            store_path: db.clone(),
            wake: Box::new(|| {}),
        }
        .spawn();
        wait_for(&h, "startup probe", |e| {
            matches!(e, BrainEvent::Availability(Ok(_)))
        });
        for r in &rows {
            h.segment_arrived(r.clone());
        }
        wait_for(
            &h,
            "unavailable",
            |e| matches!(e, BrainEvent::Availability(Err(r)) if r == "not logged in"),
        );
        wait_for(&h, "the backlog", |e| {
            matches!(
                e,
                BrainEvent::Backlog {
                    waiting_segments: 2,
                    waiting_commands: 1
                }
            )
        });
        // a few failed re-probes (each a free `--version`, then the real call fails again)
        std::thread::sleep(Duration::from_millis(600));
        assert!(!calls.exists(), "nothing ran while logged out");
        write_script(&script, &working);
        wait_for(&h, "the catch-up done", |e| {
            matches!(
                e,
                BrainEvent::Backlog {
                    waiting_segments: 0,
                    waiting_commands: 0
                }
            )
        });
        drop(h);
        let log = std::fs::read_to_string(&calls).unwrap();
        let log: Vec<&str> = log.lines().collect();
        let spoken = day.format("%Y-%m-%d").to_string();
        assert_eq!(
            log,
            vec![
                format!("command {spoken}").as_str(),
                "interpret chunk-1",
                "interpret chunk-2"
            ]
        );
        let s = Store::open(&db).unwrap();
        let events = s.events_from(i64::MIN, 10).unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        let when = Local
            .timestamp_millis_opt(events[0].when_start)
            .single()
            .unwrap();
        assert_eq!(
            when.date_naive(),
            day,
            "dated from the segment, not from today"
        );
        assert_eq!(s.uninterpreted_count().unwrap(), 0);
        let titles: Vec<String> = s
            .episodes(10)
            .unwrap()
            .into_iter()
            .map(|e| e.title)
            .collect();
        assert_eq!(titles.len(), 2, "{titles:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_reprobe_backs_off_and_caps() {
        let dir = temp_dir("backoff");
        let (mut w, _rx) = bare_worker_with_events(ClaudeCli {
            binary: dir.join("missing-claude"),
            ..script_cli(&dir, "")
        });
        w.configured_binary = dir.join("missing-claude");
        let mut waits = Vec::new();
        for _ in 0..7 {
            let before = Instant::now();
            assert!(!w.probe());
            let Some(at) = w.probe_due() else {
                unreachable!("{:?}", w.avail)
            };
            waits.push(at.saturating_duration_since(before).as_secs());
        }
        // 1 min, 2, 4, 8, then 15 (the cap)
        let mins: Vec<u64> = waits.iter().map(|s| (s + 30) / 60).collect();
        assert_eq!(mins, vec![1, 2, 4, 8, 15, 15, 15]);
        assert!(matches!(&w.avail, Avail::Down { reason, .. } if reason == "not installed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tier0_has_its_own_hourly_cap_and_a_hard_ceiling() {
        let dir = temp_dir("tier0");
        let mut w = bare_worker(script_cli(&dir, "exit 9"));
        w.cfg.tier0_per_hour = 2;
        w.cfg.daily_budget_usd = 1.0;
        let paid = |w: &Worker, tier: i64, cost: f64| {
            let id = w.store.begin_run(tier, "x", "m", now_ms() - 1_000).unwrap();
            w.store
                .finish_run(
                    id,
                    &RunOutcome {
                        finished_at: now_ms(),
                        model: None,
                        cost_usd: Some(cost),
                        duration_ms: None,
                        ok: true,
                        error: None,
                        summary: String::new(),
                    },
                )
                .unwrap();
        };
        // over the daily budget: tiers 1–3 stop, tier 0 goes on (ceiling is $2)
        paid(&w, 1, 1.5);
        assert!(!w.within_budget(Tier::Rolling));
        assert!(w.within_budget(Tier::Reactive));
        // tier 0's own hourly cap
        paid(&w, 0, 0.0);
        paid(&w, 0, 0.0);
        assert!(!w.within_budget(Tier::Reactive));
        // the hard ceiling, whatever the hourly count
        w.cfg.tier0_per_hour = 100;
        assert!(w.within_budget(Tier::Reactive));
        paid(&w, 1, 0.6);
        assert!(!w.within_budget(Tier::Reactive));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_panicking_job_is_recorded_and_the_worker_lives_on() {
        let dir = temp_dir("panic");
        let (mut w, rx) = bare_worker_with_events(script_cli(&dir, "exit 9"));
        let r = w.run_job(Tier::Rolling, "interpret", |_, _| {
            let v: Vec<i64> = Vec::new();
            // the kind of model-driven bug the review found: an index (or a chrono
            // constructor) out of range
            let i = v.len() + 3;
            Ok(JobOutput {
                summary: v[i].to_string(),
                cost_usd: None,
                duration_ms: None,
                model: None,
                lines: Vec::new(),
                changed: false,
            })
        });
        let e = r.err().unwrap();
        assert_eq!(e.class, "panic");
        let runs = w.store.runs(5).unwrap();
        assert!(!runs[0].ok);
        assert!(
            runs[0]
                .error
                .as_deref()
                .is_some_and(|m| m.contains("panic"))
        );
        assert!(
            rx.try_iter()
                .any(|e| matches!(e, BrainEvent::Warning(m) if m.contains("panic")))
        );
        // and the worker still works
        assert!(
            w.run_job(Tier::Rolling, "x", |_, _| Ok(JobOutput {
                summary: String::new(),
                cost_usd: None,
                duration_ms: None,
                model: None,
                lines: Vec::new(),
                changed: false,
            }))
            .is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: a microphone command segment marked as the echo of system audio after it
    /// was queued (the system segment closed second) must not run.
    #[test]
    fn a_command_that_became_an_echo_does_not_run() {
        let dir = temp_dir("cmdecho");
        let ran = dir.join("ran");
        let mut w = bare_worker(script_cli(
            &dir,
            &format!("touch '{}'\necho 'not json'", ran.display()),
        ));
        let sid = w.store.begin_session(0, "e").unwrap();
        let t = now_ms() - 60_000;
        let mic = w
            .store
            .insert_segment(sid, t, t + 1_000, "hey computer note the milk", None)
            .unwrap();
        let them = w
            .store
            .insert_segment_from(
                sid,
                t,
                t + 1_000,
                "hey computer note the milk",
                None,
                oats_core::AudioSourceKind::System,
                None,
            )
            .unwrap();
        w.on_segment(mic.clone());
        assert_eq!(w.pending_commands.len(), 1);
        w.store.set_segment_echo(mic.id, Some(them.id)).unwrap();
        let cmd = w.pending_commands.pop_front().unwrap();
        w.run_command(cmd);
        assert!(!ran.exists(), "claude was called for an echo");
        assert!(w.pending_commands.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_command_is_retried_with_backoff_then_given_up() {
        let dir = temp_dir("cmdfail");
        // every call returns garbage: a failure of the command, not of claude's availability
        let mut w = bare_worker(script_cli(&dir, "echo 'not json'"));
        let sid = w.store.begin_session(0, "e").unwrap();
        let t = now_ms() - 60_000;
        let seg = w
            .store
            .insert_segment(sid, t, t + 1_000, "hey computer note the milk", None)
            .unwrap();
        w.queue_command(seg.clone());
        let mut waits = Vec::new();
        for _ in 0..3 {
            let Some(cmd) = w.pending_commands.pop_front() else {
                unreachable!("given up too early")
            };
            let before = Instant::now();
            w.run_command(cmd);
            if let Some(p) = w.pending_commands.front() {
                waits.push(
                    p.not_before
                        .map(|t| t.saturating_duration_since(before).as_secs())
                        .unwrap_or(0),
                );
            }
        }
        assert_eq!(waits, vec![60, 120], "one minute, then two");
        assert!(w.pending_commands.is_empty(), "given up after 3 tries");
        let stored = w.store.segment(seg.id).unwrap().unwrap();
        assert!(
            stored.interpreted_at.is_some(),
            "consumed: no pass retries it"
        );
        let changes = w.store.changes(10).unwrap();
        assert!(
            changes
                .iter()
                .any(|c| c.kind == "command_failed" && c.description.starts_with("Warning")),
            "{changes:?}"
        );
        assert_eq!(w.store.runs(10).unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
