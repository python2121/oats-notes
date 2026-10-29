//! The brain's worker end to end against a fake `claude`: the budget guards (daily cap and
//! hourly run cap pause tiers 1–3, never tier 0), the new-node cap applied by a real
//! interpretation run, and the undo / re-interpret commands.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oats_brain::{Brain, BrainCommand, BrainConfig, BrainEvent, BrainHandle, ClaudeCli, Tier};
use oats_core::{EpisodeContent, NodeKind, Origin, RunOutcome, Store, TranscriptRow, View};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "oats-brain-sched-it-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
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

/// Answers by schema; appends the model of every real request to `calls`. The interpretation
/// files under Work (id 1) and proposes a new project and topic.
fn fake_cli(dir: &Path) -> ClaudeCli {
    let script = dir.join("claude");
    let body = format!(
        r#"#!/bin/sh
case " $* " in *" --version "*) echo "2.1.312 (Claude Code)"; exit 0;; esac
cat > /dev/null
prev=""; model=""
for a in "$@"; do [ "$prev" = "--model" ] && model="$a"; prev="$a"; done
echo "$model" >> '{calls}'
case "$*" in
  *merge_episodes*) echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.003,"duration_ms":10,"structured_output":{{"actions":[]}}}}';;
  *merge_nodes*) echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.004,"duration_ms":10,"structured_output":{{"actions":[]}}}}';;
  *low_signal*) echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.01,"duration_ms":10,"structured_output":{{"title":"Exporter planning","summary":"- ship","low_signal":false,"node_id":1,"new_project":{{"parent_id":1,"name":"Exporter","description":""}},"new_topic":{{"parent_id":null,"name":"CSV","description":""}},"decisions":[],"action_items":[],"topics":[],"events":[]}}}}';;
  *intent*) echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.002,"duration_ms":10,"structured_output":{{"actions":[{{"intent":"add_note","title":null,"date":null,"time":null,"all_day":false,"relative_to_event":false,"offset_days":null,"text":"buy milk","details":null,"node_name":null,"category_name":null}}]}}}}';;
  *segment_id*) echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.001,"duration_ms":10,"structured_output":{{"items":[]}}}}';;
  *) echo '{{"type":"result","is_error":true,"result":"unexpected request"}}';;
esac
"#,
        calls = dir.join("calls").display()
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

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn spawn(dir: &Path, config: BrainConfig) -> BrainHandle {
    let h = Brain {
        config: BrainConfig {
            cli: fake_cli(dir),
            ..config
        },
        store_path: dir.join("oats.sqlite"),
        wake: Box::new(|| {}),
    }
    .spawn();
    let mut seen = Vec::new();
    wait_for(&h, &mut seen, "availability", |e| {
        matches!(e, BrainEvent::Availability(Ok(_)))
    });
    h
}

fn wait_for(
    h: &BrainHandle,
    seen: &mut Vec<BrainEvent>,
    what: &str,
    pred: impl Fn(&BrainEvent) -> bool,
) -> BrainEvent {
    let deadline = Instant::now() + Duration::from_secs(15);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match h.recv_timeout(left) {
            Some(ev) => {
                seen.push(ev.clone());
                if pred(&ev) {
                    return ev;
                }
            }
            None => break,
        }
    }
    panic!("timed out waiting for {what}; saw {seen:?}");
}

/// Drain whatever arrives within `d`.
fn drain(h: &BrainHandle, seen: &mut Vec<BrainEvent>, d: Duration) {
    let deadline = Instant::now() + d;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match h.recv_timeout(left) {
            Some(ev) => seen.push(ev),
            None => break,
        }
    }
}

const LONG: &str = "Let's ship the exporter on Friday; Sam writes the CSV tests and we review on Monday before the customer call.";

/// A store with conversation to interpret, an episode today and a project (so every tier has
/// work), plus whatever `seed` adds.
fn seeded(dir: &Path, seed: impl FnOnce(&Store)) -> TranscriptRow {
    let s = Store::open(&dir.join("oats.sqlite")).unwrap();
    let work = s.categories().unwrap()[0].id;
    s.add_node(
        Some(work),
        NodeKind::Project,
        "Old project",
        "",
        Origin::Auto,
    )
    .unwrap();
    let sid = s.begin_session(0, "e").unwrap();
    let t = now_ms() - 2 * 3_600_000;
    let talk = s.insert_segment(sid, t, t + 5_000, LONG, None).unwrap();
    for (i, title) in ["Earlier", "Earlier still"].iter().enumerate() {
        let start = now_ms() - 10_000 - i as i64 * 1_000;
        s.insert_episode(
            start,
            start + 500,
            &EpisodeContent {
                title: (*title).into(),
                ..Default::default()
            },
            start,
        )
        .unwrap();
    }
    seed(&s);
    talk
}

