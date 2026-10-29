//! Per-item filing and provenance: the interpretation's decisions and action items name the
//! project they concern (`project` → `node_id`, never creating nodes) and the transcript line
//! they came from (`at` → `at_ms` / `segment_id`); the series rule's hint; the store's
//! cross-filed item query, merges and note moves.

use chrono::{Local, TimeZone};
use oats_brain::episodes::Chunk;
use oats_brain::interpret::{
    self, INTERPRET_SYSTEM, INTERPRET_USER, Interpretation, ProposedAction, ProposedDecision,
    apply_interpretation, interpret_schema, resolve_item_node, undo_change,
};
use oats_core::{
    ActionItem, AudioSourceKind, Decision, EpisodeContent, MediaClass, NodeKind, NoteItem, Origin,
    Store,
};

fn local_ms(d: u32, h: u32, m: u32, s: u32) -> i64 {
    Local
        .with_ymd_and_hms(2026, 10, d, h, m, s)
        .earliest()
        .unwrap()
        .timestamp_millis()
}

struct F {
    s: Store,
    work: i64,
    commerce: i64,
    devcall: i64,
}

fn fixture() -> F {
    let s = Store::open_in_memory().unwrap();
    let work = s.categories().unwrap()[0].id;
    let commerce = s
        .add_node(
            Some(work),
            NodeKind::Project,
            "Commerce project",
            "",
            Origin::Auto,
        )
        .unwrap()
        .id;
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
    F {
        s,
        work,
        commerce,
        devcall,
    }
}

/// Three lines at 14:30:05, 14:31:10 and 14:44:00 local on Oct 27.
fn call_chunk(s: &Store) -> Chunk {
    let sid = s.begin_session(0, "e").unwrap();
    let rows: Vec<_> = [
        (30, 5, "hello all"),
        (31, 10, "checkout ships Friday"),
        (44, 0, "bye"),
    ]
    .into_iter()
    .map(|(m, sec, text)| {
        let t = local_ms(27, 14, m, sec);
        s.insert_segment(sid, t, t + 4_000, text, None).unwrap()
    })
    .collect();
    Chunk {
        started_at: rows[0].started_at,
        ended_at: rows[2].ended_at,
        segments: rows,
    }
}

#[test]
fn the_prompt_and_schema_carry_the_item_and_series_rules() {
    assert!(INTERPRET_SYSTEM.contains("Item filing and source"));
    assert!(INTERPRET_SYSTEM.contains("items never create projects"));
    assert!(INTERPRET_SYSTEM.contains("- Series:"));
    assert!(INTERPRET_SYSTEM.contains("Development team call"));
    assert!(INTERPRET_USER.contains("{{series}}"));
    let schema = interpret_schema();
    for list in ["action_items", "decisions"] {
        let req = &schema["properties"][list]["items"]["required"];
        assert!(
            req.as_array().unwrap().iter().any(|v| v == "project"),
            "{list}"
        );
        assert!(req.as_array().unwrap().iter().any(|v| v == "at"), "{list}");
    }
}

#[test]
fn project_names_map_to_existing_nodes_only() {
    let f = fixture();
    let tree = f.s.nodes().unwrap();
    let home = Some(f.devcall);
    assert_eq!(
        resolve_item_node(&tree, Some("Commerce project"), home),
        Some(f.commerce)
    );
    assert_eq!(
        resolve_item_node(&tree, Some("  commerce   PROJECT "), home),
        Some(f.commerce)
    );
    assert_eq!(
        resolve_item_node(
            &tree,
            Some(&format!("[{}] Commerce project", f.commerce)),
            home
        ),
        Some(f.commerce)
    );
    // unknown, the note's own home, a category, blank: filed with the note
    assert_eq!(resolve_item_node(&tree, Some("Billing revamp"), home), None);
    assert_eq!(
        resolve_item_node(&tree, Some("Development team call"), home),
        None
    );
    assert_eq!(resolve_item_node(&tree, Some("Work"), home), None);
    assert_eq!(resolve_item_node(&tree, Some(""), home), None);
    assert_eq!(resolve_item_node(&tree, None, home), None);
}

