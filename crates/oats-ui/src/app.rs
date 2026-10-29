//! `OatsApp` — the window: the listen state machine over the [`PipelineHandle`], the transcript
//! list read from the [`Store`], the sidebar, the menu model shared by the native macOS menu and
//! the in-window egui bar, the key map, and sticky prefs through eframe storage.
//!
//! Idle cost: nothing here asks for a repaint. The pipeline's `wake` repaints when an event
//! arrives (≤ ~10 Hz level updates while listening); with nothing happening the app sits at
//! ~0 % CPU.

use std::any::Any;
use std::sync::mpsc;

use egui::{Key, KeyboardShortcut, Modifiers, Rect, Sense, pos2, vec2};
use oats_brain::{BrainCommand, BrainEvent, BrainHandle};
use oats_core::{
    Change, Episode, Event, Node, PipelineEvent, PipelineHandle, PipelineStatus, Reminder, Store,
    TranscriptRow, View,
};
use oats_status::{StatusAction, StatusItem, StatusState};

use crate::calendar::{CalendarAction, CalendarState, calendar_view};
use crate::menu_events::MenuEvents;
use crate::notes::{BrainStatus, NotesAction, NotesState, notes_view};
use crate::sidebar::{SidebarAction, SidebarState, sidebar_panel};
use crate::theme::Tokens;
use crate::transcript::{
    TranscriptAction, local_midnight_ms, transcript_list, until_local_midnight,
};

/// eframe storage key for [`AppPrefs`] (RON).
pub const PREFS_KEY: &str = "oatsnotes.prefs";
/// Sidebar splitter range.
pub const SIDEBAR_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 180.0..=360.0;

/// Persisted preferences.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppPrefs {
    pub sidebar_visible: bool,
    pub sidebar_width: f32,
    pub view: View,
    /// Listening was on at quit: start again once the engine is ready ("listen all day").
    pub listening: bool,
}

impl Default for AppPrefs {
    fn default() -> Self {
        Self {
            sidebar_visible: true,
            sidebar_width: 220.0,
            view: View::Today,
            listening: false,
        }
    }
}

impl AppPrefs {
    pub fn load(storage: &dyn eframe::Storage) -> AppPrefs {
        eframe::get_value(storage, PREFS_KEY).unwrap_or_default()
    }

    pub fn store(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, PREFS_KEY, self);
    }
}

/// Construction options.
#[derive(Clone, Copy, Debug)]
pub struct AppOptions {
    /// Put the menus in the macOS menu bar instead of drawing them in the window (ignored on
    /// other platforms; the binary's `NativeMenuApp` wrapper installs it).
    pub native_menu: bool,
    /// Set the UI in the operating system's own font, falling back to the bundled Inter.
    pub system_font: bool,
    /// Read/write eframe storage.
    pub persist: bool,
    /// Start listening as soon as the engine is ready, whatever the prefs say (`--listen`).
    pub start_listening: bool,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self {
            native_menu: cfg!(target_os = "macos"),
            system_font: true,
            persist: true,
            start_listening: false,
        }
    }
}

/// How the app starts the microphone: given the sender the pipeline's audio channel reads from,
/// return a guard that keeps capture running while it is alive (dropping it stops capture).
/// Boxed so `oats-ui` doesn't depend on the audio crate.
pub type AudioStarter = Box<dyn Fn(mpsc::Sender<Vec<f32>>) -> Result<Box<dyn Any>, String>>;

/// Everything a menu item or shortcut can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    ToggleListening,
    NewCategory,
    ToggleSidebar,
    ShowToday,
    ShowAll,
    ShowNotes,
    ShowCalendar,
    /// Run the brain's interpretation over whatever is new, now.
    InterpretNow,
    /// Run the coherence review of today's episodes, now.
    ReviewNow,
    /// Run the (expensive) reorganisation of the node tree, now.
    Reorganise,
}

const fn sc(m: Modifiers, k: Key) -> KeyboardShortcut {
    KeyboardShortcut::new(m, k)
}

const CMD_SHIFT: Modifiers = Modifiers {
    alt: false,
    ctrl: false,
    shift: true,
    mac_cmd: false,
    command: true,
};

/// Menu shortcuts; checked most-modifiers-first so ⇧⌘S never reads as ⌘S.
pub const SHORTCUTS: &[(KeyboardShortcut, Command)] = &[
    (sc(CMD_SHIFT, Key::S), Command::ToggleSidebar),
    (sc(Modifiers::COMMAND, Key::L), Command::ToggleListening),
    (sc(Modifiers::COMMAND, Key::N), Command::NewCategory),
    (sc(Modifiers::COMMAND, Key::Num1), Command::ShowToday),
    (sc(Modifiers::COMMAND, Key::Num2), Command::ShowAll),
    (sc(Modifiers::COMMAND, Key::Num3), Command::ShowNotes),
    (sc(Modifiers::COMMAND, Key::Num4), Command::ShowCalendar),
    (sc(Modifiers::COMMAND, Key::I), Command::InterpretNow),
];

/// The menu key equivalent of a command.
pub fn menu_shortcut(cmd: Command) -> Option<KeyboardShortcut> {
    SHORTCUTS.iter().find(|(_, c)| *c == cmd).map(|(s, _)| *s)
}

/// `"{label} ({shortcut})"` for a tooltip, in the platform's own spelling.
pub fn with_shortcut(ctx: &egui::Context, label: &str, cmd: Command) -> String {
    match menu_shortcut(cmd) {
        Some(s) => format!("{label} ({})", ctx.format_shortcut(&s)),
        None => label.to_owned(),
    }
}

/// One top-level menu ("File", "Listen", …) of [`OatsApp::menu_model`].
#[derive(Clone, Debug, PartialEq)]
pub struct MenuSection {
    pub title: &'static str,
    pub entries: Vec<MenuEntry>,
}

