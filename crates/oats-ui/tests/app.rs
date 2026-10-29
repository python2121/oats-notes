//! The whole `OatsApp` driven headlessly through `egui_kittest` (no GPU: nothing is rendered,
//! the AccessKit tree is queried): a real pipeline on the null engine fed through the
//! `AudioStarter`'s sender, a real brain against a fake `claude`, a fake status item.
#![cfg(feature = "selftest")]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use chrono::TimeZone;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use oats_brain::{Brain, BrainConfig, BrainHandle, ClaudeCli};
use oats_core::{AudioChunk, NullEngine, Pipeline, PipelineStatus, SpeechEngine, Store, View};
use oats_status::{StatusAction, StatusItem, StatusState};
use oats_ui::{AppOptions, AudioStarter, Command, MenuEntry, OatsApp, StartedAudio};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("oats-ui-it-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("oats.sqlite")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Records what it was told; hands out queued actions.
#[derive(Clone, Default)]
struct FakeItem {
    shown: Rc<RefCell<Vec<StatusState>>>,
    queue: Rc<RefCell<Vec<StatusAction>>>,
}

impl StatusItem for FakeItem {
    fn update(&mut self, state: &StatusState) {
        self.shown.borrow_mut().push(state.clone());
    }
    fn poll(&mut self) -> Vec<StatusAction> {
        std::mem::take(&mut *self.queue.borrow_mut())
    }
}

/// The microphone: hands the pipeline's sender to the test instead of opening a device.
type Mic = Rc<RefCell<Option<Sender<AudioChunk>>>>;

struct Rig {
    harness: Harness<'static, OatsApp>,
    mic: Mic,
}

fn build(db: &Path, item: Option<FakeItem>, brain: Option<BrainHandle>) -> Rig {
    let store = Store::open(db).unwrap();
    let pipeline = Pipeline {
        engine: Box::new(|| Ok(Box::new(NullEngine) as Box<dyn SpeechEngine>)),
        store_path: db.to_path_buf(),
        segmenter: Default::default(),
        wake: Box::new(|| {}),
    }
    .spawn();
    let mic: Mic = Rc::default();
    let m = mic.clone();
    let audio: AudioStarter = Box::new(move |tx, _request| {
        *m.borrow_mut() = Some(tx);
        Ok(StartedAudio::mic(Box::new(())))
    });
    let harness = Harness::builder()
        .with_size(egui::vec2(1100.0, 720.0))
        .build_eframe(move |cc| {
            OatsApp::new(
                cc,
                AppOptions {
                    native_menu: false,
                    system_font: false,
                    persist: false,
                    start_listening: false,
                },
                pipeline,
                audio,
                store,
                item.map(|i| Box::new(i) as Box<dyn StatusItem>),
                brain,
            )
        });
    let mut rig = Rig { harness, mic };
    rig.until("the engine is ready", |app| {
        *app.status() == PipelineStatus::Idle
    });
    rig
}

impl Rig {
    fn app(&mut self) -> &mut OatsApp {
        self.harness.state_mut()
    }

    /// Step frames until `pred` holds (the pipeline and brain threads don't wake the harness).
    fn until(&mut self, what: &str, pred: impl Fn(&OatsApp) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !pred(self.harness.state()) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.harness.step();
            std::thread::sleep(Duration::from_millis(5));
        }
        self.harness.step();
    }

    fn until_label(&mut self, what: &str, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.harness.query_by_label_contains(label).is_none() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what} ({label:?})"
            );
            self.harness.step();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn shows(&self, label: &str) -> bool {
        self.harness
            .query_all_by_label_contains(label)
            .next()
            .is_some()
    }

    fn click_at(&mut self, pos: egui::Pos2) {
        let h = &self.harness;
        h.event(egui::Event::PointerMoved(pos));
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        self.harness.step();
        self.harness.step();
    }

    /// Select the first category in the sidebar (its rows are painted, not labelled: click
    /// down from the "Categories" heading until the view changes).
    fn select_first_category(&mut self) -> View {
        let heading = self.harness.get_by_label("Categories").rect();
        let x = heading.left() + 40.0;
        let mut y = heading.bottom();
        while y < heading.bottom() + 80.0 {
            self.click_at(egui::pos2(x, y));
            if let View::Node(id) = self.harness.state().prefs().view {
                return View::Node(id);
            }
            y += 3.0;
        }
        panic!("no category row below the heading");
    }

    fn speak(&mut self, secs: usize) {
        let tx = self.mic.borrow().clone().expect("listening");
        let mut n = 0usize;
        for _ in 0..secs * 10 {
            let chunk: Vec<f32> = (0..1_600)
                .map(|_| {
                    n += 1;
                    0.3 * (n as f32 * 2.0 * std::f32::consts::PI * 440.0 / 16_000.0).sin()
                })
                .collect();
            tx.send(AudioChunk::mic(chunk)).unwrap();
        }
        for _ in 0..15 {
            tx.send(AudioChunk::mic(vec![0.0; 1_600])).unwrap();
        }
    }
}