#[test]
fn apply_files_items_and_resolves_where_they_were_said() {
    let f = fixture();
    let chunk = call_chunk(&f.s);
    let tree = f.s.nodes().unwrap();
    let nodes_before = tree.len();
    let interp = Interpretation {
        title: "Development team call".into(),
        summary: "- stuff".into(),
        node_id: Some(f.devcall),
        decisions: vec![
            ProposedDecision {
                text: "Checkout ships Friday".into(),
                project: Some("Commerce project".into()),
                at: Some("14:31".into()),
            },
            ProposedDecision::from("Standups move to 9:30"),
        ],
        action_items: vec![
            ProposedAction {
                text: "Prepare the checkout release".into(),
                owner: Some("Sam".into()),
                project: Some("Commerce project".into()),
                at: Some("[2:44 pm]".into()),
                ..Default::default()
            },
            ProposedAction {
                text: "Book the retro".into(),
                project: Some("Nonexistent thing".into()),
                at: Some("23:59".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let applied =
        apply_interpretation(&f.s, &tree, &chunk, &interp, None, "m", None, None, 3, 1).unwrap();
    let ep = applied.episode;
    assert_eq!(ep.node_id, Some(f.devcall));
    assert_eq!(
        f.s.nodes().unwrap().len(),
        nodes_before,
        "items never create nodes"
    );
    let d = &ep.decisions[0];
    assert_eq!(d.node_id, Some(f.commerce));
    assert_eq!(d.segment_id, Some(chunk.segments[1].id));
    assert_eq!(d.at_ms, Some(chunk.segments[1].started_at));
    assert_eq!(ep.decisions[1], Decision::plain("Standups move to 9:30"));
    let a = &ep.action_items[0];
    assert_eq!(
        (a.node_id, a.segment_id),
        (Some(f.commerce), Some(chunk.segments[2].id))
    );
    // unknown project → home; a time no line is near → no source
    let b = &ep.action_items[1];
    assert_eq!((b.node_id, b.at_ms, b.segment_id), (None, None, None));

    // the Commerce page sees the two items filed there, the note stays under the call
    let filed = f.s.items_filed_under(f.commerce).unwrap();
    let texts: Vec<&str> = filed.iter().map(|(_, i)| i.text()).collect();
    assert_eq!(
        texts,
        ["Prepare the checkout release", "Checkout ships Friday"]
    );
    assert!(filed.iter().all(|(e, _)| e.id == ep.id));
    assert!(matches!(filed[0].1, NoteItem::Action(_)));
    // …and so does its category (a subtree), not the call's page (items with no node)
    assert_eq!(f.s.items_filed_under(f.work).unwrap().len(), 2);
    assert!(f.s.items_filed_under(f.devcall).unwrap().is_empty());
}

#[test]
fn decisions_read_old_plain_strings_and_store_plain_when_bare() {
    let f = fixture();
    let ep =
        f.s.insert_episode(
            0,
            1,
            &EpisodeContent {
                title: "t".into(),
                decisions: vec![
                    Decision::plain("bare"),
                    Decision {
                        text: "filed".into(),
                        node_id: Some(f.commerce),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            1,
        )
        .unwrap();
    assert_eq!(ep.decisions[0], Decision::plain("bare"));
    assert_eq!(ep.decisions[1].node_id, Some(f.commerce));
    let json = serde_json::to_string(&ep.decisions).unwrap();
    assert!(json.starts_with("[\"bare\",{"), "{json}");
    let parsed: Interpretation = serde_json::from_value(
        serde_json::json!({"decisions": ["plain", {"text": "obj", "project": "X", "at": null}]}),
    )
    .unwrap();
    assert_eq!(parsed.decisions[0].text, "plain");
    assert_eq!(parsed.decisions[1].project.as_deref(), Some("X"));
}

#[test]
fn merges_and_deletes_refile_items_and_moves_are_undoable() {
    let f = fixture();
    let shop =
        f.s.add_node(Some(f.work), NodeKind::Project, "Shop", "", Origin::Auto)
            .unwrap()
            .id;
    let ep =
        f.s.insert_episode(
            0,
            1,
            &EpisodeContent {
                title: "Dev call".into(),
                node_id: Some(f.devcall),
                action_items: vec![ActionItem {
                    text: "x".into(),
                    node_id: Some(shop),
                    ..Default::default()
                }],
                ..Default::default()
            },
            1,
        )
        .unwrap();
    f.s.merge_nodes(shop, f.commerce).unwrap();
    assert_eq!(f.s.items_filed_under(f.commerce).unwrap().len(), 1);
    f.s.delete_node(f.commerce).unwrap();
    assert_eq!(
        f.s.episode(ep.id).unwrap().unwrap().action_items[0].node_id,
        None
    );

    // Move to: recorded, undoable
    let other =
        f.s.add_node(
            Some(f.work),
            NodeKind::Project,
            "Elsewhere",
            "",
            Origin::User,
        )
        .unwrap()
        .id;
    assert!(f.s.move_episode(ep.id, other, 5).unwrap());
    assert!(!f.s.move_episode(ep.id, other, 6).unwrap(), "already there");
    let change =
        f.s.changes(5)
            .unwrap()
            .into_iter()
            .find(|c| c.kind == "episode_move")
            .unwrap();
    assert_eq!(f.s.episode(ep.id).unwrap().unwrap().node_id, Some(other));
    assert!(undo_change(&f.s, change.id).unwrap());
    assert_eq!(
        f.s.episode(ep.id).unwrap().unwrap().node_id,
        Some(f.devcall)
    );
}

#[test]
fn a_meeting_link_heard_on_an_earlier_day_makes_a_series() {
    let f = fixture();
    let sid = f.s.begin_session(0, "e").unwrap();
    let link = "https://meet.google.com/abc-defg-hij";
    let seg = |day: u32, h: u32| {
        let t = local_ms(day, h, 0, 0);
        let r =
            f.s.insert_segment_from(
                sid,
                t,
                t + 4_000,
                "them talking",
                None,
                AudioSourceKind::System,
                None,
            )
            .unwrap();
        f.s.set_segment_media(r.id, Some(MediaClass::Meeting), Some(link))
            .unwrap();
        f.s.segment(r.id).unwrap().unwrap()
    };
    // last week's call, interpreted and filed under the series' project
    let old = seg(20, 10);
    let ep =
        f.s.insert_episode(
            old.started_at,
            old.ended_at,
            &EpisodeContent {
                title: "Weekly dev sync".into(),
                node_id: Some(f.devcall),
                ..Default::default()
            },
            1,
        )
        .unwrap();
    f.s.mark_segments_interpreted(&[old.id], ep.id, 1).unwrap();
    let tree = f.s.nodes().unwrap();
    let today = seg(27, 10);
    let chunk = Chunk {
        started_at: today.started_at,
        ended_at: today.ended_at,
        segments: vec![today],
    };
    let hint = interpret::series_hint(&f.s, &tree, &chunk);
    assert!(hint.contains("series: true"), "{hint}");
    assert!(hint.contains("Weekly dev sync"), "{hint}");
    assert!(
        hint.contains(&format!("[{}] Development team call", f.devcall)),
        "{hint}"
    );
    let req = interpret::interpret_request_full(
        &tree,
        &chunk,
        None,
        &Local::now(),
        None,
        &Default::default(),
        &hint,
    );
    assert!(req.user_prompt.contains("series: true"));

    // a first occurrence (only the same day, or another link) is no series
    let other = seg(27, 15);
    f.s.set_segment_media(
        other.id,
        Some(MediaClass::Meeting),
        Some("https://meet.google.com/zzz"),
    )
    .unwrap();
    let other = f.s.segment(other.id).unwrap().unwrap();
    let lone = Chunk {
        started_at: other.started_at,
        ended_at: other.ended_at,
        segments: vec![other],
    };
    assert!(interpret::series_hint(&f.s, &tree, &lone).is_empty());
    // no call at all
    assert!(interpret::series_hint(&f.s, &tree, &call_chunk(&f.s)).is_empty());
}
