//! `OatsApp` — the window: the listen state machine over the [`PipelineHandle`], the transcript
//! list read from the [`Store`], the sidebar, the menu model shared by the native macOS menu and
//! the in-window egui bar, the key map, and sticky prefs through eframe storage.
//!
//! Idle cost: nothing here asks for a repaint. The pipeline's `wake` repaints when an event
//! arrives (≤ ~4 Hz level updates while listening and visible); with nothing happening the app sits at
//! ~0 % CPU.

use std::any::Any;
use std::collections::HashMap;
use std::sync::mpsc;

use egui::{Key, KeyboardShortcut, Modifiers, Rect, Sense, pos2, vec2};
use oats_brain::{BrainCommand, BrainEvent, BrainHandle};
use oats_core::pipeline::ListenOptions;
use oats_core::{
    AudioChunk, Change, Episode, Event, NewTodo, Node, PipelineEvent, PipelineHandle,
    PipelineStatus, Store, Todo, TodoFilter, TranscriptRow, View,
};
use oats_status::{StatusAction, StatusItem, StatusState};

use crate::calendar::{CalendarAction, CalendarState, calendar_view};
use crate::media::{MEDIA_PAGE, MediaAction, MediaPages, media_view};
use crate::menu_events::MenuEvents;
use crate::notes::{BrainStatus, NotesAction, NotesState, episode_view, node_view, notes_view};
use crate::search::{BAR_H, SEARCH_LIMIT, SearchAction, SearchState, search_bar, search_results};
use crate::sidebar::{
    HeaderIcon, SIDEBAR_EPISODES, SidebarAction, SidebarEpisode, SidebarState, header_icon,
    sidebar_panel,
};
use crate::theme::Tokens;
use crate::thumbs::Thumbs;
use crate::toast::{Confirmation, ToastAction, ToastState, toasts};
use crate::todos::{TodoAction, TodosState, todos_view};
use crate::topic::{BAR_H as TOPIC_BAR_H, TopicAction, TopicState, topic_header, topic_list};
use crate::transcript::{
    PAGE_ROWS, TranscriptAction, TranscriptFocus, TranscriptPages, local_midnight_ms,
    transcript_list, until_local_midnight,
};
use oats_brain::MoveTarget;
use serde::Deserialize as _;

/// eframe storage key for [`AppPrefs`] (RON).
/// How long a warning stays in the status strip before it clears itself.
pub const WARNING_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// With "Keep transcripts indefinitely" off, a transcript line is deleted this long after it
/// ended, once the brain is done with it: longer than the episode cutter's widest gap (twice
/// 10 min inside a call), so a note is never purged while the brain could still revise it.
pub const TRANSCRIPT_GRACE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// Purges run at most this often.
const PURGE_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

pub const PREFS_KEY: &str = "oatsnotes.prefs";
/// Height of the Transcripts view's header strip ("Show media").
const TRANSCRIPTS_BAR_H: f32 = 30.0;
/// Sidebar splitter range.
pub const SIDEBAR_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 180.0..=360.0;

/// Which audio lanes listening opens: the microphone ("me") and / or system audio ("them").
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Lanes {
    pub mic: bool,
    pub system: bool,
}

impl Default for Lanes {
    fn default() -> Self {
        Self {
            mic: true,
            system: true,
        }
    }
}

impl Lanes {
    /// Neither lane: not listening.
    pub const NONE: Lanes = Lanes {
        mic: false,
        system: false,
    };

    pub fn any(self) -> bool {
        self.mic || self.system
    }
}

/// Reads the old plain `bool` (and an `Option<bool>`) of a pref that has been replaced.
fn legacy_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Either {
        Plain(bool),
        Opt(Option<bool>),
    }
    Ok(match Either::deserialize(d)? {
        Either::Plain(b) => Some(b),
        Either::Opt(o) => o,
    })
}

/// Persisted preferences.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppPrefs {
    pub sidebar_visible: bool,
    pub sidebar_width: f32,
    pub view: View,
    /// Listening was on at quit (a record of the user's choice; by itself it starts nothing).
    pub listening: bool,
    /// Start listening (the remembered lanes) as soon as the engine is ready at every launch.
    /// Off by default: the app opens idle until ⌘L, the status item or `--listen`.
    pub listen_at_launch: bool,
    /// The lane set "Start Listening" (⌘L, the status item, `--listen`) opens: the last one the
    /// user chose with the Mic / System audio pills. Default: both.
    pub lanes: Lanes,
    /// Pre-lanes pref (system audio next to the microphone); read once by [`AppPrefs::load`]
    /// into `lanes.system`, never written.
    #[serde(skip_serializing, deserialize_with = "legacy_bool")]
    pub capture_system: Option<bool>,
    /// Play a short ding when a voice command created something.
    pub command_sound: bool,
    /// Projects whose sidebar layer (recent notes) is open; closed by default.
    pub sidebar_episodes_open: Vec<i64>,
    /// The Transcripts view lists media rows (YouTube, music, media apps); off by default —
    /// search results always include them, tagged "media".
    pub show_media: bool,
    /// Show clock times as 14:05 instead of 2:05 pm.
    pub clock_24h: bool,
    /// The Media view shows YouTube thumbnails, fetched from YouTube's image server (the one
    /// network call the app makes itself). Off by default: nothing is fetched while off.
    pub fetch_thumbnails: bool,
    /// Recognise speakers (a voiceprint per person in the database; `OATS_SPEAKERS`
    /// overrides). Off by default: a fresh install keeps no voiceprints until asked.
    pub speakers: bool,
    /// Keep every transcript line for good (default). Off: a line is deleted
    /// [`TRANSCRIPT_GRACE`] after the brain is done with it (see
    /// [`OatsApp::purge_transcripts`]); the notes made from it stay.
    pub keep_transcripts: bool,
    /// When `keep_transcripts` was last turned off (unix ms): only lines that started at or
    /// after it are deleted; the earlier history stays until the user asks for it to go too
    /// (then 0 = everything).
    pub drop_transcripts_from: i64,
}

impl Default for AppPrefs {
    fn default() -> Self {
        Self {
            sidebar_visible: true,
            sidebar_width: 220.0,
            view: View::Transcripts,
            listening: false,
            listen_at_launch: false,
            lanes: Lanes::default(),
            capture_system: None,
            command_sound: true,
            sidebar_episodes_open: Vec::new(),
            show_media: false,
            clock_24h: false,
            fetch_thumbnails: false,
            speakers: false,
            keep_transcripts: true,
            drop_transcripts_from: 0,
        }
    }
}

impl AppPrefs {
    pub fn load(storage: &dyn eframe::Storage) -> AppPrefs {
        let mut prefs: AppPrefs = eframe::get_value(storage, PREFS_KEY).unwrap_or_default();
        prefs.migrate();
        prefs
    }

    /// Fold the old `capture_system` pref into `lanes.system`.
    fn migrate(&mut self) {
        if let Some(system) = self.capture_system.take() {
            self.lanes.system = system;
        }
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

/// What [`AudioStarter`] is asked to open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioRequest {
    /// Open the microphone. Off for system-audio-only listening: it must not be opened at all.
    pub mic: bool,
    /// Open system audio.
    pub system: bool,
}

/// What an [`AudioStarter`] opened.
pub struct StartedAudio {
    /// Keeps every capture running while alive (dropping it stops them).
    pub guard: Box<dyn Any>,
    /// The system-audio capture is running (`false` when not asked for or when it failed).
    pub system: bool,
    /// Why system audio could not be opened (shown in the status strip; the microphone runs).
    pub system_error: Option<String>,
}

impl StartedAudio {
    /// The microphone only.
    pub fn mic(guard: Box<dyn Any>) -> Self {
        Self {
            guard,
            system: false,
            system_error: None,
        }
    }
}

/// How the app starts audio: given the sender the pipeline's audio channel reads from (both
/// captures share it; chunks are tagged with their lane) and what to open, return a guard that
/// keeps capture running while it is alive. An `Err` means the microphone could not be opened.
/// Boxed so `oats-ui` doesn't depend on the audio crate.
pub type AudioStarter =
    Box<dyn Fn(mpsc::Sender<AudioChunk>, AudioRequest) -> Result<StartedAudio, String>>;

/// The voice-command confirmation sound (the binary wires `oats_audio::chime::play`; it must
/// return at once). Boxed so `oats-ui` doesn't depend on the audio crate.
pub type Chime = Box<dyn Fn() + Send + Sync>;

/// Everything a menu item or shortcut can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    ToggleListening,
    /// The Mic lane on / off (listening is on while any lane is; restarts the captures).
    ToggleMic,
    /// The System audio lane on / off (same).
    ToggleSystemAudio,
    /// "On a call" (⇧⌘C): every system-audio app is a meeting until switched off, 10 min of
    /// silence on system audio, or listening stops. Session-only.
    ToggleOnCall,
    /// The Transcripts view's "Show media" (a pref).
    ToggleShowMedia,
    /// Transcripts: scroll to the newest line (reloading the newest page after a jump).
    JumpToNow,
    NewCategory,
    /// Open the Preferences window (your name, for now).
    Preferences,
    ToggleSidebar,
    /// Open the search bar (or re-focus it).
    Search,
    /// The Transcripts view (⌘1).
    ShowTranscripts,
    ShowNotes,
    ShowCalendar,
    ShowTodos,
    /// The Media view: the watch history (⌘6).
    ShowMediaView,
    /// Run the brain's interpretation over whatever is new, now.
    InterpretNow,
    /// Run the coherence review of today's episodes, now.
    ReviewNow,
    /// Run the (expensive) reorganisation of the node tree, now.
    Reorganise,
    /// Undo the last of the user's own quick edits ([`crate::undo`]; ⌘Z, read from egui input,
    /// not a menu accelerator).
    Undo,
    /// Redo the last undone edit (⇧⌘Z, likewise).
    Redo,
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
    (sc(CMD_SHIFT, Key::C), Command::ToggleOnCall),
    (sc(Modifiers::COMMAND, Key::L), Command::ToggleListening),
    (sc(Modifiers::COMMAND, Key::N), Command::NewCategory),
    // ⌘2 was "All Transcripts" before Today went away; the rest keep their numbers
    (sc(Modifiers::COMMAND, Key::Num1), Command::ShowTranscripts),
    (sc(Modifiers::COMMAND, Key::Num3), Command::ShowNotes),
    (sc(Modifiers::COMMAND, Key::Num4), Command::ShowCalendar),
    (sc(Modifiers::COMMAND, Key::Num5), Command::ShowTodos),
    (sc(Modifiers::COMMAND, Key::Num6), Command::ShowMediaView),
    (sc(Modifiers::COMMAND, Key::F), Command::Search),
    (sc(Modifiers::COMMAND, Key::I), Command::InterpretNow),
    (sc(Modifiers::COMMAND, Key::Comma), Command::Preferences),
];

/// ⌘Z / ⇧⌘Z. Deliberately NOT in [`SHORTCUTS`]: a menu key equivalent (a muda accelerator)
/// would take ⌘Z from every text field, whose own undo must keep working. They are read from
/// egui's input each frame while no text field has focus ([`OatsApp::undo_keys`]), with or
/// without the native menu; the Edit menu items carry no accelerator.
pub const UNDO_SHORTCUT: KeyboardShortcut = sc(Modifiers::COMMAND, Key::Z);
pub const REDO_SHORTCUT: KeyboardShortcut = sc(CMD_SHIFT, Key::Z);

/// The shortcut shown next to a command in the in-window menu and in tooltips: the menu key
/// equivalent, or ⌘Z / ⇧⌘Z for Undo / Redo.
pub fn shown_shortcut(cmd: Command) -> Option<KeyboardShortcut> {
    match cmd {
        Command::Undo => Some(UNDO_SHORTCUT),
        Command::Redo => Some(REDO_SHORTCUT),
        _ => menu_shortcut(cmd),
    }
}

/// The menu key equivalent of a command.
pub fn menu_shortcut(cmd: Command) -> Option<KeyboardShortcut> {
    SHORTCUTS.iter().find(|(_, c)| *c == cmd).map(|(s, _)| *s)
}

/// `"{label} ({shortcut})"` for a tooltip, in the platform's own spelling.
pub fn with_shortcut(ctx: &egui::Context, label: &str, cmd: Command) -> String {
    match shown_shortcut(cmd) {
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
    /// The microphone lane is running in this listening session.
    pub mic_on: bool,
    /// "On a call" is on.
    pub on_call: bool,
    /// System audio is captured in this listening session ("On a call" needs it).
    pub system_on: bool,
    /// The Transcripts view's "Show media".
    pub show_media: bool,
    pub status: PipelineStatus,
    pub sidebar_visible: bool,
    pub view: View,
    /// The brain can take a job (it exists and claude answered).
    pub brain_ready: bool,
    /// Interpret / Review / Reorganise queued or running (their menu items grey out).
    pub brain_busy: [bool; 3],
    /// The label of the step Undo / Redo would take (`None`: nothing, the item greys out).
    pub undo_label: Option<String>,
    pub redo_label: Option<String>,
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
            entries: vec![
                item("New Category…", Command::NewCategory, true),
                Separator,
                item("Preferences…", Command::Preferences, true),
            ],
        },
        MenuSection {
            title: "Edit",
            entries: vec![
                item(
                    &s.undo_label
                        .as_ref()
                        .map_or("Undo".to_owned(), |l| format!("Undo {l}")),
                    Command::Undo,
                    s.undo_label.is_some(),
                ),
                item(
                    &s.redo_label
                        .as_ref()
                        .map_or("Redo".to_owned(), |l| format!("Redo {l}")),
                    Command::Redo,
                    s.redo_label.is_some(),
                ),
            ],
        },
        MenuSection {
            title: "Listen",
            entries: vec![
                item(
                    if s.listening {
                        "Stop Listening"
                    } else {
                        "Start Listening"
                    },
                    Command::ToggleListening,
                    listen_enabled,
                ),
                Separator,
                check("Microphone", Command::ToggleMic, s.mic_on),
                check("System Audio", Command::ToggleSystemAudio, s.system_on),
                MenuEntry::Item {
                    label: "On a Call".to_owned(),
                    cmd: Command::ToggleOnCall,
                    enabled: s.listening && s.system_on,
                    checked: Some(s.on_call),
                },
            ],
        },
        MenuSection {
            title: "View",
            entries: vec![
                item("Find…", Command::Search, true),
                Separator,
                check("Show Sidebar", Command::ToggleSidebar, s.sidebar_visible),
                Separator,
                check(
                    "Transcripts",
                    Command::ShowTranscripts,
                    s.view == View::Transcripts,
                ),
                check("All notes", Command::ShowNotes, s.view == View::Notes),
                check("Calendar", Command::ShowCalendar, s.view == View::Calendar),
                check("Todos", Command::ShowTodos, s.view == View::Todos),
                check("Media", Command::ShowMediaView, s.view == View::Media),
                Separator,
                check(
                    "Show Media in Transcripts",
                    Command::ToggleShowMedia,
                    s.show_media,
                ),
            ],
        },
        MenuSection {
            title: "Brain",
            entries: vec![
                item(
                    "Interpret Now",
                    Command::InterpretNow,
                    s.brain_ready && !s.brain_busy[0],
                ),
                item(
                    "Review Now",
                    Command::ReviewNow,
                    s.brain_ready && !s.brain_busy[1],
                ),
                Separator,
                item(
                    "Reorganise…",
                    Command::Reorganise,
                    s.brain_ready && !s.brain_busy[2],
                ),
            ],
        },
    ]
}

/// The open Preferences window.
struct PrefsWindow {
    name: String,
    /// Focus the field on the first frame.
    focus: bool,
    /// `(drop_transcripts_from, lines before it)`: what "Keep transcripts" off leaves alone,
    /// counted once per value of the boundary.
    kept_before: Option<(i64, i64)>,
    /// "Delete them too" was clicked: the confirmation is showing.
    confirm_purge: bool,
    /// The "Apps" table (apps seen playing ∪ overridden ones).
    apps: Vec<AppRow>,
    /// The "Speakers" table's name fields, by voice id (edited text, saved on Enter / blur).
    speaker_names: HashMap<i64, String>,
}

/// An edit in the Preferences "Speakers" table.
enum SpeakerEdit {
    Name(i64, String),
    Me(i64),
    Forget(i64),
}

/// One row of the Preferences "Apps" table.
#[derive(Clone, Debug, PartialEq)]
pub struct AppRow {
    pub bundle_id: String,
    pub name: String,
    /// The stored class (Auto when no override).
    pub class: oats_core::ProducerClass,
    pub transcribe_anyway: bool,
    /// A call app (Zoom, Teams, Webex, FaceTime): always Meeting.
    pub locked: bool,
    /// What Auto means for it.
    pub default: oats_core::ProducerClass,
    /// Its web pages take this class too (Slack, Discord): shown as "web + app".
    pub web_too: bool,
}

impl AppRow {
    /// The class it is treated as.
    pub fn effective(&self) -> oats_core::ProducerClass {
        if self.locked {
            oats_core::ProducerClass::Meeting
        } else if self.class == oats_core::ProducerClass::Auto {
            self.default
        } else {
            self.class
        }
    }
}

/// The Apps table's rows: the well-known apps (`oats_media::KNOWN_APPS`: call apps, then
/// Slack / Discord, listed even when never seen: they may only run in the browser), merged
/// with every app seen playing and every overridden one (a seen row replaces the fixed row
/// with the same bundle id), then the rest alphabetically.
pub fn app_rows(store: &Store) -> Result<Vec<AppRow>, oats_core::StoreError> {
    let overrides = oats_media::ProducerOverrides::new(store.producer_overrides()?);
    let mut rows: Vec<AppRow> = Vec::new();
    let mut add = |bundle: &str, name: &str| {
        if rows
            .iter()
            .any(|r| r.bundle_id.eq_ignore_ascii_case(bundle))
        {
            return;
        }
        let o = overrides.get(bundle);
        rows.push(AppRow {
            bundle_id: bundle.to_ascii_lowercase(),
            name: if name.trim().is_empty() {
                oats_media::Producer::from_bundle(0, bundle).name
            } else {
                name.to_owned()
            },
            class: o.map(|o| o.class).unwrap_or_default(),
            transcribe_anyway: o.is_some_and(|o| o.transcribe_anyway),
            locked: oats_media::is_locked(bundle),
            default: oats_media::default_class(bundle),
            web_too: oats_media::KNOWN_APPS
                .iter()
                .any(|(b, _, web)| *web && b.eq_ignore_ascii_case(bundle)),
        });
    };
    for p in store.producers_seen()? {
        add(&p.bundle_id, &p.name);
    }
    for o in store.producer_overrides()? {
        add(&o.bundle_id, "");
    }
    for (bundle, name, _) in oats_media::KNOWN_APPS {
        add(bundle, name);
    }
    let group = |r: &AppRow| match (r.locked, r.web_too) {
        (true, _) => 0,
        (_, true) => 1,
        _ => 2,
    };
    rows.sort_by_cached_key(|r| (group(r), r.name.to_lowercase()));
    Ok(rows)
}

/// The app.
/// The most recent note, as the sidebar marks it.
#[derive(Clone, Copy, Debug)]
struct LatestNote {
    id: i64,
    /// No project / topic holds it: the All notes row is its sidebar place.
    unfiled: bool,
}

