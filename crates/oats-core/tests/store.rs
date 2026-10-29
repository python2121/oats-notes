//! Integration tests for the SQLite store through its public API (and, where the API has no
//! window onto it, a raw `rusqlite` connection to the same file): the v1 → v2 migration of a
//! realistic database, subtree views, event/todo upsert semantics, the node helpers and
//! their effect on everything filed under a node, the change log, and several connections on
//! one file (the pipeline, the brain and the UI each open their own).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use oats_core::store::name_key;
use oats_core::{
    EpisodeContent, NewEvent, NewTodo, NodeKind, Origin, Store, StoreError, TodoFilter, View,
};

type R = Result<(), StoreError>;

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("oats-core-it-{tag}-{}-{nanos}", std::process::id()));
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

/// The v1 schema as shipped (see the unit test in `store.rs`), without data.
const V1_SCHEMA: &str = "
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
    CREATE TABLE sessions (id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL,
        ended_at INTEGER, engine TEXT NOT NULL);
    CREATE TABLE categories (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
        position INTEGER NOT NULL);
    CREATE TABLE segments (id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES sessions(id),
        started_at INTEGER NOT NULL, ended_at INTEGER NOT NULL, text TEXT NOT NULL,
        confidence REAL, category_id INTEGER REFERENCES categories(id));
    CREATE INDEX segments_started_at ON segments(started_at);
    CREATE INDEX segments_category ON segments(category_id);
    INSERT INTO meta VALUES ('schema_version', '1');
";

/// One v1 segment row as written.
#[derive(Clone, Debug, PartialEq)]
struct V1Segment {
    id: i64,
    session_id: i64,
    started_at: i64,
    ended_at: i64,
    text: String,
    confidence: Option<f64>,
    category_id: Option<i64>,
}

/// Write a v1 database the way the v1 app would have left it after a few days of use:
/// several sessions (the last one never closed), the default categories plus a user one with a
/// gap in the ids and positions out of id order, and a mix of filed and unfiled segments.
fn write_v1(path: &Path) -> Vec<V1Segment> {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    conn.execute_batch(V1_SCHEMA).unwrap();
    conn.execute_batch(
        "INSERT INTO sessions VALUES (1, 1000, 500000, 'Parakeet');
         INSERT INTO sessions VALUES (2, 600000, 900000, 'Parakeet');
         INSERT INTO sessions VALUES (3, 1000000, NULL, 'Null');
         INSERT INTO categories VALUES (1, 'Work', 0), (2, 'Admin', 2), (3, 'Personal', 1),
                                       (9, 'Side projects', 3);",
    )
    .unwrap();
    let cats = [None, Some(1), Some(2), Some(3), Some(9)];
    let mut rows = Vec::new();
    for i in 0..60i64 {
        let session_id = 1 + i / 20;
        let started_at = 1_000 + i * 15_000 + (session_id - 1) * 1_000_000;
        let row = V1Segment {
            id: i + 1,
            session_id,
            started_at,
            ended_at: started_at + 4_000 + i * 10,
            text: format!("segment {i} — naïve “quotes” 'and' emoji 🎙"),
            confidence: (i % 3 != 0).then(|| 0.5 + (i as f64) / 200.0),
            category_id: cats[(i % 5) as usize],
        };
        conn.execute(
            "INSERT INTO segments VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                row.id,
                row.session_id,
                row.started_at,
                row.ended_at,
                row.text,
                row.confidence,
                row.category_id
            ],
        )
        .unwrap();
        rows.push(row);
    }
    rows
}

