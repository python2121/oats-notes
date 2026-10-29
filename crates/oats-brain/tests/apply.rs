//! Applying canned model output to a store: the anti-sprawl rules of interpretation filing
//! (the new-node cap, name reuse, fallbacks for bad parents), event reconciliation, and
//! `undo_change` for every reversible change kind.

use oats_brain::episodes::Chunk;
use oats_brain::interpret::{
    self, Action, Actions, Interpretation, NodeProposal, ProposedEvent, apply_interpretation,
    undo_change,
};
use oats_core::{EpisodeContent, Node, NodeKind, Origin, Store, TranscriptRow};
use serde_json::json;

struct Fixture {
    s: Store,
    work: i64,
    admin: i64,
    next_t: i64,
    sid: i64,
}

fn fixture() -> Fixture {
    let s = Store::open_in_memory().unwrap();
    let cats = s.categories().unwrap();
    let (work, admin) = (cats[0].id, cats[1].id);
    let sid = s.begin_session(0, "e").unwrap();
    Fixture {
        s,
        work,
        admin,
        next_t: 1_000_000,
        sid,
    }
}

impl Fixture {
    /// A fresh one-segment chunk later than every other.
    fn chunk(&mut self) -> Chunk {
        let t = self.next_t;
        self.next_t += 3_600_000;
        let row: TranscriptRow = self
            .s
            .insert_segment(self.sid, t, t + 5_000, "we talked about the exporter", None)
            .unwrap();
        Chunk {
            started_at: t,
            ended_at: t + 5_000,
            segments: vec![row],
        }
    }

    fn tree(&self) -> Vec<Node> {
        self.s.nodes().unwrap()
    }

    fn apply(&mut self, interp: &Interpretation, max_new: usize) -> interpret::Applied {
        let chunk = self.chunk();
        let tree = self.tree();
        apply_interpretation(
            &self.s, &tree, &chunk, interp, None, "m", None, None, max_new, 1,
        )
        .unwrap()
    }

    fn project(&self, parent: i64, name: &str) -> Node {
        self.s
            .add_node(Some(parent), NodeKind::Project, name, "", Origin::Auto)
            .unwrap()
    }
}

fn proposal(parent: Option<i64>, name: &str) -> Option<NodeProposal> {
    Some(NodeProposal {
        parent_id: parent,
        name: name.into(),
        description: format!("about {name}"),
    })
}

fn filed(node: Option<i64>) -> Interpretation {
    Interpretation {
        title: "t".into(),
        summary: "s".into(),
        node_id: node,
        ..Default::default()
    }
}

#[test]
fn the_cap_counts_only_nodes_actually_created() {
    let mut f = fixture();
    // cap 1, a new project and a new topic: the project only, filed there
    let a = f.apply(
        &Interpretation {
            new_project: proposal(Some(f.work), "Exporter"),
            new_topic: proposal(None, "CSV"),
            ..filed(Some(f.work))
        },
        1,
    );
    assert_eq!(a.new_nodes.len(), 1);
    let exporter = &a.new_nodes[0];
    assert_eq!(
        (exporter.name.as_str(), exporter.kind),
        ("Exporter", NodeKind::Project)
    );
    assert_eq!(exporter.description, "about Exporter");
    assert_eq!(exporter.created_by, Origin::Auto);
    assert_eq!(a.episode.node_id, Some(exporter.id));
    assert!(f.s.find_node(Some(exporter.id), "CSV").unwrap().is_none());
    assert_eq!(f.s.changes(10).unwrap().len(), 1);

    // the same project again (other case and spacing) does not use up the cap: the topic fits
    let a = f.apply(
        &Interpretation {
            new_project: proposal(Some(f.work), "  exporter "),
            new_topic: proposal(None, "CSV"),
            ..filed(Some(f.work))
        },
        1,
    );
    assert_eq!(
        a.new_nodes
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        ["CSV"]
    );
    let csv = &a.new_nodes[0];
    assert_eq!(csv.parent_id, Some(exporter.id));
    assert_eq!(a.episode.node_id, Some(csv.id));

    // cap 0: existing names are still reused
    let a = f.apply(
        &Interpretation {
            new_project: proposal(Some(f.work), "EXPORTER"),
            new_topic: proposal(None, "csv"),
            ..filed(None)
        },
        0,
    );
    assert!(a.new_nodes.is_empty());
    assert_eq!(a.episode.node_id, Some(csv.id));
    assert_eq!(f.tree().len(), 5, "3 categories + Exporter + CSV");
}