pub struct OatsApp {
    pipeline: PipelineHandle,
    /// Every stored voice, by id: the transcript's speaker labels (published on the context
    /// every frame, reloaded only when voices change).
    speakers: std::sync::Arc<crate::speakers::SpeakerMap>,
    audio: AudioStarter,
    /// The capture guard while listening (`Some` = listening is on).
    capture: Option<Box<dyn Any>>,
    store: Store,
    prefs: AppPrefs,
    persist: bool,
    native_menu: bool,
    status: PipelineStatus,
    engine_name: String,
    /// Latest input level: RMS dBFS and whether a segment is open — the louder lane.
    level: Option<(f32, bool)>,
    /// The microphone lane is running in this listening session.
    mic_on: bool,
    /// The system-audio lane is running in this listening session.
    system_on: bool,
    /// "On a call" (session-only; mirrored in the pipeline).
    on_call: bool,
    /// The system-audio lane delivered only silence since the start (no tap signal).
    system_no_signal: bool,
    /// The Calendar day panel's media plays ("Watched: …") and the day they are for (`None`:
    /// reload at the next frame).
    media_plays: Vec<oats_core::MediaPlay>,
    media_plays_day: Option<chrono::NaiveDate>,
    /// The Media view: the loaded part of the watch history.
    media_pages: MediaPages,
    /// The media notes of the Media view's loaded rows, by play id (a row with one gets a
    /// disclosure triangle and opens the note's card inline).
    media_notes: HashMap<i64, Episode>,
    /// The Media view row to scroll to and flash (a search hit), by play id.
    media_focus: TranscriptFocus,
    /// The Media view's thumbnails (opt-in: `AppPrefs::fetch_thumbnails`).
    thumbs: Thumbs,
    /// The last URL the Media view opened in the browser.
    opened_url: Option<String>,
    /// The segments of a Node view (the Transcripts view uses `pages`).
    rows: Vec<TranscriptRow>,
    /// The Transcripts view: the loaded range of rows.
    pages: TranscriptPages,
    /// Rows per page (`PAGE_ROWS`; tests shrink it).
    page_rows: usize,
    /// The whole category → project → topic tree.
    nodes: Vec<Node>,
    today_count: i64,
    today_start: i64,
    /// The brain's worker, if the binary wired one in.
    brain: Option<BrainHandle>,
    brain_status: BrainStatus,
    /// The last executed voice command, for the status strip.
    last_command: Option<String>,
    /// Confirmation pop-ups raised where no `Context` is at hand (a local "Move to…"); shown
    /// at the next brain-event pass.
    pending_confirms: Vec<String>,
    /// The latest "Split…" that worked: (original, new note), from `BrainEvent::Split`; taken
    /// when the split field settles.
    split_done: Option<(i64, i64)>,
    /// Plain 4 s pop-ups (`text`, `error`) raised where no `Context` is at hand (undo / redo).
    pending_toasts: Vec<(String, bool)>,
    /// The view changed: drop the transcript rows' selection at the next pass
    /// ([`crate::transcript::RowSelection`]).
    clear_row_selection: bool,
    /// The in-session undo / redo of the user's own quick edits ([`crate::undo`]).
    undo: crate::undo::UndoStack<crate::undo::Entry>,
    episodes: Vec<Episode>,
    /// A project page's items filed there from notes homed elsewhere
    /// ([`Store::items_filed_under`]); empty outside a Node view.
    filed_items: Vec<(Episode, oats_core::NoteItem)>,
    events: Vec<Event>,
    /// Every todo, in display order (open, then completed).
    todos: Vec<Todo>,
    /// The todos whose pop-up is showing (due, or the snooze is over).
    due_now: Vec<Todo>,
    /// When the next todo comes due (a repaint is scheduled for then).
    next_due: Option<i64>,
    /// "Keep transcripts" off: something happened that may have left lines to delete.
    purge_due: bool,
    /// When the last purge ran (at most one a minute).
    purge_last: Option<std::time::Instant>,
    /// When the earliest line not yet old enough to go will be (a repaint is scheduled).
    purge_next_at: Option<i64>,
    changes: Vec<Change>,
    notes_state: NotesState,
    calendar_state: CalendarState,
    todos_state: TodosState,
    /// The confirmation pop-ups of voice commands.
    toasts: ToastState,
    /// A transcript row to scroll to and flash (a calendar item / todo → where it was said).
    transcript_focus: TranscriptFocus,
    /// The voice-command ding.
    chime: Chime,
    /// The last non-fatal problem (pipeline warning, store or microphone error).
    warning: Option<String>,
    /// The warning text last seen and when it first showed (for [`WARNING_TTL`]).
    warning_seen: Option<(String, std::time::Instant)>,
    /// Start listening as soon as the engine reports Idle (prefs said we were listening).
    auto_listen: bool,
    sidebar: SidebarState,
    /// The notes listed under each project in the sidebar (newest first).
    sidebar_episodes: HashMap<i64, Vec<SidebarEpisode>>,
    /// Unfiled notes that started today (the All notes row turns white).
    unfiled_today: i64,
    /// The most recent note (`Store::latest_note`): its id and whether it is unfiled (the
    /// sidebar draws its path, or the All notes row, in the accent colour).
    latest_note: Option<LatestNote>,
    /// The single-note view: this note of the viewed node, instead of the node's page.
    focus_episode: Option<i64>,
    /// The Preferences window, when open.
    prefs_window: Option<PrefsWindow>,
    /// The search bar and its results, while open (the view underneath is untouched, so
    /// closing restores it).
    search: Option<SearchState>,
    /// The Topic view (episodes tagged with a clicked topic chip), while open; sits over the
    /// current view like the search results and is never persisted.
    topic: Option<TopicState>,
    /// The status-bar item (menu bar extra), if the platform has one. While it exists, closing
    /// the window only hides it; Quit quits.
    status_item: Option<Box<dyn StatusItem>>,
    /// What the status item was last told to show.
    status_shown: Option<StatusState>,
    /// Whether `Level` events are currently requested from the pipeline (off while hidden).
    levels_on: bool,
    /// The window was hidden by close-to-status-item and not yet shown again.
    window_hidden: bool,
    /// The shared menu event stream (native menu bar + status item); macOS only.
    menu_events: Option<MenuEvents>,
    /// Quit was asked for (⌘Q, the status item's Quit): let the window close for real.
    quit_requested: bool,
    /// Set by the binary's SIGINT / SIGTERM handler; `logic` turns it into `request_quit`.
    quit_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
        crate::timefmt::set_24h(prefs.clock_24h);
        let sidebar_open = prefs.sidebar_episodes_open.iter().copied().collect();
        let speakers_on = oats_core::speakers::speakers_env().unwrap_or(prefs.speakers);
        pipeline.set_speakers(speakers_on);
        let mut app = OatsApp {
            pipeline,
            speakers: Default::default(),
            audio,
            capture: None,
            store,
            auto_listen: prefs.listen_at_launch || options.start_listening,
            prefs,
            persist: options.persist,
            native_menu: cfg!(target_os = "macos") && options.native_menu,
            levels_on: true,
            window_hidden: false,
            status: PipelineStatus::Loading("Starting…".to_owned()),
            engine_name: String::new(),
            level: None,
            mic_on: false,
            system_on: false,
            on_call: false,
            system_no_signal: false,
            media_plays: Vec::new(),
            media_plays_day: None,
            media_pages: MediaPages::default(),
            media_notes: HashMap::new(),
            media_focus: TranscriptFocus::default(),
            thumbs: Thumbs::default(),
            opened_url: None,
            rows: Vec::new(),
            pages: TranscriptPages::default(),
            page_rows: PAGE_ROWS,
            nodes: Vec::new(),
            today_count: 0,
            today_start: local_midnight_ms(),
            brain,
            brain_status: BrainStatus::default(),
            last_command: None,
            pending_confirms: Vec::new(),
            split_done: None,
            pending_toasts: Vec::new(),
            clear_row_selection: false,
            undo: Default::default(),
            episodes: Vec::new(),
            filed_items: Vec::new(),
            events: Vec::new(),
            todos: Vec::new(),
            due_now: Vec::new(),
            next_due: None,
            purge_due: true,
            purge_last: None,
            purge_next_at: None,
            changes: Vec::new(),
            notes_state: NotesState::default(),
            calendar_state: CalendarState::default(),
            todos_state: TodosState::default(),
            toasts: ToastState::default(),
            transcript_focus: TranscriptFocus::default(),
            chime: Box::new(|| {}),
            warning: None,
            warning_seen: None,
            sidebar: SidebarState {
                episodes_open: sidebar_open,
                ..SidebarState::default()
            },
            sidebar_episodes: HashMap::new(),
            unfiled_today: 0,
            latest_note: None,
            focus_episode: None,
            prefs_window: None,
            search: None,
            topic: None,
            status_item: status,
            status_shown: None,
            menu_events,
            quit_requested: false,
            quit_flag: None,
        };
        app.reload();
        app
    }

    /// The sound played when a voice command created something (default: none).
    pub fn set_chime(&mut self, chime: Chime) {
        self.chime = chime;
    }

    /// Every todo, in display order.
    pub fn todos(&self) -> &[Todo] {
        &self.todos
    }

    /// The todos whose reminder pop-up is showing.
    pub fn due_todos(&self) -> &[Todo] {
        &self.due_now
    }

    /// When the next todo comes due (the one scheduled repaint).
    pub fn next_due(&self) -> Option<i64> {
        self.next_due
    }

    /// The voice-command confirmations on screen.
    pub fn confirmations(&self) -> &[Confirmation] {
        &self.toasts.confirmations
    }

    /// The transcript row the list will scroll to and flash when it lays it out.
    pub fn pending_segment_focus(&self) -> Option<i64> {
        self.transcript_focus.pending
    }

    /// Clear a warning that has shown for [`WARNING_TTL`]; returns how long until it does
    /// (for one `request_repaint_after`). A changed text restarts the clock.
    fn expire_warning(&mut self, now: std::time::Instant) -> Option<std::time::Duration> {
        let Some(w) = self.warning.clone() else {
            self.warning_seen = None;
            return None;
        };
        let since = match &self.warning_seen {
            Some((seen, at)) if *seen == w => *at,
            _ => {
                self.warning_seen = Some((w, now));
                now
            }
        };
        let age = now.saturating_duration_since(since);
        if age >= WARNING_TTL {
            self.warning = None;
            self.warning_seen = None;
            None
        } else {
            Some(WARNING_TTL - age)
        }
    }

    /// A segment was transcribed: a stale "transcription failed" warning is superseded.
    fn supersede_transcription_warning(&mut self) {
        if self
            .warning
            .as_deref()
            .is_some_and(|w| w.starts_with("transcription failed"))
        {
            self.warning = None;
            self.warning_seen = None;
        }
    }

    /// The status strip's warning, if any.
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
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

    /// Share a flag that, once set (a termination signal), makes the app quit gracefully the
    /// same way ⌘Q does.
    pub fn set_quit_flag(&mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.quit_flag = Some(flag);
    }

    /// Start quitting when the shared flag is set (once).
    fn poll_quit_flag(&mut self, ctx: &egui::Context) {
        if !self.quit_requested
            && self
                .quit_flag
                .as_ref()
                .is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst))
        {
            self.request_quit(ctx);
        }
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
                "OatsNotes — Listening · {} · {n} segment{} today",
                self.sources_label(),
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
                    self.window_hidden = false;
                    // whatever came due while hidden pops up now
                    self.refresh_due();
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
            self.window_hidden = true;
            log::info!("window hidden; still running in the status item (Quit to exit)");
        }
    }

    /// Ask the pipeline for level events only while the window can be seen.
    fn sync_levels(&mut self, ctx: &egui::Context) {
        let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
        let want = !self.window_hidden && !minimized;
        if want != self.levels_on {
            self.levels_on = want;
            self.pipeline.set_levels_wanted(want);
            if !want {
                self.level = None;
            }
        }
    }

    /// Whether listening is on (the microphone is open and the pipeline is fed).
    pub fn is_listening(&self) -> bool {
        self.capture.is_some()
    }

    /// "mic + system" / "mic" / "system": what this listening session hears.
    pub fn sources_label(&self) -> &'static str {
        match (self.mic_on, self.system_on) {
            (true, true) => "mic + system",
            (false, true) => "system",
            _ => "mic",
        }
    }

    /// The lanes the pills show: the running ones, or (while a lane change restarts the
    /// captures) the set being opened.
    fn lanes_shown(&self) -> Lanes {
        if self.auto_listen && !self.is_listening() {
            self.prefs.lanes
        } else {
            self.session_lanes()
        }
    }

    /// The lanes running now (none while idle).
    fn session_lanes(&self) -> Lanes {
        if self.is_listening() {
            Lanes {
                mic: self.mic_on,
                system: self.system_on,
            }
        } else {
            Lanes::NONE
        }
    }

    /// Listening with exactly these lanes: none stops it; otherwise remember the set and
    /// (re)open the captures. The session is not kept across a lane change: the pipeline
    /// session is ended and a new one starts once it reports Idle (the `auto_listen` path).
    fn set_lanes(&mut self, lanes: Lanes) {
        self.auto_listen = false;
        if !lanes.any() {
            self.stop_listening();
            self.prefs.listening = false;
            return;
        }
        self.prefs.lanes = lanes;
        self.prefs.listening = true;
        if self.is_listening() {
            self.stop_listening();
            self.auto_listen = true;
        } else if !self.start_listening() {
            self.prefs.listening = false;
        }
    }

    /// The system-audio lane is running in this session.
    pub fn system_audio_on(&self) -> bool {
        self.system_on
    }

    /// "On a call" is on.
    pub fn on_call(&self) -> bool {
        self.on_call
    }

    /// The Preferences window's Apps table, while it is open.
    pub fn prefs_apps(&self) -> Option<&[AppRow]> {
        self.prefs_window.as_ref().map(|w| w.apps.as_slice())
    }

    /// Change an app's class / "transcribe anyway" (the Apps table): stored at once, applied
    /// by the media watcher at its next look (≤ 5 s).
    pub fn set_app_class(
        &mut self,
        bundle_id: &str,
        class: oats_core::ProducerClass,
        transcribe_anyway: bool,
    ) {
        if oats_media::is_locked(bundle_id) {
            return;
        }
        match self
            .store
            .set_producer_override(bundle_id, class, transcribe_anyway)
        {
            Ok(()) => {
                log::info!(
                    "app class: {bundle_id} → {}{}",
                    class.as_str(),
                    if transcribe_anyway {
                        " (transcribe anyway)"
                    } else {
                        ""
                    }
                );
                if let Some(w) = &mut self.prefs_window
                    && let Ok(rows) = app_rows(&self.store)
                {
                    w.apps = rows;
                }
                self.reload_brain_data();
            }
            Err(e) => self.warning = Some(format!("Could not save the app's class: {e}")),
        }
    }

    /// "Mark as a call" on a media note: a conversation again (out of Media, into the
    /// All notes list), its segments stamped as a call, and a re-interpretation queued
    /// (the meeting hint, none of the media rules).
    pub fn mark_episode_as_call(&mut self, id: i64) {
        match self.store.mark_episode_as_call(id) {
            Ok(true) => {
                self.brain_send(BrainCommand::Reinterpret(id));
                self.last_command = Some("Marked as a call: re-interpreting it".to_owned());
                self.reload();
            }
            Ok(false) => self.warning = Some("Not a media note".to_owned()),
            Err(e) => self.store_error(e),
        }
    }

    /// "Mark as media" on a conversation from an app classed Media: a media note (no node,
    /// the Media view), re-interpreted by the media rules (no todos, no events).
    pub fn mark_episode_as_media(&mut self, id: i64) {
        match self.store.mark_episode_as_media(id) {
            Ok(true) => {
                self.brain_send(BrainCommand::Reinterpret(id));
                self.last_command = Some("Marked as media: it is in the Media view now".to_owned());
                self.reload();
            }
            Ok(false) => self.warning = Some("Already a media note".to_owned()),
            Err(e) => self.store_error(e),
        }
    }

    /// "Mark as a call" on a Calendar play: relabelled ("Call via …"); whatever was
    /// transcribed during it is re-interpreted as a call. Under Log only nothing was kept,
    /// and the confirmation says so.
    pub fn mark_play_as_call(&mut self, id: i64) {
        match self.store.mark_play_as_call(id) {
            Ok(Some(r)) => {
                for ep in &r.episodes {
                    self.brain_send(BrainCommand::Reinterpret(*ep));
                }
                self.last_command = Some(if r.transcribed() {
                    "Marked as a call: re-interpreting it".to_owned()
                } else {
                    let policy = oats_core::MediaPolicy::effective(
                        self.store
                            .meta(oats_core::MediaPolicy::META_KEY)
                            .ok()
                            .flatten()
                            .as_deref(),
                    );
                    format!(
                        "Marked as a call. No transcript was kept for this (policy: {})",
                        policy.label().to_lowercase()
                    )
                });
                self.reload();
            }
            Ok(None) => self.warning = Some("That play is gone".to_owned()),
            Err(e) => self.store_error(e),
        }
    }

    /// The status strip's right-hand line (the last command / confirmation).
    pub fn last_message(&self) -> Option<&str> {
        self.last_command.as_deref()
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
            mic_on: self.mic_on,
            on_call: self.on_call,
            system_on: self.system_on,
            show_media: self.prefs.show_media,
            status: self.status.clone(),
            sidebar_visible: self.prefs.sidebar_visible,
            view: self.prefs.view,
            brain_ready: self.brain.is_some() && self.brain_status.available(),
            brain_busy: [
                self.brain_status.busy("interpret", None),
                self.brain_status.busy("review", None),
                self.brain_status.busy("reorganise", None),
            ],
            undo_label: self.undo.peek_undo().map(|e| e.label.clone()),
            redo_label: self.undo.peek_redo().map(|e| e.label.clone()),
        })
    }

    fn brain_send(&mut self, cmd: BrainCommand) {
        if self.brain.is_none() {
            self.warning = Some("The brain is not running (OATS_BRAIN=off?)".to_owned());
            return;
        }
        // the click registers at once; the brain's `Queued` / `Settled` events keep it honest
        let (kind, id) = match &cmd {
            BrainCommand::InterpretNow => ("interpret", None),
            BrainCommand::ReviewNow => ("review", None),
            BrainCommand::ReorganiseNow => ("reorganise", None),
            BrainCommand::Reinterpret(id) => ("reinterpret", Some(*id)),
            BrainCommand::Summarise(id) => ("summarise", Some(*id)),
            BrainCommand::Rewrite(id) => ("rewrite", Some(*id)),
            BrainCommand::ResolveMove { target, .. } => {
                let (kind, id) = target.kind_and_id();
                (kind, Some(id))
            }
            BrainCommand::Split { episode, .. } => ("split", Some(*episode)),
            _ => ("", None),
        };
        if !kind.is_empty() {
            if self.brain_status.busy(kind, id) {
                return; // already queued or running: a second click does nothing
            }
            self.brain_status.queue(kind, id);
        }
        if let Some(b) = &self.brain {
            b.send(cmd);
        }
    }

    /// Run a menu / shortcut command.
    pub fn run(&mut self, cmd: Command) {
        match cmd {
            Command::ToggleListening => {
                // the master switch: all off, or the remembered lane set
                if self.is_listening() {
                    self.set_lanes(Lanes::NONE);
                } else {
                    let lanes = self.prefs.lanes;
                    self.set_lanes(if lanes.any() { lanes } else { Lanes::default() });
                }
            }
            Command::ToggleMic => {
                let mut l = self.lanes_shown();
                l.mic = !l.mic;
                self.set_lanes(l);
            }
            Command::ToggleSystemAudio => {
                let mut l = self.lanes_shown();
                l.system = !l.system;
                self.set_lanes(l);
            }
            Command::ToggleOnCall => {
                if self.on_call {
                    self.set_on_call(false);
                } else if self.is_listening() && self.system_on {
                    self.set_on_call(true);
                } else {
                    self.warning = Some("On a call needs listening with system audio".to_owned());
                }
            }
            Command::ToggleShowMedia => {
                self.prefs.show_media = !self.prefs.show_media;
                self.reset_pages();
            }
            Command::JumpToNow => {
                if self.prefs.view != View::Transcripts {
                    self.set_view(View::Transcripts);
                }
                if self.pages.has_later {
                    self.reset_pages();
                }
                self.transcript_focus.pending = None;
                self.pages.jump_to_end = true;
            }
            Command::NewCategory => {
                self.prefs.sidebar_visible = true;
                self.sidebar.renaming = None;
                self.sidebar.adding = Some(String::new());
                self.sidebar.focus = true;
            }
            Command::Preferences => {
                let name = self.store.user_name().ok().flatten().unwrap_or_default();
                let apps = app_rows(&self.store).unwrap_or_else(|e| {
                    self.warning = Some(format!("Apps: {e}"));
                    Vec::new()
                });
                let speaker_names = self
                    .speakers
                    .values()
                    .map(|s| (s.id, s.name.clone().unwrap_or_default()))
                    .collect();
                self.prefs_window = Some(PrefsWindow {
                    name,
                    focus: true,
                    kept_before: None,
                    confirm_purge: false,
                    apps,
                    speaker_names,
                });
            }
            Command::ToggleSidebar => self.prefs.sidebar_visible = !self.prefs.sidebar_visible,
            Command::Search => match &mut self.search {
                Some(s) => {
                    s.focus = true;
                    s.select_all = true;
                }
                None => self.search = Some(SearchState::opened()),
            },
            Command::ShowTranscripts => self.set_view(View::Transcripts),
            Command::ShowNotes => self.set_view(View::Notes),
            Command::ShowCalendar => self.set_view(View::Calendar),
            Command::ShowTodos => self.set_view(View::Todos),
            Command::ShowMediaView => self.set_view(View::Media),
            Command::Undo => self.undo_step(true),
            Command::Redo => self.undo_step(false),
            Command::InterpretNow | Command::ReviewNow | Command::Reorganise => {
                // unavailable: the buttons and menu items are greyed out; the shortcut does nothing
                if self.brain.is_some() && !self.brain_status.available() {
                    return;
                }
                self.brain_send(match cmd {
                    Command::InterpretNow => BrainCommand::InterpretNow,
                    Command::ReviewNow => BrainCommand::ReviewNow,
                    _ => BrainCommand::ReorganiseNow,
                });
            }
        }
    }

    /// Whether the search bar is open.
    pub fn search_open(&self) -> bool {
        self.search.is_some()
    }

    /// Close the search bar (the query goes with it); the view underneath is back.
    fn close_search(&mut self) {
        self.search = None;
    }

    /// Run the open search's query again.
    fn run_search(&mut self) {
        let Some(s) = &mut self.search else { return };
        match self.store.search(&s.query, SEARCH_LIMIT) {
            Ok(r) => {
                s.results = r;
                s.queried = s.query.clone();
            }
            Err(e) => {
                s.results = Default::default();
                s.queried = s.query.clone();
                self.store_error(e);
            }
        }
    }

    fn apply_search(&mut self, ctx: &egui::Context, actions: Vec<SearchAction>) {
        for a in actions {
            match a {
                SearchAction::Close => self.close_search(),
                SearchAction::Notes(n) => self.apply_notes(ctx, vec![n]),
                SearchAction::OpenEpisode(id) => {
                    let node = self
                        .search
                        .as_ref()
                        .and_then(|s| s.results.episodes.iter().find(|e| e.id == id))
                        .and_then(|e| e.node_id);
                    self.close_search();
                    self.jump_to_episode(id, node);
                }
                SearchAction::OpenSegment(id) => self.jump_to_segment(Some(id)),
                SearchAction::OpenEvent(_) => {
                    self.close_search();
                    self.set_view(View::Calendar);
                }
                SearchAction::OpenTag(name) => self.open_topic(name),
                SearchAction::OpenMedia(id) => {
                    self.show_media_play(id);
                }
            }
        }
    }

    /// Whether the Topic view is open.
    pub fn topic_open(&self) -> bool {
        self.topic.is_some()
    }

    /// The note shown alone (the single-note view), if any.
    /// Whether the sidebar has this node folded (tests).
    pub fn sidebar_collapsed(&self, id: i64) -> bool {
        self.sidebar.collapsed.contains(&id)
    }

    pub fn focus_episode(&self) -> Option<i64> {
        self.focus_episode
    }

    /// Whether a project's sidebar layer (its recent notes) is open.
    pub fn sidebar_layer_open(&self, project: i64) -> bool {
        self.sidebar.episodes_open.contains(&project)
    }

    /// Notes expanded on the project page.
    pub fn open_notes(&self) -> &std::collections::HashSet<i64> {
        &self.notes_state.open_notes
    }

    /// The episode the notes list will scroll to and flash when it lays its card out.
    /// The episodes the current view lists.
    pub fn listed_episodes(&self) -> &[Episode] {
        &self.episodes
    }

    pub fn pending_focus(&self) -> Option<i64> {
        self.notes_state.pending_focus
    }

    /// Open the Topic view for `topic` (the search bar closes; the view underneath stays).
    fn open_topic(&mut self, topic: String) {
        let episodes = match self.store.episodes_with_topic(&topic) {
            Ok(e) => e,
            Err(e) => {
                self.store_error(e);
                Vec::new()
            }
        };
        self.close_search();
        self.topic = Some(TopicState { topic, episodes });
    }

    fn close_topic(&mut self) {
        self.topic = None;
    }

    /// Show an episode's note: its node's view (All notes when it has none), scrolled to its card,
    /// which flashes. The card consumes `pending_focus` when it is laid out.
    fn jump_to_episode(&mut self, id: i64, node: Option<i64>) {
        // a media note lives in the Media view (it has no node)
        if node.is_none()
            && self
                .store
                .episode(id)
                .ok()
                .flatten()
                .is_some_and(|e| e.is_media())
        {
            if !self.open_media_note(id) {
                self.warning = Some("That media note has no play in the history".to_owned());
            }
            return;
        }
        self.topic = None;
        self.focus_episode = None;
        self.notes_state.pending_focus = Some(id);
        match node.filter(|n| self.nodes.iter().any(|x| x.id == *n)) {
            Some(n) => self.set_view(View::Node(n)),
            None => self.set_view(View::Notes),
        }
    }

    fn apply_topic(&mut self, actions: Vec<TopicAction>) {
        for a in actions {
            match a {
                TopicAction::Close => self.close_topic(),
                TopicAction::Open(id) => {
                    let node = self
                        .topic
                        .as_ref()
                        .and_then(|t| t.episodes.iter().find(|e| e.id == id))
                        .and_then(|e| e.node_id);
                    self.jump_to_episode(id, node);
                }
            }
        }
    }

    fn set_view(&mut self, view: View) {
        if self.prefs.view != view {
            self.prefs.view = view;
            self.focus_episode = None;
            self.notes_state.editing = None;
            self.notes_state.confirm_delete = None;
            self.notes_state.show_transcript = false;
            self.notes_state.show_all_steps = false;
            self.notes_state.show_low_signal = false;
            self.clear_row_selection = true;
            self.reload_rows();
            self.reload_brain_data();
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
        let lanes = self.prefs.lanes;
        if !lanes.any() {
            return false;
        }
        let request = AudioRequest {
            mic: lanes.mic,
            system: lanes.system,
        };
        match (self.audio)(tx, request) {
            Ok(started) => {
                self.capture = Some(started.guard);
                self.mic_on = request.mic;
                self.system_on = started.system;
                self.system_no_signal = false;
                if let Some(e) = started.system_error {
                    log::warn!("system audio: {e}");
                    self.warning = Some(format!("System audio: {e}"));
                }
                self.pipeline.start_lanes(
                    rx,
                    ListenOptions {
                        mic: request.mic,
                        system: started.system,
                        ..ListenOptions::default()
                    },
                );
                log::info!("listening started ({})", self.sources_label());
                true
            }
            Err(e) => {
                let what = if request.mic {
                    "Microphone"
                } else {
                    "System audio"
                };
                log::warn!("{what}: {e}");
                self.warning = Some(format!("{what}: {e}"));
                false
            }
        }
    }

    /// "On a call" on / off (the pipeline treats every system-audio app as a meeting).
    fn set_on_call(&mut self, on: bool) {
        if self.on_call != on {
            log::info!("on a call: {}", if on { "on" } else { "off" });
        }
        self.on_call = on;
        self.pipeline.set_on_call(on);
    }

    /// Detach and close the microphone (the pipeline flushes the open segment).
    fn stop_listening(&mut self) {
        self.set_on_call(false);
        if self.capture.is_some() {
            self.pipeline.stop();
            self.capture = None;
            self.level = None;
            self.mic_on = false;
            self.system_on = false;
            self.system_no_signal = false;
            log::info!("listening stopped");
        }
    }

    /// Re-read the tree, the current view, today's count and the brain's output.
    fn reload(&mut self) {
        self.reload_speakers();
        match self.store.nodes() {
            Ok(n) => self.nodes = n,
            Err(e) => self.store_error(e),
        }
        // a deleted node can't stay the view
        if let View::Node(id) = self.prefs.view
            && !self.nodes.iter().any(|n| n.id == id)
            && !self.nodes.is_empty()
        {
            self.prefs.view = View::Transcripts;
        }
        self.reload_sidebar_episodes();
        self.reload_rows();
        self.reload_brain_data();
    }

    /// The last [`SIDEBAR_EPISODES`] notes (low-signal ones left out) of every project.
    fn reload_sidebar_episodes(&mut self) {
        self.unfiled_today = self
            .store
            .unfiled_episodes_since(self.today_start)
            .unwrap_or(0);
        // the most recent note is marked only on the day it started: after midnight it is
        // an ordinary row again (this reload runs on the midnight rollover)
        let today = self.today_start;
        self.latest_note = self
            .store
            .latest_note()
            .ok()
            .flatten()
            .filter(|e| e.started_at >= today)
            .map(|e| {
                let filed = e.node_id.is_some_and(|n| {
                    self.nodes
                        .iter()
                        .any(|x| x.id == n && x.kind != oats_core::NodeKind::Category)
                });
                LatestNote {
                    id: e.id,
                    unfiled: !filed,
                }
            });
        let mut map = HashMap::new();
        let projects: Vec<i64> = self
            .nodes
            .iter()
            .filter(|n| n.kind == oats_core::NodeKind::Project && !n.archived)
            .map(|n| n.id)
            .collect();
        for project in projects {
            match self.store.episodes_under(project, 40) {
                Ok(eps) => {
                    let eps: Vec<Episode> = eps
                        .into_iter()
                        .filter(|e| !e.low_signal)
                        .take(SIDEBAR_EPISODES)
                        .collect();
                    let convertible = self.media_convertible_of(&eps);
                    let list: Vec<SidebarEpisode> = eps
                        .into_iter()
                        .map(|e| SidebarEpisode {
                            id: e.id,
                            is_media: e.is_media(),
                            convertible: convertible.contains(&e.id),
                            node_id: e.node_id,
                            title: e.title,
                            started_at: e.started_at,
                        })
                        .collect();
                    map.insert(project, list);
                }
                Err(e) => {
                    self.store_error(e);
                    return;
                }
            }
        }
        self.sidebar_episodes = map;
    }

    fn reload_rows(&mut self) {
        let today = local_midnight_ms();
        self.today_start = today;
        match self.prefs.view {
            View::Transcripts => {
                self.rows.clear();
                if self.pages.rows.is_empty() {
                    self.reset_pages();
                } else {
                    self.refresh_pages();
                }
            }
            View::Node(_) => match self.store.segments(self.prefs.view) {
                Ok(r) => self.rows = r,
                Err(e) => self.store_error(e),
            },
            View::Notes | View::Calendar | View::Todos => self.rows.clear(),
            View::Media => {
                self.rows.clear();
                self.reload_media();
            }
        }
        match self.store.segment_count_since(self.today_start) {
            Ok(n) => self.today_count = n,
            Err(e) => self.store_error(e),
        }
        self.run_search();
    }

    /// The newest `limit` rows strictly before `before`, oldest first, and whether older ones
    /// remain (asked for one more than shown).
    fn fetch_before(
        &mut self,
        before: Option<(i64, i64)>,
        limit: usize,
    ) -> (Vec<TranscriptRow>, bool) {
        match self
            .store
            .segments_page_with(before, limit as i64 + 1, self.prefs.show_media)
        {
            Ok(mut rows) => {
                let more = rows.len() > limit;
                if more {
                    rows.remove(0);
                }
                (rows, more)
            }
            Err(e) => {
                self.store_error(e);
                (Vec::new(), false)
            }
        }
    }

    /// The oldest `limit` rows strictly after `after`, and whether newer ones remain.
    fn fetch_after(&mut self, after: (i64, i64), limit: usize) -> (Vec<TranscriptRow>, bool) {
        match self
            .store
            .segments_after_with(after, limit as i64 + 1, self.prefs.show_media)
        {
            Ok(mut rows) => {
                let more = rows.len() > limit;
                rows.truncate(limit);
                (rows, more)
            }
            Err(e) => {
                self.store_error(e);
                (Vec::new(), false)
            }
        }
    }

    /// The initial Transcripts list: the newest page of rows.
    fn reset_pages(&mut self) {
        let (rows, more) = self.fetch_before(None, self.page_rows);
        self.pages.set(rows, more, false);
    }

    /// Re-read the loaded range (a segment was filed, deleted, interpreted…).
    fn refresh_pages(&mut self) {
        let len = self.pages.rows.len().max(self.page_rows);
        if self.pages.has_later {
            let Some((t, id)) = self.pages.first_key() else {
                return self.reset_pages();
            };
            // strictly after the key just below the first row: the same range, re-read
            let (rows, more) = self.fetch_after((t, id - 1), len);
            let earlier = self.pages.has_earlier;
            self.pages.set(rows, earlier, more);
        } else {
            let (rows, more) = self.fetch_before(None, len);
            self.pages.set(rows, more, false);
        }
    }

    /// Make sure `row` is in the loaded range. When it is not, a window of `page_rows / 2`
    /// rows either side of it REPLACES the range; paging then continues outward both ways
    /// (forward until the newest rows are reached, where live appends resume).
    fn ensure_row_loaded(&mut self, row: &TranscriptRow) {
        if self.pages.contains_row(row.id) {
            return;
        }
        let half = (self.page_rows / 2).max(1);
        let key = (row.started_at, row.id);
        let (mut rows, earlier) = self.fetch_before(Some(key), half);
        rows.push(row.clone());
        let (later_rows, later) = self.fetch_after(key, half);
        rows.extend(later_rows);
        self.pages.set(rows, earlier, later);
    }

    /// Load the page a [`TranscriptAction::LoadEarlier`] / `LoadLater` asked for.
    fn load_adjacent(&mut self, later: bool) {
        if later {
            if !self.pages.has_later {
                return;
            }
            let Some(last) = self.pages.last_key() else {
                return;
            };
            let (rows, more) = self.fetch_after(last, self.page_rows);
            self.pages.append(rows, more);
        } else {
            if !self.pages.has_earlier {
                return;
            }
            let Some(first) = self.pages.first_key() else {
                return;
            };
            let (rows, more) = self.fetch_before(Some(first), self.page_rows);
            self.pages.prepend(rows, more);
        }
    }

    /// The Media view's newest `limit` plays strictly before `before`, and whether older ones
    /// remain.
    fn fetch_media(
        &mut self,
        before: Option<(i64, i64)>,
        limit: usize,
    ) -> (Vec<oats_core::MediaPlay>, bool) {
        match self
            .store
            .media_history_page_latest(before, limit as i64 + 1)
        {
            Ok(mut plays) => {
                let more = plays.len() > limit;
                plays.truncate(limit);
                (plays, more)
            }
            Err(e) => {
                self.store_error(e);
                (Vec::new(), false)
            }
        }
    }

    /// Re-read the Media view from the newest play, as many plays as were loaded (at least a
    /// page).
    fn reload_media(&mut self) {
        let n = self.media_pages.plays.max(MEDIA_PAGE);
        let (plays, more) = self.fetch_media(None, n);
        self.media_pages.clear();
        self.media_pages.append(plays, more);
        self.reload_media_notes();
    }

    /// The media note of every loaded Media view row (the first of its plays that has one).
    fn reload_media_notes(&mut self) {
        let mut found = HashMap::new();
        for e in &self.media_pages.entries {
            for id in &e.ids {
                match self.store.episode_of_play(*id) {
                    Ok(Some(ep)) => {
                        found.insert(*id, ep);
                        break;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        log::warn!("media notes: {err}");
                        break;
                    }
                }
            }
        }
        self.media_notes = found;
    }

    /// Open a media note in the Media view: its row (its media's last play, paged back until
    /// loaded) scrolled to and flashed. `false`: none of its plays is in the history.
    pub fn open_media_note(&mut self, id: i64) -> bool {
        let plays = match self.store.plays_of_episode(id) {
            Ok(p) => p,
            Err(e) => {
                self.store_error(e);
                return false;
            }
        };
        let Some(play) = plays.last().copied() else {
            return false;
        };
        self.show_media_play(play)
    }

    /// The Media view's media notes, by play id (tests).
    pub fn media_notes(&self) -> &HashMap<i64, Episode> {
        &self.media_notes
    }

    /// The Media view's next (older) page.
    fn load_older_media(&mut self) {
        if !self.media_pages.has_more {
            return;
        }
        let before = self.media_pages.oldest;
        let (plays, more) = self.fetch_media(before, MEDIA_PAGE);
        self.media_pages.append(plays, more);
        self.reload_media_notes();
    }

    /// A search hit on a play: the Media view, paged back until its row is loaded, scrolled
    /// to and flashed. The view lists each media on its last play, so an older play of it
    /// lands on that row. `false`: the play is gone.
    fn show_media_play(&mut self, id: i64) -> bool {
        self.close_search();
        self.topic = None;
        self.focus_episode = None;
        if self.prefs.view == View::Media {
            self.reload_media();
        } else {
            self.set_view(View::Media);
        }
        let id = match self.store.latest_play_of(id) {
            Ok(Some(latest)) => latest,
            Ok(None) => id,
            Err(e) => {
                self.store_error(e);
                id
            }
        };
        while self.media_pages.row_of(id).is_none() && self.media_pages.has_more {
            self.load_older_media();
        }
        if self.media_pages.row_of(id).is_some() {
            self.media_focus.pending = Some(id);
            true
        } else {
            self.warning = Some("That play is gone".to_owned());
            false
        }
    }

    /// The Media view's rows: `(title, play ids)`, newest first (tests).
    pub fn media_rows(&self) -> Vec<(String, Vec<i64>)> {
        self.media_pages
            .entries
            .iter()
            .map(|e| (crate::media::entry_title(&e.play), e.ids.clone()))
            .collect()
    }

    /// The last URL the Media view opened in the browser (tests).
    pub fn last_opened_url(&self) -> Option<&str> {
        self.opened_url.as_deref()
    }

    /// The Media view row a jump will scroll to (tests).
    pub fn pending_media_focus(&self) -> Option<i64> {
        self.media_focus.pending
    }

    /// The Media view row flashing after a jump (tests).
    pub fn media_flash(&self) -> Option<i64> {
        self.media_focus.flash.map(|f| f.0)
    }

    /// Use another thumbnail cache (tests point it at a temp directory and a fake fetcher).
    #[doc(hidden)]
    pub fn set_thumbs(&mut self, thumbs: Thumbs) {
        self.thumbs = thumbs;
    }

    fn apply_media(&mut self, ctx: &egui::Context, actions: Vec<MediaAction>) {
        let mut changed = false;
        // a media note's Summarise / Re-interpret: the cards' own handlers
        let mut notes = Vec::new();
        for a in actions {
            match a {
                MediaAction::OpenUrl(url) => {
                    ctx.open_url(egui::OpenUrl::new_tab(&url));
                    self.opened_url = Some(url);
                }
                MediaAction::CopyLink(url) => {
                    ctx.copy_text(url);
                    self.last_command = Some("Link copied".to_owned());
                }
                MediaAction::MarkAsCall(ids) => {
                    for id in ids {
                        self.mark_play_as_call(id);
                    }
                    changed = true;
                }
                MediaAction::ShowTranscript(ids) => match self.store.first_segment_of_plays(&ids) {
                    Ok(seg) => self.jump_to_segment(seg),
                    Err(e) => self.store_error(e),
                },
                MediaAction::Delete(ids) => {
                    // a row stands for every play of its media: older ones would resurface
                    let mut all = Vec::new();
                    for id in ids {
                        match self.store.plays_of_same_media(id) {
                            Ok(same) => all.extend(same),
                            Err(e) => {
                                self.store_error(e);
                                all.push(id);
                            }
                        }
                    }
                    all.sort_unstable();
                    all.dedup();
                    self.delete_plays(&all);
                    changed = true;
                }
                MediaAction::Summarise(id) => notes.push(NotesAction::Summarise(id)),
                MediaAction::Reinterpret(id) => notes.push(NotesAction::Reinterpret(id)),
                MediaAction::LoadOlder => {
                    self.load_older_media();
                    ctx.request_repaint();
                }
            }
        }
        if changed {
            self.reload_media();
        }
        self.apply_notes(ctx, notes);
    }

    /// Delete plays (a Media view row); a YouTube thumbnail cached for them goes too unless
    /// another play still shows that video.
    fn delete_plays(&mut self, ids: &[i64]) {
        let videos: Vec<String> = ids
            .iter()
            .filter_map(|id| self.store.media_play(*id).ok().flatten())
            .filter_map(|p| p.url.as_deref().and_then(oats_media::youtube_video_id))
            .collect();
        if let Err(e) = self.store.delete_media_plays(ids) {
            return self.store_error(e);
        }
        // the user's own delete: leave no copy in the WAL
        self.store.checkpoint();
        for v in videos {
            let still_used = match self.store.media_play_urls_containing(&v) {
                Ok(urls) => urls
                    .iter()
                    .any(|u| oats_media::youtube_video_id(u).as_deref() == Some(v.as_str())),
                Err(e) => {
                    self.store_error(e);
                    true
                }
            };
            if !still_used {
                self.thumbs.forget(&v);
            }
        }
        self.media_plays_day = None;
    }

    /// Rows loaded in the Transcripts view, and the ids of the first and last (tests).
    pub fn loaded_rows(&self) -> (usize, Option<i64>, Option<i64>) {
        (
            self.pages.rows.len(),
            self.pages.rows.first().map(|r| r.id),
            self.pages.rows.last().map(|r| r.id),
        )
    }

    /// Whether newer rows than the loaded ones exist (after a jump; tests).
    pub fn transcript_has_later(&self) -> bool {
        self.pages.has_later
    }

    /// Whether the Transcripts viewport showed the end of its content at the last layout (tests).
    pub fn transcript_at_end(&self) -> bool {
        self.pages.at_end()
    }

    /// Rows per page (tests use a small one so a few hundred rows make several pages).
    #[doc(hidden)]
    pub fn set_page_rows(&mut self, n: usize) {
        self.page_rows = n.max(2);
        if self.prefs.view == View::Transcripts {
            self.reset_pages();
        }
    }

    /// Episodes (of the selected subtree in a Node view, else all), events, reminders, the
    /// change log and the latest run.
    fn reload_brain_data(&mut self) {
        let episodes = match self.prefs.view {
            View::Node(id) => self.store.episodes_under(id, 300),
            // All notes: every conversation note, filed or not (media notes live in Media)
            View::Notes => self.store.conversations(300),
            _ => self.store.episodes(300),
        };
        match episodes {
            Ok(e) => self.episodes = e,
            Err(e) => self.store_error(e),
        }
        // a project page also lists the items filed there from notes homed elsewhere
        self.filed_items = match self.prefs.view {
            View::Node(id) => self.store.items_filed_under(id).unwrap_or_else(|e| {
                self.store_error(e);
                Vec::new()
            }),
            _ => Vec::new(),
        };
        self.notes_state.media_convertible = self.media_convertible();
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
        match self.store.todos(TodoFilter::All) {
            Ok(t) => self.todos = t,
            Err(e) => self.store_error(e),
        }
        self.refresh_due();
        match self.store.changes(50) {
            Ok(c) => self.changes = c,
            Err(e) => self.store_error(e),
        }
        // the Calendar's media list reloads with the rest, and the Media view's notes
        self.media_plays_day = None;
        if self.prefs.view == View::Media {
            self.reload_media_notes();
        }
        if let Ok(runs) = self.store.runs(1) {
            self.brain_status.last_run = runs.into_iter().next();
        }
        if let Ok(spent) = self.store.cost_since(self.today_start) {
            self.brain_status.spend_today_usd = spent;
        }
        self.run_search();
    }

    /// The listed conversation notes heard (partly) from an app classed Media: "Mark as
    /// media" is offered on them.
    fn media_convertible(&mut self) -> std::collections::HashSet<i64> {
        let episodes = std::mem::take(&mut self.episodes);
        let found = self.media_convertible_of(&episodes);
        self.episodes = episodes;
        found
    }

    /// Which of `episodes` are conversations heard (partly) from an app classed Media.
    fn media_convertible_of(&mut self, episodes: &[Episode]) -> std::collections::HashSet<i64> {
        let ids: Vec<i64> = episodes
            .iter()
            .filter(|e| !e.is_media())
            .map(|e| e.id)
            .collect();
        if ids.is_empty() {
            return Default::default();
        }
        let found = self.store.episode_media_apps(&ids).and_then(|apps| {
            let o = oats_media::ProducerOverrides::new(self.store.producer_overrides()?);
            Ok(apps
                .into_iter()
                .filter(|(_, apps)| {
                    apps.iter()
                        .any(|a| o.effective_class(a) == oats_core::ProducerClass::Media)
                })
                .map(|(id, _)| id)
                .collect())
        });
        found.unwrap_or_else(|e| {
            self.store_error(e);
            Default::default()
        })
    }

    /// Re-read which todos are due now and when the next one comes due.
    fn refresh_due(&mut self) {
        let now = chrono::Local::now().timestamp_millis();
        match self.store.due_todos(now) {
            Ok(d) => self.due_now = d,
            Err(e) => self.store_error(e),
        }
        match self.store.next_due_at(now) {
            Ok(n) => self.next_due = n,
            Err(e) => self.store_error(e),
        }
    }

    /// Show a transcript line: the Transcripts view with that segment's day loaded (whatever
    /// the day, however long ago), scrolled to the row, which flashes. A missing or hidden (echo) segment says so in the
    /// status strip.
    fn jump_to_segment(&mut self, id: Option<i64>) {
        let row = match id.map(|id| self.store.segment(id)) {
            Some(Ok(r)) => r.filter(|r| r.echo_of.is_none()),
            Some(Err(e)) => {
                self.store_error(e);
                None
            }
            None => None,
        };
        let Some(row) = row else {
            self.warning = Some("No transcript for this item".to_owned());
            return;
        };
        self.close_search();
        self.topic = None;
        self.set_view(View::Transcripts);
        self.ensure_row_loaded(&row);
        self.transcript_focus.pending = Some(row.id);
    }

    /// Where an event was said: its hotword segment, else the first segment of the episode it
    /// was interpreted from.
    fn event_source(&mut self, id: i64) -> Option<i64> {
        let event = self.events.iter().find(|e| e.id == id).cloned()?;
        if let Some(seg) = event.source_segment_id {
            return Some(seg);
        }
        let episode = event.source_episode_id?;
        match self.store.segments_of_episode(episode) {
            Ok(rows) => rows
                .iter()
                .min_by_key(|r| (r.started_at, r.id))
                .map(|r| r.id),
            Err(e) => {
                self.store_error(e);
                None
            }
        }
    }

    /// Where a todo was said: its own segment, else its event's source.
    fn todo_source(&mut self, id: i64) -> Option<i64> {
        let todo = self.todos.iter().find(|t| t.id == id).cloned()?;
        todo.source_segment_id
            .or_else(|| todo.event_id.and_then(|e| self.event_source(e)))
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

    /// Read every stored voice into the label map (one query; on startup, on a voice change).
    fn reload_speakers(&mut self) {
        match self.store.speakers() {
            Ok(all) => {
                self.speakers = std::sync::Arc::new(all.into_iter().map(|s| (s.id, s)).collect());
            }
            Err(e) => self.store_error(e),
        }
    }

    /// The user changed the stored voices: reload the labels, and have the pipeline reload its
    /// voiceprints before the next segment.
    fn speakers_edited(&mut self) {
        self.reload_speakers();
        self.pipeline.reload_speakers();
    }

    /// A voice's label as the user reads it ("Priya", "them 3", "voice 3", "me").
    fn speaker_name(&self, id: i64) -> String {
        match self.speakers.get(&id) {
            Some(s) if s.is_me => "me".to_owned(),
            Some(s) => s
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| format!("voice {id}")),
            None => format!("voice {id}"),
        }
    }

    fn store_error(&mut self, e: oats_core::StoreError) {
        log::warn!("store: {e}");
        self.warning = Some(format!("Store: {e}"));
    }

    /// Whether a freshly stored row belongs in the list being shown.
    fn in_view(&self, row: &TranscriptRow) -> bool {
        match self.prefs.view {
            // the Transcripts pages take live rows in `handle_events`
            View::Transcripts => false,
            View::Node(id) => row.node_id.is_some_and(|n| self.in_subtree(n, id)),
            View::Notes | View::Calendar | View::Todos | View::Media => false,
        }
    }

    /// Drain and apply the brain's events (`ctx` for the confirmation pop-ups' clock).
    fn handle_brain_events(&mut self, ctx: &egui::Context) {
        let local = std::mem::take(&mut self.pending_confirms);
        if !local.is_empty() {
            self.confirm(ctx, local);
        }
        self.flush_toasts(ctx);
        let events: Vec<BrainEvent> = match &self.brain {
            Some(b) => b.poll(),
            None => Vec::new(),
        };
        let mut reload = false;
        for ev in events {
            match ev {
                BrainEvent::Availability(r) => {
                    if let Err(e) = &r {
                        log::info!("brain unavailable: {e}");
                    }
                    if r.is_err() {
                        // nothing runs while claude is down; the buttons say why instead
                        self.brain_status.pending.clear();
                    }
                    self.brain_status.availability = Some(r);
                }
                BrainEvent::Queued { kind, id } => {
                    self.brain_status.ran.retain(|k| *k != kind);
                    self.brain_status.queue(&kind, id)
                }
                BrainEvent::Settled { kind, id } => {
                    let asked = self.brain_status.waiting_for(&kind);
                    let ran = self.brain_status.ran.contains(&kind);
                    self.brain_status.ran.retain(|k| *k != kind);
                    self.brain_status.settle(&kind, id);
                    self.brain_status.progress = None;
                    if asked
                        && !ran
                        && let Some(text) = crate::notes::nothing_toast(&kind)
                    {
                        self.brain_toast(ctx, text, false);
                    }
                }
                BrainEvent::Progress {
                    kind,
                    step,
                    total,
                    label,
                } => {
                    self.brain_status.progress = Some(crate::notes::Progress {
                        kind,
                        step,
                        total,
                        label,
                    })
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
                    self.brain_status.progress = None;
                    // a split says how it went itself: the confirmation pop-up, or the
                    // reopened field's "Couldn't split it"
                    if run.kind != "split" && self.brain_status.waiting_for(&run.kind) {
                        let (text, error) = crate::notes::run_toast(&run);
                        self.brain_toast(ctx, text, error);
                    }
                    self.brain_status.ran.push(run.kind.clone());
                    self.brain_status.last_run = Some(run);
                }
                BrainEvent::Command {
                    lines,
                    confirmations,
                    ..
                } => {
                    self.last_command = Some(lines.join(" · "));
                    self.confirm(ctx, confirmations);
                    reload = true;
                }
                BrainEvent::Split {
                    episode,
                    new_episode,
                } => {
                    self.split_done = Some((episode, new_episode));
                    reload = true;
                }
                BrainEvent::Changed(_) => {
                    reload = true;
                    self.purge_due = true;
                }
                BrainEvent::Spend { today_usd } => self.brain_status.spend_today_usd = today_usd,
                BrainEvent::Warning(w) => {
                    // the text can quote claude's output: details at debug level only
                    log::info!("brain: warning shown in the status strip");
                    log::debug!("brain: {w}");
                    self.warning = Some(w);
                }
            }
        }
        if self.settle_move_draft(ctx) {
            reload = true;
        }
        if reload {
            self.reload();
        }
        self.settle_split_draft(ctx);
    }

    /// A "Split…" has settled (or its wait ended): when it worked the field closes and the new
    /// note is flashed on the page (a single-note view of the original goes back to the
    /// project page, so both show); otherwise the field reopens with its text and a "Couldn't
    /// split it: …" pop-up.
    fn settle_split_draft(&mut self, ctx: &egui::Context) {
        let Some(d) = self.notes_state.split_draft.clone() else {
            self.split_done = None;
            return;
        };
        if !d.sent || self.brain_status.busy("split", Some(d.episode)) {
            return;
        }
        match self.split_done.take() {
            Some((orig, new)) if orig == d.episode => {
                self.notes_state.split_draft = None;
                if self.focus_episode == Some(orig) {
                    self.focus_episode = None;
                }
                // flashed where it shows: on this page, or in All notes
                let shown_here = match self.prefs.view {
                    View::Notes => true,
                    View::Node(view) => self
                        .store
                        .episode(new)
                        .ok()
                        .flatten()
                        .and_then(|e| e.node_id)
                        .is_some_and(|n| self.in_subtree(n, view)),
                    _ => false,
                };
                if shown_here {
                    self.notes_state.pending_focus = Some(new);
                }
            }
            _ => {
                if let Some(d) = self.notes_state.split_draft.as_mut() {
                    d.sent = false;
                    d.focus = true;
                }
                let why = self
                    .warning
                    .as_deref()
                    .and_then(|w| w.strip_prefix("split: "))
                    .unwrap_or("the brain didn't answer")
                    .to_owned();
                let why: String = why.chars().take(120).collect();
                self.brain_toast(ctx, format!("Couldn't split it: {why}"), true);
            }
        }
    }

    /// "Split…": the user's words go to the brain (`BrainCommand::Split`); the field stays
    /// open, disabled with a spinner, until it settles.
    fn submit_split(&mut self, episode: i64, text: String) {
        if self.brain.is_none() || !self.brain_status.available() {
            self.warning = Some("Splitting a note needs claude (it isn't available)".to_owned());
            return;
        }
        // the field may have gone (the "Replace your edits?" Yes came from the header)
        let draft = self
            .notes_state
            .split_draft
            .get_or_insert_with(|| crate::move_to::SplitDraft::new(episode));
        draft.episode = episode;
        draft.text = text.clone();
        draft.sent = true;
        self.split_done = None;
        self.brain_send(BrainCommand::Split { episode, text });
    }

    /// A "Move to…" the model was asked about has settled (or its wait ended): the field
    /// closes when the item's home changed, else it reopens with the text and a warning pop-up.
    /// `true` when the item moved (the caller reloads).
    fn settle_move_draft(&mut self, ctx: &egui::Context) -> bool {
        let Some(d) = self.notes_state.move_draft.clone() else {
            return false;
        };
        let (kind, id) = d.target.kind_and_id();
        if !d.sent || self.brain_status.busy(kind, Some(id)) {
            return false;
        }
        let home = match d.target {
            MoveTarget::Episode(i) => self.store.episode(i).ok().flatten().map(|e| e.node_id),
            MoveTarget::Segment(i) => self.store.segment(i).ok().flatten().map(|r| r.node_id),
        };
        match home {
            Some(n) if n != d.from => {
                self.notes_state.move_draft = None;
                if let (MoveTarget::Episode(ep), Some(node)) = (d.target, n) {
                    self.follow_moved_episode(ep, node);
                }
                true
            }
            _ => {
                if let Some(d) = self.notes_state.move_draft.as_mut() {
                    d.sent = false;
                    d.focus = true;
                }
                self.brain_toast(ctx, "Couldn't move it; try another name".to_owned(), true);
                false
            }
        }
    }

    /// Show the pop-ups queued in [`Self::pending_toasts`].
    fn flush_toasts(&mut self, ctx: &egui::Context) {
        for (text, error) in std::mem::take(&mut self.pending_toasts) {
            self.brain_toast(ctx, text, error);
        }
    }

    /// A step of the user's own history: recorded unless it is empty.
    fn record(&mut self, label: &str, edit: crate::undo::Edit) {
        self.undo.push(crate::undo::Entry::new(label, edit));
    }

    /// Undo (or redo) one step straight against the store (nothing new is recorded), reload
    /// what it touched and say so in a pop-up. A step whose target is gone is dropped.
    fn undo_step(&mut self, undo: bool) {
        let (verb, taken) = if undo {
            ("Undo", self.undo.take_undo())
        } else {
            ("Redo", self.undo.take_redo())
        };
        let Some(entry) = taken else {
            let what = if undo { "undo" } else { "redo" };
            self.pending_toasts
                .push((format!("Nothing to {what}"), false));
            return;
        };
        match entry.apply(&self.store, undo) {
            Ok(true) => {
                self.pending_toasts
                    .push((format!("{verb}: {}", entry.label), false));
                if let Some((ep, Some(node))) = entry.moved_to(undo) {
                    self.follow_moved_episode(ep, node);
                }
                let todo = matches!(entry.edit, crate::undo::Edit::Todo { .. });
                if undo {
                    self.undo.undone(entry);
                } else {
                    self.undo.redone(entry);
                }
                if todo {
                    self.reload_brain_data();
                } else {
                    self.reload();
                }
            }
            Ok(false) => self.pending_toasts.push((
                format!(
                    "Couldn't {}: {} (it no longer exists)",
                    verb.to_lowercase(),
                    entry.label
                ),
                true,
            )),
            Err(e) => self
                .pending_toasts
                .push((format!("Couldn't {}: {e}", verb.to_lowercase()), true)),
        }
    }

    /// "Keep transcripts indefinitely" off: delete the lines the brain is done with
    /// (`Store::purge_transcripts` from `drop_transcripts_from`, older than
    /// [`TRANSCRIPT_GRACE`]) — at most once a minute, and only when something happened (a
    /// stored segment, a brain change, the preference itself) or when the earliest waiting
    /// line has aged past the grace, for which ONE repaint is scheduled. Nothing polls.
    fn purge_transcripts(&mut self, ctx: &egui::Context) {
        if self.prefs.keep_transcripts {
            return;
        }
        let now = chrono::Local::now().timestamp_millis();
        if self.purge_next_at.is_some_and(|at| at <= now) {
            self.purge_due = true;
        }
        if !self.purge_due {
            return;
        }
        if let Some(last) = self.purge_last
            && last.elapsed() < PURGE_EVERY
        {
            ctx.request_repaint_after(
                PURGE_EVERY - last.elapsed() + std::time::Duration::from_millis(50),
            );
            return;
        }
        self.purge_due = false;
        self.purge_last = Some(std::time::Instant::now());
        self.purge_next_at = None;
        let grace = TRANSCRIPT_GRACE.as_millis() as i64;
        match self
            .store
            .purge_transcripts(self.prefs.drop_transcripts_from, now - grace)
        {
            Ok(p) => {
                if p.deleted > 0 {
                    log::info!(
                        "transcripts: {} lines deleted (keep transcripts is off)",
                        p.deleted
                    );
                    self.reload();
                    match self.store.segment_count_since(self.today_start) {
                        Ok(n) => self.today_count = n,
                        Err(e) => self.store_error(e),
                    }
                    ctx.request_repaint();
                }
                self.purge_next_at = p.next.map(|ended| ended + grace);
            }
            Err(e) => self.store_error(e),
        }
        if let Some(at) = self.purge_next_at {
            ctx.request_repaint_after(std::time::Duration::from_millis(
                (at - now).max(0) as u64 + 50,
            ));
        }
    }

    /// A brain run's outcome as a small pop-up (4 s, no chime).
    fn brain_toast(&mut self, ctx: &egui::Context, text: String, error: bool) {
        let now = ctx.input(|i| i.time);
        self.toasts.confirm_as(text, now, error);
        ctx.request_repaint();
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(
            crate::toast::CONFIRM_SECS,
        ));
    }

    /// Show a voice command's confirmation pop-ups (they close by themselves after 4 s: one
    /// repaint scheduled for then) and play the ding once, when anything was created.
    fn confirm(&mut self, ctx: &egui::Context, confirmations: Vec<String>) {
        if confirmations.is_empty() {
            return;
        }
        let now = ctx.input(|i| i.time);
        for c in confirmations.into_iter().rev() {
            self.toasts.confirm(c, now);
        }
        ctx.request_repaint();
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(
            crate::toast::CONFIRM_SECS,
        ));
        if self.prefs.command_sound {
            (self.chime)();
        }
    }

    /// Drain and apply the pipeline's events.
    fn handle_events(&mut self) {
        // the day rolled over since the last look: "Today" means a different list now
        if local_midnight_ms() != self.today_start {
            self.reload_rows();
            self.reload_sidebar_episodes();
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
                        self.mic_on = false;
                        self.system_on = false;
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
                PipelineEvent::Level {
                    rms_db_mic,
                    rms_db_system,
                    in_speech_mic,
                    in_speech_system,
                } => {
                    if self.capture.is_some() {
                        // the meter shows the louder lane
                        self.level = Some((
                            rms_db_mic.max(rms_db_system),
                            in_speech_mic || in_speech_system,
                        ));
                    }
                }
                PipelineEvent::Segment(row) if row.echo_of.is_some() => {
                    // the speakers heard by the microphone: kept in the store, never shown
                }
                PipelineEvent::EchoMarked { segment, .. } => {
                    self.rows.retain(|r| r.id != segment);
                    self.pages.remove_row(segment);
                    // recount rather than guess whether it was today's
                    match self.store.segment_count_since(self.today_start) {
                        Ok(n) => self.today_count = n,
                        Err(e) => self.store_error(e),
                    }
                }
                PipelineEvent::SystemAudio { signal } => {
                    self.system_no_signal = !signal && self.system_on;
                }
                PipelineEvent::SpeakersChanged => self.reload_speakers(),
                PipelineEvent::Segment(row) => {
                    self.supersede_transcription_warning();
                    self.purge_due = true;
                    if row
                        .speaker_id
                        .is_some_and(|id| !self.speakers.contains_key(&id))
                    {
                        self.reload_speakers();
                    }
                    if row.started_at >= self.today_start {
                        self.today_count += 1;
                    }
                    if let Some(b) = &self.brain {
                        b.segment_arrived(row.clone());
                    }
                    let hidden_media = !self.prefs.show_media
                        && row.source == oats_core::AudioSourceKind::System
                        && row.media_kind.is_some_and(|k| k.is_media());
                    if self.prefs.view == View::Transcripts {
                        // live rows join the loaded range when it ends at the newest row (after a
                        // jump it ends earlier; paging forward gets there); media rows only
                        // with "Show media"
                        if !self.pages.has_later && !hidden_media {
                            self.pages.rows.push(row);
                        }
                    } else if self.in_view(&row) {
                        self.rows.push(row);
                    }
                }
                PipelineEvent::ListenFailed(why) => {
                    // the pipeline dropped the audio channel: close the microphone too, so
                    // nothing pretends to listen
                    self.capture = None;
                    self.level = None;
                    self.mic_on = false;
                    self.system_on = false;
                    self.prefs.listening = false;
                    self.warning = Some(format!("Listening did not start: {why}"));
                }
                PipelineEvent::OnCallEnded { after_silence } => {
                    self.on_call = false;
                    if after_silence {
                        self.last_command =
                            Some("On a call ended: 10 minutes without system audio".to_owned());
                    }
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
        if self.has_native_menu() {
            return cmds;
        }
        if ctx.egui_wants_keyboard_input() {
            // ⌘F still re-focuses the search field while it is being typed in
            if self.search.is_some()
                && let Some(s) = menu_shortcut(Command::Search)
                && ctx.input_mut(|i| i.consume_shortcut(&s))
            {
                cmds.push(Command::Search);
            }
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

    /// ⌘Z / ⇧⌘Z from egui's input, with or without the native menu (they are not menu key
    /// equivalents, see [`UNDO_SHORTCUT`]), and only while no text field has focus: a focused
    /// field keeps its own undo. ⇧⌘Z is checked first so it never reads as ⌘Z.
    fn undo_keys(&mut self, ctx: &egui::Context) -> Option<Command> {
        if ctx.egui_wants_keyboard_input() {
            return None;
        }
        ctx.input_mut(|i| {
            if i.consume_shortcut(&REDO_SHORTCUT) {
                Some(Command::Redo)
            } else if i.consume_shortcut(&UNDO_SHORTCUT) {
                Some(Command::Undo)
            } else {
                None
            }
        })
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
                                if let Some(s) = shown_shortcut(*cmd) {
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
                SidebarAction::Select(v) => {
                    // the project's own row returns from a single note to its page
                    self.focus_episode = None;
                    self.set_view(v);
                }
                SidebarAction::SelectEpisode(id, node) => {
                    self.topic = None;
                    self.set_view(View::Node(node));
                    self.notes_state.editing = None;
                    self.focus_episode = Some(id);
                }
                SidebarAction::EpisodesToggled => {
                    let mut open: Vec<i64> = self.sidebar.episodes_open.iter().copied().collect();
                    open.sort_unstable();
                    self.prefs.sidebar_episodes_open = open;
                }
                SidebarAction::ToggleSidebar => cmds.push(Command::ToggleSidebar),
                SidebarAction::Add(name) => match self.store.add_category(&name) {
                    Ok(_) => changed = true,
                    Err(e) => self.store_error(e),
                },
                SidebarAction::Rename(id, name) => match self.store.rename_node(id, &name) {
                    Ok(()) => changed = true,
                    Err(e) => self.store_error(e),
                },
                SidebarAction::Reparent(id, text) => {
                    if self.reparent_node(id, &text) {
                        changed = true;
                    }
                }
                SidebarAction::MoveEpisode { episode, text } => {
                    self.submit_move(MoveTarget::Episode(episode), text)
                }
                SidebarAction::MarkAsCall(id) => self.mark_episode_as_call(id),
                SidebarAction::MarkAsMedia(id) => self.mark_episode_as_media(id),
                SidebarAction::MergeDrop { dragged, into, at } => self.ask_merge(dragged, into, at),
                SidebarAction::Delete(id) => match self.store.delete_node(id) {
                    Ok(()) => {
                        if self.prefs.view == View::Node(id) {
                            self.prefs.view = View::Transcripts;
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

    fn apply_transcript(&mut self, ctx: &egui::Context, actions: Vec<TranscriptAction>) {
        let mut edited = false;
        let mut loaded = false;
        for a in actions {
            let r = match a {
                TranscriptAction::Move { segment, category } => {
                    edited = true;
                    self.store.set_segment_node(segment, category)
                }
                TranscriptAction::MoveText { segment, text } => {
                    self.submit_move(MoveTarget::Segment(segment), text);
                    continue;
                }
                TranscriptAction::Delete { segment } => {
                    edited = true;
                    self.store.delete_segment(segment)
                }
                TranscriptAction::LoadEarlier => {
                    self.load_adjacent(false);
                    loaded = true;
                    continue;
                }
                TranscriptAction::LoadLater => {
                    self.load_adjacent(true);
                    loaded = true;
                    continue;
                }
                TranscriptAction::SpeakerName { speaker, name } => {
                    self.name_speaker(speaker, &name);
                    continue;
                }
                TranscriptAction::SpeakerMe { speaker } => {
                    self.speaker_is_me(speaker);
                    continue;
                }
                TranscriptAction::SpeakerMerge { speaker, into } => {
                    self.merge_speaker(speaker, &into);
                    continue;
                }
                TranscriptAction::SpeakerForget { speaker } => {
                    self.forget_speaker(speaker);
                    continue;
                }
                TranscriptAction::SpeakerReassign { segments, target } => {
                    self.reassign_lines(ctx, &segments, target);
                    continue;
                }
            };
            if let Err(e) = r {
                self.store_error(e);
            }
        }
        if edited {
            self.reload_rows();
        }
        if loaded {
            // the next frame measures the shift and looks at the edges again
            ctx.request_repaint();
        }
    }

    /// Name a voice (the user's name wins over the brain's; empty clears it).
    fn name_speaker(&mut self, id: i64, name: &str) {
        let name = name.trim();
        match self.store.set_speaker_name(
            id,
            Some(name).filter(|n| !n.is_empty()),
            oats_core::NameSource::User,
            None,
        ) {
            Ok(_) => {
                self.speakers_edited();
                let text = if name.is_empty() {
                    format!("Voice {id}: name cleared")
                } else {
                    format!("Voice {id} is now “{name}”")
                };
                self.pending_toasts.push((text, false));
            }
            Err(e) => self.store_error(e),
        }
    }

    fn speaker_is_me(&mut self, id: i64) {
        match self.store.set_speaker_me(id) {
            Ok(true) => {
                self.speakers_edited();
                self.pending_toasts
                    .push((format!("Voice {id} is you (“me”)"), false));
            }
            Ok(false) => self.speakers_edited(),
            Err(e) => self.store_error(e),
        }
    }

    /// "Merge into…": `text` names the other voice (resolved locally, no model call).
    fn merge_speaker(&mut self, from: i64, text: &str) {
        let Some(into) = crate::speakers::resolve_voice(text, &self.speakers, from) else {
            self.pending_toasts
                .push((format!("No single voice matches “{}”", text.trim()), true));
            return;
        };
        let (a, b) = (self.speaker_name(from), self.speaker_name(into));
        match self.store.merge_speakers(from, into) {
            Ok(true) => {
                self.speakers_edited();
                self.reload_rows();
                self.pending_toasts
                    .push((format!("Merged {a} into {b}"), false));
            }
            Ok(false) => self.speakers_edited(),
            Err(e) => self.store_error(e),
        }
    }

    /// "This line was ▸" / "These N lines were ▸": who said `segments`. One line goes through
    /// `Store::correct_segment_speaker` and may offer the lines that sound like it ("Move
    /// them too"); a selection through `correct_segments_speaker` (and is then cleared). The
    /// voiceprints learn only while speaker recognition is on.
    fn reassign_lines(
        &mut self,
        ctx: &egui::Context,
        segments: &[i64],
        target: oats_core::SpeakerTarget,
    ) {
        let learn = self.pipeline.speakers_enabled();
        let now = chrono::Utc::now().timestamp_millis();
        if segments.len() > 1 {
            crate::transcript::clear_row_selection(ctx);
            self.correct_lines(segments, target, learn, now);
            return;
        }
        let Some(&segment) = segments.first() else {
            return;
        };
        let row = match self.store.segment(segment) {
            Ok(r) => r,
            Err(e) => return self.store_error(e),
        };
        let c = match self
            .store
            .correct_segment_speaker(segment, target, learn, now)
        {
            Ok(Some(c)) => c,
            Ok(None) => {
                self.pending_toasts
                    .push(("That line no longer exists".to_owned(), true));
                return;
            }
            Err(e) => return self.store_error(e),
        };
        self.speakers_edited();
        self.reload_rows();
        let who = c.to.map(|id| self.speaker_name(id));
        let kept = match self.store.segment_voice(segment) {
            Ok(v) => v.is_some(),
            Err(e) => return self.store_error(e),
        };
        let note = relabel_note(c.learned, learn, kept);
        let text = match (&who, c.new_voice) {
            (Some(who), Some(_)) => format!("New voice from this line: {who}{note}"),
            (Some(who), None) => format!("Line moved to {who}{note}"),
            (None, _) => "Speaker cleared".to_owned(),
        };
        let now_t = ctx.input(|i| i.time);
        self.toasts.confirm(text, now_t);
        // the lines of the same note (else session) that sound like this one
        let Some(row) = row else {
            return;
        };
        let scope = match row.episode_id {
            Some(e) => oats_core::LineScope::Episode(e),
            None => oats_core::LineScope::Session(row.session_id),
        };
        match self.store.segments_like(
            segment,
            scope,
            c.from,
            c.to,
            oats_core::speakers::SIMILAR_LINE_THRESHOLD,
        ) {
            Ok(like) if !like.is_empty() => {
                let n = like.len();
                let place = if row.episode_id.is_some() {
                    "this note"
                } else {
                    "this session"
                };
                let text = if n == 1 {
                    format!("1 more line in {place} sounds like the same person")
                } else {
                    format!("{n} more lines in {place} sound like the same person")
                };
                let target = match c.to {
                    Some(id) => oats_core::SpeakerTarget::Voice(id),
                    None => oats_core::SpeakerTarget::None,
                };
                self.toasts.confirm_action(
                    text,
                    now_t,
                    "Move them too",
                    ToastAction::ReassignLines {
                        segments: like.into_iter().map(|(id, _)| id).collect(),
                        target,
                    },
                );
            }
            Ok(_) => {}
            Err(e) => self.store_error(e),
        }
        ctx.request_repaint();
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(
            crate::toast::CONFIRM_SECS,
        ));
    }

    /// The similar-lines pop-up's "Move them too".
    fn move_similar_lines(&mut self, segments: &[i64], target: oats_core::SpeakerTarget) {
        let learn = self.pipeline.speakers_enabled();
        let now = chrono::Utc::now().timestamp_millis();
        self.correct_lines(segments, target, learn, now);
    }

    /// Several lines at once (one `segment_speakers` change), with its pop-up.
    fn correct_lines(
        &mut self,
        segments: &[i64],
        target: oats_core::SpeakerTarget,
        learn: bool,
        now: i64,
    ) {
        let r = match self
            .store
            .correct_segments_speaker(segments, target, learn, now)
        {
            Ok(r) => r,
            Err(e) => return self.store_error(e),
        };
        self.speakers_edited();
        self.reload_rows();
        let lines = if r.moved == 1 {
            "1 line".to_owned()
        } else {
            format!("{} lines", r.moved)
        };
        let bare = r.moved - r.learned;
        let note = if r.moved == 0 || bare == 0 {
            String::new()
        } else if !learn {
            " (relabelled only: speaker recognition is off)".to_owned()
        } else {
            format!(" ({bare} relabelled only: no voiceprint kept)")
        };
        let text = match (r.to.map(|id| self.speaker_name(id)), r.new_voice) {
            _ if r.moved == 0 => "Those lines no longer exist".to_owned(),
            (Some(who), Some(_)) => format!("New voice from {lines}: {who}{note}"),
            (Some(who), None) => format!("Moved {lines} to {who}{note}"),
            (None, _) => format!("Speaker cleared on {lines}"),
        };
        self.pending_toasts.push((text, r.moved == 0));
    }

    fn forget_speaker(&mut self, id: i64) {
        let name = self.speaker_name(id);
        match self.store.forget_speaker(id) {
            Ok(_) => {
                self.speakers_edited();
                self.reload_rows();
                self.pending_toasts
                    .push((format!("Forgot the voice of {name}"), false));
            }
            Err(e) => self.store_error(e),
        }
    }

    fn apply_notes(&mut self, ctx: &egui::Context, actions: Vec<NotesAction>) {
        for a in actions {
            match a {
                NotesAction::InterpretNow => self.run(Command::InterpretNow),
                NotesAction::ReviewNow => self.run(Command::ReviewNow),
                NotesAction::Reorganise => self.run(Command::Reorganise),
                NotesAction::Reinterpret(id) => self.brain_send(BrainCommand::Reinterpret(id)),
                NotesAction::MarkAsCall(id) => self.mark_episode_as_call(id),
                NotesAction::MarkAsMedia(id) => self.mark_episode_as_media(id),
                NotesAction::Summarise(id) => {
                    self.brain_send(BrainCommand::Summarise(id));
                    self.last_command = Some("Summarising the media note…".to_owned());
                }
                NotesAction::Undo(id) => self.brain_send(BrainCommand::UndoChange(id)),
                NotesAction::SelectNode(id) => {
                    self.focus_episode = None;
                    self.set_view(View::Node(id));
                }
                NotesAction::GoToNote(id) => self.open_note_page(id),
                NotesAction::ShowSegment(seg) => self.jump_to_segment(Some(seg)),
                NotesAction::SetActionItemDone {
                    episode_id,
                    text,
                    done,
                } => {
                    let now = chrono::Local::now().timestamp_millis();
                    // unticking: its completion time, so an undo puts the same one back
                    let stamp = if done {
                        Some(now)
                    } else {
                        self.store.episode(episode_id).ok().flatten().and_then(|e| {
                            e.action_items
                                .iter()
                                .find(|a| a.text.trim() == text.trim() && a.done_at.is_some())
                                .and_then(|a| a.done_at)
                        })
                    };
                    match self
                        .store
                        .set_action_item_done(episode_id, &text, done, now)
                    {
                        Ok(true) => {
                            if let Some(stamp) = stamp {
                                self.record(
                                    if done { "Mark complete" } else { "Mark open" },
                                    crate::undo::Edit::ActionItemDone {
                                        episode_id,
                                        text,
                                        done,
                                        stamp,
                                    },
                                );
                            }
                            self.reload();
                        }
                        Ok(false) => {}
                        Err(e) => self.store_error(e),
                    }
                }
                NotesAction::MoveText { episode, text } => {
                    self.submit_move(MoveTarget::Episode(episode), text)
                }
                NotesAction::SplitText { episode, text } => self.submit_split(episode, text),
                NotesAction::SaveEdit(id, content) => {
                    let now = chrono::Local::now().timestamp_millis();
                    let before = self.store.episode(id).ok().flatten();
                    match self.store.update_episode_content(id, &content, now) {
                        Ok(()) => {
                            if let (Some(b), Ok(Some(a))) = (before, self.store.episode(id)) {
                                self.record(
                                    "Edit note",
                                    crate::undo::Edit::NoteContent {
                                        id,
                                        before: Box::new(b.content()),
                                        before_edited: b.user_edited_at,
                                        after: Box::new(a.content()),
                                        after_edited: a.user_edited_at,
                                    },
                                );
                            }
                            self.reload()
                        }
                        Err(e) => self.store_error(e),
                    }
                }
                NotesAction::DeleteEpisode(id) => self.delete_note(ctx, id),
                NotesAction::OpenTopic(topic) => self.open_topic(topic),
                NotesAction::MergeDrop { dragged, into, at } => self.ask_merge(dragged, into, at),
            }
        }
    }

    /// A confirmed Delete in a note's edit view: the note and its interpretation events go, its
    /// segments stay (still marked interpreted). Not on the undo stack.
    fn delete_note(&mut self, ctx: &egui::Context, id: i64) {
        let now = chrono::Local::now().timestamp_millis();
        match self.store.delete_episode(id, now) {
            Ok(title) => {
                self.store.checkpoint();
                self.notes_state.editing = None;
                self.notes_state.move_draft = None;
                self.notes_state.split_draft = None;
                if self.focus_episode == Some(id) {
                    self.focus_episode = None;
                }
                self.reload();
                if let Some(title) = title {
                    self.brain_toast(ctx, format!("Deleted “{title}”"), false);
                }
            }
            Err(e) => self.store_error(e),
        }
    }

    /// A note dropped onto another: open the confirmation (nothing is merged before Yes).
    fn ask_merge(&mut self, dragged: i64, into: i64, at: egui::Pos2) {
        let (from, to) = match (self.store.episode(dragged), self.store.episode(into)) {
            (Ok(Some(f)), Ok(Some(t))) => (f, t),
            (Err(e), _) | (_, Err(e)) => return self.store_error(e),
            _ => return,
        };
        if from.is_media() != to.is_media() {
            self.warning = Some("Media and conversations aren't merged".to_owned());
            return;
        }
        self.notes_state.confirm_merge = Some(crate::merge::MergeConfirm {
            dragged,
            into,
            edited: from.user_edited_at.is_some() || to.user_edited_at.is_some(),
            dragged_title: from.title,
            into_title: to.title,
            asking_edits: false,
            at,
        });
    }

    /// A confirmed merge: the dragged note folded into the target (recorded, not undoable),
    /// then the target re-interpreted in place (`BrainCommand::Rewrite`) so its text covers
    /// both (unless the user kept their edited text).
    pub fn merge_notes(&mut self, ctx: &egui::Context, merge: crate::merge::MergeAction) {
        let now = chrono::Local::now().timestamp_millis();
        match crate::merge::apply_merge(&self.store, &merge, now) {
            Ok(title) => {
                if self.focus_episode == Some(merge.dragged) {
                    self.focus_episode = Some(merge.into);
                }
                if self
                    .notes_state
                    .editing
                    .as_ref()
                    .is_some_and(|d| d.episode_id == merge.dragged)
                {
                    self.notes_state.editing = None;
                }
                // re-interpreted in place: a plain Re-interpret would re-cut the transcript by
                // its gaps and split the merged note apart again
                if merge.reinterpret {
                    self.brain_send(BrainCommand::Rewrite(merge.into));
                }
                let text = format!("Merged into {title} — undo not available");
                self.last_command = Some(text.clone());
                self.brain_toast(ctx, text, false);
                self.reload();
            }
            Err(e) => self.warning = Some(e),
        }
    }

    /// "Move to…" on a project / topic: the typed name resolved in the tree (no model call);
    /// a project goes under a category or project, a topic under a project or topic, never
    /// into its own subtree. `true` when it moved.
    fn reparent_node(&mut self, id: i64, text: &str) -> bool {
        use oats_core::NodeKind;
        let Some(node) = self.nodes.iter().find(|n| n.id == id).cloned() else {
            return false;
        };
        let candidates: Vec<oats_core::Node> = self
            .nodes
            .iter()
            // a project goes under a category or a project, a topic anywhere below one
            .filter(|n| n.id != id && (node.kind == NodeKind::Topic || n.kind != NodeKind::Topic))
            .cloned()
            .collect();
        let parent = match oats_brain::resolve_local(&candidates, text) {
            oats_brain::LocalMove::One(p) => p,
            oats_brain::LocalMove::Many(_) => {
                self.warning = Some(format!(
                    "“{text}” matches several places; type more of the name or a path"
                ));
                return false;
            }
            oats_brain::LocalMove::None => {
                self.warning = Some(format!("No category or project named “{text}”"));
                return false;
            }
        };
        let below = self.store.subtree_ids(id).unwrap_or_default();
        if below.contains(&parent) {
            self.warning = Some("It can't go there".to_owned());
            return false;
        }
        if node.parent_id == Some(parent) {
            return false;
        }
        match self.store.move_node(id, Some(parent)) {
            Ok(()) => {
                let path = oats_brain::moveto::node_path(&self.nodes, parent);
                let now = chrono::Local::now().timestamp_millis();
                if let Err(e) = self.store.record_change(
                    None,
                    "node_move",
                    &format!("Moved '{}' under {path} (by you)", node.name),
                    &serde_json::json!({"node_id": id, "parent_id": node.parent_id}),
                    &serde_json::json!({"node_id": id, "parent_id": parent}),
                    now,
                ) {
                    log::warn!("recording the move: {e}");
                }
                self.pending_confirms
                    .push(format!("Moved {} under {path}", node.name));
                true
            }
            Err(e) => {
                self.store_error(e);
                false
            }
        }
    }

    /// The note `ep` moved to `node`: a single-note view (or an open edit of it) follows it
    /// there, keeping the unsaved draft.
    fn follow_moved_episode(&mut self, ep: i64, node: i64) {
        let editing = self
            .notes_state
            .editing
            .as_ref()
            .is_some_and(|d| d.episode_id == ep);
        if self.focus_episode == Some(ep) || editing {
            let draft = self.notes_state.editing.take();
            self.set_view(View::Node(node));
            self.focus_episode = Some(ep);
            if editing {
                self.notes_state.editing = draft;
            }
        }
    }

    /// "Move to…": the typed text is matched against the tree first (no model call); an
    /// ambiguous or new target goes to the brain (`ResolveMove`), and the field stays open,
    /// disabled with a spinner, until it settles.
    fn submit_move(&mut self, target: MoveTarget, text: String) {
        let from = self
            .notes_state
            .move_draft
            .as_ref()
            .filter(|d| d.target == target)
            .and_then(|d| d.from);
        if let oats_brain::LocalMove::One(node) = oats_brain::resolve_local(&self.nodes, &text) {
            return self.move_local(target, node);
        }
        if self.brain.is_none() || !self.brain_status.available() {
            self.warning = Some(
                "That needs claude to work out (it isn't available); use an existing name"
                    .to_owned(),
            );
            return;
        }
        if let Some(d) = self.notes_state.move_draft.as_mut() {
            d.sent = true;
        }
        self.brain_send(BrainCommand::ResolveMove {
            target,
            text,
            current_node: from,
        });
    }

    /// A move the tree settled by itself: refiled at once (an `episode_move` change for a
    /// note), confirmed with a pop-up.
    fn move_local(&mut self, target: MoveTarget, node: i64) {
        let now = chrono::Local::now().timestamp_millis();
        let from = match target {
            MoveTarget::Episode(id) => self.store.episode(id).ok().flatten().map(|e| e.node_id),
            MoveTarget::Segment(_) => None,
        };
        let moved = match target {
            MoveTarget::Episode(id) => self.store.move_episode(id, node, now),
            MoveTarget::Segment(id) => self.store.set_segment_node(id, Some(node)).map(|_| true),
        };
        match moved {
            Ok(moved) => {
                let path = oats_brain::moveto::node_path(&self.nodes, node);
                self.pending_confirms.push(if moved {
                    format!("Moved to {path}")
                } else {
                    format!("Already in {path}")
                });
                self.last_command = Some(format!("Moved to {path}"));
                self.notes_state.move_draft = None;
                if let MoveTarget::Episode(ep) = target {
                    if moved && let Some(from) = from {
                        self.record(
                            "Move note",
                            crate::undo::Edit::MoveNote {
                                id: ep,
                                from,
                                to: node,
                            },
                        );
                    }
                    self.follow_moved_episode(ep, node);
                }
                self.reload();
                if matches!(target, MoveTarget::Segment(_)) {
                    self.reload_rows();
                }
            }
            Err(e) => self.store_error(e),
        }
    }

    fn apply_calendar(&mut self, actions: Vec<CalendarAction>) {
        let mut todo_actions = Vec::new();
        let mut changed = false;
        for a in actions {
            match a {
                CalendarAction::DeleteEvent(id) => {
                    match self.store.delete_event(id) {
                        // the user's own delete: leave no copy in the WAL
                        Ok(()) => self.store.checkpoint(),
                        Err(e) => self.store_error(e),
                    }
                    changed = true;
                }
                CalendarAction::ShowEvent(id) => {
                    let seg = self.event_source(id);
                    self.jump_to_segment(seg);
                }
                CalendarAction::Todo(t) => todo_actions.push(t),
                CalendarAction::MarkPlaysAsCall(ids) => {
                    for id in ids {
                        self.mark_play_as_call(id);
                    }
                }
                CalendarAction::DeletePlays(ids) => {
                    self.delete_plays(&ids);
                    self.reload_media();
                }
            }
        }
        self.apply_todos(todo_actions);
        if changed {
            self.reload_brain_data();
        }
    }

    /// Todo edits from the Todos view, the calendar's day panel and the pop-ups.
    fn apply_todos(&mut self, actions: Vec<TodoAction>) {
        if actions.is_empty() {
            return;
        }
        let now = chrono::Local::now().timestamp_millis();
        let mut changed = false;
        for a in actions {
            // a hand edit of one todo: its fields before, recorded for undo when they changed
            let undoable = match &a {
                TodoAction::SetDue(id, _) => Some((*id, "Change due")),
                TodoAction::SetDone(id, true) => Some((*id, "Complete todo")),
                TodoAction::SetDone(id, false) => Some((*id, "Reopen todo")),
                TodoAction::Snooze { id, .. } => Some((*id, "Snooze todo")),
                TodoAction::ClearSnooze(id) => Some((*id, "Clear snooze")),
                TodoAction::SetText(id, _) => Some((*id, "Edit todo")),
                _ => None,
            };
            let before = undoable.and_then(|(id, label)| {
                let state = self.store.todo(id).ok().flatten()?.state();
                Some((id, label, state))
            });
            let r = match a {
                TodoAction::Show(id) => {
                    let seg = self.todo_source(id);
                    self.jump_to_segment(seg);
                    continue;
                }
                TodoAction::Add { text, due } => self
                    .store
                    .add_todo(
                        &NewTodo {
                            text,
                            due,
                            ..Default::default()
                        },
                        now,
                    )
                    .map(|_| ()),
                TodoAction::SetDue(id, due) => self.store.set_todo_due(id, due),
                TodoAction::SetDone(id, true) => self.store.complete_todo(id, now),
                TodoAction::SetDone(id, false) => self.store.reopen_todo(id),
                TodoAction::Snooze { id, until } => self.store.snooze_todo(id, until),
                TodoAction::ClearSnooze(id) => self.store.clear_todo_snooze(id),
                TodoAction::SetText(id, text) => self.store.set_todo_text(id, &text),
                TodoAction::Delete(id) => self.store.delete_todo(id).map(|()| {
                    // the user's own delete: leave no copy in the WAL
                    self.store.checkpoint();
                }),
            };
            changed = true;
            match r {
                Ok(()) => {
                    if let Some((id, label, before)) = before
                        && let Ok(Some(t)) = self.store.todo(id)
                        && t.state() != before
                    {
                        self.record(
                            label,
                            crate::undo::Edit::Todo {
                                id,
                                before,
                                after: t.state(),
                            },
                        );
                    }
                }
                Err(e) => self.store_error(e),
            }
        }
        if changed {
            self.reload_brain_data();
        }
    }

    fn apply_toasts(&mut self, actions: Vec<ToastAction>) {
        let todo_actions = actions
            .into_iter()
            .filter_map(|a| match a {
                ToastAction::Done(id) => Some(TodoAction::SetDone(id, true)),
                ToastAction::Snooze { id, until } => Some(TodoAction::Snooze { id, until }),
                ToastAction::ShowTodos => {
                    self.close_search();
                    self.topic = None;
                    self.set_view(View::Todos);
                    None
                }
                ToastAction::ReassignLines { segments, target } => {
                    self.move_similar_lines(&segments, target);
                    None
                }
            })
            .collect();
        self.apply_todos(todo_actions);
    }

    /// The media plays of local `day` for the Calendar's day panel.
    fn load_media_plays(&mut self, day: chrono::NaiveDate) {
        use chrono::TimeZone;
        let ms = |d: chrono::NaiveDate| {
            d.and_hms_opt(0, 0, 0)
                .and_then(|t| chrono::Local.from_local_datetime(&t).earliest())
                .map(|t| t.timestamp_millis())
        };
        self.media_plays_day = Some(day);
        let (Some(from), Some(to)) = (ms(day), day.succ_opt().and_then(ms)) else {
            self.media_plays.clear();
            return;
        };
        // a video watched on several days is listed on the last one only
        match self.store.media_plays_between_latest(from, to) {
            Ok(p) => self.media_plays = p,
            Err(e) => self.store_error(e),
        }
    }

    /// "listening · mic + system", "listening · mic", "idle", …
    pub fn state_label(&self) -> String {
        match self.status {
            PipelineStatus::Loading(_) => "loading…".to_owned(),
            PipelineStatus::Idle => "idle".to_owned(),
            PipelineStatus::Listening => format!("listening · {}", self.sources_label()),
            PipelineStatus::Failed(_) => "failed".to_owned(),
        }
    }

    /// The 34 pt control bar: Listen pill, level meter, loading/failure line.
    fn control_bar(&self, ui: &mut egui::Ui, t: &Tokens) -> Vec<Command> {
        let mut cmds = Vec::new();
        ui.add_space(12.0);
        if !self.prefs.sidebar_visible {
            if header_icon(ui, HeaderIcon::ShowSidebar)
                .on_hover_text(with_shortcut(
                    ui.ctx(),
                    "Show sidebar",
                    Command::ToggleSidebar,
                ))
                .clicked()
            {
                cmds.push(Command::ToggleSidebar);
            }
            ui.add_space(6.0);
        }
        let listening = self.is_listening();
        let enabled = listening || self.status == PipelineStatus::Idle;
        let lanes = self.lanes_shown();
        let tip = if lanes.mic {
            "Listening to the microphone (you) — click to turn off"
        } else {
            "Listen to the microphone (you)"
        };
        if pill(ui, t, "Mic", lanes.mic, enabled)
            .on_hover_text(tip)
            .clicked()
        {
            cmds.push(Command::ToggleMic);
        }
        ui.add_space(6.0);
        let tip = if lanes.system && self.system_no_signal && listening {
            "Capturing system audio, but nothing has come through for 30 s — check the \
             System Audio Recording permission — click to turn off"
        } else if lanes.system {
            "Capturing system audio (what the computer plays: the other side of a call) — click to turn off"
        } else {
            "Capture system audio (what the computer plays: the other side of a call)"
        };
        if pill(ui, t, "System audio", lanes.system, enabled)
            .on_hover_text(tip)
            .clicked()
        {
            cmds.push(Command::ToggleSystemAudio);
        }
        ui.add_space(6.0);
        let call_enabled = self.on_call || (listening && self.system_on);
        let tip = if self.on_call {
            "On a call: every app playing sound is the other side of a call (transcribed as \
             “them”, never media). Turns off after 10 min without system audio — click to \
             turn off"
        } else if call_enabled {
            "On a call: treat whatever the computer plays (Slack, Discord, …) as a call, \
             whatever its class and the media policy"
        } else {
            "On a call: needs listening with system audio"
        };
        if pill(ui, t, "On a call", self.on_call, call_enabled)
            .on_hover_text(with_shortcut(ui.ctx(), tip, Command::ToggleOnCall))
            .clicked()
        {
            cmds.push(Command::ToggleOnCall);
        }
        ui.add_space(6.0);
        level_meter(ui, t, if listening { self.level } else { None });
        ui.add_space(6.0);
        // Find at the far right; the engine's loading / failure line in what is left, cut
        // with … (it never pushes the icon out)
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(12.0);
            let icon = if self.search.is_some() {
                HeaderIcon::SearchOn
            } else {
                HeaderIcon::Search
            };
            if header_icon(ui, icon)
                .on_hover_text(with_shortcut(ui.ctx(), "Find", Command::Search))
                .clicked()
            {
                cmds.push(Command::Search);
            }
            ui.add_space(6.0);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let line = match &self.status {
                    PipelineStatus::Loading(line) => Some((line, t.text_secondary)),
                    PipelineStatus::Failed(msg) => Some((msg, t.danger)),
                    _ => None,
                };
                if let Some((text, col)) = line {
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).color(col).size(11.5))
                            .truncate(),
                    );
                }
            });
        });
        cmds
    }

    /// The Preferences window: "Your name", saved on Enter or when the field loses focus.
    fn preferences_window(&mut self, ctx: &egui::Context, t: &Tokens) {
        let menu = if self.has_native_menu() {
            0.0
        } else {
            t.menu_bar_h
        };
        let Some(win) = &mut self.prefs_window else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut close = false;
        let mut sound = self.prefs.command_sound;
        let mut listen_at_launch = self.prefs.listen_at_launch;
        let mut clock_24h = self.prefs.clock_24h;
        let mut thumbnails = self.prefs.fetch_thumbnails;
        let mut keep = self.prefs.keep_transcripts;
        let mut purge_old = false;
        // the lines from before "Keep transcripts" was turned off: kept until asked
        let from = self.prefs.drop_transcripts_from;
        let kept_before = if !keep && from > 0 {
            match win.kept_before {
                Some((f, n)) if f == from => Some(n),
                _ => {
                    let n = self.store.segment_count_before(from).unwrap_or(0);
                    win.kept_before = Some((from, n));
                    Some(n)
                }
            }
        } else {
            None
        };
        let mut speakers_on = self.prefs.speakers;
        let speakers_fixed = oats_core::speakers::speakers_env();
        let mut speaker_edits: Vec<SpeakerEdit> = Vec::new();
        let voices = self.speakers.clone();
        // the YouTube & music policy: OATS_MEDIA_POLICY fixes it, else `meta.media_policy`
        let fixed_policy = oats_core::MediaPolicy::env_override();
        let stored_policy = self
            .store
            .meta(oats_core::MediaPolicy::META_KEY)
            .ok()
            .flatten();
        let policy_before = oats_core::MediaPolicy::effective(stored_policy.as_deref());
        let mut policy = policy_before;
        let mut app_changes: Vec<(String, oats_core::ProducerClass, bool)> = Vec::new();
        // Inside the window, below the in-window menu bar, 12 pt clear of every edge: as wide
        // as fits up to 560 pt, as tall as fits (egui caps the height at the constrain rect;
        // the body scrolls when it is taller).
        let screen = ctx.content_rect();
        let bounds = Rect::from_min_max(pos2(screen.left(), screen.top() + menu), screen.max)
            .shrink(PREFS_INSET);
        let margin = ctx.global_style().spacing.window_margin;
        let chrome_w = margin.sum().x + 2.0;
        let width = (bounds.width() - chrome_w).clamp(200.0, PREFS_WIDTH);
        egui::Window::new("Preferences")
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, bounds.center() - screen.center())
            .constrain_to(bounds)
            .collapsible(false)
            .resizable(false)
            .min_width(width)
            .max_width(width)
            .default_width(width)
            .default_height(f32::INFINITY)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("prefs-body")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                ui.label(egui::RichText::new("Your name").color(t.text_secondary));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut win.name)
                        .hint_text("e.g. Alex")
                        .desired_width(f32::INFINITY),
                );
                if win.focus {
                    resp.request_focus();
                    win.focus = false;
                }
                if resp.lost_focus() {
                    save = true;
                    if ui.input(|i| i.key_pressed(Key::Enter)) {
                        close = true;
                    }
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "The notes recognise when people say it: action items for you are \
                         yours, and summaries can use your name.",
                    )
                    .color(t.text_tertiary)
                    .size(11.0),
                );
                ui.add_space(8.0);
                ui.checkbox(&mut listen_at_launch, "Start listening when the app opens")
                    .on_hover_text(
                        "Off: the app opens idle; ⌘L, the status item or `--listen` starts \
                         listening. On: the remembered lanes open as soon as the engine is ready.",
                    );
                ui.checkbox(&mut sound, "Play a sound for voice commands");
                ui.checkbox(&mut clock_24h, "Use 24-hour time");
                ui.checkbox(
                    &mut thumbnails,
                    "Fetch video thumbnails (contacts YouTube's image servers)",
                )
                .on_hover_text(
                    "The Media view shows a YouTube video's thumbnail, fetched once from \
                     i.ytimg.com and kept in the data folder. Off: nothing is fetched.",
                );
                ui.checkbox(&mut keep, "Keep transcripts indefinitely")
                    .on_hover_text(
                        "On: every transcript line stays in the database. Off: a line is \
                         deleted 30 minutes after the brain has turned it into a note (or \
                         marked it as an echo); the notes, events and todos made from it stay, \
                         but there is no transcript left to show or re-interpret. Lines the \
                         brain has not processed yet are kept until it has.",
                    );
                if let Some(n) = kept_before.filter(|n| *n > 0) {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "{n} lines from before this was turned off are kept."
                            ))
                            .color(t.text_tertiary)
                            .size(11.0),
                        );
                        if win.confirm_purge {
                            ui.label(
                                egui::RichText::new("Delete them? Undo not available.")
                                    .color(t.danger)
                                    .size(11.0),
                            );
                            if ui.small_button("Delete").clicked() {
                                purge_old = true;
                                win.confirm_purge = false;
                            }
                            if ui.small_button("Cancel").clicked() {
                                win.confirm_purge = false;
                            }
                        } else if ui.small_button("Delete them too").clicked() {
                            win.confirm_purge = true;
                        }
                    });
                }
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("Media (YouTube, music, media apps)")
                        .color(t.text_secondary),
                );
                ui.add_enabled_ui(fixed_policy.is_none(), |ui| {
                    ui.horizontal(|ui| {
                        for p in oats_core::MediaPolicy::ALL {
                            ui.radio_value(&mut policy, p, p.label());
                        }
                    });
                });
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(if fixed_policy.is_some() {
                        "Set by OATS_MEDIA_POLICY."
                    } else {
                        "Never taken for your conversation. Transcribe (default): a media note \
                         in the Media view without a model call (“Summarise” runs one), hidden from \
                         Transcripts unless “Show media”. Log only: “Watched: …” in the \
                         Calendar, no transcript. Ignore: nothing kept. Music is logged, never \
                         transcribed."
                    })
                    .color(t.text_tertiary)
                    .size(11.0),
                );
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Apps").color(t.text_secondary));
                apps_table(ui, t, &win.apps, policy, &mut app_changes);
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Speakers").color(t.text_secondary));
                ui.add_enabled_ui(speakers_fixed.is_none(), |ui| {
                    ui.checkbox(
                        &mut speakers_on,
                        "Recognise speakers (keeps a voiceprint per person in the database)",
                    )
                    .on_hover_text(if speakers_fixed.is_some() {
                        "Set by OATS_SPEAKERS."
                    } else {
                        "Each person heard gets a voiceprint (256 numbers, stored only in the \
                         local database) so their lines are labelled on later calls too. Off: \
                         no speaker model runs and new lines get no voice."
                    });
                });
                speakers_table(ui, t, &voices, &mut win.speaker_names, &mut speaker_edits);
                    });
            });
        if policy != policy_before {
            match self
                .store
                .set_meta(oats_core::MediaPolicy::META_KEY, policy.as_str())
            {
                Ok(()) => self.pipeline.set_media_policy(policy),
                Err(e) => self.warning = Some(format!("Could not save the media policy: {e}")),
            }
        }
        if !open || ctx.input(|i| i.key_pressed(Key::Escape)) {
            save = true;
            close = true;
        }
        if save {
            let name = win.name.clone();
            if let Err(e) = self.store.set_user_name(&name) {
                self.warning = Some(format!("Could not save your name: {e}"));
            }
        }
        self.prefs.command_sound = sound;
        self.prefs.listen_at_launch = listen_at_launch;
        self.prefs.clock_24h = clock_24h;
        self.prefs.fetch_thumbnails = thumbnails;
        if keep != self.prefs.keep_transcripts {
            self.prefs.keep_transcripts = keep;
            if !keep {
                // the history up to now is kept; only new lines go
                self.prefs.drop_transcripts_from = chrono::Local::now().timestamp_millis();
                win.kept_before = None;
                win.confirm_purge = false;
            }
            self.purge_due = true;
        }
        if purge_old {
            self.prefs.drop_transcripts_from = 0;
            win.kept_before = None;
            self.purge_due = true;
        }
        if speakers_on != self.prefs.speakers {
            self.prefs.speakers = speakers_on;
            self.pipeline.set_speakers(speakers_on);
        }
        for edit in speaker_edits {
            match edit {
                SpeakerEdit::Name(id, name) => self.name_speaker(id, &name),
                SpeakerEdit::Me(id) => self.speaker_is_me(id),
                SpeakerEdit::Forget(id) => {
                    self.forget_speaker(id);
                    if let Some(w) = &mut self.prefs_window {
                        w.speaker_names.remove(&id);
                    }
                }
            }
        }
        crate::timefmt::set_24h(clock_24h);
        if close {
            self.prefs_window = None;
        }
        for (bundle, class, anyway) in app_changes {
            self.set_app_class(&bundle, class, anyway);
        }
    }

    /// The status strip's left text ("engine · on a call · 617 segments today · brain usage …");
    /// listening / idle and the lanes are NOT repeated here (the control bar's pills show them),
    /// only loading… / failed. No "system audio: no signal" and no "▶ <media>" line either: the
    /// no-signal state is the System audio pill's tooltip, what plays is the Media view's.
    pub fn status_text(&self) -> String {
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
                (None, Some(job)) => {
                    let what = self.brain_status.progress_text().unwrap_or(job.clone());
                    match self.brain_status.queued_line() {
                        Some(q) => {
                            format!(" · brain: {what}… · {}", q.trim_start_matches("brain: "))
                        }
                        None => format!(" · brain: {what}…"),
                    }
                }
                (None, None) if self.brain_status.queued_line().is_some() => {
                    format!(" · {}", self.brain_status.queued_line().unwrap_or_default())
                }
                (None, None) => {
                    format!(
                        " · brain usage ${:.2} today",
                        self.brain_status.spend_today_usd
                    )
                }
            },
        };
        let on_call = if self.on_call && self.is_listening() {
            " · on a call"
        } else {
            ""
        };
        // listening / idle and the open lanes are already on screen (the control bar's
        // pills); only the states the pills cannot show are spelled out
        let state = match self.status {
            PipelineStatus::Loading(_) | PipelineStatus::Failed(_) => {
                format!(" · {}", self.state_label())
            }
            PipelineStatus::Idle | PipelineStatus::Listening => String::new(),
        };
        format!(
            "{engine}{state}{on_call} · {n} segment{} today{brain}",
            if n == 1 { "" } else { "s" }
        )
    }

    fn status_line(&self, ui: &egui::Ui, t: &Tokens, rect: Rect) {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, t.inset);
        let left = self.status_text();
        let (right, col) = match (&self.warning, &self.last_command) {
            (Some(w), _) => (w.as_str(), t.danger.gamma_multiply(0.9)),
            (None, Some(c)) => (c.as_str(), t.accent.gamma_multiply(0.9)),
            (None, None) => ("", t.text_tertiary),
        };
        let font = t.mono(9.5);
        let full_w = |text: &str| {
            p.layout_no_wrap(text.to_owned(), font.clone(), col)
                .size()
                .x
        };
        let (left_w, right_w) = status_widths(
            rect.width() - 2.0 * STATUS_INSET,
            full_w(&left),
            if right.is_empty() { 0.0 } else { full_w(right) },
        );
        let left_galley = crate::widgets::truncated_galley(
            ui,
            left.clone(),
            font.clone(),
            t.text_tertiary,
            left_w,
        );
        let left_cut = left_galley.elided;
        let left_rect = Rect::from_min_size(
            pos2(
                rect.left() + STATUS_INSET,
                rect.center().y - left_galley.size().y / 2.0,
            ),
            left_galley.size(),
        );
        p.galley(left_rect.min, left_galley, t.text_tertiary);
        let mut cut = Vec::new();
        if left_cut {
            cut.push((left_rect, left));
        }
        if right_w > 0.0 {
            let galley = crate::widgets::truncated_galley(ui, right, font, col, right_w);
            let at = pos2(
                rect.right() - STATUS_INSET - galley.size().x,
                rect.center().y - galley.size().y / 2.0,
            );
            if galley.elided {
                cut.push((Rect::from_min_size(at, galley.size()), right.to_owned()));
            }
            p.galley(at, galley, col);
        }
        // a cut text reads in full on hover
        for (i, (r, text)) in cut.into_iter().enumerate() {
            ui.interact(r, egui::Id::new(("status-strip", i)), Sense::hover())
                .on_hover_text(text);
        }
    }
}