fn raw(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

// ---- v1 → v2 migration ----------------------------------------------------------------------

#[test]
fn migrates_a_realistic_v1_file_keeping_every_row_and_id() -> R {
    let dir = TempDir::new("migrate");
    let path = dir.db();
    let v1 = write_v1(&path);

    let s = Store::open(&path)?;
    assert_eq!(s.meta("schema_version")?.as_deref(), Some("6"));

    // categories became root nodes with the same ids, in position order; nothing re-seeded
    let cats = s.categories()?;
    assert_eq!(
        cats.iter()
            .map(|c| (c.id, c.name.as_str(), c.position))
            .collect::<Vec<_>>(),
        vec![
            (1, "Work", 0),
            (3, "Personal", 1),
            (2, "Admin", 2),
            (9, "Side projects", 3)
        ]
    );
    assert!(cats.iter().all(|c| c.kind == NodeKind::Category
        && c.parent_id.is_none()
        && c.created_by == Origin::User
        && !c.archived
        && c.description.is_empty()));
    assert_eq!(s.nodes()?.len(), 4);

    // every segment survives with its id, session, times, text, confidence and node
    let rows = s.segments(View::All, 0)?;
    assert_eq!(rows.len(), v1.len());
    for (new, old) in rows.iter().zip(&v1) {
        assert_eq!(new.id, old.id);
        assert_eq!(new.session_id, old.session_id);
        assert_eq!(
            (new.started_at, new.ended_at),
            (old.started_at, old.ended_at)
        );
        assert_eq!(new.text, old.text);
        assert_eq!(new.confidence, old.confidence.map(|c| c as f32));
        assert_eq!(new.node_id, old.category_id, "segment {}", old.id);
        assert_eq!(
            new.categorised,
            old.category_id.is_some(),
            "segment {}",
            old.id
        );
        assert_eq!(new.episode_id, None);
        assert_eq!(new.interpreted_at, None);
    }
    // the node views see the migrated filing
    for cat in [1, 2, 3, 9] {
        let want: Vec<i64> = v1
            .iter()
            .filter(|r| r.category_id == Some(cat))
            .map(|r| r.id)
            .collect();
        let got: Vec<i64> = s
            .segments(View::Node(cat), 0)?
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(got, want, "category {cat}");
    }
    // the unfiled ones are what the categorisation pass will look at; nothing is interpreted
    let unfiled = v1.iter().filter(|r| r.category_id.is_none()).count();
    assert_eq!(s.uncategorised_segments(1_000)?.len(), unfiled);
    assert_eq!(s.uninterpreted_count()?, v1.len() as i64);

    // new rows continue the id sequences
    let next = s.insert_segment(3, 5_000_000, 5_001_000, "after the migration", None)?;
    assert_eq!(next.id, 61);
    assert_eq!(s.add_category("Ideas")?.position, 4);
    assert_eq!(s.add_category("work")?.id, 1, "names still dedupe");
    drop(s);

    // the file itself: old table gone, sessions untouched, foreign keys and integrity fine
    let conn = raw(&path);
    assert!(!table_exists(&conn, "categories"));
    assert!(!table_exists(&conn, "segments_v2"));
    for t in [
        "nodes",
        "segments",
        "episodes",
        "events",
        "reminders",
        "notes",
        "brain_runs",
        "changes",
    ] {
        assert!(table_exists(&conn, t), "{t}");
    }
    let sessions: Vec<(i64, i64, Option<i64>, String)> = conn
        .prepare("SELECT id, started_at, ended_at, engine FROM sessions ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    assert_eq!(
        sessions,
        vec![
            (1, 1000, Some(500_000), "Parakeet".to_owned()),
            (2, 600_000, Some(900_000), "Parakeet".to_owned()),
            (3, 1_000_000, None, "Null".to_owned()),
        ]
    );
    let fk_problems: i64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })?;
    assert_eq!(fk_problems, 0);
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    assert_eq!(integrity, "ok");
    let indexes: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'segments' ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    assert!(
        indexes.contains(&"segments_started_at".to_owned()),
        "{indexes:?}"
    );
    assert!(
        !indexes.contains(&"segments_category".to_owned()),
        "{indexes:?}"
    );
    drop(conn);

    // reopening is a no-op: nothing migrates twice, nothing is lost
    let s = Store::open(&path)?;
    assert_eq!(s.segments(View::All, 0)?.len(), v1.len() + 1);
    assert_eq!(s.categories()?.len(), 5);
    Ok(())
}

#[test]
fn migrating_a_v1_file_without_categories_seeds_the_defaults() -> R {
    let dir = TempDir::new("migrate-empty");
    let path = dir.db();
    {
        let conn = raw(&path);
        conn.execute_batch(V1_SCHEMA)?;
        conn.execute_batch(
            "INSERT INTO sessions VALUES (1, 0, 10, 'Null');
             INSERT INTO segments VALUES (1, 1, 1, 2, 'loose', NULL, NULL);",
        )?;
    }
    let s = Store::open(&path)?;
    let names: Vec<String> = s.categories()?.into_iter().map(|c| c.name).collect();
    assert_eq!(names, ["Work", "Admin", "Personal"]);
    let rows = s.segments(View::All, 0)?;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].node_id, rows[0].categorised), (None, false));
    Ok(())
}

#[test]
fn a_failed_migration_rolls_back_to_the_v1_file() {
    let dir = TempDir::new("migrate-fail");
    let path = dir.db();
    let v1 = write_v1(&path);
    {
        // a stray v2 `nodes` table whose id 1 collides with the Work category: the copy fails
        let conn = raw(&path);
        conn.execute_batch(
            "CREATE TABLE nodes (id INTEGER PRIMARY KEY, parent_id INTEGER, kind TEXT NOT NULL,
                 name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
                 created_by TEXT NOT NULL DEFAULT 'user', position INTEGER NOT NULL,
                 archived INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL DEFAULT 0);
             INSERT INTO nodes(id, kind, name, position) VALUES (1, 'category', 'Clash', 0);",
        )
        .unwrap();
    }
    assert!(Store::open(&path).is_err());
    // nothing half-done: still v1, the old tables and every row in place
    let conn = raw(&path);
    let v: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, "1");
    assert!(table_exists(&conn, "categories"));
    assert!(!table_exists(&conn, "segments_v2"));
    assert!(!table_exists(&conn, "episodes"));
    let n: i64 = conn
        .query_row("SELECT COUNT(category_id) FROM segments", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        n,
        v1.iter().filter(|r| r.category_id.is_some()).count() as i64
    );
}

#[test]
fn a_newer_schema_file_is_refused_and_left_alone() -> R {
    let dir = TempDir::new("too-new");
    let path = dir.db();
    {
        let conn = raw(&path);
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO meta VALUES ('schema_version', '7');",
        )?;
    }
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::SchemaTooNew(7))
    ));
    let conn = raw(&path);
    assert!(
        !table_exists(&conn, "nodes"),
        "nothing created in a newer file"
    );
    let v: String = conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(v, "7");
    Ok(())
}