#[test]
fn bad_parents_fall_back_sensibly() {
    let mut f = fixture();
    let exporter = f.project(f.work, "Exporter");
    let topic =
        f.s.add_node(Some(exporter.id), NodeKind::Topic, "CSV", "", Origin::Auto)
            .unwrap();

    // a project proposed under a project: goes under the category above the chosen node
    let a = f.apply(
        &Interpretation {
            new_project: proposal(Some(exporter.id), "Importer"),
            ..filed(Some(topic.id))
        },
        3,
    );
    assert_eq!(a.new_nodes.len(), 1);
    assert_eq!(a.new_nodes[0].parent_id, Some(f.work));

    // a project with no valid parent and nothing chosen: not created, unfiled
    let a = f.apply(
        &Interpretation {
            new_project: proposal(Some(9_999), "Orphan"),
            ..filed(None)
        },
        3,
    );
    assert!(a.new_nodes.is_empty());
    assert_eq!(a.episode.node_id, None);

    // a topic proposed under a category: goes under the chosen project instead
    let a = f.apply(
        &Interpretation {
            new_topic: proposal(Some(f.admin), "PDF"),
            ..filed(Some(exporter.id))
        },
        3,
    );
    assert_eq!(a.new_nodes.len(), 1);
    assert_eq!(a.new_nodes[0].parent_id, Some(exporter.id));
    assert_eq!(a.new_nodes[0].kind, NodeKind::Topic);

    // a topic with no project anywhere: not created, the filing stays at the category
    let a = f.apply(
        &Interpretation {
            new_topic: proposal(None, "Loose"),
            ..filed(Some(f.admin))
        },
        3,
    );
    assert!(a.new_nodes.is_empty());
    assert_eq!(a.episode.node_id, Some(f.admin));

    // blank names are ignored
    let a = f.apply(
        &Interpretation {
            new_project: proposal(Some(f.work), "   "),
            new_topic: proposal(None, ""),
            ..filed(Some(f.work))
        },
        3,
    );
    assert!(a.new_nodes.is_empty());
    assert_eq!(a.episode.node_id, Some(f.work));
    assert_eq!(a.episode.title, "t");
}

#[test]
fn archived_nodes_are_not_filed_into_but_are_revived_by_name() {
    let mut f = fixture();
    let old = f.project(f.work, "Exporter");
    f.s.set_node_archived(old.id, true).unwrap();
    let a = f.apply(&filed(Some(old.id)), 3);
    assert_eq!(
        a.episode.node_id, None,
        "an archived node is not a valid target"
    );
    // proposing it again by name revives it, without counting as new
    let a = f.apply(
        &Interpretation {
            new_project: proposal(Some(f.work), "exporter"),
            ..filed(None)
        },
        0,
    );
    assert!(a.new_nodes.is_empty());
    assert_eq!(a.episode.node_id, Some(old.id));
    assert!(!f.s.node(old.id).unwrap().unwrap().archived);
}