fn seed(db: &Path) -> i64 {
    let s = Store::open(db).unwrap();
    let work = s.categories().unwrap()[0].id;
    let sid = s.begin_session(now_ms() - 120_000, "seed").unwrap();
    let t = now_ms() - 60_000;
    let filed = s
        .insert_segment(
            sid,
            t,
            t + 3_000,
            "filed under work: the exporter ships Friday",
            None,
        )
        .unwrap();
    s.set_segment_node(filed.id, Some(work)).unwrap();
    s.insert_segment(
        sid,
        t + 5_000,
        t + 8_000,
        "loose words about the weather, which has been unusually warm for late October",
        None,
    )
    .unwrap();
    work
}

/// Two projects under Work, one episode in each, plus the seeded segments.
fn seed_projects(db: &Path) -> (i64, i64, i64) {
    use oats_core::{EpisodeContent, NodeKind, Origin};
    let work = seed(db);
    let s = Store::open(db).unwrap();
    let t = now_ms() - 600_000;
    let mut ids = Vec::new();
    for (name, title) in [
        ("Commerce project", "Checkout flow decisions"),
        ("Apartment lease", "Lease renewal terms"),
    ] {
        let p = s
            .add_node(Some(work), NodeKind::Project, name, "", Origin::Auto)
            .unwrap();
        s.insert_episode(
            t,
            t + 60_000,
            &EpisodeContent {
                title: title.into(),
                summary: "- something concrete".into(),
                node_id: Some(p.id),
                model: "m".into(),
                ..Default::default()
            },
            t + 120_000,
        )
        .unwrap();
        ids.push(p.id);
    }
    (work, ids[0], ids[1])
}

#[test]
fn a_category_shows_its_projects_and_notes_and_the_transcript_behind_a_toggle() {
    let dir = TempDir::new("category-notes");
    let (work, commerce, _lease) = seed_projects(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    assert_eq!(rig.select_first_category(), View::Node(work));
    rig.harness.run_steps(3);
    // projects and notes, not transcript rows
    assert!(rig.shows("Commerce project"));
    assert!(rig.shows("Apartment lease"));
    assert!(rig.shows("Checkout flow decisions"));
    assert!(rig.shows("Lease renewal terms"));
    assert!(!rig.shows("filed under work"));
    assert!(rig.shows("Show transcript (1 segment)"));
    // the toggle reveals the raw segments filed under the subtree
    rig.harness.get_by_label_contains("Show transcript").click();
    rig.harness.run_steps(3);
    assert!(rig.shows("filed under work"));
    assert!(!rig.shows("loose words"));
    // a project shows only its own episode
    rig.harness.get_by_label("Commerce project").click();
    rig.harness.run_steps(3);
    assert_eq!(rig.harness.state().prefs().view, View::Node(commerce));
    assert!(rig.shows("Checkout flow decisions"));
    assert!(!rig.shows("Lease renewal terms"));
    assert!(rig.shows("Show transcript (0 segments)"));
}

#[test]
fn selecting_a_node_view_reloads_the_rows() {
    let dir = TempDir::new("node-view");
    let work = seed(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    assert_eq!(rig.harness.state().prefs().view, View::Today);
    assert!(rig.shows("filed under work"));
    assert!(rig.shows("loose words"));

    assert_eq!(rig.select_first_category(), View::Node(work));
    rig.harness.run_steps(2);
    assert!(rig.shows("No projects yet"));
    assert!(rig.shows("Show transcript (1 segment)"));
    rig.harness.get_by_label_contains("Show transcript").click();
    rig.harness.run_steps(2);
    assert!(rig.shows("filed under work"));
    assert!(!rig.shows("loose words"), "only the Work subtree is listed");

    rig.app().run(Command::ShowAll);
    rig.harness.run_steps(2);
    assert_eq!(rig.harness.state().prefs().view, View::All);
    assert!(rig.shows("loose words"));
}

#[test]
fn a_segment_for_another_view_bumps_the_count_without_appending() {
    let dir = TempDir::new("segment");
    seed(&dir.db());
    let item = FakeItem::default();
    let shown = item.shown.clone();
    let mut rig = build(&dir.db(), Some(item), None);
    rig.select_first_category();
    rig.app().run(Command::ToggleListening);
    assert!(rig.harness.state().is_listening());
    rig.until("listening", |app| {
        *app.status() == PipelineStatus::Listening
    });
    assert_eq!(
        rig.harness.state().status_item_state().tooltip,
        "OatsNotes — Listening · mic · 2 segments today"
    );

    rig.speak(2);
    rig.until("the count to go up", |app| {
        app.status_item_state().tooltip.contains("3 segments today")
    });
    // the new (unfiled) row isn't in the Work view…
    assert!(
        !rig.shows("no engine]"),
        "appended to a view it doesn't belong to"
    );
    assert!(rig.shows("Show transcript (1 segment)"));
    // …but the status item heard about the new count
    assert!(
        shown
            .borrow()
            .iter()
            .any(|s| s.listening && s.tooltip.contains("3 segments today")),
        "{:?}",
        shown.borrow()
    );

    // in a view it belongs to, the next one is appended without a reload
    rig.app().run(Command::ShowToday);
    rig.harness.run_steps(2);
    assert_eq!(
        rig.harness
            .query_all_by_label_contains("no engine]")
            .count(),
        1
    );
    rig.speak(2);
    rig.until("the count to go up again", |app| {
        app.status_item_state().tooltip.contains("4 segments today")
    });
    rig.harness.run_steps(2);
    assert_eq!(
        rig.harness
            .query_all_by_label_contains("no engine]")
            .count(),
        2
    );

    rig.app().run(Command::ToggleListening);
    assert!(!rig.harness.state().is_listening());
    rig.harness.run_steps(2);
    assert_eq!(
        rig.harness.state().status_item_state().tooltip,
        "OatsNotes — Idle"
    );
}

/// A fake `claude` that interprets any transcript for $0.25.
fn fake_cli(dir: &Path) -> ClaudeCli {
    let script = dir.join("claude");
    let body = r#"#!/bin/sh
case " $* " in *" --version "*) echo "2.1.312 (Claude Code)"; exit 0;; esac
cat > /dev/null
case "$*" in
  *low_signal*) echo '{"type":"result","is_error":false,"result":"","total_cost_usd":0.25,"duration_ms":10,"structured_output":{"title":"Weather chat","summary":"- sunny","low_signal":false,"node_id":null,"new_project":null,"new_topic":null,"decisions":[],"action_items":[],"topics":[],"events":[]}}';;
  *) echo '{"type":"result","is_error":false,"result":"","total_cost_usd":0.0,"duration_ms":10,"structured_output":{"items":[],"actions":[]}}';;