// ---- subtree views ----------------------------------------------------------------------------

#[test]
fn node_views_cover_exactly_the_subtree() -> R {
    let s = Store::open_in_memory()?;
    let cats = s.categories()?;
    let (work, admin) = (cats[0].id, cats[1].id);
    let p = s.add_node(Some(work), NodeKind::Project, "Exporter", "", Origin::Auto)?;
    let sibling = s.add_node(Some(work), NodeKind::Project, "Hiring", "", Origin::Auto)?;
    let t1 = s.add_node(Some(p.id), NodeKind::Topic, "CSV", "", Origin::Auto)?;
    let t2 = s.add_node(Some(p.id), NodeKind::Topic, "PDF", "", Origin::Auto)?;
    let sid = s.begin_session(0, "e")?;
    // filed at every level, inserted out of time order
    let at = |t: i64, text: &str, node: Option<i64>| -> Result<i64, StoreError> {
        let r = s.insert_segment(sid, t, t + 10, text, None)?;
        s.set_segment_node(r.id, node)?;
        Ok(r.id)
    };
    let on_t2 = at(500, "pdf", Some(t2.id))?;
    let on_work = at(100, "work", Some(work))?;
    let on_t1 = at(300, "csv", Some(t1.id))?;
    let on_p = at(200, "exporter", Some(p.id))?;
    let on_sib = at(400, "hiring", Some(sibling.id))?;
    let on_admin = at(50, "admin", Some(admin))?;
    let _loose = at(600, "loose", None)?;

    let ids = |v: View| -> Result<Vec<i64>, StoreError> {
        Ok(s.segments(v, 0)?.into_iter().map(|r| r.id).collect())
    };
    assert_eq!(
        ids(View::Node(work))?,
        vec![on_work, on_p, on_t1, on_sib, on_t2],
        "oldest first, whole subtree"
    );
    assert_eq!(ids(View::Node(p.id))?, vec![on_p, on_t1, on_t2]);
    assert_eq!(ids(View::Node(t1.id))?, vec![on_t1]);
    assert_eq!(ids(View::Node(sibling.id))?, vec![on_sib]);
    assert_eq!(ids(View::Node(admin))?, vec![on_admin]);
    assert!(ids(View::Node(9_999))?.is_empty(), "unknown node");
    assert!(ids(View::Calendar)?.is_empty());

    // equal start times are ordered by id
    let tie = at(300, "csv again", Some(t1.id))?;
    assert_eq!(ids(View::Node(t1.id))?, vec![on_t1, tie]);

    // episodes_under follows the same subtree, newest first
    let ep = |start: i64, node: Option<i64>| -> Result<i64, StoreError> {
        Ok(s.insert_episode(
            start,
            start + 5,
            &EpisodeContent {
                title: format!("ep {start}"),
                node_id: node,
                ..Default::default()
            },
            1,
        )?
        .id)
    };
    let e_t1 = ep(10, Some(t1.id))?;
    let e_p = ep(20, Some(p.id))?;
    let e_sib = ep(30, Some(sibling.id))?;
    let _e_none = ep(40, None)?;
    let under = |id: i64, limit: i64| -> Result<Vec<i64>, StoreError> {
        Ok(s.episodes_under(id, limit)?
            .into_iter()
            .map(|e| e.id)
            .collect())
    };
    assert_eq!(under(work, 10)?, vec![e_sib, e_p, e_t1]);
    assert_eq!(under(p.id, 10)?, vec![e_p, e_t1]);
    assert_eq!(under(work, 2)?, vec![e_sib, e_p], "limit applies");
    assert!(under(admin, 10)?.is_empty());
    Ok(())
}

// ---- events and todos -----------------------------------------------------------------------

