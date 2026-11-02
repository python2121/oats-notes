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
use oats_ui::{AppOptions, AudioRequest, AudioStarter, Command, MenuEntry, OatsApp, StartedAudio};

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
    /// Every request the fake starter received.
    requests: Rc<RefCell<Vec<AudioRequest>>>,
}

fn build(db: &Path, item: Option<FakeItem>, brain: Option<BrainHandle>) -> Rig {
    build_with(db, item, brain, false)
}

/// [`build`]; `system`: the starter reports system audio running next to the microphone.
fn build_with(db: &Path, item: Option<FakeItem>, brain: Option<BrainHandle>, system: bool) -> Rig {
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
    let requests: Rc<RefCell<Vec<AudioRequest>>> = Rc::default();
    let r = requests.clone();
    let audio: AudioStarter = Box::new(move |tx, request| {
        *m.borrow_mut() = Some(tx);
        r.borrow_mut().push(request);
        Ok(StartedAudio {
            system: system && request.system,
            ..StartedAudio::mic(Box::new(()))
        })
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
    let mut rig = Rig {
        harness,
        mic,
        requests,
    };
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
    assert_eq!(rig.harness.state().prefs().view, View::Transcripts);
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

    rig.app().run(Command::ShowTranscripts);
    rig.harness.run_steps(2);
    assert_eq!(rig.harness.state().prefs().view, View::Transcripts);
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
    rig.app().run(Command::ShowTranscripts);
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
    rig.app().run(Command::ShowTranscripts);
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
    assert_eq!(rig.app().prefs().view, View::Transcripts);
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

/// ⌘Z undoes the user's own quick edit (a todo ticked off), ⇧⌘Z redoes it; with nothing
/// left, ⌘Z changes nothing. A focused text field keeps ⌘Z for itself.
#[test]
fn cmd_z_undoes_and_shift_cmd_z_redoes_a_completed_todo() {
    use egui::accesskit::Role;
    let dir = TempDir::new("undo-todo");
    let id = add_todo(&dir.db(), "Water the plants", None);
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowTodos);
    rig.harness.run_steps(2);
    let done = |_: &mut Rig| {
        Store::open(&dir.db())
            .unwrap()
            .todo(id)
            .unwrap()
            .unwrap()
            .done()
    };
    rig.harness
        .get_all_by_role(Role::CheckBox)
        .next()
        .unwrap()
        .click();
    rig.harness.run_steps(2);
    assert!(done(&mut rig));
    rig.harness
        .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    rig.harness.run_steps(2);
    assert!(!done(&mut rig), "undone");
    assert!(rig.shows("Undo: Complete todo"));
    rig.harness.key_press_modifiers(
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::Z,
    );
    rig.harness.run_steps(2);
    assert!(done(&mut rig), "redone");
    // in the add-todo field ⌘Z is the field's own: the todo stays done
    rig.harness
        .get_all_by_role(Role::TextInput)
        .next()
        .unwrap()
        .click();
    rig.harness.run_steps(1);
    rig.harness
        .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    rig.harness.run_steps(2);
    assert!(done(&mut rig), "a focused text field keeps ⌘Z");
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
    assert_eq!(
        rig.app().prefs().view,
        View::Transcripts,
        "said transcripts"
    );
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
                            ..Default::default()
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

/// A "Development team call" note (homed under its own project) with one action item filed
/// under the Commerce project, said at a transcript line. Returns (commerce, devcall, note,
/// segment).
fn seed_cross_filed(db: &Path) -> (i64, i64, i64, i64) {
    use oats_core::{ActionItem, EpisodeContent, NodeKind, Origin};
    let (work, commerce, _lease) = seed_projects(db);
    let s = Store::open(db).unwrap();
    let devcall = s
        .add_node(
            Some(work),
            NodeKind::Project,
            "Development team call",
            "",
            Origin::User,
        )
        .unwrap()
        .id;
    let t = now_ms() - 3_600_000;
    let sid = s.begin_session(t, "e").unwrap();
    let seg = s
        .insert_segment(
            sid,
            t,
            t + 5_000,
            "the checkout needs a progress spinner",
            None,
        )
        .unwrap();
    let ep = s
        .insert_episode(
            t,
            t + 600_000,
            &EpisodeContent {
                title: "Dev call notes".into(),
                summary: "- devcall detail gamma".into(),
                node_id: Some(devcall),
                action_items: vec![
                    ActionItem {
                        text: "Add a checkout spinner".into(),
                        owner: Some("Sam".into()),
                        node_id: Some(commerce),
                        at_ms: Some(t),
                        segment_id: Some(seg.id),
                        ..Default::default()
                    },
                    ActionItem {
                        text: "Rotate the on-call rota".into(),
                        ..Default::default()
                    },
                ],
                model: "m".into(),
                ..Default::default()
            },
            t + 700_000,
        )
        .unwrap();
    s.mark_segments_interpreted(&[seg.id], ep.id, t + 700_000)
        .unwrap();
    (commerce, devcall, ep.id, seg.id)
}

#[test]
fn an_item_filed_elsewhere_shows_inline_and_its_click_chain_reaches_the_transcript() {
    let dir = TempDir::new("cross-filed");
    let (commerce, devcall, note, _seg) = seed_cross_filed(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.app().selftest_view_node(commerce);
    rig.harness.run_steps(3);
    // inline in Commerce's own next steps; the note's other item is not
    assert!(rig.shows("Next steps"));
    assert!(rig.shows("Add a checkout spinner"));
    assert!(!rig.shows("Rotate the on-call rota"));
    assert!(
        !rig.shows("Note: Dev call notes"),
        "the note itself lives elsewhere"
    );
    // its source: the dev call's page, the note opened and focused
    rig.harness
        .get_by_label_contains("Add a checkout spinner")
        .click();
    rig.harness.run_steps(4);
    assert_eq!(rig.app().prefs().view, View::Node(devcall));
    assert_eq!(rig.app().pending_focus(), None, "focus consumed");
    assert!(rig.app().open_notes().contains(&note));
    assert!(rig.shows("devcall detail gamma"));
    // inside the note the item links to the transcript at the line it was said
    rig.harness
        .get_all_by_label_contains("Add a checkout spinner")
        .last()
        .unwrap()
        .click();
    rig.harness.step();
    assert_eq!(rig.app().prefs().view, View::Transcripts);
    rig.harness.run_steps(3);
    assert_eq!(
        rig.app().pending_segment_focus(),
        None,
        "the row took the focus"
    );
    assert!(rig.shows("the checkout needs a progress spinner"));
}

/// A fake `claude` that logs every prompt and answers any "move" request with a new project
/// under the Admin category (`admin`).
fn fake_cli_move(dir: &Path, admin: i64) -> ClaudeCli {
    let script = dir.join("claude");
    let body = format!(
        r#"#!/bin/sh
case " $* " in *" --version "*) echo "2.1.312 (Claude Code)"; exit 0;; esac
cat >> '{prompts}'
echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.001,"duration_ms":10,"structured_output":{{"action":"create","node_id":null,"new_project":{{"name":"Apartment lease renewal","parent_id":{admin},"description":"Renewing the lease"}},"reason":"new"}}}}'
"#,
        prompts = dir.join("prompts").display()
    );
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

fn open_move_field(rig: &mut Rig, note: &str, text: &str) {
    rig.harness.get_by_label(note).click_secondary();
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Move to…").click();
    rig.harness.run_steps(3);
    rig.harness
        .get_by_role(egui::accesskit::Role::TextInput)
        .type_text(text);
    rig.harness.run_steps(2);
    rig.harness.key_press(egui::Key::Enter);
}

#[test]
fn move_to_an_existing_name_moves_the_note_without_a_brain_call() {
    let dir = TempDir::new("move-local");
    let (_commerce, pricing, _launch) = seed_project_notes(&dir.db());
    let lease = Store::open(&dir.db())
        .unwrap()
        .nodes()
        .unwrap()
        .into_iter()
        .find(|n| n.name == "Apartment lease")
        .unwrap()
        .id;
    let commerce = Store::open(&dir.db())
        .unwrap()
        .episode(pricing)
        .unwrap()
        .unwrap()
        .node_id
        .unwrap();
    let brain = Brain {
        config: BrainConfig {
            cli: fake_cli_logging(&dir.0),
            ..BrainConfig::default()
        },
        store_path: dir.db(),
        wake: Box::new(|| {}),
    }
    .spawn();
    let mut rig = build(&dir.db(), None, Some(brain));
    rig.app().selftest_view_node(commerce);
    rig.until("the brain to report in", |app| {
        interpret_enabled(app) == Some(true)
    });
    rig.harness.run_steps(3);
    // a unique substring of "Apartment lease"
    open_move_field(&mut rig, "Note: Pricing sync", "apartment");
    rig.harness.run_steps(3);
    let s = Store::open(&dir.db()).unwrap();
    assert_eq!(s.episode(pricing).unwrap().unwrap().node_id, Some(lease));
    assert!(
        s.changes(10)
            .unwrap()
            .iter()
            .any(|c| c.kind == "episode_move")
    );
    assert!(!rig.shows("Note: Pricing sync"), "moved off this page");
    assert!(rig.shows("Moved to Work › Apartment lease"), "the toast");
    assert!(
        !dir.0.join("prompts").exists(),
        "no model call for a name in the tree"
    );
}

#[test]
fn move_to_something_new_asks_the_model_and_creates_the_project() {
    let dir = TempDir::new("move-new-project");
    let (commerce, pricing, _launch) = seed_project_notes(&dir.db());
    let admin = Store::open(&dir.db())
        .unwrap()
        .categories()
        .unwrap()
        .into_iter()
        .find(|c| c.name == "Admin")
        .unwrap()
        .id;
    let brain = Brain {
        config: BrainConfig {
            cli: fake_cli_move(&dir.0, admin),
            ..BrainConfig::default()
        },
        store_path: dir.db(),
        wake: Box::new(|| {}),
    }
    .spawn();
    let mut rig = build(&dir.db(), None, Some(brain));
    rig.app().selftest_view_node(commerce);
    rig.until("the brain to report in", |app| {
        interpret_enabled(app) == Some(true)
    });
    rig.harness.run_steps(3);
    open_move_field(
        &mut rig,
        "Note: Pricing sync",
        "new project for the lease renewal under admin",
    );
    rig.until("the note to move", |app| {
        app.listed_episodes().iter().all(|e| e.id != pricing)
    });
    let s = Store::open(&dir.db()).unwrap();
    let p = s
        .find_node(Some(admin), "Apartment lease renewal")
        .unwrap()
        .unwrap();
    assert_eq!(p.created_by, oats_core::Origin::User);
    assert_eq!(s.episode(pricing).unwrap().unwrap().node_id, Some(p.id));
    let prompts = std::fs::read_to_string(dir.0.join("prompts")).unwrap();
    assert!(prompts.contains("lease renewal under admin"));
    assert!(
        s.changes(10)
            .unwrap()
            .iter()
            .any(|c| c.kind == "node_create")
    );
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
    rig.harness.get_all_by_label("Edit").last().unwrap().click(); // the card's, not the Edit menu
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
    rig.harness.get_all_by_label("Edit").last().unwrap().click(); // the card's, not the Edit menu
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

// ---- the Transcripts view pages by row count -----------------------------------------------

/// `n` segments "r<i>" five minutes apart, the last one a minute ago (so they span a couple of
/// days and the day headings show); returns their ids oldest first.
fn seed_rows(db: &Path, n: usize) -> Vec<i64> {
    let s = Store::open(db).unwrap();
    let sid = s.begin_session(0, "history").unwrap();
    let base = now_ms() - 60_000 - n as i64 * 300_000;
    s.with_savepoint(|s| {
        (0..n)
            .map(|i| {
                let at = base + i as i64 * 300_000;
                Ok(
                    s.insert_segment(sid, at, at + 3_000, &format!("r{i}"), None)?
                        .id,
                )
            })
            .collect()
    })
    .unwrap()
}

#[test]
fn the_sidebar_has_no_today_and_says_transcripts() {
    let dir = TempDir::new("no-today");
    let mut rig = build(&dir.db(), None, None);
    // (the sidebar's rows are painted, not accessible: they are checked against VIEW_ROWS in
    // the sidebar's unit test; the View menu is the other place the views are listed)
    rig.harness.get_by_label("View").click();
    rig.harness.run_steps(3);
    assert!(rig.shows("Transcripts"));
    assert!(rig.shows("Conversations"));
    assert!(!rig.shows("Today"));
    assert!(!rig.shows("All Transcripts"));
    assert_eq!(rig.app().prefs().view, View::Transcripts);
}

#[test]
fn an_empty_history_shows_no_empty_days() {
    let dir = TempDir::new("no-rows");
    let mut rig = build(&dir.db(), None, None);
    rig.harness.run_steps(3);
    // (the placeholder is painted text; nothing loaded and no day heading is what matters)
    assert_eq!(rig.app().loaded_rows(), (0, None, None));
    assert!(!rig.shows("Nothing today yet"));
}

#[test]
fn the_newest_page_of_rows_loads_first() {
    let dir = TempDir::new("first-page");
    let ids = seed_rows(&dir.db(), 400);
    let mut rig = build(&dir.db(), None, None);
    rig.app().set_page_rows(150);
    rig.harness.run_steps(4);
    assert_eq!(
        rig.app().loaded_rows(),
        (150, Some(ids[250]), Some(ids[399]))
    );
    assert!(rig.shows("r399"));
    assert!(!rig.shows("r10"), "older pages wait");
    assert!(!rig.app().transcript_has_later());
}

#[test]
fn the_default_page_is_a_thousand_rows_and_lays_out_in_reasonable_time() {
    let dir = TempDir::new("big-page");
    let ids = seed_rows(&dir.db(), 1_200);
    let mut rig = build(&dir.db(), None, None);
    let started = Instant::now();
    rig.harness.run_steps(10);
    let per_frame = started.elapsed() / 10;
    eprintln!("1000-row page: {per_frame:?} per frame (debug-optimised test profile)");
    assert_eq!(rig.app().loaded_rows().0, 1_000);
    assert_eq!(rig.app().loaded_rows().2, Some(ids[1_199]));
    assert!(per_frame < Duration::from_millis(400), "{per_frame:?}");
}

#[test]
fn scrolling_toward_the_top_loads_the_previous_page_without_a_jump() {
    let dir = TempDir::new("prefetch");
    let ids = seed_rows(&dir.db(), 400);
    let mut rig = build(&dir.db(), None, None);
    rig.app().set_page_rows(150);
    rig.harness.run_steps(4);
    assert_eq!(rig.app().loaded_rows().0, 150);
    rig.harness.hover_at(egui::pos2(600.0, 400.0));
    let wheel = |rig: &mut Rig, dy: f32| {
        rig.harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, dy),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        rig.harness.run_steps(6);
    };
    // the y of a row of the first page that is on screen, to follow it across the load
    let probe = |rig: &Rig| -> Option<(String, f32)> {
        (250..400).find_map(|i| {
            let label = format!("r{i}");
            let r = rig.harness.query_by_label(&label)?.rect();
            (r.top() > 150.0 && r.bottom() < 650.0).then_some((label, r.top()))
        })
    };
    let mut steps = 0;
    let mut checked = false;
    while rig.app().loaded_rows().0 < 300 {
        assert!(steps < 120, "the previous page never loaded");
        let before = probe(&rig);
        wheel(&mut rig, 200.0);
        steps += 1;
        if rig.app().loaded_rows().0 >= 300
            && let Some((label, y)) = before
            && let Some(node) = rig.harness.query_by_label(&label)
        {
            // the row moved by about the wheel step (200 px down the screen), not by the
            // height of the new page (thousands of px)
            let moved = node.rect().top() - y;
            checked = true;
            assert!((moved - 200.0).abs() < 60.0, "the list jumped by {moved}");
        }
    }
    assert_eq!(
        rig.app().loaded_rows(),
        (300, Some(ids[100]), Some(ids[399]))
    );
    assert!(
        checked,
        "the row to follow was found on both sides of the load"
    );
}

#[test]
fn a_search_hit_from_far_back_loads_a_window_around_it_and_paging_forward_reaches_the_tail() {
    let dir = TempDir::new("search-far");
    let ids = seed_rows(&dir.db(), 600);
    {
        // a unique line deep in the past (it sorts between r100 and r101)
        let s = Store::open(&dir.db()).unwrap();
        let sid = s.begin_session(0, "old").unwrap();
        let at = now_ms() - 60_000 - 600 * 300_000 + 100 * 300_000 + 1_000;
        s.insert_segment(sid, at, at + 3_000, "zebra crossing budget", None)
            .unwrap();
    }
    let mut rig = build(&dir.db(), None, None);
    rig.app().set_page_rows(100);
    rig.app().run(Command::ShowCalendar);
    rig.harness.run_steps(2);
    rig.app().run(Command::Search);
    rig.harness.run_steps(2);
    rig.harness.event(egui::Event::Text("zebra".to_owned()));
    rig.harness.run_steps(3);
    rig.until_label("the hit", "zebra crossing budget");
    rig.harness.get_by_label_contains("zebra crossing").click();
    rig.harness.run_steps(4);
    assert!(!rig.app().search_open());
    assert_eq!(rig.app().prefs().view, View::Transcripts);
    let (n, first, last) = rig.app().loaded_rows();
    assert!(n >= 100, "a window of rows around the target, got {n}");
    assert!(first.unwrap() <= ids[100] && last.unwrap() >= ids[101]);
    assert!(
        rig.app().transcript_has_later(),
        "the tail is not loaded yet"
    );
    assert_eq!(
        rig.app().pending_segment_focus(),
        None,
        "the row consumed it"
    );
    assert!(rig.shows("zebra crossing budget"));
    // scroll down: pages load forward until the newest rows are reached
    rig.harness.hover_at(egui::pos2(600.0, 400.0));
    let mut steps = 0;
    while rig.app().transcript_has_later() {
        assert!(steps < 200, "the tail was never reached");
        rig.harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -600.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        rig.harness.run_steps(4);
        steps += 1;
    }
    assert_eq!(rig.app().loaded_rows().2, Some(ids[599]));
}

// ---- per-app classes, On a call, Mark as a call / media, Show media ------------------------------

/// A Discord media note: two system segments stamped as a media app, filed under Media.
fn seed_discord_episode(db: &Path) -> i64 {
    use oats_core::{AudioSourceKind, EpisodeContent, MediaClass, MediaStamp};
    let s = Store::open(db).unwrap();
    let sid = s.begin_session(now_ms() - 600_000, "seed").unwrap();
    let media = s.media_category().unwrap();
    let mut discord = MediaStamp::new(MediaClass::App, None, "Discord", "Discord");
    discord.bundle_id = "com.hnc.discord".into();
    let t = now_ms() - 300_000;
    let mut ids = Vec::new();
    for (i, text) in [
        "so we agreed the export ships on Friday after the last round of testing",
        "and Dana sends the release notes to the whole team on Thursday evening",
    ]
    .into_iter()
    .enumerate()
    {
        let at = t + i as i64 * 10_000;
        let row = s
            .insert_segment_from(
                sid,
                at,
                at + 5_000,
                text,
                None,
                AudioSourceKind::System,
                None,
            )
            .unwrap();
        s.stamp_segment_media(row.id, &discord).unwrap();
        s.set_segment_node(row.id, Some(media.id)).unwrap();
        ids.push(row.id);
    }
    let ep = s
        .insert_episode(
            t,
            t + 15_000,
            &EpisodeContent {
                title: "Discord".into(),
                summary: "**Media:** Discord".into(),
                node_id: Some(media.id),
                model: "none".into(),
                ..Default::default()
            },
            t + 20_000,
        )
        .unwrap()
        .id;
    s.set_episode_kind(ep, oats_core::store::EPISODE_MEDIA)
        .unwrap();
    s.mark_segments_interpreted(&ids, ep, t + 20_000).unwrap();
    ep
}

#[test]
fn the_apps_table_lists_a_seen_app_and_a_class_change_writes_the_override() {
    use oats_core::ProducerClass;
    let dir = TempDir::new("apps");
    {
        let s = Store::open(&dir.db()).unwrap();
        s.note_producers_seen(
            &[
                ("com.hnc.Discord".into(), "Discord".into()),
                ("us.zoom.xos".into(), "Zoom".into()),
            ],
            now_ms(),
        )
        .unwrap();
    }
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::Preferences);
    rig.harness.run_steps(2);
    assert!(rig.shows("Discord"));
    assert!(rig.shows("Meeting 🔒"), "Zoom is locked");
    let apps = rig.app().prefs_apps().unwrap().to_vec();
    let discord = apps.iter().find(|a| a.name == "Discord").unwrap();
    assert_eq!(
        discord.effective(),
        ProducerClass::Media,
        "media by default"
    );
    assert!(apps.iter().find(|a| a.name == "Zoom").unwrap().locked);
    // pick "Meeting" in Discord's class menu (Slack's, listed after it, also says Auto (Media))
    rig.harness
        .get_all_by_value("Auto (Media)")
        .next()
        .unwrap()
        .click();
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Meeting").click();
    rig.harness.run_steps(2);
    let o = Store::open(&dir.db())
        .unwrap()
        .producer_overrides()
        .unwrap();
    assert_eq!(o.len(), 1, "{o:?}");
    assert_eq!(o[0].bundle_id, "com.hnc.discord");
    assert_eq!(o[0].class, ProducerClass::Meeting);
    let apps = rig.app().prefs_apps().unwrap().to_vec();
    assert_eq!(
        apps.iter().find(|a| a.name == "Discord").unwrap().class,
        ProducerClass::Meeting
    );
    // a locked app can't be changed
    rig.app()
        .set_app_class("us.zoom.xos", ProducerClass::Media, false);
    assert_eq!(
        Store::open(&dir.db())
            .unwrap()
            .producer_overrides()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn the_apps_table_always_lists_slack_discord_and_the_call_apps() {
    use oats_core::ProducerClass;
    let dir = TempDir::new("apps-known");
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::Preferences);
    rig.harness.run_steps(2);
    let apps = rig.app().prefs_apps().unwrap().to_vec();
    let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "FaceTime",
            "Microsoft Teams",
            "Webex",
            "Zoom",
            "Discord",
            "Slack"
        ]
    );
    assert!(apps[..4].iter().all(|a| a.locked));
    assert!(apps[4..].iter().all(|a| a.web_too && !a.locked));
    assert!(rig.shows("web + app"));
    // Discord, then Slack (alphabetical): Slack is the second "Auto (Media)" menu
    rig.harness
        .get_all_by_value("Auto (Media)")
        .nth(1)
        .unwrap()
        .click();
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Meeting").click();
    rig.harness.run_steps(2);
    let o = Store::open(&dir.db())
        .unwrap()
        .producer_overrides()
        .unwrap();
    assert_eq!(o.len(), 1, "{o:?}");
    assert_eq!(o[0].bundle_id, "com.tinyspeck.slackmacgap");
    assert_eq!(o[0].class, ProducerClass::Meeting);
    // Slack later seen playing: still one row, the override intact
    Store::open(&dir.db())
        .unwrap()
        .note_producers_seen(
            &[("com.tinyspeck.slackmacgap".into(), "Slack".into())],
            now_ms(),
        )
        .unwrap();
    rig.app()
        .set_app_class("com.tinyspeck.slackmacgap", ProducerClass::Meeting, false);
    let apps = rig.app().prefs_apps().unwrap().to_vec();
    let slack: Vec<_> = apps.iter().filter(|a| a.name == "Slack").collect();
    assert_eq!(slack.len(), 1);
    assert_eq!(slack[0].class, ProducerClass::Meeting);
    assert_eq!(apps.len(), 6);
}