#[test]
fn events_are_validated_and_reconciled_on_revision() {
    let mut f = fixture();
    let ev = |title: &str, date: &str, time: Option<&str>, end: Option<&str>| ProposedEvent {
        title: title.into(),
        date: date.into(),
        time: time.map(Into::into),
        end_date: end.map(Into::into),
        all_day: None,
        details: " d ".into(),
        confidence: Some(0.5),
    };
    let chunk = f.chunk();
    let tree = f.tree();
    let first = Interpretation {
        title: "  Planning  ".into(),
        events: vec![
            ev("Launch", "2026-11-20", None, Some("2026-11-22")),
            ev("Review", "2026-11-21", Some("9am"), None),
            ev("Backwards", "2026-11-25", None, Some("2026-11-24")),
            ev("", "2026-11-26", None, None),
            ev("No date", "someday", None, None),
        ],
        ..filed(Some(f.work))
    };
    let a =
        apply_interpretation(&f.s, &tree, &chunk, &first, None, "m", None, None, 3, 10).unwrap();
    assert_eq!(a.episode.title, "Planning");
    let titles: Vec<&str> = a.events.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, ["Launch", "Review", "Backwards"]);
    assert!(a.events[0].all_day && a.events[0].when_end.is_some());
    assert!(!a.events[1].all_day, "a time makes it timed");
    assert_eq!(
        a.events[2].when_end, None,
        "an end before the start is dropped"
    );
    assert_eq!(a.events[0].details, "d");
    assert!(a.events.iter().all(|e| e.node_id == Some(f.work)
        && e.source == Origin::Interpretation
        && e.source_episode_id == Some(a.episode.id)
        && e.source_segment_id == Some(chunk.segments[0].id)));

    // a hotword event of the same day is never touched by a revision
    let mine =
        f.s.upsert_event(
            &oats_core::NewEvent {
                when_start: 5,
                title: "Mine".into(),
                source: Some(Origin::Hotword),
                source_episode_id: Some(a.episode.id),
                ..Default::default()
            },
            1,
        )
        .unwrap();

    // revision: Launch kept (retitled case only → same key), Review gone, a new one
    let launch_id = a.events[0].id;
    let revised = Interpretation {
        title: "Planning".into(),
        events: vec![
            ev("LAUNCH", "2026-11-20", None, None),
            ev("Retro", "2026-11-27", None, None),
        ],
        ..filed(Some(f.work))
    };
    let chunk2 = Chunk {
        started_at: chunk.started_at,
        ended_at: chunk.ended_at + 1_000,
        segments: chunk.segments.clone(),
    };
    let b = apply_interpretation(
        &f.s,
        &tree,
        &chunk2,
        &revised,
        Some(&a.episode),
        "m",
        Some(0.1),
        None,
        3,
        20,
    )
    .unwrap();
    assert_eq!(b.episode.id, a.episode.id);
    assert_eq!(b.episode.revision, 2);
    let after: Vec<(i64, String)> =
        f.s.events_of_episode(a.episode.id)
            .unwrap()
            .into_iter()
            .map(|e| (e.id, e.title))
            .collect();
    assert!(after.contains(&(launch_id, "LAUNCH".into())), "{after:?}");
    assert!(after.iter().any(|(_, t)| t == "Retro"));
    assert!(
        after.iter().any(|(id, _)| *id == mine.id),
        "hotword event kept"
    );
    assert_eq!(after.len(), 3, "{after:?}");

    // turned into low signal: its interpretation events go, the hotword one stays
    let low = Interpretation {
        low_signal: true,
        events: vec![ev("Launch", "2026-11-20", None, None)],
        ..filed(Some(f.work))
    };
    let prior = f.s.episode(a.episode.id).unwrap().unwrap();
    let c = apply_interpretation(
        &f.s,
        &tree,
        &chunk2,
        &low,
        Some(&prior),
        "m",
        None,
        None,
        3,
        30,
    )
    .unwrap();
    assert!(c.episode.low_signal);
    assert_eq!(c.episode.node_id, None);
    let left: Vec<i64> =
        f.s.events_of_episode(a.episode.id)
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
    assert_eq!(left, vec![mine.id]);
}

// ---- undo ----------------------------------------------------------------------------------------

fn act(kind: &str, v: serde_json::Value) -> Action {
    let mut a: Action = serde_json::from_value(v).unwrap();
    a.kind = kind.into();
    a
}