#[test]
fn event_upsert_updates_in_place_and_keeps_provenance() -> R {
    let s = Store::open_in_memory()?;
    let work = s.categories()?[0].id;
    let admin = s.categories()?[1].id;
    let base = NewEvent {
        when_start: 1_000,
        when_end: Some(2_000),
        all_day: false,
        title: "Design review".into(),
        details: "first".into(),
        node_id: Some(work),
        source: Some(Origin::Interpretation),
        source_segment_id: Some(11),
        source_episode_id: Some(5),
        confidence: Some(0.6),
        dedupe_key: Some(format!("ep:5:2026-11-02:{}", name_key("Design  Review"))),
    };
    let first = s.upsert_event(&base, 100)?;
    assert_eq!((first.created_at, first.updated_at), (100, 100));

    // a revision: new time and text, no node or episode given, different provenance offered
    let second = s.upsert_event(
        &NewEvent {
            when_start: 3_000,
            when_end: None,
            all_day: true,
            title: "Design review (moved)".into(),
            details: "second".into(),
            node_id: None,
            source: Some(Origin::Hotword),
            source_segment_id: Some(99),
            source_episode_id: None,
            confidence: None,
            ..base.clone()
        },
        200,
    )?;
    assert_eq!(second.id, first.id);
    assert_eq!(second.when_start, 3_000);
    assert_eq!(second.when_end, None, "the end is replaced, not merged");
    assert!(second.all_day);
    assert_eq!(second.title, "Design review (moved)");
    assert_eq!(second.details, "second");
    assert_eq!(second.confidence, None);
    assert_eq!(
        second.node_id,
        Some(work),
        "a missing node keeps the old filing"
    );
    assert_eq!(
        second.source_episode_id,
        Some(5),
        "a missing episode keeps the old one"
    );
    assert_eq!(
        second.source,
        Origin::Interpretation,
        "provenance is fixed at insert"
    );
    assert_eq!(second.source_segment_id, Some(11));
    assert_eq!(second.dedupe_key, base.dedupe_key);
    assert_eq!((second.created_at, second.updated_at), (100, 200));

    // a node given on update wins
    let third = s.upsert_event(
        &NewEvent {
            node_id: Some(admin),
            ..base.clone()
        },
        300,
    )?;
    assert_eq!((third.id, third.node_id), (first.id, Some(admin)));
    assert_eq!(s.events_from(i64::MIN, 100)?.len(), 1);

    // no key: never deduplicated; another key: another row
    let mut keyless = base.clone();
    keyless.dedupe_key = None;
    let a = s.upsert_event(&keyless, 1)?;
    let b = s.upsert_event(&keyless, 1)?;
    assert_ne!(a.id, b.id);
    let other = s.upsert_event(
        &NewEvent {
            dedupe_key: Some("ep:6:2026-11-02:design review".into()),
            ..base.clone()
        },
        1,
    )?;
    assert_ne!(other.id, first.id);
    assert_eq!(s.events_from(i64::MIN, 100)?.len(), 4);
    // an insert without a source counts as the interpretation's
    assert_eq!(a.source, Origin::Interpretation);

    // ordering: events_from soonest first, events_before most recent first, ties by id
    let from: Vec<i64> = s
        .events_from(1_000, 100)?
        .iter()
        .map(|e| e.when_start)
        .collect();
    assert!(from.windows(2).all(|w| w[0] <= w[1]), "{from:?}");
    let before: Vec<i64> = s
        .events_before(5_000, 100)?
        .iter()
        .map(|e| e.when_start)
        .collect();
    assert!(before.windows(2).all(|w| w[0] >= w[1]), "{before:?}");
    assert_eq!(s.events_from(1_000, 2)?.len(), 2, "limit applies");
    Ok(())
}

#[test]
fn todo_upsert_keeps_completion_and_event_link() -> R {
    let s = Store::open_in_memory()?;
    let ev = s.upsert_event(
        &NewEvent {
            when_start: 10_000,
            title: "Taxes".into(),
            ..Default::default()
        },
        1,
    )?;
    let key = Some("hotword:3:reminder:2026-12-05:taxes".to_owned());
    let r = s.add_todo(
        &NewTodo {
            event_id: Some(ev.id),
            due: Some(5_000),
            text: "Taxes in a week".into(),
            source: Some(Origin::Hotword),
            source_segment_id: Some(3),
            dedupe_key: key.clone(),
            ..Default::default()
        },
        1,
    )?;
    s.complete_todo(r.id, 7)?;
    let again = s.add_todo(
        &NewTodo {
            due: Some(6_000),
            text: "Taxes soon".into(),
            source: Some(Origin::Interpretation),
            dedupe_key: key,
            ..Default::default()
        },
        2,
    )?;
    assert_eq!(again.id, r.id);
    assert_eq!(
        (again.due, again.text.as_str()),
        (Some(6_000), "Taxes soon")
    );
    assert_eq!(
        again.event_id,
        Some(ev.id),
        "a missing event keeps the link"
    );
    assert!(again.done(), "re-dictating doesn't reopen a completed todo");
    assert_eq!(again.source, Origin::Hotword);
    assert_eq!(again.source_segment_id, Some(3));
    assert_eq!(again.created_at, 1);

    // ordering and the open filter
    let earliest = s.add_todo(
        &NewTodo {
            due: Some(1_000),
            text: "earliest".into(),
            ..Default::default()
        },
        3,
    )?;
    let all: Vec<i64> = s.todos(TodoFilter::All)?.iter().map(|r| r.id).collect();
    assert_eq!(all, vec![earliest.id, r.id], "open first, completed last");
    let open: Vec<i64> = s.todos(TodoFilter::Open)?.iter().map(|r| r.id).collect();
    assert_eq!(open, vec![earliest.id]);
    s.reopen_todo(r.id)?;
    assert_eq!(s.todos(TodoFilter::Open)?.len(), 2);
    Ok(())
}

// ---- node helpers and what they do to filed rows ----------------------------------------------

/// A category with a project and a topic, something of every kind filed at each level.
struct Filed {
    s: Store,
    work: i64,
    admin: i64,
    project: i64,
    topic: i64,
    segs: [i64; 3],
    eps: [i64; 3],
    events: [i64; 3],
    notes: [i64; 3],
}