#[test]
fn transcribe_anyway_shows_only_while_the_policy_keeps_no_transcript() {
    let dir = TempDir::new("anyway");
    Store::open(&dir.db())
        .unwrap()
        .note_producers_seen(&[("com.hnc.Discord".into(), "Discord".into())], now_ms())
        .unwrap();
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::Preferences);
    rig.harness.run_steps(2);
    assert!(
        !rig.shows("transcribe anyway"),
        "the default policy transcribes"
    );
    rig.harness.get_by_label("Log only").click();
    rig.harness.run_steps(2);
    assert!(rig.shows("transcribe anyway"));
    // Discord's row comes before Slack's fixed one
    rig.harness
        .get_all_by_label("transcribe anyway")
        .next()
        .unwrap()
        .click();
    rig.harness.run_steps(2);
    let o = Store::open(&dir.db())
        .unwrap()
        .producer_overrides()
        .unwrap();
    assert!(o[0].transcribe_anyway, "{o:?}");
}

#[test]
fn the_on_a_call_pill_toggles_and_stopping_turns_it_off() {
    let dir = TempDir::new("on-call");
    let mut rig = build_with(&dir.db(), None, None, true);
    rig.app().run(Command::ToggleListening);
    rig.until("listening", |app| {
        *app.status() == PipelineStatus::Listening
    });
    assert!(rig.app().system_audio_on());
    assert!(!rig.app().on_call());
    rig.harness.get_by_label("On a call").click();
    rig.harness.run_steps(2);
    assert!(rig.app().on_call());
    assert!(rig.app().status_text().contains("· on a call"));
    let checked = rig
        .app()
        .menu_model()
        .iter()
        .flat_map(|s| s.entries.clone())
        .find_map(|e| match e {
            MenuEntry::Item {
                cmd: Command::ToggleOnCall,
                checked,
                ..
            } => checked,
            _ => None,
        });
    assert_eq!(checked, Some(true));
    rig.harness.get_by_label("On a call").click();
    rig.harness.run_steps(2);
    assert!(!rig.app().on_call());
    rig.app().run(Command::ToggleOnCall);
    assert!(rig.app().on_call());
    rig.app().run(Command::ToggleListening);
    assert!(!rig.app().on_call(), "listening stopped: off");
}