/// A row of a [`MenuSection`].
#[derive(Clone, Debug, PartialEq)]
pub enum MenuEntry {
    /// A command; `checked` is `Some` for toggles.
    Item {
        label: String,
        cmd: Command,
        enabled: bool,
        checked: Option<bool>,
    },
    Separator,
    /// A non-interactive caption.
    Heading(&'static str),
}

/// The state the menus depend on.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuState {
    pub listening: bool,
    pub status: PipelineStatus,
    pub sidebar_visible: bool,
    pub view: View,
    /// The brain can take a job (claude answered, nothing running).
    pub brain_ready: bool,
}

/// The menu bar as data, from `s` (pure; [`OatsApp::menu_model`] feeds it the app's state).
pub fn build_menu_model(s: &MenuState) -> Vec<MenuSection> {
    use MenuEntry::Separator;
    let item = |label: &str, cmd: Command, enabled: bool| MenuEntry::Item {
        label: label.to_owned(),
        cmd,
        enabled,
        checked: None,
    };
    let check = |label: &str, cmd: Command, on: bool| MenuEntry::Item {
        label: label.to_owned(),
        cmd,
        enabled: true,
        checked: Some(on),
    };
    let listen_enabled = s.listening || s.status == PipelineStatus::Idle;
    vec![
        MenuSection {
            title: "File",
            entries: vec![item("New Category…", Command::NewCategory, true)],
        },
        MenuSection {
            title: "Listen",
            entries: vec![item(
                if s.listening {
                    "Stop Listening"
                } else {
                    "Start Listening"
                },
                Command::ToggleListening,
                listen_enabled,
            )],
        },
        MenuSection {
            title: "View",
            entries: vec![
                check("Show Sidebar", Command::ToggleSidebar, s.sidebar_visible),
                Separator,
                check("Today", Command::ShowToday, s.view == View::Today),
                check("All Transcripts", Command::ShowAll, s.view == View::All),
                check("Notes", Command::ShowNotes, s.view == View::Notes),
                check("Calendar", Command::ShowCalendar, s.view == View::Calendar),
            ],
        },
        MenuSection {
            title: "Notes",
            entries: vec![
                item("Interpret Now", Command::InterpretNow, s.brain_ready),
                item("Review Now", Command::ReviewNow, s.brain_ready),
                Separator,
                item("Reorganise…", Command::Reorganise, s.brain_ready),
            ],
        },
    ]
}

/// The app.
pub struct OatsApp {
    pipeline: PipelineHandle,
    audio: AudioStarter,
    /// The capture guard while listening (`Some` = listening is on).
    capture: Option<Box<dyn Any>>,
    store: Store,
    prefs: AppPrefs,
    persist: bool,
    native_menu: bool,
    status: PipelineStatus,
    engine_name: String,
    /// Latest input level: RMS dBFS and whether a segment is open.
    level: Option<(f32, bool)>,
    rows: Vec<TranscriptRow>,
    /// The whole category → project → topic tree.
    nodes: Vec<Node>,
    today_count: i64,
    today_start: i64,
    /// The brain's worker, if the binary wired one in.
    brain: Option<BrainHandle>,
    brain_status: BrainStatus,
    /// The last executed voice command, for the status strip.
    last_command: Option<String>,
    episodes: Vec<Episode>,
    events: Vec<Event>,
    reminders: Vec<Reminder>,
    changes: Vec<Change>,
    notes_state: NotesState,
    calendar_state: CalendarState,
    /// The last non-fatal problem (pipeline warning, store or microphone error).
    warning: Option<String>,
    /// Start listening as soon as the engine reports Idle (prefs said we were listening).
    auto_listen: bool,
    sidebar: SidebarState,
    data_dir: String,
    /// The status-bar item (menu bar extra), if the platform has one. While it exists, closing
    /// the window only hides it; Quit quits.
    status_item: Option<Box<dyn StatusItem>>,
    /// What the status item was last told to show.
    status_shown: Option<StatusState>,
    /// The shared menu event stream (native menu bar + status item); macOS only.
    menu_events: Option<MenuEvents>,
    /// Quit was asked for (⌘Q, the status item's Quit): let the window close for real.
    quit_requested: bool,
}