esac
"#;
    std::fs::write(&script, body).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
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

fn interpret_enabled(app: &OatsApp) -> Option<bool> {
    app.menu_model()
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
}

#[test]
fn brain_events_update_readiness_spend_and_the_notes() {
    let dir = TempDir::new("brain");
    seed(&dir.db());
    let brain = Brain {
        config: BrainConfig {
            cli: fake_cli(&dir.0),
            ..BrainConfig::default()
        },
        store_path: dir.db(),
        wake: Box::new(|| {}),
    }
    .spawn();
    let mut rig = build(&dir.db(), None, Some(brain));
    rig.app().run(Command::ShowNotes);
    rig.until("the brain to report in", |app| {
        interpret_enabled(app) == Some(true)
    });
    rig.harness.run_steps(2);
    assert!(rig.shows("usage 0.00 today"));
    assert!(rig.shows("no brain run yet"));

    rig.app().run(Command::InterpretNow);
    rig.until_label("the spend", "usage 0.25 today");
    // the Changed event reloaded the episodes: the new one is listed
    rig.until_label("the new episode", "Weather chat");
    rig.until_label("the last run line", "last run: interpret");
    assert_eq!(
        interpret_enabled(rig.harness.state()),
        Some(true),
        "idle again"
    );
}

#[test]
fn closing_hides_with_a_status_item_and_quit_really_quits() {
    let dir = TempDir::new("quit");
    let item = FakeItem::default();
    let queue = item.queue.clone();
    let mut rig = build(&dir.db(), Some(item), None);

    let commands = |h: &Harness<'_, OatsApp>| -> Vec<egui::ViewportCommand> {
        h.output()
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.commands.clone())
            .unwrap_or_default()
    };
    let close_window = |h: &mut Harness<'_, OatsApp>| {
        h.input_mut()
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .events
            .push(egui::ViewportEvent::Close);
        h.step();
    };

    // the window's close button only hides it while a status item exists
    close_window(&mut rig.harness);
    let cmds = commands(&rig.harness);
    assert!(
        cmds.contains(&egui::ViewportCommand::CancelClose),
        "{cmds:?}"
    );
    assert!(
        cmds.contains(&egui::ViewportCommand::Visible(false)),
        "{cmds:?}"
    );

    // "Show OatsNotes" brings it back
    queue.borrow_mut().push(StatusAction::ShowWindow);
    rig.harness.step();
    let cmds = commands(&rig.harness);
    assert!(
        cmds.contains(&egui::ViewportCommand::Visible(true)),
        "{cmds:?}"
    );

    // Quit from the status item: a real close, and the follow-up close request isn't cancelled
    queue.borrow_mut().push(StatusAction::Quit);
    rig.harness.step();
    assert!(commands(&rig.harness).contains(&egui::ViewportCommand::Close));
    close_window(&mut rig.harness);
    let cmds = commands(&rig.harness);
    assert!(
        !cmds.contains(&egui::ViewportCommand::CancelClose),
        "{cmds:?}"
    );
}

#[test]
fn request_quit_closes_for_real() {
    let dir = TempDir::new("request-quit");
    let mut rig = build(&dir.db(), Some(FakeItem::default()), None);
    let ctx = rig.harness.ctx.clone();
    rig.app().request_quit(&ctx);
    rig.harness.step();
    let cmds = rig
        .harness
        .output()
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .map(|v| v.commands.clone())
        .unwrap_or_default();
    assert!(cmds.contains(&egui::ViewportCommand::Close), "{cmds:?}");
}