#[test]
fn on_a_call_needs_system_audio() {
    let dir = TempDir::new("on-call-mic");
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ToggleListening);
    rig.until("listening", |app| {
        *app.status() == PipelineStatus::Listening
    });
    rig.app().run(Command::ToggleOnCall);
    assert!(!rig.app().on_call());
    assert!(rig.app().warning().unwrap().contains("system audio"));
}

/// A fake `claude` that interprets any transcript as "Release planning" (and logs the prompt).
fn fake_cli_logging(dir: &Path) -> ClaudeCli {
    let script = dir.join("claude");
    let body = format!(
        r#"#!/bin/sh
case " $* " in *" --version "*) echo "2.1.312 (Claude Code)"; exit 0;; esac
cat >> '{prompts}'
case "$*" in
  *low_signal*) echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.01,"duration_ms":10,"structured_output":{{"title":"Release planning","summary":"- ship Friday","low_signal":false,"node_id":null,"new_project":null,"new_topic":null,"decisions":[],"action_items":[],"topics":[],"events":[]}}}}';;
  *) echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.0,"duration_ms":10,"structured_output":{{"items":[],"actions":[]}}}}';;
esac
"#,
        prompts = dir.join("prompts").display()
    );
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

#[test]
fn mark_as_a_call_moves_a_media_note_to_conversations_and_reinterprets_it_as_a_meeting() {
    let dir = TempDir::new("mark-call");
    let ep = seed_discord_episode(&dir.db());
    let brain = Brain {
        config: BrainConfig {
            cli: fake_cli_logging(&dir.0),
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
    assert!(
        rig.app().listed_episodes().iter().all(|e| e.id != ep),
        "a media note is filed under Media, not in Conversations"
    );
    rig.app().mark_episode_as_call(ep);
    let s = Store::open(&dir.db()).unwrap();
    // at once: a conversation, unfiled (or already re-interpreted into a new one)
    if let Some(e) = s.episode(ep).unwrap() {
        assert!(!e.is_media());
        assert_eq!(e.node_id, None);
    }
    // the re-interpretation lands in Conversations
    rig.until_label("the re-interpreted note", "Release planning");
    let listed = rig.app().listed_episodes().to_vec();
    let note = listed
        .iter()
        .find(|e| e.title == "Release planning")
        .unwrap();
    assert!(!note.is_media());
    assert_eq!(note.node_id, None);
    let prompts = std::fs::read_to_string(dir.0.join("prompts")).unwrap();
    assert!(prompts.contains("meeting: true"), "the meeting hint");
    assert!(!prompts.contains("media: true"));
}

#[test]
fn mark_as_media_files_a_media_app_call_under_media() {
    let dir = TempDir::new("mark-media");
    let ep = seed_discord_episode(&dir.db());
    let s = Store::open(&dir.db()).unwrap();
    assert!(s.mark_episode_as_call(ep).unwrap());
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowNotes);
    rig.harness.run_steps(2);
    assert!(rig.app().listed_episodes().iter().any(|e| e.id == ep));
    rig.app().mark_episode_as_media(ep);
    rig.harness.run_steps(2);
    let e = s.episode(ep).unwrap().unwrap();
    assert!(e.is_media());
    assert_eq!(e.node_id, Some(s.media_category().unwrap().id));
    assert!(rig.app().listed_episodes().iter().all(|e| e.id != ep));
}

#[test]
fn a_log_only_play_marked_as_a_call_says_no_transcript_was_kept() {
    use oats_core::{MediaClass, MediaPolicy, MediaStamp};
    let dir = TempDir::new("mark-play");
    let play = {
        let s = Store::open(&dir.db()).unwrap();
        s.set_meta(MediaPolicy::META_KEY, "log").unwrap();
        let p = s
            .begin_media_play(
                now_ms() - 600_000,
                &MediaStamp::new(MediaClass::App, None, "Discord", "Discord"),
            )
            .unwrap();
        s.update_media_play(p, now_ms() - 60_000, 0).unwrap();
        p
    };
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowCalendar);
    rig.harness.run_steps(3);
    assert!(rig.shows("Listened: Discord"));
    rig.app().mark_play_as_call(play);
    rig.harness.run_steps(3);
    assert!(
        rig.app()
            .last_message()
            .unwrap()
            .contains("No transcript was kept for this (policy: log only)")
    );
    assert!(rig.shows("Call via Discord"));
}

