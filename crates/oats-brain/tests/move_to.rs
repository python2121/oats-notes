//! "Move to…" end to end against a fake `claude`: ambiguous text → the model picks an existing
//! node; a new-project request → one project created under the named category (the user's
//! own), a `node_create` change, and the item moved; a bad node id → a warning, nothing moved.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oats_brain::{
    Brain, BrainCommand, BrainConfig, BrainEvent, BrainHandle, ClaudeCli, MoveTarget,
};
use oats_core::{EpisodeContent, NodeKind, Origin, Store};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "oats-brain-move-{tag}-{}-{nanos}",
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

/// Answers by what the prompt says: "lease renewal" → create a project under `admin`; "bogus"
/// → an unknown node; anything else → the existing node `pick`.
fn fake_cli(dir: &Path, pick: i64, admin: i64) -> ClaudeCli {
    let script = dir.join("claude");
    let body = format!(
        r#"#!/bin/sh
case " $* " in *" --version "*) echo "2.1.312 (Claude Code)"; exit 0;; esac
in=$(cat)
echo call >> '{calls}'
case "$in" in
  *"lease renewal"*) echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.001,"duration_ms":10,"structured_output":{{"action":"create","node_id":null,"new_project":{{"name":"Apartment lease renewal","parent_id":{admin},"description":"Renewing the lease"}},"reason":"new"}}}}';;
  *bogus*) echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.001,"duration_ms":10,"structured_output":{{"action":"existing","node_id":9999,"new_project":null,"reason":"x"}}}}';;
  *) echo '{{"type":"result","is_error":false,"result":"","total_cost_usd":0.001,"duration_ms":10,"structured_output":{{"action":"existing","node_id":{pick},"new_project":null,"reason":"fits"}}}}';;
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

fn wait_for(
    h: &BrainHandle,
    seen: &mut Vec<BrainEvent>,
    what: &str,
    pred: impl Fn(&BrainEvent) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match h.recv_timeout(left) {
            Some(ev) => {
                seen.push(ev.clone());
                if pred(&ev) {
                    return;
                }
            }
            None => break,
        }
    }
    panic!("timed out waiting for {what}; saw {seen:?}");
}

struct Rig {
    dir: TempDir,
    h: BrainHandle,
    store: Store,
    work: i64,
    commerce: i64,
    pricing: i64,
    episode: i64,
    segment: i64,
}

fn rig(tag: &str) -> Rig {
    let dir = TempDir::new(tag);
    let db = dir.0.join("oats.sqlite");
    let store = Store::open(&db).unwrap();
    let cats = store.categories().unwrap();
    let work = cats.iter().find(|c| c.name == "Work").unwrap().id;
    let admin = cats.iter().find(|c| c.name == "Admin").unwrap().id;
    let commerce = store
        .add_node(Some(work), NodeKind::Project, "Commerce", "", Origin::Auto)
        .unwrap()
        .id;
    let pricing = store
        .add_node(Some(commerce), NodeKind::Topic, "Pricing", "", Origin::Auto)
        .unwrap()
        .id;
    let t = 1_700_000_000_000;
    let episode = store
        .insert_episode(
            t,
            t + 1000,
            &EpisodeContent {
                title: "Lease call".into(),
                ..Default::default()
            },
            t,
        )
        .unwrap()
        .id;
    let sid = store.begin_session(0, "e").unwrap();
    let segment = store
        .insert_segment(sid, t, t + 5000, "about the lease", None)
        .unwrap()
        .id;
    let h = Brain {
        config: BrainConfig {
            cli: fake_cli(&dir.0, pricing, admin),
            ..BrainConfig::default()
        },
        store_path: db,
        wake: Box::new(|| {}),
    }
    .spawn();
    let mut seen = Vec::new();
    wait_for(&h, &mut seen, "availability", |e| {
        matches!(e, BrainEvent::Availability(Ok(_)))
    });
    Rig {
        dir,
        h,
        store,
        work,
        commerce,
        pricing,
        episode,
        segment,
    }
}

fn settled(r: &Rig, kind: &'static str, id: i64) -> Vec<BrainEvent> {
    let mut seen = Vec::new();
    wait_for(
        &r.h,
        &mut seen,
        "settled",
        |e| matches!(e, BrainEvent::Settled { kind: k, id: Some(i) } if k == kind && *i == id),
    );
    seen
}

#[test]
fn ambiguous_text_lets_the_model_pick_an_existing_node() {
    let r = rig("existing");
    r.h.send(BrainCommand::ResolveMove {
        target: MoveTarget::Episode(r.episode),
        text: "pricing".into(),
        current_node: None,
    });
    let seen = settled(&r, "move", r.episode);
    assert_eq!(
        r.store.episode(r.episode).unwrap().unwrap().node_id,
        Some(r.pricing)
    );
    let said = seen.iter().any(|e| {
        matches!(e, BrainEvent::Command { confirmations, .. }
            if confirmations == &vec!["Moved to Work › Commerce › Pricing".to_owned()])
    });
    assert!(said, "{seen:?}");
    assert!(
        r.store
            .changes(10)
            .unwrap()
            .iter()
            .any(|c| c.kind == "episode_move")
    );
    let _ = (&r.dir, r.work, r.commerce);
}

#[test]
fn a_new_project_request_creates_it_under_the_named_category_and_moves_the_item() {
    let r = rig("create");
    r.h.send(BrainCommand::ResolveMove {
        target: MoveTarget::Segment(r.segment),
        text: "new project for the lease renewal under admin".into(),
        current_node: Some(r.commerce),
    });
    let seen = settled(&r, "move_segment", r.segment);
    let admin = r
        .store
        .categories()
        .unwrap()
        .into_iter()
        .find(|c| c.name == "Admin")
        .unwrap();
    let p = r
        .store
        .find_node(Some(admin.id), "Apartment lease renewal")
        .unwrap()
        .expect("project created");
    assert_eq!(p.created_by, Origin::User);
    assert_eq!(p.kind, NodeKind::Project);
    assert_eq!(
        r.store.segment(r.segment).unwrap().unwrap().node_id,
        Some(p.id)
    );
    assert!(
        r.store
            .changes(10)
            .unwrap()
            .iter()
            .any(|c| c.kind == "node_create")
    );
    let said = seen.iter().any(|e| {
        matches!(e, BrainEvent::Command { confirmations, .. }
            if confirmations == &vec!["Created Admin › Apartment lease renewal and moved the segment".to_owned()])
    });
    assert!(said, "{seen:?}");
}

#[test]
fn an_unknown_node_id_is_a_warning_and_nothing_moves() {
    let r = rig("bogus");
    r.h.send(BrainCommand::ResolveMove {
        target: MoveTarget::Episode(r.episode),
        text: "bogus".into(),
        current_node: None,
    });
    let seen = settled(&r, "move", r.episode);
    assert!(
        seen.iter().any(|e| matches!(e, BrainEvent::Warning(_))),
        "{seen:?}"
    );
    assert_eq!(r.store.episode(r.episode).unwrap().unwrap().node_id, None);
}