#[test]
fn without_a_status_item_closing_quits() {
    let dir = TempDir::new("no-item");
    let mut rig = build(&dir.db(), None, None);
    assert!(!rig.harness.state().has_status_item());
    rig.harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .events
        .push(egui::ViewportEvent::Close);
    rig.harness.step();
    let cmds = rig
        .harness
        .output()
        .viewport_output
        .get(&egui::ViewportId::ROOT)
        .map(|v| v.commands.clone())
        .unwrap_or_default();
    assert!(
        !cmds.contains(&egui::ViewportCommand::CancelClose),
        "{cmds:?}"
    );
}

#[test]
fn the_preferences_command_opens_the_window_and_enter_saves_the_name() {
    let dir = TempDir::new("prefs");
    let mut rig = build(&dir.db(), None, None);
    Store::open(&dir.db())
        .unwrap()
        .set_user_name("Ale")
        .unwrap();
    assert!(!rig.shows("Your name"));
    rig.app().run(Command::Preferences);
    rig.harness.step();
    rig.harness.step();
    assert!(rig.shows("Your name"));
    // the current value is shown; typing appends to it
    rig.harness
        .get_all_by_value("Ale")
        .next()
        .unwrap()
        .type_text("x");
    rig.harness.step();
    rig.harness.key_press(egui::Key::Enter);
    rig.harness.step();
    rig.harness.step();
    assert_eq!(
        Store::open(&dir.db())
            .unwrap()
            .user_name()
            .unwrap()
            .as_deref(),
        Some("Alex")
    );
    assert!(!rig.shows("Your name"), "Enter closes the window");
}

#[test]
fn a_hidden_sidebar_has_a_restore_button_in_the_control_bar() {
    let dir = TempDir::new("sidebar-restore");
    let mut rig = build(&dir.db(), None, None);
    assert!(rig.app().prefs().sidebar_visible);
    rig.app().run(Command::ToggleSidebar);
    rig.harness.step();
    rig.harness.step();
    assert!(!rig.app().prefs().sidebar_visible);
    // the button is painted (no label): 24 pt wide after the bar's 12 pt margin, in the 34 pt
    // control bar under the menu bar
    let menu = rig.harness.get_by_label("File").rect();
    rig.click_at(egui::pos2(24.0, menu.bottom() + 12.0));
    assert!(
        rig.app().prefs().sidebar_visible,
        "the button shows the sidebar"
    );
}

#[test]
fn search_opens_finds_segments_and_episodes_and_escape_restores_the_view() {
    let dir = TempDir::new("search");
    seed_projects(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowAll);
    rig.harness.run_steps(2);
    assert!(!rig.app().search_open());
    rig.app().run(Command::Search);
    rig.harness.run_steps(2);
    assert!(rig.app().search_open());
    // the field is focused: typing goes straight in
    rig.harness.event(egui::Event::Text("exporter".to_owned()));
    rig.harness.run_steps(3);
    rig.until_label("the transcript hit", "filed under work");
    assert!(rig.shows("1 transcript line"));
    assert!(!rig.shows("loose words"));
    // an episode title from the seeded notes
    rig.harness
        .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    rig.harness
        .event(egui::Event::Text("lease renewal".to_owned()));
    rig.harness.run_steps(3);
    assert!(rig.shows("1 note"), "episode matched");
    // Escape, with the field focused, closes the bar and the previous view is back
    rig.harness.key_press(egui::Key::Escape);
    rig.harness.run_steps(2);
    assert!(!rig.app().search_open());
    assert_eq!(rig.app().prefs().view, View::All);
    assert!(rig.shows("loose words"));
}

#[test]
fn the_search_icon_opens_the_bar() {
    let dir = TempDir::new("search-icon");
    let mut rig = build(&dir.db(), None, None);
    rig.harness.run_steps(2);
    assert!(!rig.app().search_open());
    // far right of the 34 pt control bar (under the menu bar), 12 pt margin, 24 pt icon
    let menu = rig.harness.get_by_label("File").rect();
    let width = rig.harness.ctx.content_rect().right();
    rig.click_at(egui::pos2(width - 24.0, menu.bottom() + 12.0));
    assert!(rig.app().search_open(), "the icon opens the search bar");
}