fn filed() -> Result<Filed, StoreError> {
    let s = Store::open_in_memory()?;
    let cats = s.categories()?;
    let (work, admin) = (cats[0].id, cats[1].id);
    let project = s
        .add_node(Some(work), NodeKind::Project, "Exporter", "", Origin::Auto)?
        .id;
    let topic = s
        .add_node(Some(project), NodeKind::Topic, "CSV", "", Origin::Auto)?
        .id;
    let sid = s.begin_session(0, "e")?;
    let mut segs = [0; 3];
    let mut eps = [0; 3];
    let mut events = [0; 3];
    let mut notes = [0; 3];
    for (i, node) in [work, project, topic].into_iter().enumerate() {
        let t = i as i64 * 100;
        let seg = s.insert_segment(sid, t, t + 10, &format!("seg {i}"), None)?;
        s.set_segment_node(seg.id, Some(node))?;
        segs[i] = seg.id;
        let ep = s.insert_episode(
            t,
            t + 10,
            &EpisodeContent {
                title: format!("ep {i}"),
                node_id: Some(node),
                ..Default::default()
            },
            1,
        )?;
        s.mark_segments_interpreted(&[seg.id], ep.id, 2)?;
        eps[i] = ep.id;
        events[i] = s
            .upsert_event(
                &NewEvent {
                    when_start: t,
                    title: format!("event {i}"),
                    node_id: Some(node),
                    source_episode_id: Some(ep.id),
                    dedupe_key: Some(format!("k{i}")),
                    ..Default::default()
                },
                1,
            )?
            .id;
        notes[i] = s
            .insert_note(
                Some(node),
                &format!("note {i}"),
                Origin::Hotword,
                Some(seg.id),
                1,
            )?
            .id;
    }
    Ok(Filed {
        s,
        work,
        admin,
        project,
        topic,
        segs,
        eps,
        events,
        notes,
    })
}

/// `(segments, episodes, events, notes)` node ids, in creation order.
fn filing(f: &Filed) -> Result<[Vec<Option<i64>>; 4], StoreError> {
    let s = &f.s;
    let segs = f
        .segs
        .iter()
        .map(|id| s.segment(*id).map(|r| r.and_then(|r| r.node_id)))
        .collect::<Result<Vec<_>, _>>()?;
    let eps = f
        .eps
        .iter()
        .map(|id| s.episode(*id).map(|e| e.and_then(|e| e.node_id)))
        .collect::<Result<Vec<_>, _>>()?;
    let all_events = s.events_from(i64::MIN, 100)?;
    let events = f
        .events
        .iter()
        .map(|id| {
            all_events
                .iter()
                .find(|e| e.id == *id)
                .and_then(|e| e.node_id)
        })
        .collect();
    let all_notes = s.notes(100)?;
    let notes = f
        .notes
        .iter()
        .map(|id| {
            all_notes
                .iter()
                .find(|n| n.id == *id)
                .and_then(|n| n.node_id)
        })
        .collect();
    Ok([segs, eps, events, notes])
}

#[test]
fn delete_category_unfiles_everything_in_the_subtree() -> R {
    let f = filed()?;
    let other_seg = {
        let sid = f.s.begin_session(0, "e")?;
        let r = f.s.insert_segment(sid, 900, 910, "admin stuff", None)?;
        f.s.set_segment_node(r.id, Some(f.admin))?;
        r.id
    };
    f.s.delete_category(f.work)?;
    for col in filing(&f)? {
        assert_eq!(col, vec![None, None, None]);
    }
    for id in [f.work, f.project, f.topic] {
        assert!(f.s.node(id)?.is_none(), "node {id} deleted");
    }
    // nothing else is lost: rows stay, episodes keep their segments, other filing untouched
    assert_eq!(f.s.segments(View::All, 0)?.len(), 4);
    assert_eq!(f.s.episodes(10)?.len(), 3);
    assert_eq!(f.s.segments_of_episode(f.eps[1])?.len(), 1);
    assert_eq!(f.s.events_from(i64::MIN, 10)?.len(), 3);
    assert_eq!(f.s.notes(10)?.len(), 3);
    assert_eq!(
        f.s.segment(other_seg)?.and_then(|r| r.node_id),
        Some(f.admin)
    );
    assert_eq!(
        f.s.categories()?
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["Admin", "Personal"]
    );
    // the views of the deleted nodes are simply empty
    assert!(f.s.segments(View::Node(f.work), 0)?.is_empty());
    Ok(())
}

#[test]
fn deleting_a_project_leaves_its_category_alone() -> R {
    let f = filed()?;
    f.s.delete_node(f.project)?;
    let [segs, eps, events, notes] = filing(&f)?;
    let want = vec![Some(f.work), None, None];
    assert_eq!(
        (segs, eps, events, notes),
        (want.clone(), want.clone(), want.clone(), want)
    );
    assert!(f.s.node(f.topic)?.is_none());
    assert_eq!(f.s.subtree_ids(f.work)?, vec![f.work]);
    Ok(())
}