/// The Preferences window's widest content and its distance from the window's edges.
const PREFS_WIDTH: f32 = 560.0;
const PREFS_INSET: f32 = 12.0;

/// The status strip's side insets.
const STATUS_INSET: f32 = 8.0;
/// The gap kept between the strip's left and right texts.
const STATUS_GAP: f32 = 16.0;

/// The widths the status strip gives its left text (engine · state · …) and its right text
/// (the warning or the last voice command) out of `room`, given their full widths: both in
/// full when they fit; else the right text keeps its full width up to half the strip (more
/// when the left text is short) and the left one is cut before it, then the right one too.
/// `right` = 0: no right text, the left one has the whole strip.
pub fn status_widths(room: f32, left: f32, right: f32) -> (f32, f32) {
    let room = room.max(0.0);
    if right <= 0.0 {
        return (left.min(room), 0.0);
    }
    let text_room = (room - STATUS_GAP).max(0.0);
    if left + right <= text_room {
        return (left, right);
    }
    let right_w = right.min((text_room - left).max(text_room / 2.0));
    (left.min(text_room - right_w), right_w)
}

/// The Preferences "Apps" table: each app seen playing sound, its class (Auto shows what it
/// means for that app; call apps are locked to Meeting) and, for a media app while the
/// policy keeps no transcript (Ignore / Log only), "transcribe anyway". Changes are pushed
/// to `changes` as `(bundle_id, class, transcribe_anyway)`.
fn apps_table(
    ui: &mut egui::Ui,
    t: &Tokens,
    apps: &[AppRow],
    policy: oats_core::MediaPolicy,
    changes: &mut Vec<(String, oats_core::ProducerClass, bool)>,
) {
    use oats_core::ProducerClass;
    if apps.is_empty() {
        return;
    }
    let class_label = |row: &AppRow, c: ProducerClass| -> String {
        if c == ProducerClass::Auto && row.default != ProducerClass::Auto {
            format!("Auto ({})", row.default.label())
        } else {
            c.label().to_owned()
        }
    };
    let anyway_shown = policy.skips_transcription();
    // (no scroll area of its own: the whole Preferences body scrolls)
    {
        {
            egui::Grid::new("prefs-apps-grid")
                .num_columns(3)
                .spacing(vec2(12.0, 6.0))
                .show(ui, |ui| {
                    for row in apps {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&row.name).color(t.text))
                                .on_hover_text(&row.bundle_id);
                            if row.web_too {
                                ui.label(
                                    egui::RichText::new("web + app")
                                        .color(t.text_tertiary)
                                        .size(11.0),
                                )
                                .on_hover_text("Its web pages (in a browser) take this class too");
                            }
                        });
                        if row.locked {
                            ui.add_enabled(
                                false,
                                egui::Label::new(
                                    egui::RichText::new("Meeting 🔒").color(t.text_disabled),
                                ),
                            )
                            .on_disabled_hover_text(
                                "A call app: always a meeting (you'd never watch a video \
                                 through it)",
                            );
                        } else {
                            let mut class = row.class;
                            egui::ComboBox::from_id_salt(("app-class", row.bundle_id.as_str()))
                                .selected_text(class_label(row, class))
                                .width(130.0)
                                .show_ui(ui, |ui| {
                                    for c in ProducerClass::ALL {
                                        ui.selectable_value(&mut class, c, class_label(row, c));
                                    }
                                });
                            if class != row.class {
                                changes.push((row.bundle_id.clone(), class, row.transcribe_anyway));
                            }
                        }
                        let media = !row.locked && row.effective() == ProducerClass::Media;
                        // music is never transcribed whatever the policy: a music app can
                        // always be transcribed anyway
                        let music = matches!(
                            oats_media::classify_bundle(&row.bundle_id),
                            oats_media::MediaKind::Music
                        );
                        if media && (anyway_shown || music) {
                            let mut anyway = row.transcribe_anyway;
                            if ui
                                .checkbox(&mut anyway, "transcribe anyway")
                                .on_hover_text(
                                    "Transcribe this app's audio even though the media policy \
                                     keeps no transcript; it is still filed as media (no \
                                     todos, no events), so “Mark as a call” can recover it",
                                )
                                .changed()
                            {
                                changes.push((row.bundle_id.clone(), row.class, anyway));
                            }
                        } else {
                            ui.label("");
                        }
                        ui.end_row();
                    }
                });
        }
    }
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(
            "Meeting: transcribed as the other side of a call. Media: follows the policy above, \
             its notes in the Media view. Ignore: nothing kept. Changes apply within 5 s. In a call on \
             a media app, turn on “On a call” (⇧⌘C).",
        )
        .color(t.text_tertiary)
        .size(11.0),
    );
}