#[test]
fn a_topic_chip_lists_its_notes_and_a_row_jumps_to_the_card() {
    use oats_core::EpisodeContent;
    let dir = TempDir::new("topic");
    let (_work, commerce, _lease) = seed_projects(&dir.db());
    {
        let s = Store::open(&dir.db()).unwrap();
        let t = now_ms() - 3_600_000;
        s.insert_episode(
            t,
            t + 60_000,
            &EpisodeContent {
                title: "Pricing sync".into(),
                summary: "- numbers".into(),
                node_id: Some(commerce),
                topics: vec!["Q4 roadmap".into()],
                model: "m".into(),
                ..Default::default()
            },
            t + 120_000,
        )
        .unwrap();
    }
    {
        // an unfiled conversation carries the chip the project's note no longer shows here
        let s = Store::open(&dir.db()).unwrap();
        let t = now_ms() - 7_200_000;
        s.insert_episode(
            t,
            t + 60_000,
            &EpisodeContent {
                title: "Loose chat".into(),
                summary: "- chatter".into(),
                topics: vec!["Q4 roadmap".into()],
                model: "m".into(),
                ..Default::default()
            },
            t + 120_000,
        )
        .unwrap();
    }
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowNotes);
    rig.harness.run_steps(3);
    assert!(!rig.app().topic_open());
    rig.harness.get_by_label("Q4 roadmap").click();
    rig.harness.run_steps(3);
    assert!(rig.app().topic_open());
    assert!(rig.shows("Tagged “Q4 roadmap”"));
    assert!(rig.shows("2 notes"));
    assert!(rig.shows("Pricing sync"));
    assert!(!rig.shows("- numbers"), "no summaries in the list");
    assert_eq!(
        rig.app().prefs().view,
        View::Notes,
        "the saved view is untouched"
    );
    // a row jumps to the note in its node's view; the card consumes the focus
    rig.harness.get_by_label("Pricing sync").click();
    rig.harness.run_steps(3);
    assert!(!rig.app().topic_open());
    assert_eq!(rig.app().prefs().view, View::Node(commerce));
    assert_eq!(
        rig.app().pending_focus(),
        None,
        "the card consumed the focus"
    );
    assert!(rig.shows("Pricing sync"));
    // Escape closes the topic view back to the previous view
    rig.app().run(Command::ShowNotes);
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Q4 roadmap").click();
    rig.harness.run_steps(2);
    assert!(rig.app().topic_open());
    rig.harness.key_press(egui::Key::Escape);
    rig.harness.run_steps(2);
    assert!(!rig.app().topic_open());
}

#[test]
fn conversations_lists_only_unfiled_episodes() {
    use oats_core::EpisodeContent;
    let dir = TempDir::new("conversations");
    let (work, _commerce, _lease) = seed_projects(&dir.db());
    {
        let s = Store::open(&dir.db()).unwrap();
        let t = now_ms() - 3_600_000;
        s.insert_episode(
            t,
            t + 60_000,
            &EpisodeContent {
                title: "Bare category chat".into(),
                summary: "- hello".into(),
                node_id: Some(work),
                model: "m".into(),
                ..Default::default()
            },
            t + 120_000,
        )
        .unwrap();
    }
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowNotes);
    rig.harness.run_steps(3);
    assert!(rig.shows("Bare category chat"));
    assert!(
        !rig.shows("Checkout flow decisions"),
        "filed under a project"
    );
    assert!(!rig.shows("Lease renewal terms"));
    let titles: Vec<_> = rig
        .app()
        .listed_episodes()
        .iter()
        .map(|e| e.title.clone())
        .collect();
    assert_eq!(titles, ["Bare category chat"]);
}

#[test]
fn conversations_says_so_when_everything_is_filed() {
    let dir = TempDir::new("conversations-empty");
    seed_projects(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowNotes);
    rig.harness.run_steps(3);
    assert!(
        rig.app().listed_episodes().is_empty(),
        "the empty state shows"
    );
}

// ---- todos, reminder pop-ups, the calendar's jump to the transcript ----------------------------

fn add_todo(db: &Path, text: &str, due: Option<i64>) -> i64 {
    let s = Store::open(db).unwrap();
    s.add_todo(
        &oats_core::NewTodo {
            text: text.into(),
            due,
            ..Default::default()
        },
        now_ms(),
    )
    .unwrap()
    .id
}

#[test]
fn a_due_todo_pops_up_and_done_completes_it() {
    let dir = TempDir::new("toast-done");
    let id = add_todo(&dir.db(), "Pay the property taxes", Some(now_ms() - 60_000));
    add_todo(&dir.db(), "Later thing", Some(now_ms() + 3_600_000));
    let mut rig = build(&dir.db(), None, None);
    rig.harness.run_steps(2);
    assert_eq!(
        rig.app()
            .due_todos()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        [id]
    );
    assert!(rig.shows("Pay the property taxes"));
    assert!(rig.app().next_due().is_some(), "the later one is scheduled");
    rig.harness.get_by_label("Done").click();
    rig.harness.run_steps(2);
    assert!(rig.app().due_todos().is_empty(), "the pop-up closed");
    let t = rig
        .app()
        .todos()
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .unwrap();
    assert!(t.done(), "Done completes it");
    assert_eq!(
        rig.app().todos().last().map(|t| t.id),
        Some(id),
        "completed todos go to the end"
    );
}

#[test]
fn the_close_button_of_a_reminder_completes_it_too() {
    let dir = TempDir::new("toast-x");
    let id = add_todo(&dir.db(), "Call the landlord", Some(now_ms() - 1_000));
    let mut rig = build(&dir.db(), None, None);
    rig.harness.run_steps(2);
    rig.harness.get_by_label("×").click();
    rig.harness.run_steps(2);
    assert!(rig.app().due_todos().is_empty());
    assert!(
        Store::open(&dir.db())
            .unwrap()
            .todo(id)
            .unwrap()
            .unwrap()
            .done()
    );
}