/// A hotword segment, stored the moment it is needed (one already in the store would be
/// routed to tier 0 by the next interpret run on its own).
fn hot_segment(dir: &Path) -> TranscriptRow {
    let s = Store::open(&dir.join("oats.sqlite")).unwrap();
    let t = now_ms() - 60_000;
    s.insert_segment(1, t, t + 5_000, "hey computer note to buy milk", None)
        .unwrap()
}

fn paid_run(s: &Store, tier: i64, kind: &str, cost: f64) {
    let at = now_ms() - 1_000;
    let id = s.begin_run(tier, kind, "sonnet", at).unwrap();
    s.finish_run(
        id,
        &RunOutcome {
            finished_at: at + 10,
            cost_usd: Some(cost),
            ok: true,
            ..Default::default()
        },
    )
    .unwrap();
}

/// Ask every tier 1–3 job to run now, give the worker time to look at them, then send a hotword
/// command and wait for it to finish.
fn ask_everything_then_a_command(
    h: &BrainHandle,
    seen: &mut Vec<BrainEvent>,
    dir: &Path,
) -> TranscriptRow {
    h.send(BrainCommand::InterpretNow);
    h.send(BrainCommand::ReviewNow);
    h.send(BrainCommand::ReorganiseNow);
    drain(h, seen, Duration::from_millis(500));
    let hot = hot_segment(dir);
    h.segment_arrived(hot.clone());
    wait_for(
        h,
        seen,
        "the command run",
        |e| matches!(e, BrainEvent::RunFinished(r) if r.kind == "command"),
    );
    drain(h, seen, Duration::from_millis(200));
    hot
}

fn started_tiers(seen: &[BrainEvent]) -> Vec<Tier> {
    seen.iter()
        .filter_map(|e| match e {
            BrainEvent::RunStarted { tier, .. } => Some(*tier),
            _ => None,
        })
        .collect()
}

#[test]
fn the_daily_cap_pauses_tiers_1_to_3_but_not_commands() {
    let dir = TempDir::new("budget");
    seeded(&dir.0, |s| paid_run(s, 1, "interpret", 2.50));
    let h = spawn(
        &dir.0,
        BrainConfig {
            daily_budget_usd: 2.0,
            ..BrainConfig::default()
        },
    );
    let mut seen = Vec::new();
    let hot = ask_everything_then_a_command(&h, &mut seen, &dir.0);
    assert_eq!(started_tiers(&seen), vec![Tier::Reactive], "{seen:?}");
    let warnings: Vec<&String> = seen
        .iter()
        .filter_map(|e| match e {
            BrainEvent::Warning(w) => Some(w),
            _ => None,
        })
        .collect();
    assert_eq!(warnings.len(), 1, "warned once, not per tier: {warnings:?}");
    assert!(
        warnings[0].contains("daily brain budget reached"),
        "{warnings:?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, BrainEvent::Command { segment_id, .. } if *segment_id == hot.id)),
        "{seen:?}"
    );
    // the spend reported after the command includes the seeded runs
    assert!(
        seen.iter()
            .any(|e| matches!(e, BrainEvent::Spend { today_usd } if *today_usd >= 2.5))
    );
    drop(h);
    assert_eq!(
        calls(&dir.0),
        vec!["haiku".to_owned()],
        "only the command reached claude"
    );
    let s = Store::open(&dir.0.join("oats.sqlite")).unwrap();
    assert!(
        s.episodes(10)
            .unwrap()
            .iter()
            .all(|e| e.title.starts_with("Earlier"))
    );
    assert_eq!(s.notes(10).unwrap().len(), 1);
}

#[test]
fn the_hourly_run_cap_pauses_tiers_1_to_3() {
    let dir = TempDir::new("hourly");
    seeded(&dir.0, |s| {
        paid_run(s, 0, "categorise", 0.001);
        paid_run(s, 0, "categorise", 0.001);
    });
    let h = spawn(
        &dir.0,
        BrainConfig {
            daily_budget_usd: 100.0,
            max_runs_per_hour: 2,
            ..BrainConfig::default()
        },
    );
    let mut seen = Vec::new();
    ask_everything_then_a_command(&h, &mut seen, &dir.0);
    assert_eq!(started_tiers(&seen), vec![Tier::Reactive], "{seen:?}");
    assert!(
        seen.iter()
            .any(|e| matches!(e, BrainEvent::Warning(w) if w.contains("in the last hour"))),
        "{seen:?}"
    );
    drop(h);
    assert_eq!(calls(&dir.0), vec!["haiku".to_owned()]);
}