#[test]
fn merge_moves_everything_and_reparents_children() -> R {
    let f = filed()?;
    let s = &f.s;
    let target = s.add_node(
        Some(f.admin),
        NodeKind::Project,
        "Paperwork",
        "",
        Origin::Auto,
    )?;
    s.merge_nodes(f.project, target.id)?;
    let [segs, eps, events, notes] = filing(&f)?;
    let want = vec![Some(f.work), Some(target.id), Some(f.topic)];
    assert_eq!(segs, want);
    assert_eq!(eps, want);
    assert_eq!(events, want);
    assert_eq!(notes, want);
    assert!(s.node(f.project)?.is_none());
    assert_eq!(s.node(f.topic)?.and_then(|n| n.parent_id), Some(target.id));
    assert_eq!(s.subtree_ids(f.admin)?, vec![f.admin, target.id, f.topic]);
    assert_eq!(s.segments(View::Node(f.admin), 0)?.len(), 2);

    // refused: into itself, into its own descendant (would orphan a cycle)
    s.merge_nodes(target.id, target.id)?;
    s.merge_nodes(target.id, f.topic)?;
    assert!(s.node(target.id)?.is_some());
    assert_eq!(s.node(f.topic)?.and_then(|n| n.parent_id), Some(target.id));
    // a merge of a node that is gone is harmless
    s.merge_nodes(f.project, target.id)?;
    assert_eq!(s.nodes()?.len(), 5);
    Ok(())
}

#[test]
fn move_node_appends_and_refuses_cycles() -> R {
    let f = filed()?;
    let s = &f.s;
    let first = s.add_node(Some(f.admin), NodeKind::Project, "A", "", Origin::Auto)?;
    let second = s.add_node(Some(f.admin), NodeKind::Project, "B", "", Origin::Auto)?;
    assert_eq!((first.position, second.position), (0, 1));
    s.move_node(f.project, Some(f.admin))?;
    let moved = s.node(f.project)?.unwrap();
    assert_eq!((moved.parent_id, moved.position), (Some(f.admin), 2));
    // the filed rows travel with the node (they point at the node, not the category)
    assert_eq!(s.segments(View::Node(f.admin), 0)?.len(), 2);
    assert_eq!(s.segments(View::Node(f.work), 0)?.len(), 1);
    // under itself and under its own topic: refused
    s.move_node(f.project, Some(f.project))?;
    s.move_node(f.project, Some(f.topic))?;
    assert_eq!(s.node(f.project)?.and_then(|n| n.parent_id), Some(f.admin));
    // to the root: it becomes a top-level node
    s.move_node(f.topic, None)?;
    assert_eq!(s.node(f.topic)?.and_then(|n| n.parent_id), None);
    assert!(
        s.nodes()?
            .iter()
            .any(|n| n.id == f.topic && n.parent_id.is_none())
    );
    Ok(())
}

#[test]
fn archive_hides_categories_and_add_revives_by_name() -> R {
    let f = filed()?;
    let s = &f.s;
    s.set_node_archived(f.work, true)?;
    assert!(!s.categories()?.iter().any(|c| c.id == f.work));
    assert!(
        s.nodes()?.iter().any(|n| n.id == f.work && n.archived),
        "still in the tree"
    );
    // archiving changes no filing, and the view still answers
    assert_eq!(s.segments(View::Node(f.work), 0)?.len(), 3);
    let revived = s.add_category("  WORK ")?;
    assert_eq!(revived.id, f.work);
    assert!(!revived.archived);
    assert!(s.categories()?.iter().any(|c| c.id == f.work));
    // find_node is per parent: the same name under another parent is a different node
    let elsewhere = s.add_node(
        Some(f.admin),
        NodeKind::Project,
        "exporter",
        "",
        Origin::Auto,
    )?;
    assert_ne!(elsewhere.id, f.project);
    assert_eq!(
        s.find_node(Some(f.work), "EXPORTER")?.map(|n| n.id),
        Some(f.project)
    );
    assert_eq!(s.find_node(None, "exporter")?, None);
    // descriptions and renames are trimmed
    s.set_node_description(f.project, "  the exporter  ")?;
    s.rename_node(f.project, "  Exporter v2 ")?;
    let n = s.node(f.project)?.unwrap();
    assert_eq!(
        (n.name.as_str(), n.description.as_str()),
        ("Exporter v2", "the exporter")
    );
    Ok(())
}

#[test]
fn episode_merge_and_reset_carry_events_and_segments() -> R {
    let f = filed()?;
    let s = &f.s;
    s.merge_episodes(f.eps[1], f.eps[0])?;
    assert!(s.episode(f.eps[1])?.is_none());
    let into = s.episode(f.eps[0])?.unwrap();
    assert_eq!((into.started_at, into.ended_at), (0, 110));
    assert_eq!(s.segments_of_episode(f.eps[0])?.len(), 2);
    let evs: Vec<i64> = s
        .events_of_episode(f.eps[0])?
        .iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(
        evs,
        vec![f.events[0], f.events[1]],
        "events follow the merge"
    );

    // reset: the segments become uninterpreted, the events stay (now pointing nowhere)
    assert_eq!(s.uninterpreted_count()?, 0);
    s.reset_episode(f.eps[0])?;
    assert_eq!(s.uninterpreted_count()?, 2);
    assert!(s.segments_of_episode(f.eps[0])?.is_empty());
    assert_eq!(s.events_from(i64::MIN, 10)?.len(), 3);
    // consumed segments are out of both automatic passes
    let sid = s.begin_session(0, "e")?;
    let cmd = s.insert_segment(sid, 5_000, 5_010, "hey computer", None)?;
    s.mark_segment_consumed(cmd.id, 9)?;
    assert!(
        s.uninterpreted_segments(100)?
            .iter()
            .all(|r| r.id != cmd.id)
    );
    assert!(
        s.uncategorised_segments(100)?
            .iter()
            .all(|r| r.id != cmd.id)
    );
    let cmd = s.segment(cmd.id)?.unwrap();
    assert_eq!((cmd.episode_id, cmd.interpreted_at), (None, Some(9)));
    Ok(())
}