impl OatsApp {
    /// Build the app: theme, prefs, the first store read. `status` is the platform's status
    /// item (built by the binary from its chosen `oats_status` implementation), `None` for none;
    /// `brain` the interpretation worker (`None` runs the app without it).
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        options: AppOptions,
        pipeline: PipelineHandle,
        audio: AudioStarter,
        store: Store,
        status: Option<Box<dyn StatusItem>>,
        brain: Option<BrainHandle>,
    ) -> Self {
        let face = crate::theme::install_with(
            &cc.egui_ctx,
            if options.system_font {
                crate::theme::FontChoice::System
            } else {
                crate::theme::FontChoice::Bundled
            },
        );
        log::info!("UI font: {face}");
        let prefs: AppPrefs = if options.persist {
            cc.storage.map(AppPrefs::load).unwrap_or_default()
        } else {
            AppPrefs::default()
        };
        // one muda event handler for every menu the app has (see `menu_events`)
        #[cfg(target_os = "macos")]
        let menu_events =
            (status.is_some() || options.native_menu).then(|| MenuEvents::install(&cc.egui_ctx));
        #[cfg(not(target_os = "macos"))]
        let menu_events = None;
        let mut app = OatsApp {
            pipeline,
            audio,
            capture: None,
            store,
            auto_listen: prefs.listening || options.start_listening,
            prefs,
            persist: options.persist,
            native_menu: cfg!(target_os = "macos") && options.native_menu,
            status: PipelineStatus::Loading("Starting…".to_owned()),
            engine_name: String::new(),
            level: None,
            rows: Vec::new(),
            nodes: Vec::new(),
            today_count: 0,
            today_start: local_midnight_ms(),
            brain,
            brain_status: BrainStatus::default(),
            last_command: None,
            episodes: Vec::new(),
            events: Vec::new(),
            reminders: Vec::new(),
            changes: Vec::new(),
            notes_state: NotesState::default(),
            calendar_state: CalendarState::default(),
            warning: None,
            sidebar: SidebarState::default(),
            data_dir: oats_core::paths::data_dir().display().to_string(),
            status_item: status,
            status_shown: None,
            menu_events,
            quit_requested: false,
        };
        app.reload();
        app
    }

    /// Whether the menus live in the macOS menu bar (and not in the window).
    pub fn has_native_menu(&self) -> bool {
        self.native_menu
    }

    /// Hand the menus back to the window (the native menu failed to install) or take them.
    pub fn set_native_menu(&mut self, on: bool) {
        self.native_menu = cfg!(target_os = "macos") && on;
    }

    /// Quit for real: close the window without the status item's hide-instead (⌘Q, Quit).
    pub fn request_quit(&mut self, ctx: &egui::Context) {
        self.quit_requested = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Whether a status item is installed (closing the window then hides it).
    pub fn has_status_item(&self) -> bool {
        self.status_item.is_some()
    }

    /// Drain the shared menu event stream: the status item's ids go to it, the rest are kept
    /// for the native menu bar. Returns those (empty when the menus are in the window).
    pub fn take_native_menu_ids(&mut self) -> Vec<String> {
        self.route_menu_events();
        self.menu_events
            .as_mut()
            .map(MenuEvents::take_native)
            .unwrap_or_default()
    }

    fn route_menu_events(&mut self) {
        let keep = self.native_menu;
        if let Some(ev) = &mut self.menu_events {
            ev.route(self.status_item.as_deref_mut(), keep);
        }
    }

    /// What the status item should show now.
    pub fn status_item_state(&self) -> StatusState {
        let listening = self.is_listening();
        let n = self.today_count;
        let tooltip = match &self.status {
            PipelineStatus::Listening if listening => format!(
                "OatsNotes — Listening · {n} segment{} today",
                if n == 1 { "" } else { "s" }
            ),
            PipelineStatus::Idle | PipelineStatus::Listening => "OatsNotes — Idle".to_owned(),
            PipelineStatus::Loading(_) => "OatsNotes — Loading…".to_owned(),
            PipelineStatus::Failed(_) => "OatsNotes — Speech engine failed".to_owned(),
        };
        StatusState {
            listening,
            enabled: matches!(
                self.status,
                PipelineStatus::Idle | PipelineStatus::Listening
            ),
            tooltip,
        }
    }

    /// Push the state to the status item (when it changed) and run what was picked there.
    fn sync_status_item(&mut self, ctx: &egui::Context) {
        if self.status_item.is_none() {
            return;
        }
        let state = self.status_item_state();
        let actions = match self.status_item.as_mut() {
            Some(item) => {
                if self.status_shown.as_ref() != Some(&state) {
                    item.update(&state);
                    self.status_shown = Some(state);
                }
                item.poll()
            }
            None => Vec::new(),
        };
        if !actions.is_empty() {
            // the menu was used: the next frame shows its effect (e.g. the toggle's new label)
            ctx.request_repaint();
        }
        for a in actions {
            match a {
                StatusAction::ToggleListening => self.run(Command::ToggleListening),
                StatusAction::ShowWindow => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                StatusAction::Quit => self.request_quit(ctx),
            }
        }
    }

    /// With a status item, closing the window hides it (listening carries on in the
    /// background; the status item brings it back); Quit really quits.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if self.status_item.is_some()
            && !self.quit_requested
            && ctx.input(|i| i.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            log::info!("window hidden; still running in the status item (Quit to exit)");
        }
    }

    /// Whether listening is on (the microphone is open and the pipeline is fed).
    pub fn is_listening(&self) -> bool {
        self.capture.is_some()
    }

    pub fn status(&self) -> &PipelineStatus {
        &self.status
    }

    pub fn prefs(&self) -> &AppPrefs {
        &self.prefs
    }

    /// The menu bar as data: the egui bar and the native macOS menu are both generated from
    /// this, so labels, order, enabled and checked state can't drift.
    pub fn menu_model(&self) -> Vec<MenuSection> {
        build_menu_model(&MenuState {
            listening: self.is_listening(),
            status: self.status.clone(),
            sidebar_visible: self.prefs.sidebar_visible,
            view: self.prefs.view,
            brain_ready: self.brain.is_some()
                && self.brain_status.available()
                && self.brain_status.running.is_none(),
        })
    }

    fn brain_send(&mut self, cmd: BrainCommand) {
        match &self.brain {
            Some(b) => b.send(cmd),
            None => self.warning = Some("The brain is not running (OATS_BRAIN=off?)".to_owned()),
        }
    }

    /// Run a menu / shortcut command.
    pub fn run(&mut self, cmd: Command) {
        match cmd {
            Command::ToggleListening => {
                if self.is_listening() {
                    self.stop_listening();
                    self.prefs.listening = false;
                } else if self.start_listening() {
                    self.prefs.listening = true;
                }
                self.auto_listen = false;
            }
            Command::NewCategory => {
                self.prefs.sidebar_visible = true;
                self.sidebar.renaming = None;
                self.sidebar.adding = Some(String::new());
                self.sidebar.focus = true;
            }
            Command::ToggleSidebar => self.prefs.sidebar_visible = !self.prefs.sidebar_visible,
            Command::ShowToday => self.set_view(View::Today),
            Command::ShowAll => self.set_view(View::All),
            Command::ShowNotes => self.set_view(View::Notes),
            Command::ShowCalendar => self.set_view(View::Calendar),
            Command::InterpretNow => self.brain_send(BrainCommand::InterpretNow),
            Command::ReviewNow => self.brain_send(BrainCommand::ReviewNow),
            Command::Reorganise => self.brain_send(BrainCommand::ReorganiseNow),
        }
    }

    fn set_view(&mut self, view: View) {
        if self.prefs.view != view {
            self.prefs.view = view;
            self.reload_rows();
        }
    }

    /// Open the microphone and attach it to the pipeline. `false` (with the error in the status
    /// line) when the engine isn't ready or the microphone can't be opened.
    fn start_listening(&mut self) -> bool {
        if self.capture.is_some() {
            return true;
        }
        if self.status != PipelineStatus::Idle {
            return false;
        }
        let (tx, rx) = mpsc::channel();
        match (self.audio)(tx) {
            Ok(guard) => {
                self.capture = Some(guard);
                self.pipeline.start(rx);
                log::info!("listening started");
                true
            }
            Err(e) => {
                log::warn!("microphone: {e}");
                self.warning = Some(format!("Microphone: {e}"));
                false
            }
        }
    }

    /// Detach and close the microphone (the pipeline flushes the open segment).
    fn stop_listening(&mut self) {
        if self.capture.is_some() {
            self.pipeline.stop();
            self.capture = None;
            self.level = None;
            log::info!("listening stopped");
        }
    }

    /// Re-read the tree, the current view, today's count and the brain's output.
    fn reload(&mut self) {
        match self.store.nodes() {
            Ok(n) => self.nodes = n,
            Err(e) => self.store_error(e),
        }
        // a deleted node can't stay the view
        if let View::Node(id) = self.prefs.view
            && !self.nodes.iter().any(|n| n.id == id)
            && !self.nodes.is_empty()
        {
            self.prefs.view = View::Today;
        }
        self.reload_rows();
        self.reload_brain_data();
    }

    fn reload_rows(&mut self) {
        self.today_start = local_midnight_ms();
        match self.store.segments(self.prefs.view, self.today_start) {
            Ok(r) => self.rows = r,
            Err(e) => self.store_error(e),
        }
        match self.store.segment_count_since(self.today_start) {
            Ok(n) => self.today_count = n,
            Err(e) => self.store_error(e),
        }
    }

    /// Episodes (of the selected subtree in a Node view, else all), events, reminders, the
    /// change log and the latest run.
    fn reload_brain_data(&mut self) {
        let episodes = match self.prefs.view {
            View::Node(id) => self.store.episodes_under(id, 300),
            _ => self.store.episodes(300),
        };
        match episodes {
            Ok(e) => self.episodes = e,
            Err(e) => self.store_error(e),
        }
        // the most recent past and the nearest upcoming: a plain oldest-first window would,
        // once 500 events piled up, never show anything upcoming again
        let events = self
            .store
            .events_before(self.today_start, 200)
            .and_then(|mut past| {
                past.extend(self.store.events_from(self.today_start, 500)?);
                Ok(past)
            });
        match events {
            Ok(e) => self.events = e,
            Err(e) => self.store_error(e),
        }
        // every open reminder first (oldest-due-first with done ones mixed in would, past the
        // limit, hide the newest open ones), then done ones for "show done"
        let reminders = self.store.reminders(true, 300).and_then(|mut open| {
            let done = self.store.reminders(false, 300)?;
            open.extend(done.into_iter().filter(|r| r.done));
            open.sort_by_key(|r| (r.due, r.id));
            Ok(open)
        });
        match reminders {
            Ok(r) => self.reminders = r,
            Err(e) => self.store_error(e),
        }
        match self.store.changes(50) {
            Ok(c) => self.changes = c,
            Err(e) => self.store_error(e),
        }
        if let Ok(runs) = self.store.runs(1) {
            self.brain_status.last_run = runs.into_iter().next();
        }
        if let Ok(spent) = self.store.cost_since(self.today_start) {
            self.brain_status.spend_today_usd = spent;
        }
    }

    /// Whether `node` is `root` or below it.
    fn in_subtree(&self, node: i64, root: i64) -> bool {
        let mut cur = Some(node);
        for _ in 0..16 {
            match cur {
                Some(id) if id == root => return true,
                Some(id) => {
                    cur = self
                        .nodes
                        .iter()
                        .find(|n| n.id == id)
                        .and_then(|n| n.parent_id)
                }
                None => return false,
            }
        }
        false
    }

    fn store_error(&mut self, e: oats_core::StoreError) {
        log::warn!("store: {e}");
        self.warning = Some(format!("Store: {e}"));
    }

    /// Whether a freshly stored row belongs in the list being shown.
    fn in_view(&self, row: &TranscriptRow) -> bool {
        match self.prefs.view {
            View::Today => row.started_at >= self.today_start,
            View::All => true,
            View::Node(id) => row.node_id.is_some_and(|n| self.in_subtree(n, id)),
            View::Notes | View::Calendar => false,
        }
    }

    /// Drain and apply the brain's events.
    fn handle_brain_events(&mut self) {
        let events: Vec<BrainEvent> = match &self.brain {
            Some(b) => b.poll(),
            None => return,
        };
        let mut reload = false;
        for ev in events {
            match ev {
                BrainEvent::Availability(r) => {
                    if let Err(e) = &r {
                        log::info!("brain unavailable: {e}");
                    }
                    self.brain_status.availability = Some(r);
                }
                BrainEvent::Backlog {
                    waiting_segments,
                    waiting_commands,
                } => {
                    self.brain_status.backlog = Some((waiting_segments, waiting_commands));
                    // each chunk worked off the backlog lands in the views
                    reload = true;
                }
                BrainEvent::RunStarted { kind, .. } => self.brain_status.running = Some(kind),
                BrainEvent::RunFinished(run) => {
                    self.brain_status.running = None;
                    self.brain_status.last_run = Some(run);
                }
                BrainEvent::Command { lines, .. } => {
                    self.last_command = Some(lines.join(" · "));
                    reload = true;
                }
                BrainEvent::Changed(_) => reload = true,
                BrainEvent::Spend { today_usd } => self.brain_status.spend_today_usd = today_usd,
                BrainEvent::Warning(w) => {
                    // the text can quote claude's output: details at debug level only
                    log::info!("brain: warning shown in the status strip");
                    log::debug!("brain: {w}");
                    self.warning = Some(w);
                }
            }
        }
        if reload {
            self.reload();
        }
    }

    /// Drain and apply the pipeline's events.
    fn handle_events(&mut self) {
        // the day rolled over since the last look: "Today" means a different list now
        if local_midnight_ms() != self.today_start {
            self.reload_rows();
        }
        for ev in self.pipeline.poll() {
            match ev {
                PipelineEvent::Status(s) => {
                    if s == PipelineStatus::Idle
                        && self.status == PipelineStatus::Listening
                        && self.capture.is_some()
                    {
                        // the audio stream ended under us (the device went away: the capture's
                        // error callback closed the channel)
                        self.capture = None;
                        self.level = None;
                        log::warn!("listening stopped: the audio input closed");
                        self.warning = Some(
                            "Listening stopped: the audio input closed (microphone disconnected?)"
                                .to_owned(),
                        );
                    }
                    if let PipelineStatus::Failed(msg) = &s {
                        log::error!("engine failed: {msg}");
                        self.capture = None;
                        self.auto_listen = false;
                    }
                    self.status = s;
                }
                PipelineEvent::Engine(name) => self.engine_name = name,
                PipelineEvent::Level { rms_db, in_speech } => {
                    if self.capture.is_some() {
                        self.level = Some((rms_db, in_speech));
                    }
                }
                PipelineEvent::Segment(row) => {
                    if row.started_at >= self.today_start {
                        self.today_count += 1;
                    }
                    if let Some(b) = &self.brain {
                        b.segment_arrived(row.clone());
                    }
                    if self.in_view(&row) {
                        self.rows.push(row);
                    }
                }
                PipelineEvent::ListenFailed(why) => {
                    // the pipeline dropped the audio channel: close the microphone too, so
                    // nothing pretends to listen
                    self.capture = None;
                    self.level = None;
                    self.prefs.listening = false;
                    self.warning = Some(format!("Listening did not start: {why}"));
                }
                PipelineEvent::SessionStarted(_) | PipelineEvent::SessionEnded(_) => {}
                PipelineEvent::Warning(w) => {
                    log::warn!("pipeline: {w}");
                    self.warning = Some(w);
                }
            }
        }
        if self.auto_listen && self.status == PipelineStatus::Idle {
            self.auto_listen = false;
            // (also `--listen`, which leaves the remembered pref as it was)
            if !self.start_listening() {
                self.prefs.listening = false;
            }
        }
    }

    /// The key monitor: stands down while a text field has focus, and leaves the ⌘-chords to
    /// AppKit while the native menu is installed (nothing fires twice).
    fn handle_keys(&mut self, ctx: &egui::Context) -> Vec<Command> {
        let mut cmds = Vec::new();
        if ctx.egui_wants_keyboard_input() || self.has_native_menu() {
            return cmds;
        }
        ctx.input_mut(|i| {
            for (s, c) in SHORTCUTS {
                if i.consume_shortcut(s) {
                    cmds.push(*c);
                }
            }
        });
        cmds
    }

    /// The in-window egui menu bar, drawn from [`Self::menu_model`].
    fn menu_bar(&mut self, ui: &mut egui::Ui) -> Vec<Command> {
        let mut cmds = Vec::new();
        let model = self.menu_model();
        egui::MenuBar::new().ui(ui, |ui| {
            for section in &model {
                ui.menu_button(section.title, |ui| {
                    for entry in &section.entries {
                        match entry {
                            MenuEntry::Separator => {
                                ui.separator();
                            }
                            MenuEntry::Heading(text) => {
                                ui.label(egui::RichText::new(*text).weak());
                            }
                            MenuEntry::Item {
                                label,
                                cmd,
                                enabled,
                                checked,
                            } => {
                                let text = match checked {
                                    Some(true) => format!("✔ {label}"),
                                    _ => label.clone(),
                                };
                                let mut b = egui::Button::new(text);
                                if let Some(s) = menu_shortcut(*cmd) {
                                    b = b.shortcut_text(ui.ctx().format_shortcut(&s));
                                }
                                if ui.add_enabled(*enabled, b).clicked() {
                                    ui.close();
                                    cmds.push(*cmd);
                                }
                            }
                        }
                    }
                });
            }
        });
        cmds
    }

    fn apply_sidebar(&mut self, actions: Vec<SidebarAction>, cmds: &mut Vec<Command>) {
        let mut changed = false;
        for a in actions {
            match a {
                SidebarAction::Select(v) => self.set_view(v),
                SidebarAction::ToggleSidebar => cmds.push(Command::ToggleSidebar),
                SidebarAction::Add(name) => match self.store.add_category(&name) {
                    Ok(_) => changed = true,
                    Err(e) => self.store_error(e),
                },
                SidebarAction::Rename(id, name) => match self.store.rename_node(id, &name) {
                    Ok(()) => changed = true,
                    Err(e) => self.store_error(e),
                },
                SidebarAction::Delete(id) => match self.store.delete_node(id) {
                    Ok(()) => {
                        if self.prefs.view == View::Node(id) {
                            self.prefs.view = View::Today;
                        }
                        changed = true;
                    }
                    Err(e) => self.store_error(e),
                },
            }
        }
        if changed {
            self.reload();
        }
    }

    fn apply_transcript(&mut self, actions: Vec<TranscriptAction>) {
        if actions.is_empty() {
            return;
        }
        for a in actions {
            let r = match a {
                TranscriptAction::Move { segment, category } => {
                    self.store.set_segment_node(segment, category)
                }
                TranscriptAction::Delete { segment } => self.store.delete_segment(segment),
            };
            if let Err(e) = r {
                self.store_error(e);
            }
        }
        self.reload_rows();
    }

    fn apply_notes(&mut self, actions: Vec<NotesAction>) {
        for a in actions {
            match a {
                NotesAction::InterpretNow => self.run(Command::InterpretNow),
                NotesAction::ReviewNow => self.run(Command::ReviewNow),
                NotesAction::Reorganise => self.run(Command::Reorganise),
                NotesAction::Reinterpret(id) => self.brain_send(BrainCommand::Reinterpret(id)),
                NotesAction::Undo(id) => self.brain_send(BrainCommand::UndoChange(id)),
            }
        }
    }

    fn apply_calendar(&mut self, actions: Vec<CalendarAction>) {
        if actions.is_empty() {
            return;
        }
        for a in actions {
            let r = match a {
                CalendarAction::SetReminderDone(id, done) => self.store.set_reminder_done(id, done),
                CalendarAction::DeleteEvent(id) => self.store.delete_event(id).map(|()| {
                    // the user's own delete: leave no copy in the WAL
                    self.store.checkpoint();
                }),
            };
            if let Err(e) = r {
                self.store_error(e);
            }
        }
        self.reload_brain_data();
    }

    fn state_label(&self) -> &'static str {
        match self.status {
            PipelineStatus::Loading(_) => "Loading…",
            PipelineStatus::Idle => "Idle",
            PipelineStatus::Listening => "Listening",
            PipelineStatus::Failed(_) => "Failed",
        }
    }

    /// The 34 pt control bar: Listen pill, level meter, loading/failure line, engine name.
    fn control_bar(&self, ui: &mut egui::Ui, t: &Tokens) -> Vec<Command> {
        let mut cmds = Vec::new();
        ui.add_space(12.0);
        let listening = self.is_listening();
        let enabled = listening || self.status == PipelineStatus::Idle;
        let (label, tip) = if listening {
            ("● Listening", "Stop listening")
        } else {
            ("Start listening", "Start listening")
        };
        if pill(ui, t, label, listening, enabled)
            .on_hover_text(with_shortcut(ui.ctx(), tip, Command::ToggleListening))
            .clicked()
        {
            cmds.push(Command::ToggleListening);
        }
        ui.add_space(6.0);
        level_meter(ui, t, if listening { self.level } else { None });
        ui.add_space(6.0);
        match &self.status {
            PipelineStatus::Loading(line) => {
                ui.add(
                    egui::Label::new(egui::RichText::new(line).color(t.text_secondary).size(11.5))
                        .truncate(),
                );
            }
            PipelineStatus::Failed(msg) => {
                ui.add(
                    egui::Label::new(egui::RichText::new(msg).color(t.danger).size(11.5))
                        .truncate(),
                );
            }
            _ => {}
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(12.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&self.engine_name)
                        .color(t.text_tertiary)
                        .size(11.0),
                )
                .truncate(),
            );
        });
        cmds
    }

    fn status_line(&self, ui: &egui::Ui, t: &Tokens, rect: Rect) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, t.inset);
        let engine = if self.engine_name.is_empty() {
            "engine"
        } else {
            &self.engine_name
        };
        let n = self.today_count;
        let brain = match (&self.brain, &self.brain_status.running) {
            (None, _) => String::new(),
            (Some(_), running) => match (self.brain_status.backlog_line(), running) {
                // "claude unavailable (not logged in) — 42 segments waiting"
                (Some(line), _) => format!(" · {line}"),
                (None, Some(job)) => format!(" · brain: {job}…"),
                (None, None) => {
                    format!(
                        " · brain usage {:.2} today",
                        self.brain_status.spend_today_usd
                    )
                }
            },
        };
        let left = format!(
            "{engine} · {} · {n} segment{} today{brain}",
            self.state_label(),
            if n == 1 { "" } else { "s" }
        );
        let left_galley = p.layout_no_wrap(left, t.mono(9.5), t.text_tertiary);
        let left_w = left_galley.size().x;
        p.galley(
            pos2(
                rect.left() + 8.0,
                rect.center().y - left_galley.size().y / 2.0,
            ),
            left_galley,
            t.text_tertiary,
        );
        let (right, col) = match (&self.warning, &self.last_command) {
            (Some(w), _) => (w.as_str(), t.danger.gamma_multiply(0.9)),
            (None, Some(c)) => (c.as_str(), t.accent.gamma_multiply(0.9)),
            (None, None) => (self.data_dir.as_str(), t.text_tertiary),
        };
        // the right text never runs over the left
        let room = rect.width() - left_w - 32.0;
        if room > 40.0 {
            let galley = p.layout(right.to_owned(), t.mono(9.5), col, f32::INFINITY);
            let clip = Rect::from_min_max(pos2(rect.right() - 8.0 - room, rect.top()), rect.max);
            p.with_clip_rect(clip).galley(
                pos2(
                    rect.right() - 8.0 - galley.size().x.min(room),
                    rect.center().y - galley.size().y / 2.0,
                ),
                galley,
                col,
            );
        }
    }
}