#[test]
fn within_budget_every_tier_runs_with_its_model() {
    let dir = TempDir::new("all-tiers");
    seeded(&dir.0, |_| {});
    let h = spawn(&dir.0, BrainConfig::default());
    let mut seen = Vec::new();
    h.send(BrainCommand::InterpretNow);
    wait_for(
        &h,
        &mut seen,
        "interpret",
        |e| matches!(e, BrainEvent::RunFinished(r) if r.kind == "interpret"),
    );
    h.send(BrainCommand::ReviewNow);
    wait_for(
        &h,
        &mut seen,
        "review",
        |e| matches!(e, BrainEvent::RunFinished(r) if r.kind == "review"),
    );
    h.send(BrainCommand::ReorganiseNow);
    wait_for(
        &h,
        &mut seen,
        "reorganise",
        |e| matches!(e, BrainEvent::RunFinished(r) if r.kind == "reorganise"),
    );
    h.segment_arrived(hot_segment(&dir.0));
    wait_for(
        &h,
        &mut seen,
        "command",
        |e| matches!(e, BrainEvent::RunFinished(r) if r.kind == "command"),
    );
    drop(h);
    let models = calls(&dir.0);
    for m in ["sonnet", "opus", "haiku"] {
        assert!(models.contains(&m.to_owned()), "{m} in {models:?}");
    }
    let s = Store::open(&dir.0.join("oats.sqlite")).unwrap();
    let runs = s.runs(20).unwrap();
    for (kind, tier) in [
        ("interpret", 1),
        ("review", 2),
        ("reorganise", 3),
        ("command", 0),
    ] {
        let r = runs
            .iter()
            .find(|r| r.kind == kind)
            .unwrap_or_else(|| panic!("{kind}: {runs:?}"));
        assert_eq!(r.tier, tier, "{kind}");
        assert!(r.ok, "{r:?}");
    }
}

#[test]
fn an_interpretation_run_respects_the_new_node_cap() {
    let dir = TempDir::new("sprawl");
    let talk = seeded(&dir.0, |_| {});
    let h = spawn(
        &dir.0,
        BrainConfig {
            max_new_nodes_per_run: 1,
            ..BrainConfig::default()
        },
    );
    let mut seen = Vec::new();
    h.send(BrainCommand::InterpretNow);
    let ev = wait_for(
        &h,
        &mut seen,
        "interpret",
        |e| matches!(e, BrainEvent::RunFinished(r) if r.kind == "interpret"),
    );
    let BrainEvent::RunFinished(run) = ev else {
        unreachable!()
    };
    assert!(run.ok, "{run:?}");
    let s = Store::open(&dir.0.join("oats.sqlite")).unwrap();
    let nodes = s.nodes().unwrap();
    let exporter = nodes
        .iter()
        .find(|n| n.name == "Exporter")
        .expect("the project");
    assert!(
        !nodes.iter().any(|n| n.name == "CSV"),
        "the topic is over the cap: {nodes:?}"
    );
    let ep = s.segment(talk.id).unwrap().unwrap().episode_id.unwrap();
    assert_eq!(s.episode(ep).unwrap().unwrap().node_id, Some(exporter.id));

    // undo through the worker: the node_create change can't be undone; a rename can
    let created = s
        .changes(10)
        .unwrap()
        .into_iter()
        .find(|c| c.kind == "node_create")
        .unwrap();
    h.send(BrainCommand::UndoChange(created.id));
    wait_for(
        &h,
        &mut seen,
        "undo refused",
        |e| matches!(e, BrainEvent::Warning(w) if w.contains("cannot be undone")),
    );
    s.rename_node(exporter.id, "Exporter v2").unwrap();
    let rename = s
        .record_change(
            None,
            "node_rename",
            "Renamed",
            &serde_json::json!({"node_id": exporter.id, "name": "Exporter"}),
            &serde_json::json!({"node_id": exporter.id, "name": "Exporter v2"}),
            now_ms(),
        )
        .unwrap();
    h.send(BrainCommand::UndoChange(rename));
    wait_for(
        &h,
        &mut seen,
        "undo",
        |e| matches!(e, BrainEvent::Changed(lines) if lines.iter().any(|l| l.contains("Undid"))),
    );
    assert_eq!(s.node(exporter.id).unwrap().unwrap().name, "Exporter");

    // re-interpret: the episode is forgotten and interpreted again (a fresh row)
    h.send(BrainCommand::Reinterpret(ep));
    wait_for(
        &h,
        &mut seen,
        "re-interpret",
        |e| matches!(e, BrainEvent::RunFinished(r) if r.kind == "interpret"),
    );
    drop(h);
    let again = s.segment(talk.id).unwrap().unwrap();
    let fresh = s.episode(again.episode_id.unwrap()).unwrap().unwrap();
    assert_eq!(
        fresh.revision, 1,
        "a new interpretation, not a revision: {fresh:?}"
    );
    assert!(seen.iter().any(
        |e| matches!(e, BrainEvent::Changed(l) if l.iter().any(|l| l.contains("re-interpretation")))
    ));
    assert_eq!(
        s.nodes()
            .unwrap()
            .iter()
            .filter(|n| n.name == "Exporter")
            .count(),
        1,
        "the project is reused, not duplicated"
    );
    assert_eq!(s.segments(View::Transcripts).unwrap().len(), 1);
}