#[test]
fn node_usage_counts_episodes_per_node() -> R {
    let f = filed()?;
    let s = &f.s;
    s.insert_episode(
        5_000,
        5_010,
        &EpisodeContent {
            node_id: Some(f.topic),
            ..Default::default()
        },
        1,
    )?;
    let mut usage = s.node_usage()?;
    usage.sort();
    assert_eq!(
        usage,
        vec![(f.work, 1, 0), (f.project, 1, 100), (f.topic, 2, 5_000)]
    );
    Ok(())
}

// ---- the change log --------------------------------------------------------------------------

#[test]
fn changes_are_listed_newest_first_and_undone_once() -> R {
    let s = Store::open_in_memory()?;
    let run = s.begin_run(3, "reorganise", "opus", 10)?;
    let a = s.record_change(
        Some(run),
        "node_rename",
        "first",
        &serde_json::json!({"node_id": 1, "name": "Old"}),
        &serde_json::json!({"node_id": 1, "name": "New"}),
        100,
    )?;
    let b = s.record_change(
        None,
        "node_create",
        "second",
        &serde_json::Value::Null,
        &serde_json::json!({}),
        200,
    )?;
    let c = s.record_change(
        None,
        "node_archive",
        "same time, later id",
        &serde_json::json!({}),
        &serde_json::json!({}),
        200,
    )?;
    let order: Vec<i64> = s.changes(10)?.iter().map(|c| c.id).collect();
    assert_eq!(order, vec![c, b, a]);
    assert_eq!(s.changes(1)?.len(), 1);
    let first = s.change(a)?.unwrap();
    assert_eq!(first.run_id, Some(run));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&first.before_json)?,
        serde_json::json!({"node_id": 1, "name": "Old"})
    );
    assert_eq!(s.change(b)?.unwrap().before_json, "null");
    assert!(!first.undone);
    s.mark_change_undone(a)?;
    s.mark_change_undone(a)?;
    assert!(s.change(a)?.unwrap().undone);
    assert!(!s.change(b)?.unwrap().undone);
    assert_eq!(s.change(9_999)?, None);
    Ok(())
}

#[test]
fn runs_budget_counts_window_boundaries() -> R {
    let s = Store::open_in_memory()?;
    for (i, (start, cost)) in [(1_000, Some(0.5)), (2_000, None), (3_000, Some(0.25))]
        .into_iter()
        .enumerate()
    {
        let id = s.begin_run(i as i64, "interpret", "sonnet", start)?;
        s.finish_run(
            id,
            &oats_core::RunOutcome {
                finished_at: start + 1,
                cost_usd: cost,
                ok: cost.is_some(),
                ..Default::default()
            },
        )?;
    }
    assert!(
        (s.cost_since(1_000)? - 0.75).abs() < 1e-12,
        "inclusive lower bound"
    );
    assert!((s.cost_since(1_001)? - 0.25).abs() < 1e-12);
    assert_eq!(s.cost_since(3_001)?, 0.0);
    assert_eq!(s.run_count_since(2_000)?, 2);
    // an unfinished run counts towards the hourly cap but isn't "the last run"
    s.begin_run(1, "review", "sonnet", 4_000)?;
    assert_eq!(s.run_count_since(0)?, 4);
    assert_eq!(s.last_run_of("review")?, None);
    let last = s.last_run_of("interpret")?.unwrap();
    assert_eq!(last.started_at, 3_000);
    // finish_run without a model keeps the one the run started with
    assert_eq!(last.model, "sonnet");
    Ok(())
}

// ---- counts ----------------------------------------------------------------------------------

#[test]
fn segment_count_since_is_inclusive_and_follows_deletes() -> R {
    let s = Store::open_in_memory()?;
    assert_eq!(s.segment_count_since(i64::MIN)?, 0);
    let sid = s.begin_session(0, "e")?;
    let a = s.insert_segment(sid, -5_000, -4_000, "before the epoch", None)?;
    let b = s.insert_segment(sid, 1_000, 2_000, "on the boundary", None)?;
    s.insert_segment(sid, 1_001, 2_000, "just after", None)?;
    assert_eq!(s.segment_count_since(1_000)?, 2);
    assert_eq!(s.segment_count_since(1_001)?, 1);
    assert_eq!(s.segment_count_since(1_002)?, 0);
    assert_eq!(s.segment_count_since(i64::MIN)?, 3);
    // the count is by start: a segment that ends after `since` but started before isn't counted
    assert_eq!(s.segment_count_since(1_500)?, 0);
    s.delete_segment(b.id)?;
    assert_eq!(s.segment_count_since(1_000)?, 1);
    s.delete_segment(a.id)?;
    s.delete_segment(a.id)?; // twice is harmless
    assert_eq!(s.segment_count_since(i64::MIN)?, 1);
    // segments_between is half-open
    assert_eq!(s.segments_between(1_000, 1_001)?.len(), 0);
    assert_eq!(s.segments_between(1_001, 1_002)?.len(), 1);
    Ok(())
}