/// The Preferences "Speakers" table: one row per stored voice (the user's first, then the
/// most recently heard): its name (saved on Enter / when the field loses focus; empty clears
/// it), "me", when it was last heard, how much it said, and Forget.
fn speakers_table(
    ui: &mut egui::Ui,
    t: &Tokens,
    voices: &crate::speakers::SpeakerMap,
    names: &mut HashMap<i64, String>,
    edits: &mut Vec<SpeakerEdit>,
) {
    if voices.is_empty() {
        ui.label(
            egui::RichText::new("No voices yet: they appear as people are heard.")
                .color(t.text_tertiary)
                .size(11.0),
        );
        return;
    }
    use chrono::Datelike;
    let this_year = chrono::Local::now().year();
    egui::Grid::new("prefs-speakers-grid")
        .num_columns(5)
        .spacing(vec2(10.0, 6.0))
        .show(ui, |ui| {
            for s in crate::speakers::table_order(voices) {
                let text = names
                    .entry(s.id)
                    .or_insert_with(|| s.name.clone().unwrap_or_default());
                let hint = if s.is_me {
                    "me".to_owned()
                } else {
                    format!("voice {}", s.id)
                };
                // add_sized: a Grid cell's first pass offers little width, which a bare
                // desired_width would yield to
                let r = ui.add_sized(
                    vec2(130.0, ui.spacing().interact_size.y),
                    egui::TextEdit::singleline(text)
                        .hint_text(hint)
                        .text_color(t.speaker_colour(s.id)),
                );
                let r = match crate::speakers::name_note(&s) {
                    "" => r,
                    note => r.on_hover_text(note),
                };
                if r.lost_focus() && text.trim() != s.name.as_deref().unwrap_or("").trim() {
                    edits.push(SpeakerEdit::Name(s.id, text.trim().to_owned()));
                }
                let mut me = s.is_me;
                if ui
                    .add_enabled(!s.is_me, egui::Checkbox::new(&mut me, "me"))
                    .on_hover_text("This voice is you: its lines read “me”")
                    .changed()
                    && me
                {
                    edits.push(SpeakerEdit::Me(s.id));
                }
                let heard = crate::transcript::day_of(s.last_heard_at, &chrono::Local)
                    .map(|d| crate::transcript::day_heading(d, this_year))
                    .unwrap_or_default();
                ui.label(
                    egui::RichText::new(heard)
                        .color(t.text_secondary)
                        .size(11.5),
                );
                ui.label(
                    egui::RichText::new(format!("{:.0} min", (s.speech_secs / 60.0).ceil()))
                        .color(t.text_secondary)
                        .size(11.5),
                )
                .on_hover_text("Speech heard from this voice in total");
                if ui
                    .button(egui::RichText::new("Forget").color(t.danger))
                    .on_hover_text("Delete this voiceprint; its lines lose the label")
                    .clicked()
                {
                    edits.push(SpeakerEdit::Forget(s.id));
                }
                ui.end_row();
            }
        });
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(
            "Names from the conversation (the brain) never replace yours. Right-click a \
             transcript line ▸ Speaker to name, merge or forget a voice.",
        )
        .color(t.text_tertiary)
        .size(11.0),
    );
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
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, on, label));
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
    /// Selftest only: select the node with this name; `false` when there is none.
    pub(crate) fn selftest_show_node(&mut self, name: &str) -> bool {
        let Some(id) = self.nodes.iter().find(|n| n.name == name).map(|n| n.id) else {
            return false;
        };
        self.set_view(View::Node(id));
        true
    }

    /// Test hook: speaker recognition on / off in the pipeline (as the Preferences switch).
    pub fn selftest_set_speakers(&mut self, on: bool) {
        self.pipeline.set_speakers(on);
    }

    /// Show a node's page (a test hook for `View::Node`).
    pub fn selftest_view_node(&mut self, id: i64) {
        self.set_view(View::Node(id));
    }

    /// Open a project's sidebar layer (its recent notes), as the triangle does.
    pub fn open_project_layer(&mut self, id: i64) {
        self.sidebar.episodes_open.insert(id);
    }

    /// Jump to a note as the sidebar does: the single-note view under its project, with the
    /// project's layer open and its category expanded so the note's row is the selected one
    /// in the sidebar. An unfiled note (or one under a bare category) has no sidebar row: it is
    /// focused and flashed on its page instead.
    fn open_note_page(&mut self, id: i64) {
        self.topic = None;
        self.focus_episode = None;
        self.notes_state.editing = None;
        let node = match self.episodes.iter().find(|e| e.id == id) {
            Some(e) => e.node_id,
            None => match self.store.episode(id) {
                Ok(Some(e)) => e.node_id,
                Ok(None) => return,
                Err(e) => {
                    self.store_error(e);
                    return;
                }
            },
        };
        // only a note the sidebar lists (one of the project's recent, non-low-signal notes)
        // has a row to select; the rest open inline on the page
        let project = node
            .and_then(|n| crate::notes::project_of(&self.nodes, n).map(|p| p.id))
            .filter(|p| {
                self.sidebar_episodes
                    .get(p)
                    .is_some_and(|list| list.iter().any(|e| e.id == id))
            });
        match project {
            Some(p) => {
                // the sidebar row exists only while the project's layer is open and every
                // ancestor is expanded
                self.sidebar.episodes_open.insert(p);
                let mut cur = self
                    .nodes
                    .iter()
                    .find(|n| n.id == p)
                    .and_then(|n| n.parent_id);
                while let Some(a) = cur {
                    self.sidebar.collapsed.remove(&a);
                    cur = self
                        .nodes
                        .iter()
                        .find(|n| n.id == a)
                        .and_then(|n| n.parent_id);
                }
                let mut open: Vec<i64> = self.sidebar.episodes_open.iter().copied().collect();
                open.sort_unstable();
                self.prefs.sidebar_episodes_open = open;
                self.sidebar.reveal_selected = Some(crate::sidebar::SidebarRow::Episode {
                    episode: id,
                    project: p,
                });
                self.set_view(View::Node(p));
                self.focus_episode = Some(id);
            }
            None => {
                self.set_view(match node {
                    Some(n) if self.nodes.iter().any(|x| x.id == n) => View::Node(n),
                    _ => View::Notes,
                });
                self.notes_state.pending_focus = Some(id);
            }
        }
    }

    /// Selftest only: open a project's sidebar layer.
    pub(crate) fn selftest_open_project_layer(&mut self, name: &str) {
        if let Some(id) = self.nodes.iter().find(|n| n.name == name).map(|n| n.id) {
            self.sidebar.episodes_open.insert(id);
        }
    }

    /// Selftest only: expand every note on the project page.
    pub(crate) fn selftest_expand_notes(&mut self) {
        for e in &self.episodes {
            self.notes_state.open_notes.insert(e.id);
        }
    }

    /// Selftest only: open the search bar with this query already run.
    pub(crate) fn selftest_search(&mut self, query: &str) {
        self.run(Command::Search);
        if let Some(s) = &mut self.search {
            s.query = query.to_owned();
        }
        self.run_search();
    }

    /// Selftest only: open the Topic view for this topic.
    pub(crate) fn selftest_topic(&mut self, topic: &str) {
        self.open_topic(topic.to_owned());
    }

    /// Selftest only: a voice command's result as the brain reports it (`BrainEvent::Command`
    /// without a brain): the status line, the confirmation pop-ups, the ding.
    pub fn selftest_command(&mut self, ctx: &egui::Context, confirmations: Vec<String>) {
        self.last_command = confirmations.first().cloned();
        self.confirm(ctx, confirmations);
        self.reload();
    }

    /// Selftest only: the brain mid-work: claude answered, a review running, an interpret
    /// queued behind it.
    pub fn selftest_brain_busy(&mut self) {
        self.brain_status.availability = Some(Ok("2.1.312".to_owned()));
        self.brain_status.running = Some("review".to_owned());
        self.brain_status.queue("review", None);
        self.brain_status.queue("interpret", None);
        self.brain_status.progress = Some(crate::notes::Progress {
            kind: "review".to_owned(),
            step: 2,
            total: 3,
            label: "asking claude".to_owned(),
        });
    }

    /// Selftest only: show the Listening state with a fixed input level, no audio opened.
    pub(crate) fn selftest_fake_listening(&mut self, rms_db: f32, in_speech: bool) {
        self.capture = Some(Box::new(()));
        self.mic_on = self.prefs.lanes.mic;
        self.system_on = self.prefs.lanes.system;
        self.status = PipelineStatus::Listening;
        self.level = Some((rms_db, in_speech));
    }
}