#[test]
fn snoozing_a_reminder_hides_it_for_an_hour() {
    let dir = TempDir::new("toast-snooze");
    let id = add_todo(&dir.db(), "Water the plants", Some(now_ms() - 1_000));
    let mut rig = build(&dir.db(), None, None);
    rig.harness.run_steps(2);
    let before = now_ms();
    rig.harness.get_by_label("1h").click();
    rig.harness.run_steps(2);
    assert!(rig.app().due_todos().is_empty(), "snoozed: hidden");
    let t = Store::open(&dir.db()).unwrap().todo(id).unwrap().unwrap();
    assert!(!t.done());
    let until = t.snoozed_until.unwrap();
    assert!((before + 3_600_000..=now_ms() + 3_600_000).contains(&until));
    assert_eq!(rig.app().next_due(), Some(until), "it wakes the UI then");
}

#[test]
fn the_todos_view_lists_open_then_completed_and_adds_by_hand() {
    let dir = TempDir::new("todos-view");
    let done = add_todo(&dir.db(), "Already done", None);
    Store::open(&dir.db())
        .unwrap()
        .complete_todo(done, now_ms())
        .unwrap();
    add_todo(&dir.db(), "Undated thing", None);
    add_todo(&dir.db(), "Dated thing", Some(now_ms() + 2 * 86_400_000));
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowTodos);
    rig.harness.run_steps(2);
    assert_eq!(rig.app().prefs().view, View::Todos);
    let texts: Vec<String> = rig.app().todos().iter().map(|t| t.text.clone()).collect();
    assert_eq!(texts, ["Dated thing", "Undated thing", "Already done"]);
    assert!(rig.shows("Completed (1)"));
    assert!(rig.shows("in 2 days"));
    assert!(rig.shows("Already done"));
    // typed by hand: Enter adds an undated todo
    use egui::accesskit::Role;
    rig.harness
        .get_all_by_role(Role::TextInput)
        .next()
        .unwrap()
        .click();
    rig.harness.run_steps(1);
    rig.harness
        .event(egui::Event::Text("Buy oat milk".to_owned()));
    rig.harness.key_press(egui::Key::Enter);
    rig.harness.run_steps(2);
    let added = rig
        .app()
        .todos()
        .iter()
        .find(|t| t.text == "Buy oat milk")
        .cloned();
    assert!(added.is_some_and(|t| t.due.is_none() && !t.done()));
    // the first checkbox completes the first open todo; it moves under "Completed"
    let first = rig.app().todos()[0].id;
    rig.harness
        .get_all_by_role(Role::CheckBox)
        .next()
        .unwrap()
        .click();
    rig.harness.run_steps(2);
    assert!(rig.shows("Completed (2)"));
    assert!(
        rig.app()
            .todos()
            .iter()
            .find(|t| t.id == first)
            .is_some_and(|t| t.done())
    );
    assert!(rig.app().todos()[0].id != first);
}

#[test]
fn a_dated_todo_lands_on_the_calendar_and_set_due_dates_an_undated_one() {
    use chrono::{Duration, Local, Timelike};
    use egui::accesskit::Role;
    let dir = TempDir::new("todo-due");
    let plain = add_todo(&dir.db(), "Water the plants", None);
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowTodos);
    rig.harness.run_steps(2);
    // "text @ date" in the text field
    rig.harness
        .get_all_by_role(Role::TextInput)
        .next()
        .unwrap()
        .click();
    rig.harness.run_steps(1);
    rig.harness
        .event(egui::Event::Text("call landlord @ tomorrow 9am".to_owned()));
    rig.harness.key_press(egui::Key::Enter);
    rig.harness.run_steps(2);
    let tomorrow = Local::now().date_naive() + Duration::days(1);
    let added = rig
        .app()
        .todos()
        .iter()
        .find(|t| t.text == "call landlord")
        .cloned()
        .expect("added without the suffix");
    let due = Local
        .timestamp_millis_opt(added.due.expect("dated"))
        .unwrap();
    assert_eq!(
        (due.date_naive(), due.hour(), due.minute()),
        (tomorrow, 9, 0)
    );
    let todos = rig.app().todos().to_vec();
    let by_day = oats_ui::calendar::items_by_day(&[], &todos, &Local, false);
    assert!(
        by_day[&tomorrow]
            .iter()
            .any(|i| matches!(i, oats_ui::calendar::DayItem::Todo(t) if t.id == added.id))
    );
    rig.app().run(Command::ShowCalendar);
    rig.harness.run_steps(2);
    // (the grid paints its chips as plain shapes; the offscreen selftest shows them)
    // an unreadable due in the due field adds nothing
    rig.app().run(Command::ShowTodos);
    rig.harness.run_steps(2);
    let fields: Vec<_> = rig.harness.get_all_by_role(Role::TextInput).collect();
    fields[0].click();
    rig.harness.run_steps(1);
    rig.harness.event(egui::Event::Text("Bad one".to_owned()));
    rig.harness.run_steps(1);
    rig.harness
        .get_all_by_role(Role::TextInput)
        .nth(1)
        .unwrap()
        .click();
    rig.harness.run_steps(1);
    rig.harness.event(egui::Event::Text("someday".to_owned()));
    rig.harness.key_press(egui::Key::Enter);
    rig.harness.run_steps(2);
    assert!(rig.app().todos().iter().all(|t| t.text != "Bad one"));
    // "Set due…" on an undated todo dates it
    rig.harness
        .get_by_label("Water the plants")
        .click_secondary();
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Set due…").click();
    rig.harness.run_steps(3);
    rig.harness
        .event(egui::Event::Text("2030-05-06 15:30".to_owned()));
    rig.harness.key_press(egui::Key::Enter);
    rig.harness.run_steps(3);
    let due = rig
        .app()
        .todos()
        .iter()
        .find(|t| t.id == plain)
        .and_then(|t| t.due)
        .expect("now dated");
    let d = Local.timestamp_millis_opt(due).unwrap();
    assert_eq!(d.format("%Y-%m-%d %H:%M").to_string(), "2030-05-06 15:30");
    // and "Clear due" undates it again
    rig.harness
        .get_by_label("Water the plants")
        .click_secondary();
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Clear due").click();
    rig.harness.run_steps(3);
    assert_eq!(
        rig.app()
            .todos()
            .iter()
            .find(|t| t.id == plain)
            .and_then(|t| t.due),
        None
    );
}