#[test]
fn every_reversible_change_can_be_undone_once() {
    let f = fixture();
    let s = &f.s;
    let exporter = f.project(f.work, "Exporter");
    s.set_node_description(exporter.id, "old words").unwrap();
    let topic = s
        .add_node(Some(exporter.id), NodeKind::Topic, "CSV", "", Origin::Auto)
        .unwrap();
    let ep = s
        .insert_episode(
            0,
            10,
            &EpisodeContent {
                title: "Standup".into(),
                node_id: None,
                ..Default::default()
            },
            1,
        )
        .unwrap();

    let tree = s.nodes().unwrap();
    let reorg = Actions {
        actions: vec![
            act("archive_node", json!({"node_id": exporter.id})),
            act(
                "set_description",
                json!({"node_id": exporter.id, "description": " new words "}),
            ),
            act(
                "move_node",
                json!({"node_id": topic.id, "new_parent_id": f.admin}),
            ),
        ],
    };
    assert_eq!(
        interpret::apply_reorganisation(s, &tree, &reorg, None, 5)
            .unwrap()
            .len(),
        3
    );
    let episodes = vec![ep.clone()];
    let review = Actions {
        actions: vec![
            act(
                "set_node",
                json!({"episode_id": ep.id, "node_id": topic.id}),
            ),
            act("mark_low_signal", json!({"episode_id": ep.id})),
        ],
    };
    assert_eq!(
        interpret::apply_review(s, &tree, &episodes, &review, None, 6)
            .unwrap()
            .len(),
        2
    );
    let n = s.node(exporter.id).unwrap().unwrap();
    assert!(n.archived);
    assert_eq!(n.description, "new words");
    assert_eq!(s.node(topic.id).unwrap().unwrap().parent_id, Some(f.admin));
    let e = s.episode(ep.id).unwrap().unwrap();
    assert_eq!((e.node_id, e.low_signal), (Some(topic.id), true));

    for c in s.changes(20).unwrap() {
        assert!(undo_change(s, c.id).unwrap(), "{} should undo", c.kind);
        assert!(!undo_change(s, c.id).unwrap(), "{} undone twice", c.kind);
        assert!(s.change(c.id).unwrap().unwrap().undone);
    }
    let n = s.node(exporter.id).unwrap().unwrap();
    assert!(!n.archived);
    assert_eq!(n.description, "old words");
    assert_eq!(
        s.node(topic.id).unwrap().unwrap().parent_id,
        Some(exporter.id)
    );
    let e = s.episode(ep.id).unwrap().unwrap();
    assert_eq!((e.node_id, e.low_signal), (None, false), "back to unfiled");
}

#[test]
fn irreversible_or_malformed_changes_are_refused() {
    let f = fixture();
    let s = &f.s;
    assert!(!undo_change(s, 12_345).unwrap(), "unknown id");
    let create = s
        .record_change(
            None,
            "node_create",
            "Created",
            &serde_json::Value::Null,
            &json!({}),
            1,
        )
        .unwrap();
    let broken = s
        .record_change(
            None,
            "node_rename",
            "no name in before",
            &json!({"node_id": f.work}),
            &json!({}),
            1,
        )
        .unwrap();
    let odd = s
        .record_change(None, "something_new", "?", &json!({}), &json!({}), 1)
        .unwrap();
    for id in [create, broken, odd] {
        assert!(!undo_change(s, id).unwrap());
        assert!(
            !s.change(id).unwrap().unwrap().undone,
            "a refused undo isn't marked"
        );
    }
    assert_eq!(s.node(f.work).unwrap().unwrap().name, "Work");
}

// ---- events across re-interpretation -------------------------------------------------------------

fn call_event(title: &str, details: &str) -> ProposedEvent {
    ProposedEvent {
        title: title.into(),
        date: "2026-10-29".into(),
        time: Some("12:00".into()),
        end_date: None,
        all_day: None,
        details: details.into(),
        confidence: Some(0.7),
    }
}

fn with_events(f: &Fixture, events: Vec<ProposedEvent>) -> Interpretation {
    Interpretation {
        title: "Call".into(),
        events,
        ..filed(Some(f.work))
    }
}