impl eframe::App for OatsApp {
    // Runs every frame, and also while the window is hidden (eframe then runs no ui pass), so
    // everything the status item drives lives here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        crate::timefmt::set_24h(self.prefs.clock_24h);
        self.poll_quit_flag(ctx);
        self.handle_events();
        self.brain_status.now = ctx.input(|i| i.time);
        self.brain_status.expire();
        self.handle_brain_events(ctx);
        // a todo came due (or a snooze ended): show its pop-up; then ONE repaint is scheduled
        // for the next due time — no polling
        let now = chrono::Local::now().timestamp_millis();
        if self.next_due.is_some_and(|at| at <= now) {
            self.refresh_due();
            ctx.request_repaint();
        }
        if let Some(at) = self.next_due {
            ctx.request_repaint_after(std::time::Duration::from_millis(
                (at - now).max(0) as u64 + 50,
            ));
        }
        self.purge_transcripts(ctx);
        self.route_menu_events();
        self.sync_status_item(ctx);
        self.handle_close(ctx);
        self.sync_levels(ctx);
        if let Some(left) = self.expire_warning(std::time::Instant::now()) {
            ctx.request_repaint_after(left + std::time::Duration::from_millis(50));
        }
        // "Today" rolls over at local midnight even when nothing else wakes the UI: one
        // repaint scheduled for then (not a continuous one)
        ctx.request_repaint_after(until_local_midnight());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let t = Tokens::get(&ctx);
        crate::move_to::drop_stale(&mut self.notes_state.move_draft);
        crate::move_to::drop_stale_split(&mut self.notes_state.split_draft);
        // every card's brain buttons read it (All notes, a project page, a single note, search)
        self.notes_state.brain.clone_from(&self.brain_status);
        crate::speakers::publish(&ctx, self.speakers.clone());
        let mut cmds = self.handle_keys(&ctx);
        cmds.extend(self.undo_keys(&ctx));
        // Escape closes the search, also from the field (taken before the text edit sees it)
        if self.search.is_some()
            && self.prefs_window.is_none()
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.close_search();
        } else if self.topic.is_some()
            && self.search.is_none()
            && self.prefs_window.is_none()
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.close_topic();
        } else if self.focus_episode.is_some()
            && self.search.is_none()
            && self.topic.is_none()
            && self.prefs_window.is_none()
            && self.notes_state.editing.is_none()
            && self.notes_state.move_draft.is_none()
            && self.notes_state.split_draft.is_none()
            && self.notes_state.confirm_merge.is_none()
            && self.notes_state.confirm_delete.is_none()
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            // back from a single note to its project's page
            self.focus_episode = None;
        }

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
            // ↑ ↓ ← → move through the sidebar unless a text field or a modal has the keys
            if !ui.ctx().egui_wants_keyboard_input()
                && self.prefs_window.is_none()
                && self.notes_state.confirm_merge.is_none()
                && self.notes_state.confirm_delete.is_none()
            {
                sidebar_actions = crate::sidebar::key_nav(
                    ui.ctx(),
                    &mut self.sidebar,
                    &self.nodes,
                    &self.sidebar_episodes,
                    self.prefs.view,
                    self.focus_episode,
                );
            }
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
                    let latest = self.latest_note.filter(|l| !l.unfiled).map(|l| l.id);
                    let latest_nodes =
                        crate::sidebar::latest_nodes(&self.nodes, &self.sidebar_episodes, latest);
                    let marks = crate::sidebar::TodayMarks {
                        start: self.today_start,
                        nodes: crate::sidebar::today_nodes(
                            &self.nodes,
                            &self.sidebar_episodes,
                            self.today_start,
                        ),
                        conversations: self.unfiled_today > 0,
                        latest: latest.filter(|_| latest_nodes.is_some()),
                        latest_nodes: latest_nodes.unwrap_or_default(),
                        latest_conversations: self.latest_note.is_some_and(|l| l.unfiled),
                    };
                    sidebar_actions.extend(sidebar_panel(
                        ui,
                        &mut self.sidebar,
                        self.prefs.view,
                        &self.nodes,
                        &self.sidebar_episodes,
                        self.focus_episode,
                        &mut self.notes_state.move_draft,
                        &marks,
                    ));
                });
            let w = resp.response.rect.width();
            if (w - self.prefs.sidebar_width).abs() > 0.5 {
                self.prefs.sidebar_width = w;
            }
        }

        let mut transcript_actions = Vec::new();
        if std::mem::take(&mut self.clear_row_selection) {
            crate::transcript::clear_row_selection(&ctx);
        }
        let mut notes_actions = Vec::new();
        let mut calendar_actions = Vec::new();
        let mut todo_actions = Vec::new();
        let mut media_actions = Vec::new();
        let mut list_rect_seen = Rect::NOTHING;
        let mut search_actions = Vec::new();
        let mut topic_actions = Vec::new();
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
                list_rect_seen = list_rect;

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
                // the note rows' marks: today's in white, the most recent note in the accent
                self.notes_state.marks = crate::notes::RowMarks {
                    today_start: self.today_start,
                    latest: self.latest_note.map(|l| l.id),
                };
                if self.search.is_some() {
                    // the search bar (chrome, like the control bar) and its results replace
                    // the view underneath
                    let bar = Rect::from_min_size(list_rect.min, vec2(list_rect.width(), BAR_H));
                    list_ui.painter().rect_filled(bar, 0.0, t.chrome);
                    list_ui.painter().hline(
                        bar.x_range(),
                        bar.bottom() - 0.5,
                        egui::Stroke::new(1.0, t.divider),
                    );
                    let mut field_ui = list_ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(bar)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    let mut changed = false;
                    if let Some(s) = &mut self.search {
                        let (c, close) = search_bar(&mut field_ui, s);
                        changed = c;
                        if close {
                            search_actions.push(SearchAction::Close);
                        }
                    }
                    if changed {
                        self.run_search();
                    }
                    let body =
                        Rect::from_min_max(pos2(list_rect.left(), bar.bottom()), list_rect.max);
                    let mut body_ui = list_ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(body)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    body_ui.set_clip_rect(body);
                    if let Some(s) = &self.search {
                        search_actions.extend(search_results(
                            &mut body_ui,
                            s,
                            &mut self.notes_state,
                            &self.nodes,
                        ));
                    }
                } else if let Some(topic) = &self.topic {
                    let bar =
                        Rect::from_min_size(list_rect.min, vec2(list_rect.width(), TOPIC_BAR_H));
                    list_ui.painter().rect_filled(bar, 0.0, t.chrome);
                    list_ui.painter().hline(
                        bar.x_range(),
                        bar.bottom() - 0.5,
                        egui::Stroke::new(1.0, t.divider),
                    );
                    let mut head_ui = list_ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(bar)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    if topic_header(&mut head_ui, topic) {
                        topic_actions.push(TopicAction::Close);
                    }
                    let body =
                        Rect::from_min_max(pos2(list_rect.left(), bar.bottom()), list_rect.max);
                    let mut body_ui = list_ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(body)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    body_ui.set_clip_rect(body);
                    topic_actions.extend(topic_list(&mut body_ui, topic, &self.nodes));
                } else {
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
                            let day = self
                                .calendar_state
                                .selected
                                .unwrap_or_else(|| chrono::Local::now().date_naive());
                            if self.media_plays_day != Some(day) {
                                self.load_media_plays(day);
                            }
                            calendar_actions = calendar_view(
                                &mut list_ui,
                                &mut self.calendar_state,
                                &self.events,
                                &self.todos,
                                &self.nodes,
                                &self.media_plays,
                            );
                            if self.calendar_state.selected.is_some_and(|d| d != day) {
                                // another day was picked: its plays next frame
                                ui.ctx().request_repaint();
                            }
                        }
                        View::Media => {
                            media_actions = media_view(
                                &mut list_ui,
                                &self.media_pages,
                                &mut self.media_focus,
                                &mut self.thumbs,
                                self.prefs.fetch_thumbnails,
                                &self.media_notes,
                                &self.brain_status,
                            );
                        }
                        View::Todos => {
                            todo_actions = todos_view(
                                &mut list_ui,
                                &mut self.todos_state,
                                &self.todos,
                                &self.nodes,
                            );
                        }
                        View::Node(id) if self.nodes.iter().any(|n| n.id == id) => {
                            let focus = self
                                .focus_episode
                                .and_then(|f| self.episodes.iter().find(|e| e.id == f));
                            if let Some(e) = focus {
                                notes_actions = episode_view(
                                    &mut list_ui,
                                    &mut self.notes_state,
                                    e,
                                    &self.nodes,
                                );
                            } else if let Some(node) = self.nodes.iter().find(|n| n.id == id) {
                                let out = node_view(
                                    &mut list_ui,
                                    &mut self.notes_state,
                                    node,
                                    &self.episodes,
                                    &self.filed_items,
                                    &self.rows,
                                    &self.nodes,
                                );
                                notes_actions = out.notes;
                                transcript_actions = out.transcript;
                            }
                        }
                        _ => {
                            // the view header: "Show media" (media rows hidden by default)
                            let bar = Rect::from_min_size(
                                list_rect.min,
                                vec2(list_rect.width(), TRANSCRIPTS_BAR_H),
                            );
                            list_ui.painter().rect_filled(bar, 0.0, t.chrome);
                            list_ui.painter().hline(
                                bar.x_range(),
                                bar.bottom() - 0.5,
                                egui::Stroke::new(1.0, t.divider),
                            );
                            let mut head_ui = list_ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(bar)
                                    .layout(egui::Layout::right_to_left(egui::Align::Center)),
                            );
                            head_ui.add_space(12.0);
                            let at_now = !self.pages.has_later && self.pages.at_end();
                            if pill(&mut head_ui, &t, "Jump to now", false, !at_now)
                                .on_hover_text(if at_now {
                                    "Showing the newest lines"
                                } else {
                                    "Scroll to the newest line, where transcription is \
                                     happening now"
                                })
                                .clicked()
                            {
                                cmds.push(Command::JumpToNow);
                            }
                            head_ui.add_space(8.0);
                            let on = self.prefs.show_media;
                            if pill(&mut head_ui, &t, "Show media", on, true)
                                .on_hover_text(if on {
                                    "Showing what the computer played (videos, music, media \
                                     apps) — click to hide it"
                                } else {
                                    "Media rows (videos, music, media apps) are hidden; search \
                                     still finds them"
                                })
                                .clicked()
                            {
                                cmds.push(Command::ToggleShowMedia);
                            }
                            let body = Rect::from_min_max(
                                pos2(list_rect.left(), bar.bottom()),
                                list_rect.max,
                            );
                            let mut body_ui = list_ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(body)
                                    .layout(egui::Layout::top_down(egui::Align::Min)),
                            );
                            body_ui.set_clip_rect(body);
                            transcript_actions = transcript_list(
                                &mut body_ui,
                                &mut self.pages,
                                &mut self.notes_state.move_draft,
                                &mut self.transcript_focus,
                            );
                        }
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

        // the pop-ups: over every view, in the content area's upper right
        let toast_actions = toasts(&ctx, list_rect_seen, &self.due_now, &mut self.toasts);
        self.preferences_window(&ctx, &t);
        // a note dropped on another: the Yes / No before the (irreversible) merge
        if let Some(merge) =
            crate::merge::confirm_popup(&ctx, &t, &mut self.notes_state.confirm_merge)
        {
            self.merge_notes(&ctx, merge);
        }
        if let Some(id) =
            crate::notes::confirm_delete_popup(&ctx, &t, &mut self.notes_state.confirm_delete)
        {
            notes_actions.push(NotesAction::DeleteEpisode(id));
        }
        self.apply_sidebar(sidebar_actions, &mut cmds);
        self.apply_transcript(&ctx, transcript_actions);
        self.apply_notes(&ctx, notes_actions);
        self.apply_calendar(calendar_actions);
        self.apply_todos(todo_actions);
        self.apply_media(&ctx, media_actions);
        self.apply_toasts(toast_actions);
        self.apply_search(&ctx, search_actions);
        self.apply_topic(topic_actions);
        for c in cmds {
            self.run(c);
        }
        // an undo's pop-up now, not at the next brain-event pass
        self.flush_toasts(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.persist {
            self.prefs.store(storage);
        }
    }

    fn on_exit(&mut self) {
        // prefs.listening keeps the user's choice on record; whether the next launch listens
        // is prefs.listen_at_launch
        self.stop_listening();
    }
}