#[test]
fn the_transcripts_view_hides_media_rows_until_show_media() {
    let dir = TempDir::new("show-media");
    seed_discord_episode(&dir.db());
    seed(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.harness.run_steps(3);
    assert!(rig.shows("loose words about the weather"));
    assert!(!rig.shows("the export ships on Friday"), "media hidden");
    rig.harness.get_by_label("Show media").click();
    rig.harness.run_steps(3);
    assert!(rig.app().prefs().show_media);
    assert!(rig.shows("the export ships on Friday"));
    rig.harness.get_by_label("Show media").click();
    rig.harness.run_steps(2);
    assert!(!rig.shows("the export ships on Friday"));
}

/// A fake `claude` that takes a while, so the busy state can be looked at.
fn slow_cli(dir: &Path) -> ClaudeCli {
    let cli = fake_cli(dir);
    let body = std::fs::read_to_string(&cli.binary).unwrap();
    let body = body.replace("cat > /dev/null", "cat > /dev/null; sleep 1");
    std::fs::write(&cli.binary, body).unwrap();
    cli
}

#[test]
fn interpret_now_greys_out_with_a_spinner_label_until_the_brain_settles() {
    let dir = TempDir::new("busy");
    seed(&dir.db());
    let brain = Brain {
        config: BrainConfig {
            cli: slow_cli(&dir.0),
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
    assert!(rig.shows("Interpret now"));
    rig.app().run(Command::InterpretNow);
    // at once: the menu item is disabled, the strip shows the busy label (not the button)
    assert_eq!(interpret_enabled(rig.harness.state()), Some(false));
    rig.harness.run_steps(2);
    assert!(
        rig.harness
            .query_by_label_contains("Interpreting…")
            .is_some()
    );
    assert!(rig.harness.query_by_label("Interpret now").is_none());
    // Review stays enabled
    assert!(rig.harness.query_by_label("Review now").is_some());
    // a second trigger does nothing (one run only)
    rig.app().run(Command::InterpretNow);
    rig.until_label("the spend", "usage 0.25 today");
    rig.until("the button to come back", |app| {
        interpret_enabled(app) == Some(true)
    });
    // the run's outcome pops up once
    assert!(
        rig.app()
            .confirmations()
            .iter()
            .any(|c| c.text.starts_with("Interpreted:") && !c.error),
        "{:?}",
        rig.app().confirmations()
    );
    rig.harness.run_steps(2);
    assert!(rig.harness.query_by_label("Interpret now").is_some());
    assert!(
        rig.harness
            .query_by_label_contains("Interpreting…")
            .is_none()
    );
}

#[test]
fn toggling_24_hour_time_in_preferences_rerenders_transcript_times() {
    let dir = TempDir::new("clock24");
    seed(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.harness.step();
    let t = now_ms() - 60_000;
    let local = chrono::Local.timestamp_millis_opt(t).single().unwrap();
    use chrono::Timelike;
    let h12 = if local.hour().is_multiple_of(12) {
        12
    } else {
        local.hour() % 12
    };
    let ap = if local.hour() < 12 { "am" } else { "pm" };
    let twelve = format!("{h12}:{:02} {ap}", local.minute());
    let twenty_four = format!("{:02}:{:02}", local.hour(), local.minute());
    rig.until_label("rows", "loose words");
    assert!(rig.shows(&twelve), "12-hour by default: {twelve}");
    rig.app().run(Command::Preferences);
    rig.harness.step();
    rig.harness.step();
    rig.harness.get_by_label("Use 24-hour time").click();
    rig.harness.step();
    rig.harness.step();
    assert!(rig.app().prefs().clock_24h);
    assert!(
        rig.shows(&twenty_four),
        "24-hour after the toggle: {twenty_four}"
    );
    assert!(!rig.shows(&twelve) || twelve.starts_with(&twenty_four));
}

// ---- move while editing, the sidebar note menu, drag-to-merge -------------------------------

fn lease_id(db: &Path) -> i64 {
    Store::open(db)
        .unwrap()
        .nodes()
        .unwrap()
        .into_iter()
        .find(|n| n.name == "Apartment lease")
        .unwrap()
        .id
}

#[test]
fn moving_a_note_while_editing_keeps_the_draft_and_save_writes_it() {
    let dir = TempDir::new("edit-move");
    let (commerce, pricing, _launch) = seed_project_notes(&dir.db());
    let lease = lease_id(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.app().selftest_view_node(commerce);
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Note: Pricing sync").click();
    rig.harness.run_steps(2);
    rig.harness.get_all_by_label("Edit").last().unwrap().click(); // the card's, not the Edit menu
    rig.harness.run_steps(2);
    let title = rig
        .harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .next()
        .unwrap();
    title.focus();
    rig.harness.run_steps(1);
    rig.harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .next()
        .unwrap()
        .type_text(" EDITED");
    rig.harness.run_steps(2);
    // "Move to…" next to Save / Cancel opens the move field (focused)
    rig.harness.get_by_label("Move to…").click();
    rig.harness.run_steps(3);
    rig.harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .last()
        .unwrap()
        .type_text("apartment");
    rig.harness.run_steps(2);
    rig.harness.key_press(egui::Key::Enter);
    rig.harness.run_steps(3);
    let s = Store::open(&dir.db()).unwrap();
    let e = s.episode(pricing).unwrap().unwrap();
    assert_eq!(e.node_id, Some(lease), "moved at once");
    assert_eq!(
        e.title, "Pricing sync",
        "the draft is not saved by the move"
    );
    assert_eq!(
        rig.app().prefs().view,
        View::Node(lease),
        "the edit followed it"
    );
    assert_eq!(rig.app().focus_episode(), Some(pricing));
    assert!(rig.shows("Save"), "still editing");
    rig.harness.get_by_label("Save").click();
    rig.harness.run_steps(3);
    let e = s.episode(pricing).unwrap().unwrap();
    assert_eq!(e.title, "Pricing sync EDITED");
    assert_eq!(e.node_id, Some(lease));
    assert!(e.user_edited_at.is_some());
}

#[test]
fn a_sidebar_note_row_has_move_to() {
    let dir = TempDir::new("sidebar-move");
    let (commerce, pricing, _launch) = seed_project_notes(&dir.db());
    let lease = lease_id(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.app().open_project_layer(commerce);
    rig.harness.run_steps(3);
    rig.harness
        .get_by_label("Note: Pricing sync")
        .click_secondary();
    rig.harness.run_steps(2);
    assert!(!rig.shows("Mark as a call"), "not a media note");
    rig.harness.get_by_label("Move to…").click();
    rig.harness.run_steps(3);
    rig.harness
        .get_by_role(egui::accesskit::Role::TextInput)
        .type_text("apartment");
    rig.harness.run_steps(2);
    rig.harness.key_press(egui::Key::Enter);
    rig.harness.run_steps(3);
    let s = Store::open(&dir.db()).unwrap();
    assert_eq!(s.episode(pricing).unwrap().unwrap().node_id, Some(lease));
    assert!(rig.shows("Moved to Work › Apartment lease"));
}

#[test]
fn a_sidebar_project_moves_under_another_category_by_name() {
    let dir = TempDir::new("sidebar-reparent");
    let (_commerce, _pricing, _launch) = seed_project_notes(&dir.db());
    let lease = lease_id(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.harness.run_steps(2);
    rig.harness
        .get_by_label("Project: Apartment lease")
        .click_secondary();
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Move to…").click();
    rig.harness.run_steps(3);
    rig.harness
        .get_by_role(egui::accesskit::Role::TextInput)
        .type_text("Personal");
    rig.harness.run_steps(2);
    rig.harness.key_press(egui::Key::Enter);
    rig.harness.run_steps(3);
    let s = Store::open(&dir.db()).unwrap();
    let personal = s
        .categories()
        .unwrap()
        .into_iter()
        .find(|c| c.name == "Personal")
        .unwrap()
        .id;
    assert_eq!(s.node(lease).unwrap().unwrap().parent_id, Some(personal));
    assert!(s.changes(5).unwrap().iter().any(|c| c.kind == "node_move"));
}

/// Give each seeded note a segment of its own (so a re-interpretation has a transcript).
fn add_note_segments(db: &Path, notes: &[(i64, &str)]) {
    let s = Store::open(db).unwrap();
    for (ep, text) in notes {
        let e = s.episode(*ep).unwrap().unwrap();
        let sid = s.begin_session(e.started_at, "seed").unwrap();
        let seg = s
            .insert_segment(sid, e.started_at, e.started_at + 5_000, text, None)
            .unwrap();
        s.mark_segments_interpreted(&[seg.id], *ep, e.started_at)
            .unwrap();
    }
}

fn drag(rig: &mut Rig, from: egui::Pos2, to: egui::Pos2) {
    let h = &rig.harness;
    h.event(egui::Event::PointerMoved(from));
    h.event(egui::Event::PointerButton {
        pos: from,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    rig.harness.step();
    for i in 1..=6 {
        let p = from + (to - from) * (i as f32 / 6.0);
        rig.harness.event(egui::Event::PointerMoved(p));
        rig.harness.step();
    }
    rig.harness.event(egui::Event::PointerButton {
        pos: to,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    // the pop-up's first frame is egui's sizing pass: let it settle before it is queried
    rig.harness.run_steps(3);
}

#[test]
fn dropping_a_note_on_another_asks_then_merges_and_reinterprets_the_target() {
    let dir = TempDir::new("drag-merge");
    let (commerce, pricing, launch) = seed_project_notes(&dir.db());
    add_note_segments(
        &dir.db(),
        &[
            (pricing, "the price table needs a VAT column"),
            (launch, "we book the launch room for Thursday"),
        ],
    );
    let brain = Brain {
        config: BrainConfig {
            cli: fake_cli_logging(&dir.0),
            ..BrainConfig::default()
        },
        store_path: dir.db(),
        wake: Box::new(|| {}),
    }
    .spawn();
    let mut rig = build(&dir.db(), None, Some(brain));
    rig.app().selftest_view_node(commerce);
    rig.until("the brain to report in", |app| {
        interpret_enabled(app) == Some(true)
    });
    rig.harness.run_steps(3);
    let from = rig
        .harness
        .get_by_label("Note: Launch review")
        .rect()
        .center();
    let to = rig
        .harness
        .get_by_label("Note: Pricing sync")
        .rect()
        .center();
    drag(&mut rig, from, to);
    assert!(
        rig.shows("Merge “Launch review” into “Pricing sync”?"),
        "asks before merging"
    );
    let s = Store::open(&dir.db()).unwrap();
    assert!(s.episode(launch).unwrap().is_some(), "nothing merged yet");
    // No: nothing happens
    rig.harness.get_by_label("No").click();
    rig.harness.run_steps(2);
    assert!(!rig.shows("Undo not available"));
    assert!(s.episode(launch).unwrap().is_some());
    // again, Yes
    drag(&mut rig, from, to);
    rig.harness.get_by_label("Yes").click();
    rig.harness.run_steps(2);
    assert!(
        s.episode(launch).unwrap().is_none(),
        "the dragged note is gone"
    );
    // (the segments' re-pointing is checked in `merge::tests`: the brain may already be
    // re-interpreting here)
    assert!(
        s.changes(5)
            .unwrap()
            .iter()
            .any(|c| c.kind == "episode_merge")
    );
    assert!(rig.shows("Merged into Pricing sync — undo not available"));
    // the target is re-interpreted with both transcripts
    let prompts = dir.0.join("prompts");
    rig.until("the re-interpretation", |_| {
        std::fs::read_to_string(&prompts)
            .is_ok_and(|p| p.contains("VAT column") && p.contains("launch room"))
    });
    // in place: the same note, revised, not split apart again by the 30 min between the parts
    rig.until("the rewrite to land", |_| {
        s.episode(pricing)
            .unwrap()
            .is_some_and(|e| e.title == "Release planning")
    });
    assert_eq!(s.segments_of_episode(pricing).unwrap().len(), 2);
}

#[test]
fn merging_into_a_hand_edited_note_asks_to_replace_the_edits() {
    let dir = TempDir::new("merge-edited");
    let (commerce, pricing, launch) = seed_project_notes(&dir.db());
    let s = Store::open(&dir.db()).unwrap();
    let mut content = oats_core::EpisodeContent {
        title: "Pricing sync (mine)".into(),
        ..Default::default()
    };
    content.summary = "- my own words".into();
    s.update_episode_content(pricing, &content, now_ms())
        .unwrap();
    let mut rig = build(&dir.db(), None, None);
    rig.app().selftest_view_node(commerce);
    rig.harness.run_steps(3);
    let from = rig
        .harness
        .get_by_label("Note: Launch review")
        .rect()
        .center();
    let to = rig
        .harness
        .get_by_label("Note: Pricing sync (mine)")
        .rect()
        .center();
    drag(&mut rig, from, to);
    rig.harness.get_by_label("Yes").click();
    rig.harness.run_steps(2);
    assert!(rig.shows("Replace your edits?"));
    assert!(s.episode(launch).unwrap().is_some(), "still nothing merged");
    rig.harness.get_by_label("Keep my text").click();
    rig.harness.run_steps(2);
    assert!(s.episode(launch).unwrap().is_none());
    let e = s.episode(pricing).unwrap().unwrap();
    assert_eq!(e.title, "Pricing sync (mine)", "kept");
    assert!(rig.app().warning().is_none(), "no re-interpret was sent");
}

// ---- the Media view (watch history) and tag search ------------------------------------------

/// Three plays: one YouTube video watched in two sittings 5 min apart (folded into one row),
/// a Spotify play. Returns `(the folded pair, spotify)`.
fn seed_plays(db: &Path) -> ([i64; 2], i64) {
    use oats_core::{MediaClass, MediaStamp};
    let s = Store::open(db).unwrap();
    let now = now_ms();
    let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
    let yt = MediaStamp::new(
        MediaClass::YouTube,
        Some(url.into()),
        "Never gonna give you up",
        "Firefox",
    );
    let a = s.begin_media_play(now - 3_600_000, &yt).unwrap();
    s.update_media_play(a, now - 3_000_000, 0).unwrap();
    let b = s.begin_media_play(now - 2_700_000, &yt).unwrap();
    s.update_media_play(b, now - 2_400_000, 0).unwrap();
    let sp = s
        .begin_media_play(
            now - 600_000,
            &MediaStamp::new(MediaClass::Music, None, "Deep focus", "Spotify"),
        )
        .unwrap();
    s.update_media_play(sp, now - 60_000, 0).unwrap();
    ([a, b], sp)
}

#[test]
fn the_media_view_says_nothing_watched_yet_when_empty() {
    let dir = TempDir::new("media-empty");
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::ShowMediaView);
    rig.harness.run_steps(3);
    assert_eq!(rig.app().prefs().view, View::Media);
    assert!(rig.shows("Nothing watched yet."));
}

#[test]
fn the_media_view_lists_plays_folded_and_open_in_browser_opens_the_url() {
    let dir = TempDir::new("media-view");
    let ([a, b], sp) = seed_plays(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.harness.run_steps(2);
    // ⌘6 is the view's shortcut (the in-window menu: the key monitor runs it)
    rig.harness
        .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Num6);
    rig.harness.run_steps(3);
    assert_eq!(rig.app().prefs().view, View::Media);
    assert_eq!(
        rig.app().media_rows(),
        [
            ("Deep focus".to_owned(), vec![sp]),
            ("Never gonna give you up".to_owned(), vec![b, a]),
        ],
        "newest first; the two sittings are one row"
    );
    assert!(rig.shows("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
    // right-click → Open in browser: the app asks egui to open the URL
    rig.harness
        .get_by_label("Never gonna give you up")
        .click_secondary();
    rig.harness.run_steps(2);
    assert!(rig.shows("Copy link"));
    assert!(rig.shows("Mark as a call"));
    assert!(!rig.shows("Show transcript"), "nothing was transcribed");
    rig.harness.get_by_label("Open in browser").click();
    rig.harness.run_steps(2);
    assert_eq!(
        rig.app().last_opened_url(),
        Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
    );
    // Delete removes the whole folded row
    rig.harness
        .get_by_label("Never gonna give you up")
        .click_secondary();
    rig.harness.run_steps(2);
    rig.harness.get_by_label("Delete").click();
    rig.harness.run_steps(3);
    assert_eq!(
        rig.app().media_rows(),
        [("Deep focus".to_owned(), vec![sp])]
    );
    let s = Store::open(&dir.db()).unwrap();
    assert!(s.media_play(a).unwrap().is_none() && s.media_play(b).unwrap().is_none());
}

#[test]
fn a_media_search_hit_opens_the_media_view_at_its_row() {
    let dir = TempDir::new("media-search");
    let ([_, b], _) = seed_plays(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::Search);
    rig.harness.run_steps(2);
    rig.harness
        .event(egui::Event::Text("gonna give".to_owned()));
    rig.harness.run_steps(3);
    assert!(rig.shows("2 plays"));
    rig.harness
        .get_all_by_label_contains("Never gonna give you up")
        .next()
        .unwrap()
        .click();
    rig.harness.run_steps(1);
    assert!(!rig.app().search_open());
    assert_eq!(rig.app().prefs().view, View::Media);
    rig.harness.run_steps(3);
    assert!(rig.app().pending_media_focus().is_none_or(|p| p == b));
}

/// Notes tagged around a commerce project.
fn seed_tagged(db: &Path) {
    use oats_core::EpisodeContent;
    let s = Store::open(db).unwrap();
    let now = now_ms();
    for (i, (title, topics)) in [
        ("Commerce checkout review", vec!["Commerce", "COD delay"]),
        ("Courier escalation", vec!["COD delay"]),
        ("Weekly sync", vec!["commerce"]),
    ]
    .into_iter()
    .enumerate()
    {
        let at = now - (i as i64 + 1) * 3_600_000;
        s.insert_episode(
            at,
            at + 600_000,
            &EpisodeContent {
                title: title.into(),
                summary: "- seeded".into(),
                topics: topics.into_iter().map(str::to_owned).collect(),
                ..Default::default()
            },
            now,
        )
        .unwrap();
    }
}

#[test]
fn search_shows_tags_first_and_a_tag_opens_its_topic_view() {
    let dir = TempDir::new("tag-search");
    seed_tagged(&dir.db());
    let mut rig = build(&dir.db(), None, None);
    rig.app().run(Command::Search);
    rig.harness.run_steps(2);
    rig.harness.event(egui::Event::Text("commerce".to_owned()));
    rig.harness.run_steps(3);
    // a note title matches too: the Tags section sits above the Notes section
    let tags = rig.harness.get_by_label("Tags").rect();
    let notes = rig.harness.get_by_label("Notes").rect();
    assert!(tags.top() < notes.top(), "Tags first");
    assert!(rig.shows("Commerce · 2"));
    // several words: any of them, the better match first
    rig.harness
        .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    rig.harness
        .event(egui::Event::Text("commerce cod delay".to_owned()));
    rig.harness.run_steps(3);
    let cod = rig.harness.get_by_label("COD delay · 2").rect();
    let commerce = rig.harness.get_by_label("Commerce · 2").rect();
    assert!(cod.left() < commerce.left(), "two words beat one");
    rig.harness.get_by_label("Commerce · 2").click();
    rig.harness.run_steps(3);
    assert!(!rig.app().search_open());
    assert!(rig.app().topic_open());
    assert!(rig.shows("Tagged “Commerce”"));
    assert!(
        rig.shows("Weekly sync"),
        "case-insensitive: the lower-case tag too"
    );
}

fn last_request(rig: &Rig) -> AudioRequest {
    *rig.requests.borrow().last().expect("a request")
}

#[test]
fn the_mic_and_system_pills_are_independent_lanes() {
    let dir = TempDir::new("lanes");
    let mut rig = build_with(&dir.db(), None, None, true);
    // Mic alone
    rig.harness.get_by_label("Mic").click();
    rig.harness.run_steps(2);
    assert!(rig.app().is_listening());
    assert_eq!(
        last_request(&rig),
        AudioRequest {
            mic: true,
            system: false
        }
    );
    rig.until("listening", |app| {
        *app.status() == PipelineStatus::Listening
    });
    assert_eq!(rig.app().state_label(), "listening · mic");
    assert_eq!(
        rig.app().status_item_state().tooltip,
        "OatsNotes — Listening · mic · 0 segments today"
    );
    // add System: restart with both
    rig.harness.get_by_label("System audio").click();
    rig.until("both lanes", |app| {
        app.is_listening() && app.system_audio_on() && *app.status() == PipelineStatus::Listening
    });
    assert_eq!(
        last_request(&rig),
        AudioRequest {
            mic: true,
            system: true
        }
    );
    assert_eq!(rig.app().state_label(), "listening · mic + system");
    // drop Mic: system only, and the starter is told not to open the microphone
    rig.harness.get_by_label("Mic").click();
    rig.until("system only", |app| {
        app.is_listening() && app.state_label() == "listening · system"
    });
    assert_eq!(
        last_request(&rig),
        AudioRequest {
            mic: false,
            system: true
        }
    );
    // the last lane off stops listening
    rig.harness.get_by_label("System audio").click();
    rig.harness.run_steps(2);
    assert!(!rig.app().is_listening());
    assert!(!rig.app().prefs().listening);
    rig.until("idle", |app| *app.status() == PipelineStatus::Idle);
    assert_eq!(rig.app().state_label(), "idle");
    // System alone from idle
    rig.harness.get_by_label("System audio").click();
    rig.harness.run_steps(2);
    assert_eq!(
        last_request(&rig),
        AudioRequest {
            mic: false,
            system: true
        }
    );
    assert!(rig.app().prefs().lanes.system && !rig.app().prefs().lanes.mic);
    // the master switch stops, then restores the remembered set; a fresh default is both
    rig.app().run(Command::ToggleListening);
    assert!(!rig.app().is_listening());
    rig.until("idle", |app| *app.status() == PipelineStatus::Idle);
    rig.app().run(Command::ToggleListening);
    assert_eq!(
        last_request(&rig),
        AudioRequest {
            mic: false,
            system: true
        }
    );
}

#[test]
fn the_master_switch_turns_both_lanes_on() {
    let dir = TempDir::new("lanes-master");
    let mut rig = build_with(&dir.db(), None, None, true);
    rig.app().run(Command::ToggleListening);
    assert_eq!(
        last_request(&rig),
        AudioRequest {
            mic: true,
            system: true
        }
    );
    rig.until("listening", |app| {
        *app.status() == PipelineStatus::Listening
    });
    assert_eq!(rig.app().state_label(), "listening · mic + system");
    rig.app().run(Command::ToggleListening);
    assert!(!rig.app().is_listening());
}
