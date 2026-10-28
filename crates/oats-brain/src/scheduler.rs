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
//! Budget: a daily USD cap summed from `brain_runs` (tiers 1–3 skip when hit; tier 0 never
//! skips), a max-runs-per-hour guard, and `--max-budget-usd` on every CLI call. Every run is
//! recorded with model, cost and duration; structural changes land in `changes`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeZone, Timelike};
use oats_core::{BrainRun, EpisodeContent, RunOutcome, Store, TranscriptRow};

use crate::claude_cli::{ClaudeCli, CliError, Request};
use crate::episodes::{Chunk, EpisodeRule, cut};
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
/// (per tier), `OATS_INTERPRET_EVERY_MIN`, `OATS_BRAIN_BUDGET_USD`, `OATS_HOTWORDS`.
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
    pub max_runs_per_hour: i64,
    pub episode_rule: EpisodeRule,
    pub hotword: HotwordConfig,
    pub max_new_nodes_per_run: usize,
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
            episode_rule: EpisodeRule::default(),
            hotword: HotwordConfig::default(),
            max_new_nodes_per_run: 3,
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
    /// Forget an episode's interpretation and redo it.
    Reinterpret(i64),
    UndoChange(i64),
    Shutdown,
}

/// What the brain reports.
#[derive(Clone, Debug, PartialEq)]
pub enum BrainEvent {
    /// The CLI's version, or why it can't be used.
    Availability(Result<String, String>),
    RunStarted {
        tier: Tier,
        kind: String,
    },
    RunFinished(BrainRun),
    /// A hotword command was executed; one line per action.
    Command {
        segment_id: i64,
        lines: Vec<String>,
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
        let out = Outbox { tx: event_tx, wake };
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("oats-brain".to_owned())
            .spawn(move || worker(config, &store_path, cmd_rx, out, worker_cancel));
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

struct Outbox {
    tx: Sender<BrainEvent>,
    wake: Box<dyn Fn() + Send + Sync>,
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

/// A failed job: why, and what it had already spent (earlier calls of a multi-call job
/// succeeded and were paid for; the daily budget must still see that).
struct JobError {
    msg: String,
    cost_usd: Option<f64>,
}

impl From<String> for JobError {
    fn from(msg: String) -> Self {
        JobError {
            msg,
            cost_usd: None,
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

struct Worker {
    cfg: BrainConfig,
    store: Store,
    out: Outbox,
    cli_ok: bool,
    pending_commands: VecDeque<TranscriptRow>,
    reinterpret: VecDeque<i64>,
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

fn worker(
    cfg: BrainConfig,
    store_path: &std::path::Path,
    cmd_rx: Receiver<BrainCommand>,
    out: Outbox,
    cancel: Arc<AtomicBool>,
) {
    let store = match Store::open(store_path) {
        Ok(s) => s,
        Err(e) => {
            out.send(BrainEvent::Warning(format!(
                "brain: could not open the store: {e}"
            )));
            while let Ok(cmd) = cmd_rx.recv() {
                if matches!(cmd, BrainCommand::Shutdown) {
                    return;
                }
            }
            return;
        }
    };
    let mut w = Worker {
        cfg,
        store,
        out,
        cli_ok: false,
        pending_commands: VecDeque::new(),
        reinterpret: VecDeque::new(),
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
    };
    if !w.cfg.enabled {
        w.out
            .send(BrainEvent::Availability(Err("OATS_BRAIN=off".into())));
        while let Ok(cmd) = cmd_rx.recv() {
            if matches!(cmd, BrainCommand::Shutdown) {
                return;
            }
        }
        return;
    }
    w.check_cli();
    w.schedule_initial();
    w.run_loop(cmd_rx);
}

impl Worker {
    fn check_cli(&mut self) -> bool {
        match self.cfg.cli.available() {
            Ok(v) => {
                if !self.cli_ok {
                    log::info!("claude CLI {} at {}", v.raw, self.cfg.cli.binary.display());
                    self.out.send(BrainEvent::Availability(Ok(v.raw)));
                }
                self.cli_ok = true;
            }
            Err(e) => {
                log::warn!("claude CLI unavailable: {e}");
                self.out.send(BrainEvent::Availability(Err(e.to_string())));
                self.cli_ok = false;
            }
        }
        self.cli_ok
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

    fn run_loop(&mut self, cmd_rx: Receiver<BrainCommand>) {
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
            if let Some(seg) = self.pending_commands.pop_front() {
                self.run_command(seg);
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
                } else {
                    self.interpret_force = true;
                    self.interpret_due = Some(now);
                    self.out.send(BrainEvent::Changed(vec![format!(
                        "Episode {id} queued for re-interpretation"
                    )]));
                }
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
                continue;
            }
            if due(self.review_due) {
                self.review_due = None;
                let force = std::mem::take(&mut self.review_force);
                self.run_review(force);
                continue;
            }
            if due(self.reorganise_due) {
                self.reorganise_due = None;
                let force = std::mem::take(&mut self.reorganise_force);
                self.run_reorganise(force);
                continue;
            }
            let next = [
                self.categorise_due,
                self.interpret_due,
                self.review_due,
                self.reorganise_due,
            ]
            .into_iter()
            .flatten()
            .min();
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
                self.interpret_force = true;
                self.interpret_due = Some(Instant::now());
            }
            BrainCommand::ReviewNow => {
                self.review_force = true;
                self.review_due = Some(Instant::now());
            }
            BrainCommand::ReorganiseNow => {
                self.reorganise_force = true;
                self.reorganise_due = Some(Instant::now());
            }
            BrainCommand::Reinterpret(id) => self.reinterpret.push_back(id),
            BrainCommand::UndoChange(id) => self.undo.push_back(id),
            BrainCommand::Shutdown => return false,
        }
        true
    }

    fn on_segment(&mut self, row: TranscriptRow) {
        if hotword::detect(&row.text, &self.cfg.hotword).is_some() {
            if !self.pending_commands.iter().any(|p| p.id == row.id) {
                self.pending_commands.push_back(row);
            }
            return;
        }
        let now = Instant::now();
        if self.categorise_due.is_none() {
            self.categorise_due = Some(now + self.cfg.categorise_after);
        }
        if self.interpret_due.is_none() {
            self.interpret_due = Some(now + self.cfg.interpret_every);
        }
    }

    // ---- guards ---------------------------------------------------------------------------

    /// `false` (with a warning, at most hourly) when the daily cap or the hourly run cap is hit.
    fn within_budget(&mut self, tier: Tier) -> bool {
        if tier == Tier::Reactive {
            return true;
        }
        let midnight = crate::dates::local_midnight_ms(now_ms());
        let spent = self.store.cost_since(midnight).unwrap_or(0.0);
        let runs = self
            .store
            .run_count_since(now_ms() - 3_600_000)
            .unwrap_or(0);
        let over = if spent >= self.cfg.daily_budget_usd {
            Some(format!(
                "daily brain budget reached (${spent:.2} of ${:.2}); tiers 1–3 paused until midnight",
                self.cfg.daily_budget_usd
            ))
        } else if runs >= self.cfg.max_runs_per_hour {
            Some(format!("{runs} brain runs in the last hour; waiting"))
        } else {
            None
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

    fn cli_ready(&mut self) -> bool {
        self.cli_ok || self.check_cli()
    }

    /// Run one CLI request; `NotFound`/`NotLoggedIn` flip the availability flag.
    fn call(&mut self, req: &Request) -> Result<crate::claude_cli::Response, CliError> {
        let r = self.cfg.cli.run_cancellable(req, &self.cancel);
        if let Err(e @ (CliError::NotFound(..) | CliError::NotLoggedIn(_))) = &r {
            self.cli_ok = false;
            self.out.send(BrainEvent::Availability(Err(e.to_string())));
        }
        r
    }

    /// Bookkeeping around a job: the `brain_runs` row, events, spend. `true` when the job
    /// succeeded.
    fn run_job(
        &mut self,
        tier: Tier,
        kind: &str,
        job: impl FnOnce(&mut Worker, i64) -> Result<JobOutput, JobError>,
    ) -> bool {
        let model = self.cfg.models.for_tier(tier).to_owned();
        let started = now_ms();
        let run_id = match self.store.begin_run(tier.number(), kind, &model, started) {
            Ok(id) => id,
            Err(e) => {
                self.out.send(BrainEvent::Warning(format!("brain: {e}")));
                return false;
            }
        };
        self.out.send(BrainEvent::RunStarted {
            tier,
            kind: kind.to_owned(),
        });
        let t = Instant::now();
        let result = job(self, run_id);
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
        match &result {
            Ok(o) => {
                log::info!(
                    "brain {kind} ({model}): {} — ${:.4}, {} ms",
                    o.summary,
                    o.cost_usd.unwrap_or(0.0),
                    outcome.duration_ms.unwrap_or(0)
                );
                if o.changed {
                    self.out.send(BrainEvent::Changed(o.lines.clone()));
                }
            }
            Err(e) => {
                log::warn!("brain {kind} failed: {}", e.msg);
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
        result.is_ok()
    }

    // ---- tier 0: hotword command ----------------------------------------------------------

    fn run_command(&mut self, seg: TranscriptRow) {
        let Some(text) = hotword::detect(&seg.text, &self.cfg.hotword) else {
            return;
        };
        // already executed (the segment arrived twice, or an interpret run re-queued it after
        // it ran): notes and projects aren't deduplicated, so never run a command twice
        if let Ok(Some(stored)) = self.store.segment(seg.id)
            && stored.interpreted_at.is_some()
        {
            return;
        }
        if !self.cli_ready() {
            self.out.send(BrainEvent::Warning(
                "voice command ignored: claude CLI unavailable".into(),
            ));
            return;
        }
        let model = self.cfg.models.reactive.clone();
        self.run_job(Tier::Reactive, "command", move |w, _run_id| {
            let tree = w.store.nodes().map_err(|e| e.to_string())?;
            let req = hotword::command_request(&tree, &text, &Local::now(), Some(model));
            let resp = w.call(&req).map_err(|e| e.to_string())?;
            let result = resp
                .structured
                .as_ref()
                .ok_or_else(|| "no structured output".to_owned())
                .and_then(|v| hotword::parse_command(v).map_err(|e| e.to_string()))?;
            let done = hotword::execute(&w.store, &tree, &seg, &result, now_ms(), &Local)
                .map_err(|e| e.to_string())?;
            let _ = w.store.mark_segment_consumed(seg.id, now_ms());
            let lines: Vec<String> = done.iter().map(|d| d.describe(&Local)).collect();
            w.out.send(BrainEvent::Command {
                segment_id: seg.id,
                lines: lines.clone(),
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
        if !self.cli_ready() {
            self.categorise_due = Some(Instant::now() + Duration::from_secs(30 * 60));
            return;
        }
        let model = self.cfg.models.reactive.clone();
        let more = batch.len() >= self.cfg.categorise_batch;
        let ok = self.run_job(Tier::Reactive, "categorise", move |w, _| {
            let tree = w.store.nodes().map_err(|e| e.to_string())?;
            let req = interpret::categorise_request(&tree, &batch, &Local::now(), Some(model));
            let resp = w.call(&req).map_err(|e| e.to_string())?;
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
        });
        if !ok {
            // tier 0 has no budget guard: a failing batch (bad output, refusals) must not be
            // retried — and paid for — every 20 s; try again after a pause
            self.categorise_due = Some(Instant::now() + FAILURE_BACKOFF);
        } else if more {
            self.categorise_due = Some(Instant::now() + Duration::from_secs(20));
        }
    }

    // ---- tier 1: rolling-window interpretation --------------------------------------------

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
        let (commands, rows): (Vec<TranscriptRow>, Vec<TranscriptRow>) = rows
            .into_iter()
            .partition(|r| hotword::detect(&r.text, &self.cfg.hotword).is_some());
        for c in commands {
            if !self.pending_commands.iter().any(|p| p.id == c.id) {
                self.pending_commands.push_back(c);
            }
        }
        if rows.is_empty() {
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
        let ok = self.run_job(Tier::Rolling, "interpret", |w, run_id| {
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
                let step = w.interpret_chunk(chunk, force, run_id, &model, new_nodes);
                let step = match step {
                    Ok(s) => s,
                    Err(msg) => {
                        // what the earlier chunks cost was spent all the same
                        return Err(JobError {
                            msg,
                            cost_usd: any_cost.then_some(cost),
                        });
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
        });
        self.episodes_since_review += episodes_touched;
        if !ok || rate_limited {
            // don't hammer a failing or rate-limited CLI; the segments stay uninterpreted
            self.interpret_due = Some(Instant::now() + FAILURE_BACKOFF);
        } else if leftover {
            self.interpret_due = Some(Instant::now() + Duration::from_secs(60));
        } else if waiting && self.interpret_due.is_none() {
            // a short chunk still in progress was left for later: make sure "later" comes
            // even if no new speech arrives to schedule it
            self.interpret_due = Some(Instant::now() + self.cfg.interpret_every);
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
    ) -> Result<ChunkStep, String> {
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
            .max_by_key(|e| e.ended_at);
        let mut prior = None;
        let mut full = chunk.clone();
        if let Some(p) = candidate {
            let mut segs = self
                .store
                .segments_of_episode(p.id)
                .map_err(|e| e.to_string())?;
            if segs.len() + chunk.segments.len() <= rule.max_segments
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
        let req = interpret::interpret_request(
            &tree,
            &full,
            prior.as_ref(),
            &Local::now(),
            Some(model.to_owned()),
        );
        let resp = match self.call(&req) {
            Ok(r) => r,
            Err(CliError::RateLimited(m)) => return Ok(ChunkStep::RateLimited(m)),
            Err(e) => return Err(e.to_string()),
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
        self.run_job(Tier::Review, "review", move |w, run_id| {
            let tree = w.store.nodes().map_err(|e| e.to_string())?;
            let events = w
                .store
                .events_from(midnight, 200)
                .map_err(|e| e.to_string())?;
            let req =
                interpret::review_request(&tree, &episodes, &events, &Local::now(), Some(model));
            let resp = w.call(&req).map_err(|e| e.to_string())?;
            let actions: interpret::Actions = resp
                .structured
                .as_ref()
                .ok_or_else(|| "no structured output".to_owned())
                .and_then(|v| serde_json::from_value(v.clone()).map_err(|e| e.to_string()))?;
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
        self.run_job(Tier::Reorganise, "reorganise", move |w, run_id| {
            let usage = w.store.node_usage().map_err(|e| e.to_string())?;
            let episodes = w.store.episodes(60).map_err(|e| e.to_string())?;
            let req = interpret::reorganise_request(
                &nodes,
                &usage,
                &episodes,
                &Local::now(),
                Some(model),
            );
            let resp = w.call(&req).map_err(|e| e.to_string())?;
            let actions: interpret::Actions = resp
                .structured
                .as_ref()
                .ok_or_else(|| "no structured output".to_owned())
                .and_then(|v| serde_json::from_value(v.clone()).map_err(|e| e.to_string()))?;
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
    use oats_core::View;

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

        h.send(BrainCommand::InterpretNow);
        let fin = wait_for(
            &h,
            "interpret run",
            |e| matches!(e, BrainEvent::RunFinished(r) if r.kind == "interpret"),
        );
        let BrainEvent::RunFinished(run) = fin else {
            unreachable!()
        };
        assert!(run.ok, "{run:?}");
        assert_eq!(run.tier, 1);
        assert!((run.cost_usd.unwrap_or(0.0) - 0.011).abs() < 1e-9);

        // the hotword segment was in the store already: the interpret run routes it to tier 0
        // by itself; a second arrival of the same segment must not run it twice
        h.segment_arrived(seg_hot.clone());
        let cmd = wait_for(&h, "command", |e| matches!(e, BrainEvent::Command { .. }));
        let BrainEvent::Command { segment_id, lines } = cmd else {
            unreachable!()
        };
        assert_eq!(segment_id, seg_hot.id);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("Event: Property taxes due on"));
        assert!(lines[1].starts_with("Reminder: Property taxes due in one week on"));
        wait_for(
            &h,
            "spend",
            |e| matches!(e, BrainEvent::Spend { today_usd } if *today_usd > 0.012),
        );
        drop(h);

        let s = Store::open(&db).unwrap();
        let eps = s.episodes(10).unwrap();
        assert!(
            eps.iter().any(|e| e.title == "Exporter planning"),
            "{eps:?}"
        );
        assert!(s.nodes().unwrap().iter().any(|n| n.name == "Exporter"));
        assert_eq!(s.events_from(0, 10).unwrap().len(), 1);
        assert_eq!(s.reminders(true, 10).unwrap().len(), 1);
        assert_eq!(
            s.segments(View::All, 0)
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
        let (tx, _rx) = std::sync::mpsc::channel();
        Worker {
            cfg: BrainConfig {
                cli,
                ..BrainConfig::default()
            },
            store: Store::open_in_memory().unwrap(),
            out: Outbox {
                tx,
                wake: Box::new(|| {}),
            },
            cli_ok: true,
            pending_commands: VecDeque::new(),
            reinterpret: VecDeque::new(),
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
            cancel: Arc::new(AtomicBool::new(false)),
        }
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
}