/// Why a corrected line was relabelled only (empty when its voiceprint moved too).
fn relabel_note(learned: bool, learn: bool, kept: bool) -> &'static str {
    if learned {
        ""
    } else if !learn && kept {
        " (relabelled only: speaker recognition is off)"
    } else if !kept {
        " (relabelled only: no voiceprint kept for this line)"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_status_strip_cuts_the_left_text_before_the_right_one() {
        use super::status_widths;
        // both fit: both in full
        assert_eq!(status_widths(400.0, 200.0, 100.0), (200.0, 100.0));
        // no right text: the left one takes the strip, cut at its width
        assert_eq!(status_widths(300.0, 500.0, 0.0), (300.0, 0.0));
        // too long together: the right text keeps its width, the left one is cut
        assert_eq!(status_widths(400.0, 350.0, 120.0), (264.0, 120.0));
        // a right text over half the strip is cut to half, the left one to the rest
        assert_eq!(status_widths(416.0, 350.0, 400.0), (200.0, 200.0));
        // a short left text leaves the right one more than half
        assert_eq!(status_widths(416.0, 100.0, 400.0), (100.0, 300.0));
    }

    use super::*;
    use eframe::Storage as _;

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
    fn the_old_capture_system_pref_migrates_into_the_lanes() {
        for old in [false, true] {
            let mut s = MemStorage::default();
            s.set_string(
                PREFS_KEY,
                format!("(listening: true, capture_system: {old})"),
            );
            let prefs = AppPrefs::load(&s);
            assert_eq!(
                prefs.lanes,
                Lanes {
                    mic: true,
                    system: old
                }
            );
            assert_eq!(prefs.capture_system, None);
            assert!(prefs.listening);
            // saved again, the old key is gone and the lanes stay
            prefs.store(&mut s);
            assert!(!s.get_string(PREFS_KEY).unwrap().contains("capture_system"));
            assert_eq!(AppPrefs::load(&s).lanes, prefs.lanes);
        }
    }

    #[test]
    fn prefs_round_trip() {
        let mut s = MemStorage::default();
        let prefs = AppPrefs {
            sidebar_visible: false,
            sidebar_width: 250.0,
            view: View::Node(7),
            listening: true,
            listen_at_launch: true,
            lanes: Lanes {
                mic: true,
                system: false,
            },
            capture_system: None,
            command_sound: false,
            sidebar_episodes_open: vec![3, 9],
            show_media: true,
            clock_24h: true,
            fetch_thumbnails: true,
            speakers: true,
            keep_transcripts: false,
            drop_transcripts_from: 1_700_000_000_000,
        };
        prefs.store(&mut s);
        assert_eq!(AppPrefs::load(&s), prefs);
        // a missing or bad blob falls back to the defaults
        assert_eq!(AppPrefs::load(&MemStorage::default()), AppPrefs::default());
        s.set_string(PREFS_KEY, "garbage(".to_owned());
        assert_eq!(AppPrefs::load(&s), AppPrefs::default());
    }

    #[test]
    fn prefs_saved_with_the_old_today_or_all_view_load_as_transcripts() {
        for old in ["Today", "All"] {
            let mut s = MemStorage::default();
            s.set_string(
                PREFS_KEY,
                format!("(sidebar_width: 250.0, view: {old}, listening: true)"),
            );
            let prefs = AppPrefs::load(&s);
            assert_eq!(prefs.view, View::Transcripts, "{old}");
            assert_eq!(
                prefs.sidebar_width, 250.0,
                "the rest of the blob still loads"
            );
            assert!(prefs.listening);
        }
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
            Command::Preferences,
            Command::ToggleSidebar,
            Command::ShowTranscripts,
            Command::ShowNotes,
            Command::ShowCalendar,
            Command::ShowTodos,
            Command::ShowMediaView,
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
            mic_on: false,
            on_call: false,
            system_on: false,
            show_media: false,
            status: PipelineStatus::Loading("x".to_owned()),
            sidebar_visible: true,
            view: View::Transcripts,
            brain_ready: false,
            brain_busy: [false; 3],
            undo_label: None,
            redo_label: None,
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
        assert_eq!(titles, ["File", "Edit", "Listen", "View", "Brain"]);
        // the lane toggles are checked items in the Listen menu, following the session
        let lane = |model: &[MenuSection], c: Command| {
            model
                .iter()
                .flat_map(|s| s.entries.iter())
                .find_map(|e| match e {
                    MenuEntry::Item {
                        cmd,
                        label,
                        checked,
                        ..
                    } if *cmd == c => Some((label.clone(), *checked)),
                    _ => None,
                })
        };
        assert_eq!(
            lane(&build_menu_model(&s), Command::ToggleSystemAudio),
            Some(("System Audio".to_owned(), Some(false)))
        );
        s.system_on = true;
        s.mic_on = true;
        assert_eq!(
            lane(&build_menu_model(&s), Command::ToggleSystemAudio).and_then(|(_, c)| c),
            Some(true)
        );
        assert_eq!(
            lane(&build_menu_model(&s), Command::ToggleMic),
            Some(("Microphone".to_owned(), Some(true)))
        );
        assert_eq!(
            AppPrefs::default().lanes,
            Lanes::default(),
            "both on by default"
        );
        assert!(AppPrefs::default().lanes.mic && AppPrefs::default().lanes.system);
        // the Brain menu follows the brain's readiness
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

    #[test]
    fn edit_menu_has_undo_and_redo_without_accelerators() {
        let item = |model: &[MenuSection], c: Command| {
            model.iter().find(|s| s.title == "Edit").and_then(|s| {
                s.entries.iter().find_map(|e| match e {
                    MenuEntry::Item {
                        cmd,
                        label,
                        enabled,
                        ..
                    } if *cmd == c => Some((label.clone(), *enabled)),
                    _ => None,
                })
            })
        };
        let mut s = MenuState {
            listening: false,
            mic_on: false,
            on_call: false,
            system_on: false,
            show_media: false,
            status: PipelineStatus::Idle,
            sidebar_visible: true,
            view: View::Transcripts,
            brain_ready: false,
            brain_busy: [false; 3],
            undo_label: None,
            redo_label: None,
        };
        let m = build_menu_model(&s);
        assert_eq!(item(&m, Command::Undo), Some(("Undo".to_owned(), false)));
        assert_eq!(item(&m, Command::Redo), Some(("Redo".to_owned(), false)));
        s.undo_label = Some("Mark complete".to_owned());
        s.redo_label = Some("Snooze todo".to_owned());
        let m = build_menu_model(&s);
        assert_eq!(
            item(&m, Command::Undo),
            Some(("Undo Mark complete".to_owned(), true))
        );
        assert_eq!(
            item(&m, Command::Redo),
            Some(("Redo Snooze todo".to_owned(), true))
        );
        // not menu key equivalents: ⌘Z must stay with focused text fields
        assert_eq!(menu_shortcut(Command::Undo), None);
        assert_eq!(menu_shortcut(Command::Redo), None);
        assert!(
            SHORTCUTS
                .iter()
                .all(|(k, _)| *k != UNDO_SHORTCUT && *k != REDO_SHORTCUT)
        );
        assert_eq!(shown_shortcut(Command::Undo), Some(UNDO_SHORTCUT));
        assert_eq!(shown_shortcut(Command::Redo), Some(REDO_SHORTCUT));
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

    /// "Keep transcripts indefinitely" off: the headless app deletes the lines the brain is
    /// done with (older than the grace), keeps the uninterpreted ones, and leaves the history
    /// from before the switch alone until asked; on by default nothing is touched.
    #[test]
    fn keep_transcripts_off_purges_interpreted_lines_past_the_grace() {
        let Ok(store) = Store::open_in_memory() else {
            eprintln!("skipped: Store::open_in_memory unavailable");
            return;
        };
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx);
        let dir = std::env::temp_dir().join(format!("oats-ui-purge-{}", std::process::id()));
        let pipeline = oats_core::Pipeline {
            engine: Box::new(|| {
                Ok(Box::new(oats_core::NullEngine) as Box<dyn oats_core::SpeechEngine>)
            }),
            store_path: dir.join("oats.sqlite"),
            segmenter: Default::default(),
            wake: Box::new(|| {}),
        }
        .spawn();
        let audio: AudioStarter = Box::new(|_tx, _req| Err("no microphone in tests".to_owned()));
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
            None,
            None,
        );
        let now = chrono::Local::now().timestamp_millis();
        let grace = TRANSCRIPT_GRACE.as_millis() as i64;
        let hour = 3_600_000;
        let sess = app.store.begin_session(now - 3 * hour, "t").unwrap();
        let seg = |app: &OatsApp, at: i64, text: &str| {
            app.store
                .insert_segment(sess, at, at + 1_000, text, None)
                .unwrap()
                .id
        };
        let before_switch = seg(&app, now - 2 * hour, "history");
        app.store.mark_segment_consumed(before_switch, now).unwrap();
        let done = seg(&app, now - 2 * grace, "interpreted");
        app.store.mark_segment_consumed(done, now).unwrap();
        let waiting = seg(&app, now - 2 * grace, "not interpreted yet");
        let fresh = seg(&app, now - grace / 2, "interpreted, too recent");
        app.store.mark_segment_consumed(fresh, now).unwrap();
        let alive = |app: &OatsApp, id: i64| app.store.segment(id).unwrap().is_some();
        // the default: everything stays
        assert!(app.prefs.keep_transcripts);
        app.purge_due = true;
        app.purge_transcripts(&cc.egui_ctx);
        assert!(alive(&app, done));
        // off since an hour ago: the interpreted line past the grace goes, the rest stays
        app.prefs.keep_transcripts = false;
        app.prefs.drop_transcripts_from = now - hour;
        app.purge_due = true;
        app.purge_transcripts(&cc.egui_ctx);
        assert!(!alive(&app, done));
        assert!(alive(&app, waiting));
        assert!(alive(&app, fresh));
        assert!(alive(&app, before_switch));
        assert!(!app.purge_due);
        assert_eq!(app.purge_next_at, Some(now - grace / 2 + 1_000 + grace));
        // "Delete them too": the history goes on the next purge (throttled to one a minute,
        // so the request waits until then)
        app.prefs.drop_transcripts_from = 0;
        app.purge_due = true;
        app.purge_transcripts(&cc.egui_ctx);
        assert!(app.purge_due);
        assert!(alive(&app, before_switch));
        app.purge_last = None;
        app.purge_transcripts(&cc.egui_ctx);
        assert!(!alive(&app, before_switch));
        assert!(alive(&app, waiting));
        let _ = std::fs::remove_dir_all(dir);
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
        let audio: AudioStarter = Box::new(|_tx, _req| Err("no microphone in tests".to_owned()));
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
        // a warning clears itself after the TTL, and a transcribed segment supersedes a
        // transcription failure
        let t0 = std::time::Instant::now();
        app.warning = Some("something odd".to_owned());
        assert_eq!(app.expire_warning(t0), Some(WARNING_TTL));
        let half = WARNING_TTL / 2;
        assert_eq!(app.expire_warning(t0 + half), Some(WARNING_TTL - half));
        assert!(app.warning.is_some());
        assert_eq!(app.expire_warning(t0 + WARNING_TTL), None);
        assert!(app.warning.is_none());
        app.warning = Some("transcription failed: boom".to_owned());
        app.supersede_transcription_warning();
        assert!(app.warning.is_none());
        app.warning = Some("Microphone: stopped".to_owned());
        app.supersede_transcription_warning();
        assert!(app.warning.is_some());
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
        // a termination signal (the shared flag) quits the same way
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.set_quit_flag(flag.clone());
        app.poll_quit_flag(&cc.egui_ctx);
        assert!(!app.quit_requested);
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        app.poll_quit_flag(&cc.egui_ctx);
        assert!(app.quit_requested);
        app.quit_requested = false;
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