// ---- several connections on one file ---------------------------------------------------------

#[test]
fn a_second_connection_sees_committed_writes() -> R {
    let dir = TempDir::new("two-conns");
    let path = dir.db();
    let writer = Store::open(&path)?;
    let reader = Store::open(&path)?;
    assert_eq!(
        reader.categories()?.len(),
        3,
        "the second open didn't seed twice"
    );
    assert_eq!(writer.nodes()?.len(), 3);
    assert_eq!(reader.segment_count_since(i64::MIN)?, 0);

    let sid = writer.begin_session(0, "e")?;
    let row = writer.insert_segment(sid, 10, 20, "hello", Some(0.75))?;
    assert_eq!(reader.segment(row.id)?, Some(row.clone()));
    assert_eq!(reader.segment_count_since(i64::MIN)?, 1);

    // a multi-row write (one transaction) appears all at once
    let more: Vec<i64> = (0..5)
        .map(|i| {
            writer
                .insert_segment(sid, 100 + i, 110 + i, "x", None)
                .map(|r| r.id)
        })
        .collect::<Result<_, _>>()?;
    let work = writer.categories()?[0].id;
    let filed: Vec<(i64, Option<i64>)> = more.iter().map(|id| (*id, Some(work))).collect();
    writer.apply_categorisation(&filed)?;
    assert_eq!(reader.segments(View::Node(work), 0)?.len(), 5);

    // and the other way round: the reader's edit shows up in the writer
    reader.set_segment_node(row.id, Some(work))?;
    assert_eq!(writer.segments(View::Node(work), 0)?.len(), 6);
    Ok(())
}

#[test]
fn concurrent_writers_on_separate_threads_lose_nothing() {
    let dir = TempDir::new("concurrent");
    let path = dir.db();
    // create the schema once, so the threads race on writes and not on creation
    drop(Store::open(&path).unwrap());
    const PER_THREAD: i64 = 150;
    let barrier = Arc::new(Barrier::new(3));
    let spawn_writer = |tag: &'static str| {
        let path = path.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || -> Result<(), StoreError> {
            let s = Store::open(&path)?;
            let sid = s.begin_session(0, tag)?;
            barrier.wait();
            for i in 0..PER_THREAD {
                let row = s.insert_segment(sid, i, i + 1, tag, None)?;
                if i % 10 == 0 {
                    // a transaction in the mix (the brain's batch writes)
                    s.apply_categorisation(&[(row.id, None)])?;
                }
            }
            Ok(())
        })
    };
    let a = spawn_writer("pipeline");
    let b = spawn_writer("brain");
    let reader = {
        let path = path.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || -> Result<i64, StoreError> {
            let s = Store::open(&path)?;
            barrier.wait();
            // counts only ever grow while the writers run
            let mut last = 0;
            for _ in 0..50 {
                let n = s.segment_count_since(i64::MIN)?;
                assert!(n >= last, "count went backwards: {last} → {n}");
                last = n;
            }
            Ok(last)
        })
    };
    a.join().unwrap().unwrap();
    b.join().unwrap().unwrap();
    reader.join().unwrap().unwrap();
    let s = Store::open(&path).unwrap();
    let rows = s.segments(View::All, 0).unwrap();
    assert_eq!(rows.len() as i64, 2 * PER_THREAD);
    for tag in ["pipeline", "brain"] {
        assert_eq!(
            rows.iter().filter(|r| r.text == tag).count() as i64,
            PER_THREAD,
            "{tag}"
        );
    }
    assert_eq!(
        rows.iter().filter(|r| r.categorised).count() as i64,
        2 * PER_THREAD / 10
    );
}

#[test]
fn store_open_creates_nested_directories_and_wal() -> R {
    let dir = TempDir::new("nested");
    let path = dir.0.join("a").join("b").join("oats.sqlite");
    let s = Store::open(&path)?;
    let sid = s.begin_session(0, "e")?;
    s.insert_segment(sid, 1, 2, "x", None)?;
    assert!(path.is_file());
    let mode: String = raw(&path).query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    assert_eq!(mode.to_lowercase(), "wal");
    Ok(())
}

#[test]
fn events_of_a_reset_episode_are_not_adopted_by_the_next_episode() -> R {
    let s = Store::open_in_memory()?;
    let content = |title: &str| EpisodeContent {
        title: title.into(),
        ..Default::default()
    };
    let a = s.insert_episode(0, 10, &content("a"), 1)?;
    s.upsert_event(
        &NewEvent {
            when_start: 5,
            title: "from a".into(),
            source_episode_id: Some(a.id),
            dedupe_key: Some(format!("ep:{}:1970-01-01:from a", a.id)),
            ..Default::default()
        },
        1,
    )?;
    s.reset_episode(a.id)?;
    let b = s.insert_episode(1_000_000, 1_000_010, &content("unrelated"), 2)?;
    let adopted = s.events_of_episode(b.id)?;
    assert!(
        adopted.is_empty(),
        "episode {} (reset was {}) inherited {adopted:?}",
        b.id,
        a.id
    );
    Ok(())
}