/// Apply `interp` to `chunk` as a fresh episode (the scheduler's path after `reset_episode`).
fn apply_to(f: &Fixture, chunk: &Chunk, interp: &Interpretation) -> interpret::Applied {
    let tree = f.tree();
    apply_interpretation(&f.s, &tree, chunk, interp, None, "m", None, None, 3, 50).unwrap()
}

#[test]
fn reinterpreting_a_reset_episode_keeps_one_event_pointing_at_the_new_episode() {
    let mut f = fixture();
    let chunk = f.chunk();
    let one = with_events(&f, vec![call_event("KPMG call", "first")]);
    let a = apply_to(&f, &chunk, &one);
    assert_eq!(
        a.events[0].dedupe_key.as_deref(),
        Some("ev:2026-10-29:kpmg call")
    );

    f.s.reset_episode(a.episode.id).unwrap();
    let again = with_events(&f, vec![call_event("KPMG  Call", "second")]);
    let b = apply_to(&f, &chunk, &again);
    assert_ne!(a.episode.id, b.episode.id);
    let all = f.s.events_from(i64::MIN, 10).unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].id, a.events[0].id);
    assert_eq!(all[0].source_episode_id, Some(b.episode.id));
    assert_eq!(all[0].details, "second");
}

#[test]
fn reinterpreting_with_a_different_set_drops_what_is_no_longer_proposed() {
    let mut f = fixture();
    let chunk = f.chunk();
    let first = with_events(
        &f,
        vec![call_event("KPMG call", "x"), call_event("Dentist", "y")],
    );
    let a = apply_to(&f, &chunk, &first);
    // a hotword event on the same episode id is never touched
    let mine =
        f.s.upsert_event(
            &oats_core::NewEvent {
                when_start: 5,
                title: "Mine".into(),
                source: Some(Origin::Hotword),
                source_episode_id: Some(a.episode.id),
                dedupe_key: Some("hotword:1:2026-10-29:mine".into()),
                ..Default::default()
            },
            1,
        )
        .unwrap();

    f.s.reset_episode(a.episode.id).unwrap();
    let second = with_events(&f, vec![call_event("KPMG call", "updated")]);
    let b = apply_to(&f, &chunk, &second);
    let all = f.s.events_from(i64::MIN, 10).unwrap();
    let mut titles: Vec<&str> = all.iter().map(|e| e.title.as_str()).collect();
    titles.sort_unstable();
    assert_eq!(titles, ["KPMG call", "Mine"], "{all:?}");
    let kpmg = all.iter().find(|e| e.title == "KPMG call").unwrap();
    assert_eq!(kpmg.id, a.events[0].id);
    assert_eq!(kpmg.details, "updated");
    assert_eq!(kpmg.source_episode_id, Some(b.episode.id));
    assert!(all.iter().any(|e| e.id == mine.id));
}

#[test]
fn a_second_episode_mentioning_the_same_event_updates_the_one_row() {
    let mut f = fixture();
    let c1 = f.chunk();
    let c2 = f.chunk();
    let a = apply_to(
        &f,
        &c1,
        &with_events(&f, vec![call_event("KPMG call", "a")]),
    );
    let b = apply_to(
        &f,
        &c2,
        &with_events(&f, vec![call_event("kpmg call", "b")]),
    );
    assert_ne!(a.episode.id, b.episode.id);
    let all = f.s.events_from(i64::MIN, 10).unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].source_episode_id, Some(b.episode.id));
    assert_eq!(all[0].details, "b");
    // revising the first episode without the event does not delete the second one's claim
    let tree = f.tree();
    let prior = f.s.episode(a.episode.id).unwrap().unwrap();
    apply_interpretation(
        &f.s,
        &tree,
        &c1,
        &with_events(&f, vec![]),
        Some(&prior),
        "m",
        None,
        None,
        3,
        60,
    )
    .unwrap();
    assert_eq!(f.s.events_from(i64::MIN, 10).unwrap().len(), 1);
}