/// The Listen toggle: a pill, accent-filled when on.
fn pill(ui: &mut egui::Ui, t: &Tokens, label: &str, on: bool, enabled: bool) -> egui::Response {
    let font = crate::theme::font_medium(ui.ctx(), 12.5);
    let text_col = match (enabled, on) {
        (false, _) => t.text_disabled,
        (true, true) => t.accent,
        (true, false) => t.text,
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, text_col);
    let size = vec2(galley.size().x + 24.0, 22.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let radius = rect.height() / 2.0;
        let (fill, border) = if on {
            (
                t.accent_alpha(if resp.hovered() { 0.28 } else { 0.2 }),
                t.accent_alpha(0.6),
            )
        } else if !enabled {
            (
                t.button.gamma_multiply(0.6),
                t.button_border.gamma_multiply(0.6),
            )
        } else if resp.is_pointer_button_down_on() {
            (t.pressed, t.button_border)
        } else if resp.hovered() {
            (t.hover, t.button_border)
        } else {
            (t.button, t.button_border)
        };
        p.rect(
            rect,
            radius,
            fill,
            egui::Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );
        p.galley(rect.center() - galley.size() / 2.0, galley, text_col);
    }
    resp
}

/// Map an RMS level (dBFS) onto the meter's 0…1 (−60 dBFS → 0, 0 dBFS → 1).
pub fn level_fraction(rms_db: f32) -> f32 {
    ((rms_db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// The 60×8 level meter.
fn level_meter(ui: &mut egui::Ui, t: &Tokens, level: Option<(f32, bool)>) {
    let (rect, _) = ui.allocate_exact_size(vec2(60.0, 8.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, t.field);
    if let Some((db, speech)) = level {
        let f = level_fraction(db);
        if f > 0.0 {
            let fill =
                Rect::from_min_size(rect.min, vec2((rect.width() * f).max(8.0), rect.height()));
            p.rect_filled(fill, 4.0, if speech { t.accent } else { t.text_tertiary });
        }
    }
}

#[cfg(feature = "selftest")]
impl OatsApp {
    /// Selftest only: what the status line names as the data location.
    pub(crate) fn selftest_set_data_dir(&mut self, label: String) {
        self.data_dir = label;
    }

    /// Selftest only: show the Listening state with a fixed input level, no audio opened.
    pub(crate) fn selftest_fake_listening(&mut self, rms_db: f32, in_speech: bool) {
        self.capture = Some(Box::new(()));
        self.status = PipelineStatus::Listening;
        self.level = Some((rms_db, in_speech));
    }
}

impl eframe::App for OatsApp {
    // Runs every frame, and also while the window is hidden (eframe then runs no ui pass), so
    // everything the status item drives lives here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_events();
        self.handle_brain_events();
        self.route_menu_events();
        self.sync_status_item(ctx);
        self.handle_close(ctx);
        // "Today" rolls over at local midnight even when nothing else wakes the UI: one
        // repaint scheduled for then (not a continuous one)
        ctx.request_repaint_after(until_local_midnight());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let t = Tokens::get(&ctx);
        let mut cmds = self.handle_keys(&ctx);

        if !self.has_native_menu() {
            egui::Panel::top("menu-bar")
                .exact_size(t.menu_bar_h)
                .frame(
                    egui::Frame::new()
                        .fill(t.chrome)
                        .inner_margin(egui::Margin::symmetric(6, 2)),
                )
                .show(ui, |ui| {
                    cmds.extend(self.menu_bar(ui));
                });
        }

        let mut sidebar_actions = Vec::new();
        if self.prefs.sidebar_visible {
            let resp = egui::Panel::left("sidebar")
                .default_size(
                    self.prefs
                        .sidebar_width
                        .clamp(*SIDEBAR_WIDTH_RANGE.start(), *SIDEBAR_WIDTH_RANGE.end()),
                )
                .size_range(SIDEBAR_WIDTH_RANGE)
                .resizable(true)
                .frame(
                    egui::Frame::new()
                        .fill(t.chrome)
                        .inner_margin(egui::Margin::symmetric(6, 6)),
                )
                .show(ui, |ui| {
                    sidebar_actions =
                        sidebar_panel(ui, &mut self.sidebar, self.prefs.view, &self.nodes);
                });
            let w = resp.response.rect.width();
            if (w - self.prefs.sidebar_width).abs() > 0.5 {
                self.prefs.sidebar_width = w;
            }
        }

        let mut transcript_actions = Vec::new();
        let mut notes_actions = Vec::new();
        let mut calendar_actions = Vec::new();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(t.chrome))
            .show(ui, |ui| {
                let full = ui.available_rect_before_wrap();
                let bar_rect =
                    Rect::from_min_max(full.min, pos2(full.right(), full.top() + t.control_bar_h));
                let status_rect =
                    Rect::from_min_max(pos2(full.left(), full.bottom() - t.status_h), full.max);
                let list_rect = Rect::from_min_max(
                    pos2(full.left(), bar_rect.bottom() + 1.0),
                    pos2(full.right(), status_rect.top()),
                );

                let mut bar_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(bar_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                cmds.extend(self.control_bar(&mut bar_ui, &t));
                ui.painter().hline(
                    full.x_range(),
                    bar_rect.bottom() + 0.5,
                    egui::Stroke::new(1.0, t.divider),
                );

                let mut list_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(list_rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                list_ui.painter().rect_filled(list_rect, 0.0, t.canvas);
                list_ui.set_clip_rect(list_rect);
                match self.prefs.view {
                    View::Notes => {
                        notes_actions = notes_view(
                            &mut list_ui,
                            &mut self.notes_state,
                            &self.brain_status,
                            &self.episodes,
                            &self.nodes,
                            &self.changes,
                        );
                    }
                    View::Calendar => {
                        calendar_actions = calendar_view(
                            &mut list_ui,
                            &mut self.calendar_state,
                            &self.events,
                            &self.reminders,
                            &self.nodes,
                        );
                    }
                    _ => {
                        transcript_actions = transcript_list(&mut list_ui, &self.rows, &self.nodes);
                    }
                }

                self.status_line(ui, &t, status_rect);
                ui.painter().hline(
                    full.x_range(),
                    status_rect.top() - 0.5,
                    egui::Stroke::new(1.0, t.divider),
                );
                // claim the whole panel so egui doesn't shrink it around the children
                ui.advance_cursor_after_rect(full);
            });

        self.apply_sidebar(sidebar_actions, &mut cmds);
        self.apply_transcript(transcript_actions);
        self.apply_notes(notes_actions);
        self.apply_calendar(calendar_actions);
        for c in cmds {
            self.run(c);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.persist {
            self.prefs.store(storage);
        }
    }

    fn on_exit(&mut self) {
        // prefs.listening keeps the user's choice: listening resumes at the next launch
        self.stop_listening();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::Storage as _;
    use std::collections::HashMap;

    #[derive(Default)]
    struct MemStorage(HashMap<String, String>);

    impl eframe::Storage for MemStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn prefs_round_trip() {
        let mut s = MemStorage::default();
        let prefs = AppPrefs {
            sidebar_visible: false,
            sidebar_width: 250.0,
            view: View::Node(7),
            listening: true,
        };
        prefs.store(&mut s);
        assert_eq!(AppPrefs::load(&s), prefs);
        // a missing or bad blob falls back to the defaults
        assert_eq!(AppPrefs::load(&MemStorage::default()), AppPrefs::default());
        s.set_string(PREFS_KEY, "garbage(".to_owned());
        assert_eq!(AppPrefs::load(&s), AppPrefs::default());
    }

    #[test]
    fn shortcuts_are_unique_and_cover_the_menu() {
        for (i, (a, _)) in SHORTCUTS.iter().enumerate() {
            for (b, _) in &SHORTCUTS[i + 1..] {
                assert_ne!(a, b);
            }
        }
        for cmd in [
            Command::ToggleListening,
            Command::NewCategory,
            Command::ToggleSidebar,
            Command::ShowToday,
            Command::ShowAll,
            Command::ShowNotes,
            Command::ShowCalendar,
            Command::InterpretNow,
        ] {
            assert!(menu_shortcut(cmd).is_some(), "{cmd:?}");
        }
        assert_eq!(
            menu_shortcut(Command::InterpretNow),
            Some(KeyboardShortcut::new(Modifiers::COMMAND, Key::I))
        );
        assert_eq!(
            menu_shortcut(Command::ToggleListening),
            Some(KeyboardShortcut::new(Modifiers::COMMAND, Key::L))
        );
    }

    fn listen_item(model: &[MenuSection]) -> Option<(String, bool)> {
        model
            .iter()
            .flat_map(|s| s.entries.iter())
            .find_map(|e| match e {
                MenuEntry::Item {
                    label,
                    cmd: Command::ToggleListening,
                    enabled,
                    ..
                } => Some((label.clone(), *enabled)),
                _ => None,
            })
    }

    #[test]
    fn menu_model_has_the_listen_item() {
        let mut s = MenuState {
            listening: false,
            status: PipelineStatus::Loading("x".to_owned()),
            sidebar_visible: true,
            view: View::Today,
            brain_ready: false,
        };
        assert_eq!(
            listen_item(&build_menu_model(&s)),
            Some(("Start Listening".to_owned(), false))
        );
        s.status = PipelineStatus::Idle;
        assert_eq!(
            listen_item(&build_menu_model(&s)),
            Some(("Start Listening".to_owned(), true))
        );
        s.status = PipelineStatus::Listening;
        s.listening = true;
        assert_eq!(
            listen_item(&build_menu_model(&s)),
            Some(("Stop Listening".to_owned(), true))
        );
        s.status = PipelineStatus::Failed("no".to_owned());
        s.listening = false;
        assert_eq!(
            listen_item(&build_menu_model(&s)),
            Some(("Start Listening".to_owned(), false))
        );
        let titles: Vec<_> = build_menu_model(&s).iter().map(|m| m.title).collect();
        assert_eq!(titles, ["File", "Listen", "View", "Notes"]);
        // the Notes menu follows the brain's readiness
        let interpret = |model: &[MenuSection]| {
            model
                .iter()
                .flat_map(|s| s.entries.iter())
                .find_map(|e| match e {
                    MenuEntry::Item {
                        cmd: Command::InterpretNow,
                        enabled,
                        ..
                    } => Some(*enabled),
                    _ => None,
                })
        };
        assert_eq!(interpret(&build_menu_model(&s)), Some(false));
        s.brain_ready = true;
        assert_eq!(interpret(&build_menu_model(&s)), Some(true));
    }

    /// Records what it was told to show; hands out whatever the test queued.
    #[derive(Default)]
    struct FakeStatusItem {
        shown: std::rc::Rc<std::cell::RefCell<Vec<StatusState>>>,
        queue: std::rc::Rc<std::cell::RefCell<Vec<StatusAction>>>,
    }

    impl StatusItem for FakeStatusItem {
        fn update(&mut self, state: &StatusState) {
            self.shown.borrow_mut().push(state.clone());
        }
        fn poll(&mut self) -> Vec<StatusAction> {
            std::mem::take(&mut *self.queue.borrow_mut())
        }
    }

    #[test]
    fn level_maps_minus_60_to_0_dbfs() {
        assert_eq!(level_fraction(-100.0), 0.0);
        assert_eq!(level_fraction(-60.0), 0.0);
        assert!((level_fraction(-30.0) - 0.5).abs() < 1e-6);
        assert_eq!(level_fraction(0.0), 1.0);
        assert_eq!(level_fraction(6.0), 1.0);
    }

    /// The whole app built headlessly (kittest's creation context, the null engine, an
    /// in-memory store): its menu model carries the Listen item. Skipped while the store can't
    /// open an in-memory database (oats-core still stubbed).
    #[test]
    fn app_menu_model_has_the_listen_item() {
        let Ok(store) = Store::open_in_memory() else {
            eprintln!("skipped: Store::open_in_memory unavailable");
            return;
        };
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx);
        let dir = std::env::temp_dir().join(format!("oats-ui-test-{}", std::process::id()));
        let pipeline = oats_core::Pipeline {
            engine: Box::new(|| {
                Ok(Box::new(oats_core::NullEngine) as Box<dyn oats_core::SpeechEngine>)
            }),
            store_path: dir.join("oats.sqlite"),
            segmenter: Default::default(),
            wake: Box::new(|| {}),
        }
        .spawn();
        let audio: AudioStarter = Box::new(|_tx| Err("no microphone in tests".to_owned()));
        let item = FakeStatusItem::default();
        let shown = item.shown.clone();
        let queue = item.queue.clone();
        let mut app = OatsApp::new(
            &cc,
            AppOptions {
                native_menu: false,
                system_font: false,
                persist: false,
                start_listening: false,
            },
            pipeline,
            audio,
            store,
            Some(Box::new(item)),
            None,
        );
        assert!(app.has_status_item());
        // without a brain, Interpret Now says so instead of doing nothing
        app.run(Command::InterpretNow);
        assert!(app.warning.as_deref().is_some_and(|w| w.contains("brain")));
        app.warning = None;
        // the status item follows the app's state, and is only told about changes
        app.sync_status_item(&cc.egui_ctx);
        app.sync_status_item(&cc.egui_ctx);
        assert_eq!(
            *shown.borrow(),
            [StatusState {
                listening: false,
                enabled: false,
                tooltip: "OatsNotes — Loading…".to_owned(),
            }]
        );
        app.status = PipelineStatus::Idle;
        app.sync_status_item(&cc.egui_ctx);
        assert_eq!(
            shown
                .borrow()
                .last()
                .map(|s| (s.enabled, s.tooltip.as_str())),
            Some((true, "OatsNotes — Idle"))
        );
        // its Quit really quits (closing the window would only hide it)
        queue.borrow_mut().push(StatusAction::Quit);
        assert!(!app.quit_requested);
        app.sync_status_item(&cc.egui_ctx);
        assert!(app.quit_requested);
        app.status = PipelineStatus::Loading("Starting…".to_owned());
        assert!(listen_item(&app.menu_model()).is_some());
        assert!(!app.is_listening());
        // the toggle refuses while the engine is still loading, and a failing microphone
        // surfaces in the status line instead of turning listening on
        app.status = PipelineStatus::Idle;
        app.run(Command::ToggleListening);
        assert!(!app.is_listening());
        assert!(
            app.warning
                .as_deref()
                .is_some_and(|w| w.contains("microphone"))
        );
        app.run(Command::ToggleSidebar);
        assert!(!app.prefs().sidebar_visible);
        let _ = std::fs::remove_dir_all(dir);
    }
}