#[test]
fn a_voice_command_confirmation_shows_and_closes_after_four_seconds() {
    let dir = TempDir::new("confirm");
    let mut rig = build(&dir.db(), None, None);
    let dinged = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let d = dinged.clone();
    rig.app().set_chime(Box::new(move || {
        d.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let ctx = rig.harness.ctx.clone();
    rig.app().selftest_command(
        &ctx,
        vec![
            "Todo added: Buy oat milk".to_owned(),
            "Note filed: the landlord's number".to_owned(),
        ],
    );
    rig.harness.run_steps(1);
    assert!(rig.shows("Todo added: Buy oat milk"));
    assert!(rig.shows("Note filed: the landlord's number"));
    assert_eq!(rig.app().confirmations().len(), 2);
    assert_eq!(
        dinged.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one ding per command"
    );
    // the harness advances egui's clock by 0.25 s a step: 4 s later they are gone
    rig.harness.run_steps(17);
    assert!(rig.app().confirmations().is_empty());
    assert!(!rig.shows("Todo added: Buy oat milk"));
}

#[test]
fn a_calendar_item_jumps_to_where_it_was_said() {
    let dir = TempDir::new("cal-jump");
    seed(&dir.db());
    let seg = {
        let s = Store::open(&dir.db()).unwrap();
        let sid = s.begin_session(now_ms() - 30_000, "e").unwrap();
        let t = now_ms() - 20_000;
        let seg = s
            .insert_segment(
                sid,
                t,
                t + 3_000,
                "hey computer dentist today at five",
                None,
            )
            .unwrap();
        s.upsert_event(
            &oats_core::NewEvent {
                when_start: now_ms(),
                title: "Dentist appointment".into(),
                source: Some(oats_core::Origin::Hotword),
                source_segment_id: Some(seg.id),
                ..Default::default()
            },
            now_ms(),
        )
        .unwrap();
        s.upsert_event(
            &oats_core::NewEvent {
                when_start: now_ms(),
                title: "Orphan event".into(),
                source: Some(oats_core::Origin::User),
                ..Default::default()
            },
            now_ms(),
        )
        .unwrap();
        seg
    };
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowCalendar);
    rig.harness.run_steps(2);
    // the day panel (today is selected) lists it; clicking it shows the transcript
    rig.harness.get_by_label("Dentist appointment").click();
    rig.harness.step();
    assert_eq!(rig.app().prefs().view, View::Today, "said today");
    rig.harness.run_steps(3);
    assert_eq!(
        rig.app().pending_segment_focus(),
        None,
        "the row consumed the focus"
    );
    assert!(rig.shows(&seg.text));
    // an item with no source says so
    rig.app().run(Command::ShowCalendar);
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Orphan event").click();
    rig.harness.run_steps(2);
    assert_eq!(rig.app().prefs().view, View::Calendar);
    assert_eq!(rig.app().warning(), Some("No transcript for this item"));
}

// ---- project landing page, sidebar notes layer, single-note view, editing -----------------------

/// Two notes under the "Commerce project", the older with two action items, the newer with one.
fn seed_project_notes(db: &Path) -> (i64, i64, i64) {
    use oats_core::{ActionItem, EpisodeContent};
    let (_work, commerce, _lease) = seed_projects(db);
    let s = Store::open(db).unwrap();
    let t = now_ms() - 7_200_000;
    let mut ids = Vec::new();
    for (i, (title, summary, items)) in [
        (
            "Pricing sync",
            "- pricing detail alpha",
            vec![
                ("Draft the price table", Some("Ana")),
                ("Check VAT rules", None),
            ],
        ),
        (
            "Launch review",
            "- launch detail beta",
            vec![("Book the launch room", Some("Joe"))],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let at = t + i as i64 * 1_800_000;
        let e = s
            .insert_episode(
                at,
                at + 60_000,
                &EpisodeContent {
                    title: title.into(),
                    summary: summary.into(),
                    node_id: Some(commerce),
                    action_items: items
                        .into_iter()
                        .map(|(text, owner)| ActionItem {
                            text: text.into(),
                            owner: owner.map(str::to_owned),
                            due: None,
                        })
                        .collect(),
                    model: "m".into(),
                    ..Default::default()
                },
                at + 120_000,
            )
            .unwrap();
        ids.push(e.id);
    }
    (commerce, ids[0], ids[1])
}

#[test]
fn the_project_page_lists_next_steps_over_collapsed_notes_and_links_back_to_them() {
    let dir = TempDir::new("project-page");
    let (commerce, pricing, launch) = seed_project_notes(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.app().selftest_view_node(commerce);
    rig.harness.run_steps(3);
    assert!(rig.shows("Next steps"));
    assert!(rig.shows("Draft the price table"));
    assert!(rig.shows("Book the launch room"));
    assert!(rig.shows("Check VAT rules"));
    assert!(rig.shows("Note: Pricing sync") && rig.shows("Note: Launch review"));
    assert!(!rig.shows("pricing detail alpha"), "notes start collapsed");
    assert!(rig.app().open_notes().is_empty());
    // a row expands its note inline
    rig.harness.get_by_label("Note: Pricing sync").click();
    rig.harness.run_steps(3);
    assert!(rig.shows("pricing detail alpha"));
    assert!(rig.app().open_notes().contains(&pricing));
    assert!(!rig.shows("launch detail beta"));
    // collapse all, then a next step's source link lands on the expanded note
    rig.harness.get_by_label("Collapse all").click();
    rig.harness.run_steps(3);
    assert!(!rig.shows("pricing detail alpha"));
    rig.harness
        .get_by_label_contains("Book the launch room")
        .click();
    rig.harness.run_steps(4);
    assert_eq!(rig.app().pending_focus(), None, "focus consumed");
    assert!(rig.app().open_notes().contains(&launch));
    assert!(rig.shows("launch detail beta"));
    assert!(!rig.shows("pricing detail alpha"));
}

#[test]
fn a_project_with_no_action_items_says_so() {
    let dir = TempDir::new("project-empty");
    let (_work, commerce, _lease) = seed_projects(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.app().selftest_view_node(commerce);
    rig.harness.run_steps(3);
    assert!(rig.shows("No next steps yet"));
}

#[test]
fn the_sidebar_lists_a_projects_notes_and_one_opens_alone_until_escape() {
    let dir = TempDir::new("sidebar-notes");
    let (commerce, _pricing, launch) = seed_project_notes(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    assert!(!rig.shows("Note: Launch review"), "the layer starts closed");
    rig.app().open_project_layer(commerce);
    rig.harness.run_steps(3);
    assert!(rig.shows("Note: Launch review") && rig.shows("Note: Pricing sync"));
    rig.harness.get_by_label("Note: Launch review").click();
    rig.harness.run_steps(3);
    assert_eq!(rig.app().prefs().view, View::Node(commerce));
    assert_eq!(rig.app().focus_episode(), Some(launch));
    assert!(rig.shows("launch detail beta"), "the full card");
    assert!(!rig.shows("Next steps"), "not the project page");
    rig.harness.key_press(egui::Key::Escape);
    rig.harness.run_steps(3);
    assert_eq!(rig.app().focus_episode(), None);
    assert!(rig.shows("Next steps"));
}

#[test]
fn editing_a_note_saves_to_the_store_and_cancel_discards() {
    let dir = TempDir::new("edit-note");
    let (commerce, pricing, _launch) = seed_project_notes(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.app().selftest_view_node(commerce);
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Note: Pricing sync").click();
    rig.harness.run_steps(2);
    // cancel first: nothing is written
    rig.harness.get_all_by_label("Edit").next().unwrap().click();
    rig.harness.run_steps(2);
    let inputs: Vec<_> = rig
        .harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .collect();
    inputs[0].focus();
    rig.harness.run_steps(1);
    rig.harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .next()
        .unwrap()
        .type_text(" DISCARDED");
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Cancel").click();
    rig.harness.run_steps(2);
    let s = Store::open(&dir.db()).unwrap();
    assert_eq!(s.episode(pricing).unwrap().unwrap().title, "Pricing sync");
    assert!(
        s.episode(pricing)
            .unwrap()
            .unwrap()
            .user_edited_at
            .is_none()
    );
    // now edit and save
    rig.harness.get_all_by_label("Edit").next().unwrap().click();
    rig.harness.run_steps(2);
    let summary = rig
        .harness
        .get_all_by_role(egui::accesskit::Role::MultilineTextInput)
        .next()
        .unwrap();
    summary.focus();
    rig.harness.run_steps(1);
    rig.harness
        .get_all_by_role(egui::accesskit::Role::MultilineTextInput)
        .next()
        .unwrap()
        .type_text("\n- my added line");
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Save").click();
    rig.harness.run_steps(3);
    let e = s.episode(pricing).unwrap().unwrap();
    assert!(e.summary.contains("my added line"), "{:?}", e.summary);
    assert!(e.summary.contains("pricing detail alpha"));
    assert!(e.user_edited_at.is_some());
    assert!(rig.shows("my added line"), "read-only card re-rendered");
}
