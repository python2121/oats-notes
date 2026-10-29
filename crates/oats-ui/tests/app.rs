//! The whole `OatsApp` driven headlessly through `egui_kittest` (no GPU: nothing is rendered,
//! the AccessKit tree is queried): a real pipeline on the null engine fed through the
//! `AudioStarter`'s sender, a real brain against a fake `claude`, a fake status item.
#![cfg(feature = "selftest")]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

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
        self.harness.query_by_label_contains(label).is_some()
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
