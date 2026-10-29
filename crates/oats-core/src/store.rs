//! The transcript store: one SQLite file (WAL mode), opened once per thread that uses it (the
//! pipeline writes segments, the brain writes interpretations, the UI reads). Times are Unix
//! milliseconds (UTC); the UI formats them in local time.
//!
//! Schema version 5 (`meta.schema_version`; v1 to v4 databases are migrated on open; v5 added
//! `episodes.user_edited_at`, the time the user last edited a note by hand):
//! - `sessions(id, started_at, ended_at NULL, engine)` — one per listen start/stop.
//! - `nodes(id, parent_id NULL, kind, name, description, created_by, position, archived,
//!   created_at)` — ONE tree: the user's top-level categories (`kind = category`, seeded
//!   Work / Admin / Personal), the projects the brain creates under them, and topics under
//!   projects. Segments, episodes, events and notes link to the most specific node.
//! - `segments(id, session_id, started_at, ended_at, text, confidence NULL, node_id NULL,
//!   episode_id NULL, interpreted_at NULL, categorised, source, echo_of NULL)` — one per
//!   transcribed speech segment, indexed on `started_at`. `categorised` = the cheap
//!   per-segment pass (or the user) filed it; `interpreted_at` = when it last went through
//!   episode interpretation. `source` = `mic` | `system` (the lane; v3, default `mic`);
//!   `echo_of` = the system segment a microphone segment merely repeats (the speakers heard
//!   by the microphone, see [`crate::echo`]); echoes are kept but left out of every listing
//!   the UI and the brain use.
//! - `episodes(…)` — a "meeting"/conversation the brain cut out of the day and interpreted:
//!   title, markdown summary, node, decisions / action items / topics as JSON, `low_signal`
//!   for TV / music / self-talk, the model and cost, and a `revision` counter bumped by every
//!   re-interpretation of the rolling window.
//! - `events(…)` — the calendar: significant dates the brain extracted or the user dictated
//!   with a hotword; `dedupe_key` keeps re-interpretation from duplicating them.
//! - `reminders(id, event_id NULL, due NULL, text, done, completed_at NULL, snoozed_until NULL,
//!   source, source_segment_id NULL, node_id NULL, position, dedupe_key, created_at)` — the
//!   TODOS (the table keeps its v3 name): a reminder is a todo with a `due` time, which pops
//!   up in the window when it comes (or when its snooze ends). Open = `completed_at IS NULL`;
//!   `done` is kept in sync for the record (v4 rebuilt the v3 table: `due` became nullable).
//! - `notes(…)` — free-text notes filed under a node by a hotword command.
//! - `brain_runs(…)` — every brain job with tier, model, cost, duration and outcome (the daily
//!   budget is summed from here); `changes(…)` — structural changes a run made (node
//!   merges/renames/moves, episode merges) with before/after JSON for audit and undo.

use std::path::Path;

use crate::engine::AudioSourceKind;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("json column: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the database schema is version {0}, newer than this build understands")]
    SchemaTooNew(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub engine: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptRow {
    pub id: i64,
    pub session_id: i64,
    pub started_at: i64,
    pub ended_at: i64,
    pub text: String,
    pub confidence: Option<f32>,
    /// The most specific node it is filed under (a category, project or topic).
    pub node_id: Option<i64>,
    pub episode_id: Option<i64>,
    pub interpreted_at: Option<i64>,
    pub categorised: bool,
    /// The lane it was heard on: the microphone ("me") or system audio ("them").
    pub source: AudioSourceKind,
    /// For a microphone segment that only repeats a system segment (speaker echo): that
    /// segment's id. Echoes are hidden from the default views and the brain.
    pub echo_of: Option<i64>,
}

/// What [`Store::search`] found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchResults {
    /// Transcript lines, newest first (echoes excluded).
    pub segments: Vec<TranscriptRow>,
    pub episodes: Vec<Episode>,
    pub events: Vec<Event>,
}

/// What kind of node in the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    /// A top-level bucket (Work / Admin / Personal); user-managed.
    Category,
    /// Ongoing work or a life thread under a category ("Export rewrite", "Apartment").
    Project,
    /// A sub-topic under a project.
    Topic,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Category => "category",
            NodeKind::Project => "project",
            NodeKind::Topic => "topic",
        }
    }

    fn parse(s: &str) -> NodeKind {
        match s {
            "project" => NodeKind::Project,
            "topic" => NodeKind::Topic,
            _ => NodeKind::Category,
        }
    }
}

/// Who created a row: the user, the brain's interpretation, or a hotword command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    User,
    Auto,
    Interpretation,
    Hotword,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::User => "user",
            Origin::Auto => "auto",
            Origin::Interpretation => "interpretation",
            Origin::Hotword => "hotword",
        }
    }

    fn parse(s: &str) -> Origin {
        match s {
            "auto" => Origin::Auto,
            "interpretation" => Origin::Interpretation,
            "hotword" => Origin::Hotword,
            _ => Origin::User,
        }
    }
}

/// A node of the category → project → topic tree.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Node {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub kind: NodeKind,
    pub name: String,
    pub description: String,
    pub created_by: Origin,
    pub position: i64,
    pub archived: bool,
}

/// An action item inside an episode.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActionItem {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
}

/// An interpreted episode (a meeting / conversation).
#[derive(Clone, Debug, PartialEq)]
pub struct Episode {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: i64,
    pub title: String,
    /// Markdown.
    pub summary: String,
    pub node_id: Option<i64>,
    pub decisions: Vec<String>,
    pub action_items: Vec<ActionItem>,
    pub topics: Vec<String>,
    pub low_signal: bool,
    pub model: String,
    pub cost_usd: Option<f64>,
    pub revision: i64,
    pub created_at: i64,
    pub last_revised_at: i64,
    /// Set when the user edited the note by hand ([`Store::update_episode_content`]); the
    /// brain then never rewrites its text.
    pub user_edited_at: Option<i64>,
}

/// The interpreted content of an episode (what a revision writes).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EpisodeContent {
    pub title: String,
    pub summary: String,
    pub node_id: Option<i64>,
    pub decisions: Vec<String>,
    pub action_items: Vec<ActionItem>,
    pub topics: Vec<String>,
    pub low_signal: bool,
    pub model: String,
    pub cost_usd: Option<f64>,
}

/// A calendar event.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: i64,
    pub when_start: i64,
    pub when_end: Option<i64>,
    pub all_day: bool,
    pub title: String,
    pub details: String,
    pub node_id: Option<i64>,
    pub source: Origin,
    pub source_segment_id: Option<i64>,
    pub source_episode_id: Option<i64>,
    pub confidence: Option<f64>,
    pub dedupe_key: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// An event to insert or update (matched on `dedupe_key` when set).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewEvent {
    pub when_start: i64,
    pub when_end: Option<i64>,
    pub all_day: bool,
    pub title: String,
    pub details: String,
    pub node_id: Option<i64>,
    pub source: Option<Origin>,
    pub source_segment_id: Option<i64>,
    pub source_episode_id: Option<i64>,
    pub confidence: Option<f64>,
    pub dedupe_key: Option<String>,
}

/// A todo. Todos and reminders are one thing: a reminder is a todo with a `due` time, and it
/// pops up in the window when that time comes (see [`Store::due_todos`]).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Todo {
    pub id: i64,
    /// The event it belongs to (a hotword event's own todo, or "one week ahead" of it).
    pub event_id: Option<i64>,
    pub text: String,
    /// When it comes due (Unix ms); `None` = an undated todo (never pops up).
    pub due: Option<i64>,
    /// When it was completed; `None` = open.
    pub completed_at: Option<i64>,
    /// Snoozed: don't pop up before this time (Unix ms), whatever `due` says.
    pub snoozed_until: Option<i64>,
    pub source: Origin,
    pub source_segment_id: Option<i64>,
    /// The node it is filed under, if any.
    pub node_id: Option<i64>,
    /// Manual order among the undated todos (new ones go last).
    pub position: i64,
    pub dedupe_key: Option<String>,
    pub created_at: i64,
}

impl Todo {
    pub fn done(&self) -> bool {
        self.completed_at.is_some()
    }

    /// When it next pops up: the end of its snooze, else its due time; `None` when it is
    /// completed, or undated and not snoozed.
    pub fn alert_at(&self) -> Option<i64> {
        if self.done() {
            None
        } else {
            self.snoozed_until.or(self.due)
        }
    }

    /// Whether it should be showing as a pop-up at `now`.
    pub fn is_due(&self, now: i64) -> bool {
        self.alert_at().is_some_and(|at| at <= now)
    }
}

/// A todo to insert (or update, matched on `dedupe_key` when set). `source` defaults to the
/// user.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewTodo {
    pub text: String,
    pub due: Option<i64>,
    pub event_id: Option<i64>,
    pub source: Option<Origin>,
    pub source_segment_id: Option<i64>,
    pub node_id: Option<i64>,
    pub dedupe_key: Option<String>,
}

/// Which todos [`Store::todos`] lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TodoFilter {
    Open,
    Completed,
    #[default]
    All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub id: i64,
    pub node_id: Option<i64>,
    pub text: String,
    pub source: Origin,
    pub source_segment_id: Option<i64>,
    pub created_at: i64,
}

/// One brain job.
#[derive(Clone, Debug, PartialEq)]
pub struct BrainRun {
    pub id: i64,
    pub tier: i64,
    pub kind: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub model: String,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<i64>,
    pub ok: bool,
    pub error: Option<String>,
    pub summary: String,
}

/// How a run ended.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOutcome {
    pub finished_at: i64,
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<i64>,
    pub ok: bool,
    pub error: Option<String>,
    pub summary: String,
}

/// A structural change a run made, recorded for audit and undo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub id: i64,
    pub run_id: Option<i64>,
    pub kind: String,
    pub description: String,
    pub before_json: String,
    pub after_json: String,
    pub undone: bool,
    pub created_at: i64,
}

/// What the transcript list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum View {
    /// Segments since local midnight.
    #[default]
    Today,
    /// Everything.
    All,
    /// Segments filed under one node or anything below it.
    Node(i64),
    /// The interpreted episodes (the Notes view).
    Notes,
    /// The month calendar: events and dated todos.
    Calendar,
    /// The todo list (reminders are todos with a due time).
    Todos,
}

/// The default categories, in sidebar order.
pub const DEFAULT_CATEGORIES: [&str; 3] = ["Work", "Admin", "Personal"];

pub struct Store {
    conn: rusqlite::Connection,
}

const SCHEMA_VERSION: i64 = 5;

/// The v2 schema, first half (meta, sessions, nodes, episodes); every statement is idempotent so
/// it doubles as the "make sure everything exists" step.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);
CREATE TABLE IF NOT EXISTS sessions (
    id         INTEGER PRIMARY KEY,
    started_at INTEGER NOT NULL,
    ended_at   INTEGER,
    engine     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS nodes (
    id          INTEGER PRIMARY KEY,
    parent_id   INTEGER REFERENCES nodes(id),
    kind        TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_by  TEXT NOT NULL DEFAULT 'user',
    position    INTEGER NOT NULL,
    archived    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS nodes_parent ON nodes(parent_id);
CREATE TABLE IF NOT EXISTS episodes (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at        INTEGER NOT NULL,
    ended_at          INTEGER NOT NULL,
    title             TEXT NOT NULL,
    summary           TEXT NOT NULL,
    node_id           INTEGER REFERENCES nodes(id),
    decisions_json    TEXT NOT NULL DEFAULT '[]',
    action_items_json TEXT NOT NULL DEFAULT '[]',
    topics_json       TEXT NOT NULL DEFAULT '[]',
    low_signal        INTEGER NOT NULL DEFAULT 0,
    model             TEXT NOT NULL DEFAULT '',
    cost_usd          REAL,
    revision          INTEGER NOT NULL DEFAULT 1,
    created_at        INTEGER NOT NULL,
    last_revised_at   INTEGER NOT NULL,
    user_edited_at    INTEGER
);
CREATE INDEX IF NOT EXISTS episodes_started_at ON episodes(started_at);
";

/// The rest of the v2 schema (needs `nodes` and `episodes`; on a v1 database the `segments`
/// rebuild runs in between).
const SCHEMA_REST: &str = "
CREATE TABLE IF NOT EXISTS segments (
    id             INTEGER PRIMARY KEY,
    session_id     INTEGER NOT NULL REFERENCES sessions(id),
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER NOT NULL,
    text           TEXT NOT NULL,
    confidence     REAL,
    node_id        INTEGER REFERENCES nodes(id),
    episode_id     INTEGER REFERENCES episodes(id),
    interpreted_at INTEGER,
    categorised    INTEGER NOT NULL DEFAULT 0,
    source         TEXT NOT NULL DEFAULT 'mic',
    echo_of        INTEGER
);
CREATE INDEX IF NOT EXISTS segments_started_at ON segments(started_at);
CREATE INDEX IF NOT EXISTS segments_node ON segments(node_id);
CREATE INDEX IF NOT EXISTS segments_episode ON segments(episode_id);
CREATE TABLE IF NOT EXISTS events (
    id                INTEGER PRIMARY KEY,
    when_start        INTEGER NOT NULL,
    when_end          INTEGER,
    all_day           INTEGER NOT NULL DEFAULT 0,
    title             TEXT NOT NULL,
    details           TEXT NOT NULL DEFAULT '',
    node_id           INTEGER REFERENCES nodes(id),
    source            TEXT NOT NULL,
    source_segment_id INTEGER,
    source_episode_id INTEGER,
    confidence        REAL,
    dedupe_key        TEXT UNIQUE,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS events_when ON events(when_start);
CREATE TABLE IF NOT EXISTS notes (
    id                INTEGER PRIMARY KEY,
    node_id           INTEGER REFERENCES nodes(id),
    text              TEXT NOT NULL,
    source            TEXT NOT NULL,
    source_segment_id INTEGER,
    created_at        INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS brain_runs (
    id          INTEGER PRIMARY KEY,
    tier        INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,
    model       TEXT NOT NULL DEFAULT '',
    cost_usd    REAL,
    duration_ms INTEGER,
    ok          INTEGER NOT NULL DEFAULT 0,
    error       TEXT,
    summary     TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS brain_runs_started ON brain_runs(started_at);
CREATE TABLE IF NOT EXISTS changes (
    id          INTEGER PRIMARY KEY,
    run_id      INTEGER REFERENCES brain_runs(id),
    kind        TEXT NOT NULL,
    description TEXT NOT NULL,
    before_json TEXT NOT NULL,
    after_json  TEXT NOT NULL,
    undone      INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);
";

/// v1 → v2: `categories` become the root `nodes` (same ids, so nothing else changes meaning),
/// `segments` is rebuilt with the new columns (`category_id` → `node_id`; a filed segment
/// counts as categorised), the old table goes.
const MIGRATE_V1_TO_V2: &str = "
INSERT INTO nodes(id, parent_id, kind, name, description, created_by, position, archived, created_at)
    SELECT id, NULL, 'category', name, '', 'user', position, 0, 0 FROM categories;
CREATE TABLE segments_v2 (
    id             INTEGER PRIMARY KEY,
    session_id     INTEGER NOT NULL REFERENCES sessions(id),
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER NOT NULL,
    text           TEXT NOT NULL,
    confidence     REAL,
    node_id        INTEGER REFERENCES nodes(id),
    episode_id     INTEGER REFERENCES episodes(id),
    interpreted_at INTEGER,
    categorised    INTEGER NOT NULL DEFAULT 0
);
INSERT INTO segments_v2(id, session_id, started_at, ended_at, text, confidence, node_id, categorised)
    SELECT id, session_id, started_at, ended_at, text, confidence, category_id,
           CASE WHEN category_id IS NULL THEN 0 ELSE 1 END
    FROM segments;
DROP INDEX IF EXISTS segments_started_at;
DROP INDEX IF EXISTS segments_category;
DROP TABLE segments;
ALTER TABLE segments_v2 RENAME TO segments;
DROP TABLE categories;
";

/// v2 → v3: the lane a segment came from and the echo link (columns added in place; every
/// existing row is a microphone segment and nobody's echo).
const MIGRATE_V2_TO_V3: &str = "
ALTER TABLE segments ADD COLUMN source TEXT NOT NULL DEFAULT 'mic';
ALTER TABLE segments ADD COLUMN echo_of INTEGER;
";

const SEGMENT_COLUMNS: &str = "id, session_id, started_at, ended_at, text, confidence, node_id, episode_id, interpreted_at, categorised, source, echo_of";

/// Longer than any stored segment (the segmenter caps them at 30 s): how far before a
/// window [`Store::segments_near`] looks for segments reaching into it.
const NEAR_MAX_SEGMENT_MS: i64 = 10 * 60_000;

/// Echoes (see [`crate::echo`]) are never part of a listing.
const NOT_ECHO: &str = "echo_of IS NULL";

fn row_to_segment(row: &rusqlite::Row<'_>) -> rusqlite::Result<TranscriptRow> {
    Ok(TranscriptRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        text: row.get(4)?,
        confidence: row.get::<_, Option<f64>>(5)?.map(|c| c as f32),
        node_id: row.get(6)?,
        episode_id: row.get(7)?,
        interpreted_at: row.get(8)?,
        categorised: row.get::<_, i64>(9)? != 0,
        source: AudioSourceKind::parse(&row.get::<_, String>(10)?),
        echo_of: row.get(11)?,
    })
}

const NODE_COLUMNS: &str = "id, parent_id, kind, name, description, created_by, position, archived";

fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        kind: NodeKind::parse(&row.get::<_, String>(2)?),
        name: row.get(3)?,
        description: row.get(4)?,
        created_by: Origin::parse(&row.get::<_, String>(5)?),
        position: row.get(6)?,
        archived: row.get::<_, i64>(7)? != 0,
    })
}

const EPISODE_COLUMNS: &str = "id, started_at, ended_at, title, summary, node_id, decisions_json, \
     action_items_json, topics_json, low_signal, model, cost_usd, revision, created_at, \
     last_revised_at, user_edited_at";

fn row_to_episode(row: &rusqlite::Row<'_>) -> rusqlite::Result<Episode> {
    let json = |i: usize| -> rusqlite::Result<String> { row.get(i) };
    let decisions: Vec<String> = serde_json::from_str(&json(6)?).unwrap_or_default();
    let action_items: Vec<ActionItem> = serde_json::from_str(&json(7)?).unwrap_or_default();
    let topics: Vec<String> = serde_json::from_str(&json(8)?).unwrap_or_default();
    Ok(Episode {
        id: row.get(0)?,
        started_at: row.get(1)?,
        ended_at: row.get(2)?,
        title: row.get(3)?,
        summary: row.get(4)?,
        node_id: row.get(5)?,
        decisions,
        action_items,
        topics,
        low_signal: row.get::<_, i64>(9)? != 0,
        model: row.get(10)?,
        cost_usd: row.get(11)?,
        revision: row.get(12)?,
        created_at: row.get(13)?,
        last_revised_at: row.get(14)?,
        user_edited_at: row.get(15)?,
    })
}

const EVENT_COLUMNS: &str = "id, when_start, when_end, all_day, title, details, node_id, source, \
     source_segment_id, source_episode_id, confidence, dedupe_key, created_at, updated_at";

fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: row.get(0)?,
        when_start: row.get(1)?,
        when_end: row.get(2)?,
        all_day: row.get::<_, i64>(3)? != 0,
        title: row.get(4)?,
        details: row.get(5)?,
        node_id: row.get(6)?,
        source: Origin::parse(&row.get::<_, String>(7)?),
        source_segment_id: row.get(8)?,
        source_episode_id: row.get(9)?,
        confidence: row.get(10)?,
        dedupe_key: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

/// The `reminders` table (the todos) as of schema v4: `due` nullable, `completed_at`
/// replacing the meaning of `done` (kept, in sync, for the record), the snooze, the node, a
/// manual order. Created by [`Store::ensure_todos_table`], which also rebuilds a v3 table.
const TODOS_TABLE: &str = "(
    id                INTEGER PRIMARY KEY,
    event_id          INTEGER REFERENCES events(id),
    due               INTEGER,
    text              TEXT NOT NULL,
    done              INTEGER NOT NULL DEFAULT 0,
    completed_at      INTEGER,
    snoozed_until     INTEGER,
    source            TEXT NOT NULL DEFAULT 'hotword',
    source_segment_id INTEGER,
    node_id           INTEGER REFERENCES nodes(id),
    position          INTEGER NOT NULL DEFAULT 0,
    dedupe_key        TEXT UNIQUE,
    created_at        INTEGER NOT NULL
)";

const TODO_COLUMNS: &str = "id, event_id, text, due, completed_at, snoozed_until, source, \
     source_segment_id, node_id, position, dedupe_key, created_at";

fn row_to_todo(row: &rusqlite::Row<'_>) -> rusqlite::Result<Todo> {
    Ok(Todo {
        id: row.get(0)?,
        event_id: row.get(1)?,
        text: row.get(2)?,
        due: row.get(3)?,
        completed_at: row.get(4)?,
        snoozed_until: row.get(5)?,
        source: Origin::parse(&row.get::<_, String>(6)?),
        source_segment_id: row.get(7)?,
        node_id: row.get(8)?,
        position: row.get(9)?,
        dedupe_key: row.get(10)?,
        created_at: row.get(11)?,
    })
}

const RUN_COLUMNS: &str =
    "id, tier, kind, started_at, finished_at, model, cost_usd, duration_ms, ok, error, summary";

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<BrainRun> {
    Ok(BrainRun {
        id: row.get(0)?,
        tier: row.get(1)?,
        kind: row.get(2)?,
        started_at: row.get(3)?,
        finished_at: row.get(4)?,
        model: row.get(5)?,
        cost_usd: row.get(6)?,
        duration_ms: row.get(7)?,
        ok: row.get::<_, i64>(8)? != 0,
        error: row.get(9)?,
        summary: row.get(10)?,
    })
}

fn row_to_change(row: &rusqlite::Row<'_>) -> rusqlite::Result<Change> {
    Ok(Change {
        id: row.get(0)?,
        run_id: row.get(1)?,
        kind: row.get(2)?,
        description: row.get(3)?,
        before_json: row.get(4)?,
        after_json: row.get(5)?,
        undone: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
    })
}

/// Case- and whitespace-insensitive key for matching node names ("Export  Rewrite" ==
/// "export rewrite"), the anti-sprawl rule.
pub fn name_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The subtree rooted at `?1`, as a CTE named `sub(id)`.
const SUBTREE_CTE: &str = "WITH RECURSIVE sub(id) AS (
        SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN sub ON n.parent_id = sub.id)";

/// A nested write transaction: the outermost level is `BEGIN IMMEDIATE` (it takes the write
/// lock up front, waiting under the busy timeout, so a read-then-write body can't fail with
/// `SQLITE_BUSY_SNAPSHOT` when another connection wrote in between); inner levels are
/// `SAVEPOINT`s. Rolled back on drop unless [`Savepoint::commit`] ran, so a body that fails
/// half-way leaves nothing behind.
struct Savepoint<'a> {
    conn: &'a rusqlite::Connection,
    outer: bool,
    done: bool,
}

impl<'a> Savepoint<'a> {
    fn begin(conn: &'a rusqlite::Connection) -> Result<Self, StoreError> {
        let outer = conn.is_autocommit();
        conn.execute_batch(if outer {
            "BEGIN IMMEDIATE;"
        } else {
            "SAVEPOINT oats_sp;"
        })?;
        Ok(Self {
            conn,
            outer,
            done: false,
        })
    }

    fn commit(mut self) -> Result<(), StoreError> {
        self.done = true;
        self.conn.execute_batch(if self.outer {
            "COMMIT;"
        } else {
            "RELEASE oats_sp;"
        })?;
        Ok(())
    }
}

impl Drop for Savepoint<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.conn.execute_batch(if self.outer {
                "ROLLBACK;"
            } else {
                "ROLLBACK TO oats_sp; RELEASE oats_sp;"
            });
        }
    }
}

impl std::ops::Deref for Savepoint<'_> {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &rusqlite::Connection {
        self.conn
    }
}

/// Owner-only permissions for the database: the directory is created 0700 when missing, the
/// file (and an existing `-wal` / `-shm`, which SQLite otherwise creates with the main file's
/// mode) 0600 — before SQLite opens it, so it never exists world-readable. Unix only.
#[cfg(unix)]
fn secure_db_path(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    for suffix in ["", "-wal", "-shm"] {
        let p = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        if p.exists() {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_db_path(path: &Path) -> Result<(), StoreError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

impl Store {
    /// Open (creating the file, its parent directory and the schema as needed; migrating an
    /// older schema). The directory is created 0700 and the file 0600 (unix), and deleted
    /// content is overwritten (`secure_delete`).
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        secure_db_path(path)?;
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA secure_delete=ON;")?;
        // switching a fresh file to WAL needs an exclusive lock and SQLite answers SQLITE_BUSY
        // at once (no busy handler) while another connection is opening it too
        let mut tries = 0;
        loop {
            match conn.execute_batch("PRAGMA journal_mode=WAL;") {
                Ok(()) => break,
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::DatabaseBusy && tries < 100 =>
                {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(e.into()),
            }
        }
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        Self::init(conn)
    }

    /// An in-memory store (tests).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self, StoreError> {
        // the rebuild of `segments` must run with foreign keys off (the pragma is a no-op
        // inside a transaction, so it is set here, before anything else)
        conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
        // the UI, the pipeline and the brain each open their own connection, at about the same
        // moment: the version check, the migration and the seeding of the default categories
        // run under one write lock, so two openers can't both migrate or both seed
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let store = Self { conn };
        if let Err(e) = store.init_locked() {
            let _ = store.conn.execute_batch("ROLLBACK;");
            return Err(e);
        }
        store.conn.execute_batch("COMMIT;")?;
        store.conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(store)
    }

    /// [`Self::init`]'s body, inside its transaction.
    fn init_locked(&self) -> Result<(), StoreError> {
        let conn = &self.conn;
        match Self::schema_version(conn)? {
            None => {
                conn.execute_batch(SCHEMA)?;
                conn.execute_batch(SCHEMA_REST)?;
            }
            Some(1) => {
                log::info!("migrating the database from schema 1 to {SCHEMA_VERSION}");
                conn.execute_batch(SCHEMA)?;
                conn.execute_batch(MIGRATE_V1_TO_V2)?;
                conn.execute_batch(MIGRATE_V2_TO_V3)?;
                conn.execute_batch(SCHEMA_REST)?;
            }
            Some(2) => {
                log::info!("migrating the database from schema 2 to {SCHEMA_VERSION}");
                conn.execute_batch(SCHEMA)?;
                conn.execute_batch(MIGRATE_V2_TO_V3)?;
                conn.execute_batch(SCHEMA_REST)?;
            }
            Some(3) => {
                // v3 → v4 is the todos table, rebuilt by `ensure_todos_table` below
                log::info!("migrating the database from schema 3 to {SCHEMA_VERSION}");
                conn.execute_batch(SCHEMA)?;
                conn.execute_batch(SCHEMA_REST)?;
            }
            Some(4) => {
                // v4 → v5 is `episodes.user_edited_at`, added by `ensure_episode_edit_column`
                log::info!("migrating the database from schema 4 to {SCHEMA_VERSION}");
                conn.execute_batch(SCHEMA)?;
                conn.execute_batch(SCHEMA_REST)?;
            }
            Some(v) if v == SCHEMA_VERSION => {
                conn.execute_batch(SCHEMA)?;
                conn.execute_batch(SCHEMA_REST)?;
            }
            Some(v) => return Err(StoreError::SchemaTooNew(v)),
        }
        // before the AUTOINCREMENT rebuild, which copies `SELECT *` into the current layout
        Self::ensure_episode_edit_column(conn)?;
        Self::episode_ids_never_reused(conn)?;
        Self::ensure_todos_table(conn)?;
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
            [SCHEMA_VERSION.to_string()],
        )?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE parent_id IS NULL",
            [],
            |r| r.get(0),
        )?;
        if count == 0 {
            for name in DEFAULT_CATEGORIES {
                self.add_category(name)?;
            }
        }
        Ok(())
    }

    /// `episodes.user_edited_at` (v5): added in place to an older table (last column, so the
    /// rebuild in [`Self::episode_ids_never_reused`] still lines up).
    fn ensure_episode_edit_column(conn: &rusqlite::Connection) -> Result<(), StoreError> {
        let has: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('episodes') WHERE name = 'user_edited_at'",
            [],
            |r| r.get(0),
        )?;
        if has == 0 {
            conn.execute_batch("ALTER TABLE episodes ADD COLUMN user_edited_at INTEGER;")?;
        }
        Ok(())
    }

    /// Episode ids are referenced loosely (`events.source_episode_id`, dedupe keys
    /// `ep:<id>:…`), so a deleted episode's id must never come back: a database created before
    /// `episodes` was `AUTOINCREMENT` has the table rebuilt in place (same columns, same ids;
    /// still schema version 2, which older builds read unchanged), and the id sequence starts
    /// above every id anything still points at. Runs inside `init`'s transaction with foreign
    /// keys off.
    fn episode_ids_never_reused(conn: &rusqlite::Connection) -> Result<(), StoreError> {
        let sql: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'episodes'",
            [],
            |r| r.get(0),
        )?;
        if sql.to_uppercase().contains("AUTOINCREMENT") {
            return Ok(());
        }
        log::info!("upgrading the episodes table to never reuse ids");
        let create = SCHEMA
            .split("CREATE TABLE IF NOT EXISTS episodes")
            .nth(1)
            .and_then(|rest| rest.split(");").next())
            .map(|cols| format!("CREATE TABLE episodes_new{cols});"))
            .ok_or_else(|| StoreError::Io(std::io::Error::other("episodes schema not found")))?;
        conn.execute_batch(&create)?;
        conn.execute_batch(
            "INSERT INTO episodes_new SELECT * FROM episodes;
             DROP TABLE episodes;
             ALTER TABLE episodes_new RENAME TO episodes;
             CREATE INDEX IF NOT EXISTS episodes_started_at ON episodes(started_at);
             DELETE FROM sqlite_sequence WHERE name = 'episodes';
             INSERT INTO sqlite_sequence(name, seq) VALUES ('episodes', (SELECT MAX(m) FROM (
                 SELECT COALESCE(MAX(id), 0) AS m FROM episodes
                 UNION ALL SELECT COALESCE(MAX(source_episode_id), 0) FROM events
                 UNION ALL SELECT COALESCE(MAX(episode_id), 0) FROM segments)));",
        )?;
        Ok(())
    }

    /// The todos (`reminders`) table in its v4 form: created when missing; a v3 table (`due`
    /// NOT NULL, a `done` flag) rebuilt in place — same ids, `completed_at` = `created_at`
    /// for the done ones (the moment they were ticked was never recorded), `position` = id.
    /// Runs inside `init`'s transaction with foreign keys off; idempotent.
    fn ensure_todos_table(conn: &rusqlite::Connection) -> Result<(), StoreError> {
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('reminders')")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        if columns.is_empty() {
            conn.execute_batch(&format!("CREATE TABLE reminders {TODOS_TABLE};"))?;
        } else if !columns.iter().any(|c| c == "completed_at") {
            log::info!("upgrading the reminders table to todos");
            conn.execute_batch(&format!(
                "CREATE TABLE reminders_v4 {TODOS_TABLE};
                 INSERT INTO reminders_v4(id, event_id, due, text, done, completed_at,
                     snoozed_until, source, source_segment_id, node_id, position, dedupe_key,
                     created_at)
                 SELECT id, event_id, due, text, done,
                     CASE WHEN done != 0 THEN created_at END, NULL, source, source_segment_id,
                     NULL, id, dedupe_key, created_at
                 FROM reminders;
                 DROP TABLE reminders;
                 ALTER TABLE reminders_v4 RENAME TO reminders;"
            ))?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS reminders_due ON reminders(due);
             CREATE INDEX IF NOT EXISTS reminders_open ON reminders(completed_at);",
        )?;
        Ok(())
    }

    /// `None` for a fresh database (no `meta` table or no version row).
    fn schema_version(conn: &rusqlite::Connection) -> Result<Option<i64>, StoreError> {
        use rusqlite::OptionalExtension;
        let has_meta: bool = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !has_meta {
            return Ok(None);
        }
        let v: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|s| s.parse().ok()))
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    // ---- meta -------------------------------------------------------------------------

    pub fn meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            [key, value],
        )?;
        Ok(())
    }

    /// The user's own name (`meta.user_name`), `None` when unset or blank.
    pub fn user_name(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .meta("user_name")?
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty()))
    }

    /// Set the user's name; a blank name clears it.
    pub fn set_user_name(&self, name: &str) -> Result<(), StoreError> {
        let name = name.trim();
        if name.is_empty() {
            self.conn
                .execute("DELETE FROM meta WHERE key = 'user_name'", [])?;
            Ok(())
        } else {
            self.set_meta("user_name", name)
        }
    }

    // ---- sessions ---------------------------------------------------------------------

    pub fn begin_session(&self, started_at: i64, engine: &str) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO sessions(started_at, engine) VALUES (?1, ?2)",
            rusqlite::params![started_at, engine],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn end_session(&self, id: i64, ended_at: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?2 WHERE id = ?1",
            rusqlite::params![id, ended_at],
        )?;
        Ok(())
    }

    /// Close sessions a dead process left open (`ended_at` NULL, started before `before`):
    /// their end becomes the last segment's end, or their start when they have none. Call once
    /// at startup, before the pipeline opens a session of its own; returns how many were closed.
    pub fn close_dangling_sessions(&self, before: i64) -> Result<usize, StoreError> {
        Ok(self.conn.execute(
            "UPDATE sessions SET ended_at = COALESCE(
                 (SELECT MAX(ended_at) FROM segments WHERE session_id = sessions.id), started_at)
             WHERE ended_at IS NULL AND started_at < ?1",
            rusqlite::params![before],
        )?)
    }

    // ---- segments ---------------------------------------------------------------------

    /// Insert a microphone segment; returns the full row.
    pub fn insert_segment(
        &self,
        session_id: i64,
        started_at: i64,
        ended_at: i64,
        text: &str,
        confidence: Option<f32>,
    ) -> Result<TranscriptRow, StoreError> {
        self.insert_segment_from(
            session_id,
            started_at,
            ended_at,
            text,
            confidence,
            AudioSourceKind::Mic,
            None,
        )
    }

    /// Insert a segment heard on `source`, optionally as the echo of a system segment;
    /// returns the full row.
    pub fn insert_segment_from(
        &self,
        session_id: i64,
        started_at: i64,
        ended_at: i64,
        text: &str,
        confidence: Option<f32>,
        source: AudioSourceKind,
        echo_of: Option<i64>,
    ) -> Result<TranscriptRow, StoreError> {
        self.conn.execute(
            "INSERT INTO segments(session_id, started_at, ended_at, text, confidence, source, echo_of)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                session_id,
                started_at,
                ended_at,
                text,
                confidence.map(f64::from),
                source.as_str(),
                echo_of
            ],
        )?;
        Ok(TranscriptRow {
            id: self.conn.last_insert_rowid(),
            session_id,
            started_at,
            ended_at,
            text: text.to_owned(),
            confidence,
            node_id: None,
            episode_id: None,
            interpreted_at: None,
            categorised: false,
            source,
            echo_of,
        })
    }

    /// Segments heard on `source` whose span reaches into `[from, to]` (ms), echoes left out,
    /// oldest first — the candidates of the echo rule.
    pub fn segments_near(
        &self,
        source: AudioSourceKind,
        from: i64,
        to: i64,
    ) -> Result<Vec<TranscriptRow>, StoreError> {
        // the lower bound on `started_at` keeps this an index range (segments are ≤ 30 s
        // long; without it `started_at <= to` matches the whole history: a full scan per
        // transcribed segment, all day)
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SEGMENT_COLUMNS} FROM segments
             WHERE started_at >= ?4 AND started_at <= ?3
               AND source = ?1 AND {NOT_ECHO} AND ended_at >= ?2
             ORDER BY started_at, id"
        ))?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    source.as_str(),
                    from,
                    to,
                    from.saturating_sub(NEAR_MAX_SEGMENT_MS)
                ],
                row_to_segment,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark a segment as the echo of `echo_of` (or clear the mark with `None`).
    pub fn set_segment_echo(&self, id: i64, echo_of: Option<i64>) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE segments SET echo_of = ?2 WHERE id = ?1",
            rusqlite::params![id, echo_of],
        )?;
        Ok(())
    }

    /// The echoes recorded for a system segment, oldest first (audit; never listed elsewhere).
    pub fn echoes_of(&self, system_id: i64) -> Result<Vec<TranscriptRow>, StoreError> {
        self.query_segments(
            &format!(
                "SELECT {SEGMENT_COLUMNS} FROM segments WHERE echo_of = ?1 ORDER BY started_at, id"
            ),
            &[system_id],
        )
    }

    fn query_segments(&self, sql: &str, args: &[i64]) -> Result<Vec<TranscriptRow>, StoreError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), row_to_segment)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Segments for a view, oldest first. `today_start` is local midnight in Unix ms (the UI
    /// computes it; the store stays timezone-free). The Notes and Calendar views have no
    /// segments.
    pub fn segments(&self, view: View, today_start: i64) -> Result<Vec<TranscriptRow>, StoreError> {
        let order = "ORDER BY started_at, id";
        match view {
            View::Today => self.query_segments(
                &format!(
                    "SELECT {SEGMENT_COLUMNS} FROM segments WHERE started_at >= ?1 AND {NOT_ECHO} {order}"
                ),
                &[today_start],
            ),
            View::All => self.query_segments(
                &format!("SELECT {SEGMENT_COLUMNS} FROM segments WHERE {NOT_ECHO} {order}"),
                &[],
            ),
            View::Node(id) => self.query_segments(
                &format!(
                    "{SUBTREE_CTE} SELECT {SEGMENT_COLUMNS} FROM segments
                     WHERE node_id IN (SELECT id FROM sub) AND {NOT_ECHO} {order}"
                ),
                &[id],
            ),
            View::Notes | View::Calendar | View::Todos => Ok(Vec::new()),
        }
    }

    /// Segments with `started_at` in `[from, to)`, oldest first.
    pub fn segments_between(&self, from: i64, to: i64) -> Result<Vec<TranscriptRow>, StoreError> {
        self.query_segments(
            &format!(
                "SELECT {SEGMENT_COLUMNS} FROM segments
                 WHERE started_at >= ?1 AND started_at < ?2 AND {NOT_ECHO}
                 ORDER BY started_at, id"
            ),
            &[from, to],
        )
    }

    pub fn segment(&self, id: i64) -> Result<Option<TranscriptRow>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SEGMENT_COLUMNS} FROM segments WHERE id = ?1"),
                [id],
                row_to_segment,
            )
            .optional()?)
    }

    /// Segments of an episode, oldest first.
    pub fn segments_of_episode(&self, episode_id: i64) -> Result<Vec<TranscriptRow>, StoreError> {
        self.query_segments(
            &format!(
                "SELECT {SEGMENT_COLUMNS} FROM segments WHERE episode_id = ?1 AND {NOT_ECHO}
                 ORDER BY started_at, id"
            ),
            &[episode_id],
        )
    }

    /// Segments the cheap categorisation pass hasn't seen, oldest first (at most `limit`).
    pub fn uncategorised_segments(&self, limit: i64) -> Result<Vec<TranscriptRow>, StoreError> {
        self.query_segments(
            &format!(
                "SELECT {SEGMENT_COLUMNS} FROM segments WHERE categorised = 0 AND {NOT_ECHO}
                 ORDER BY started_at, id LIMIT ?1"
            ),
            &[limit],
        )
    }

    /// Segments never interpreted into an episode, oldest first (at most `limit`).
    pub fn uninterpreted_segments(&self, limit: i64) -> Result<Vec<TranscriptRow>, StoreError> {
        self.query_segments(
            &format!(
                "SELECT {SEGMENT_COLUMNS} FROM segments WHERE interpreted_at IS NULL AND {NOT_ECHO}
                 ORDER BY started_at, id LIMIT ?1"
            ),
            &[limit],
        )
    }

    pub fn uninterpreted_count(&self) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE interpreted_at IS NULL AND echo_of IS NULL",
            [],
            |r| r.get(0),
        )?)
    }

    /// File a segment under a node (`None` = unfiled). Marks it categorised either way (the
    /// user decided), so the automatic pass leaves it alone.
    pub fn set_segment_node(
        &self,
        segment_id: i64,
        node_id: Option<i64>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE segments SET node_id = ?2, categorised = 1 WHERE id = ?1",
            rusqlite::params![segment_id, node_id],
        )?;
        Ok(())
    }

    /// The automatic categorisation pass's result for a batch: `(segment_id, node_id)`.
    /// Segments the user already filed are left as they are.
    pub fn apply_categorisation(&self, filed: &[(i64, Option<i64>)]) -> Result<(), StoreError> {
        let tx = Savepoint::begin(&self.conn)?;
        for (segment_id, node_id) in filed {
            tx.execute(
                "UPDATE segments SET node_id = ?2, categorised = 1 WHERE id = ?1 AND categorised = 0",
                rusqlite::params![segment_id, node_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Attach segments to an episode and stamp them interpreted.
    pub fn mark_segments_interpreted(
        &self,
        segment_ids: &[i64],
        episode_id: i64,
        at: i64,
    ) -> Result<(), StoreError> {
        let tx = Savepoint::begin(&self.conn)?;
        for id in segment_ids {
            tx.execute(
                "UPDATE segments SET episode_id = ?2, interpreted_at = ?3 WHERE id = ?1",
                rusqlite::params![id, episode_id, at],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete a segment (the user's own delete): the text is overwritten in the file
    /// (`secure_delete`) and the WAL is checkpointed and truncated so no copy lingers there.
    pub fn delete_segment(&self, segment_id: i64) -> Result<(), StoreError> {
        // its echoes repeat the same words: they go with it
        self.conn.execute(
            "DELETE FROM segments WHERE id = ?1 OR echo_of = ?1",
            [segment_id],
        )?;
        self.checkpoint();
        Ok(())
    }

    /// `PRAGMA wal_checkpoint(TRUNCATE)` after a user-initiated delete, so the deleted text
    /// doesn't survive in the WAL. Best effort (a no-op inside a transaction or while another
    /// connection is mid-read; the next checkpoint catches up).
    pub fn checkpoint(&self) {
        if !self.conn.is_autocommit() {
            return;
        }
        if let Err(e) = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);") {
            log::debug!("wal checkpoint: {e}");
        }
    }

    /// Run `f` as one transaction (nested calls become savepoints): if it returns an error,
    /// everything it wrote is rolled back.
    pub fn with_savepoint<R>(
        &self,
        f: impl FnOnce(&Self) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let sp = Savepoint::begin(&self.conn)?;
        let r = f(self)?;
        sp.commit()?;
        Ok(r)
    }

    /// Count of segments whose `started_at` ≥ `since` (the status line's "N today").
    pub fn segment_count_since(&self, since: i64) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE started_at >= ?1 AND echo_of IS NULL",
            [since],
            |r| r.get(0),
        )?)
    }

    // ---- nodes ------------------------------------------------------------------------

    /// Every node, parents before children (by depth, then position, then id).
    pub fn nodes(&self) -> Result<Vec<Node>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes ORDER BY parent_id IS NOT NULL, position, id"
        ))?;
        let mut all = stmt
            .query_map([], row_to_node)?
            .collect::<Result<Vec<_>, _>>()?;
        // order as a tree walk: roots in position order, each followed by its subtree
        let mut out = Vec::with_capacity(all.len());
        let roots: Vec<Node> = all
            .iter()
            .filter(|n| n.parent_id.is_none())
            .cloned()
            .collect();
        fn walk(parent: &Node, all: &mut Vec<Node>, out: &mut Vec<Node>) {
            out.push(parent.clone());
            let mut children: Vec<Node> = all
                .iter()
                .filter(|n| n.parent_id == Some(parent.id))
                .cloned()
                .collect();
            children.sort_by_key(|c| (c.position, c.id));
            for c in children {
                walk(&c, all, out);
            }
        }
        for r in roots {
            walk(&r, &mut all, &mut out);
        }
        Ok(out)
    }

    /// The top-level categories in sidebar order (archived ones excluded).
    pub fn categories(&self) -> Result<Vec<Node>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE parent_id IS NULL AND archived = 0
             ORDER BY position, id"
        ))?;
        let rows = stmt
            .query_map([], row_to_node)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn node(&self, id: i64) -> Result<Option<Node>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = ?1"),
                [id],
                row_to_node,
            )
            .optional()?)
    }

    /// A child of `parent` (`None` = a root) whose name matches `name` under [`name_key`].
    pub fn find_node(&self, parent: Option<i64>, name: &str) -> Result<Option<Node>, StoreError> {
        let key = name_key(name);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE parent_id IS ?1 ORDER BY position, id"
        ))?;
        let rows = stmt
            .query_map([parent], row_to_node)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().find(|n| name_key(&n.name) == key))
    }

    /// Append a top-level category (trimmed; a duplicate name returns the existing one).
    pub fn add_category(&self, name: &str) -> Result<Node, StoreError> {
        self.add_node(None, NodeKind::Category, name, "", Origin::User)
    }

    /// Append a node under `parent` (trimmed; a name already there, case-insensitively,
    /// returns the existing node, un-archiving it).
    pub fn add_node(
        &self,
        parent: Option<i64>,
        kind: NodeKind,
        name: &str,
        description: &str,
        created_by: Origin,
    ) -> Result<Node, StoreError> {
        let name = name.trim();
        if let Some(existing) = self.find_node(parent, name)? {
            if existing.archived {
                self.set_node_archived(existing.id, false)?;
                return Ok(Node {
                    archived: false,
                    ..existing
                });
            }
            return Ok(existing);
        }
        self.conn.execute(
            "INSERT INTO nodes(parent_id, kind, name, description, created_by, position, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5,
                     (SELECT COALESCE(MAX(position), -1) + 1 FROM nodes WHERE parent_id IS ?1),
                     ?6)",
            rusqlite::params![
                parent,
                kind.as_str(),
                name,
                description.trim(),
                created_by.as_str(),
                Self::now_ms()
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(self.conn.query_row(
            &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = ?1"),
            [id],
            row_to_node,
        )?)
    }

    pub fn rename_node(&self, id: i64, name: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE nodes SET name = ?2 WHERE id = ?1",
            rusqlite::params![id, name.trim()],
        )?;
        Ok(())
    }

    pub fn set_node_description(&self, id: i64, description: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE nodes SET description = ?2 WHERE id = ?1",
            rusqlite::params![id, description.trim()],
        )?;
        Ok(())
    }

    pub fn set_node_archived(&self, id: i64, archived: bool) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE nodes SET archived = ?2 WHERE id = ?1",
            rusqlite::params![id, i64::from(archived)],
        )?;
        Ok(())
    }

    /// Re-parent a node (`None` = make it a root). Refuses a move under its own subtree.
    pub fn move_node(&self, id: i64, new_parent: Option<i64>) -> Result<(), StoreError> {
        if let Some(p) = new_parent
            && self.subtree_ids(id)?.contains(&p)
        {
            return Ok(());
        }
        self.conn.execute(
            "UPDATE nodes SET parent_id = ?2,
                position = (SELECT COALESCE(MAX(position), -1) + 1 FROM nodes WHERE parent_id IS ?2)
             WHERE id = ?1",
            rusqlite::params![id, new_parent],
        )?;
        Ok(())
    }

    /// Ids of `id` and everything below it.
    pub fn subtree_ids(&self, id: i64) -> Result<Vec<i64>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(&format!("{SUBTREE_CTE} SELECT id FROM sub"))?;
        let ids = stmt
            .query_map([id], |r| r.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(ids)
    }

    /// Move everything filed under `from` (segments, episodes, events, notes) to `into` and
    /// re-parent `from`'s children there; `from` is deleted. The merge the reorganisation
    /// tier and the sprawl guard use.
    pub fn merge_nodes(&self, from: i64, into: i64) -> Result<(), StoreError> {
        if from == into || self.subtree_ids(from)?.contains(&into) {
            return Ok(());
        }
        let tx = Savepoint::begin(&self.conn)?;
        for table in ["segments", "episodes", "events", "notes", "reminders"] {
            tx.execute(
                &format!("UPDATE {table} SET node_id = ?2 WHERE node_id = ?1"),
                [from, into],
            )?;
        }
        tx.execute(
            "UPDATE nodes SET parent_id = ?2 WHERE parent_id = ?1",
            [from, into],
        )?;
        tx.execute("DELETE FROM nodes WHERE id = ?1", [from])?;
        tx.commit()?;
        Ok(())
    }

    /// Delete a node and its subtree; whatever was filed there goes back to unfiled.
    pub fn delete_node(&self, id: i64) -> Result<(), StoreError> {
        let ids = self.subtree_ids(id)?;
        let tx = Savepoint::begin(&self.conn)?;
        for nid in ids.iter().rev() {
            for table in ["segments", "episodes", "events", "notes", "reminders"] {
                tx.execute(
                    &format!("UPDATE {table} SET node_id = NULL WHERE node_id = ?1"),
                    [nid],
                )?;
            }
            tx.execute("DELETE FROM nodes WHERE id = ?1", [nid])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `rename_node` under its old name, kept for callers that think in categories.
    pub fn rename_category(&self, id: i64, name: &str) -> Result<(), StoreError> {
        self.rename_node(id, name)
    }

    /// `delete_node` under its old name.
    pub fn delete_category(&self, id: i64) -> Result<(), StoreError> {
        self.delete_node(id)
    }

    // ---- episodes ---------------------------------------------------------------------

    pub fn insert_episode(
        &self,
        started_at: i64,
        ended_at: i64,
        content: &EpisodeContent,
        now: i64,
    ) -> Result<Episode, StoreError> {
        self.conn.execute(
            "INSERT INTO episodes(started_at, ended_at, title, summary, node_id, decisions_json,
                 action_items_json, topics_json, low_signal, model, cost_usd, revision,
                 created_at, last_revised_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, ?12, ?12)",
            rusqlite::params![
                started_at,
                ended_at,
                content.title,
                content.summary,
                content.node_id,
                serde_json::to_string(&content.decisions)?,
                serde_json::to_string(&content.action_items)?,
                serde_json::to_string(&content.topics)?,
                i64::from(content.low_signal),
                content.model,
                content.cost_usd,
                now
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.episode(id)?
            .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows.into())
    }

    /// Replace an episode's content and time range; bumps `revision`, adds the cost.
    pub fn revise_episode(
        &self,
        id: i64,
        started_at: i64,
        ended_at: i64,
        content: &EpisodeContent,
        now: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE episodes SET started_at = ?2, ended_at = ?3, title = ?4, summary = ?5,
                 node_id = ?6, decisions_json = ?7, action_items_json = ?8, topics_json = ?9,
                 low_signal = ?10, model = ?11,
                 cost_usd = COALESCE(cost_usd, 0) + COALESCE(?12, 0),
                 revision = revision + 1, last_revised_at = ?13
             WHERE id = ?1",
            rusqlite::params![
                id,
                started_at,
                ended_at,
                content.title,
                content.summary,
                content.node_id,
                serde_json::to_string(&content.decisions)?,
                serde_json::to_string(&content.action_items)?,
                serde_json::to_string(&content.topics)?,
                i64::from(content.low_signal),
                content.model,
                content.cost_usd,
                now
            ],
        )?;
        Ok(())
    }

    /// The user's own edit of a note: replaces title, summary, decisions, action items and
    /// topics (not the node, low-signal flag, model or cost), stamps `user_edited_at = now`
    /// and leaves `revision` alone. One transaction. The brain never content-revises an
    /// episode with `user_edited_at` set.
    pub fn update_episode_content(
        &self,
        id: i64,
        content: &EpisodeContent,
        now: i64,
    ) -> Result<(), StoreError> {
        self.with_savepoint(|s| {
            s.conn.execute(
                "UPDATE episodes SET title = ?2, summary = ?3, decisions_json = ?4,
                     action_items_json = ?5, topics_json = ?6, user_edited_at = ?7
                 WHERE id = ?1",
                rusqlite::params![
                    id,
                    content.title,
                    content.summary,
                    serde_json::to_string(&content.decisions)?,
                    serde_json::to_string(&content.action_items)?,
                    serde_json::to_string(&content.topics)?,
                    now
                ],
            )?;
            Ok(())
        })
    }

    pub fn set_episode_low_signal(&self, id: i64, low_signal: bool) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE episodes SET low_signal = ?2 WHERE id = ?1",
            rusqlite::params![id, i64::from(low_signal)],
        )?;
        Ok(())
    }

    /// Per node: `(node_id, episode count, latest episode start)` for nodes with episodes.
    pub fn node_usage(&self) -> Result<Vec<(i64, i64, i64)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT node_id, COUNT(*), MAX(started_at) FROM episodes
             WHERE node_id IS NOT NULL GROUP BY node_id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// A segment handled outside interpretation (a hotword command): stamped interpreted and
    /// categorised, attached to no episode, so no later pass picks it up.
    pub fn mark_segment_consumed(&self, segment_id: i64, at: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE segments SET interpreted_at = ?2, categorised = 1 WHERE id = ?1",
            rusqlite::params![segment_id, at],
        )?;
        Ok(())
    }

    pub fn set_episode_node(&self, id: i64, node_id: Option<i64>) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE episodes SET node_id = ?2 WHERE id = ?1",
            rusqlite::params![id, node_id],
        )?;
        Ok(())
    }

    pub fn episode(&self, id: i64) -> Result<Option<Episode>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {EPISODE_COLUMNS} FROM episodes WHERE id = ?1"),
                [id],
                row_to_episode,
            )
            .optional()?)
    }

    fn query_episodes(&self, sql: &str, args: &[i64]) -> Result<Vec<Episode>, StoreError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), row_to_episode)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Case-insensitive substring search over transcript text (echoes excluded, newest
    /// first), episodes (title, summary, decisions, action items; newest first) and events
    /// (title, details; newest first). Each list holds at most `limit` hits; a blank query
    /// finds nothing. `%`, `_` and `\\` in the query match themselves.
    pub fn search(&self, query: &str, limit: i64) -> Result<SearchResults, StoreError> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(SearchResults::default());
        }
        let mut pat = String::with_capacity(q.len() + 2);
        pat.push('%');
        for c in q.chars() {
            if matches!(c, '%' | '_' | '\\') {
                pat.push('\\');
            }
            pat.push(c);
        }
        pat.push('%');
        let run =
            |sql: &str| -> rusqlite::Result<rusqlite::Statement<'_>> { self.conn.prepare(sql) };
        let mut stmt = run(&format!(
            "SELECT {SEGMENT_COLUMNS} FROM segments WHERE {NOT_ECHO} AND text LIKE ?1 ESCAPE '\\'
             ORDER BY started_at DESC, id DESC LIMIT ?2"
        ))?;
        let segments = stmt
            .query_map(rusqlite::params![pat, limit], row_to_segment)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut stmt = run(&format!(
            "SELECT {EPISODE_COLUMNS} FROM episodes WHERE title LIKE ?1 ESCAPE '\\'
                OR summary LIKE ?1 ESCAPE '\\' OR decisions_json LIKE ?1 ESCAPE '\\'
                OR action_items_json LIKE ?1 ESCAPE '\\'
             ORDER BY started_at DESC, id DESC LIMIT ?2"
        ))?;
        let episodes = stmt
            .query_map(rusqlite::params![pat, limit], row_to_episode)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut stmt = run(&format!(
            "SELECT {EVENT_COLUMNS} FROM events WHERE title LIKE ?1 ESCAPE '\\'
                OR details LIKE ?1 ESCAPE '\\'
             ORDER BY when_start DESC, id DESC LIMIT ?2"
        ))?;
        let events = stmt
            .query_map(rusqlite::params![pat, limit], row_to_event)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SearchResults {
            segments,
            episodes,
            events,
        })
    }

    /// Newest first (at most `limit`).
    pub fn episodes(&self, limit: i64) -> Result<Vec<Episode>, StoreError> {
        self.query_episodes(
            &format!(
                "SELECT {EPISODE_COLUMNS} FROM episodes ORDER BY started_at DESC, id DESC LIMIT ?1"
            ),
            &[limit],
        )
    }

    /// Episodes filed under a node or anything below it, newest first.
    pub fn episodes_under(&self, node_id: i64, limit: i64) -> Result<Vec<Episode>, StoreError> {
        self.query_episodes(
            &format!(
                "{SUBTREE_CTE} SELECT {EPISODE_COLUMNS} FROM episodes
                 WHERE node_id IN (SELECT id FROM sub) ORDER BY started_at DESC, id DESC LIMIT ?2"
            ),
            &[node_id, limit],
        )
    }

    /// Episodes overlapping `[from, to)`, oldest first.
    pub fn episodes_between(&self, from: i64, to: i64) -> Result<Vec<Episode>, StoreError> {
        self.query_episodes(
            &format!(
                "SELECT {EPISODE_COLUMNS} FROM episodes
                 WHERE ended_at >= ?1 AND started_at < ?2 ORDER BY started_at, id"
            ),
            &[from, to],
        )
    }

    /// Forget an episode's interpretation: its segments become uninterpreted again (the
    /// next run re-cuts and re-interprets them) and the row goes; the events it produced stay.
    pub fn reset_episode(&self, id: i64) -> Result<(), StoreError> {
        let tx = Savepoint::begin(&self.conn)?;
        tx.execute(
            "UPDATE segments SET episode_id = NULL, interpreted_at = NULL WHERE episode_id = ?1",
            [id],
        )?;
        // the events keep pointing at the old id, which (AUTOINCREMENT) no later episode gets
        tx.execute("DELETE FROM episodes WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Fold `from` into `into` (segments move, the row goes); the coherence review's merge.
    pub fn merge_episodes(&self, from: i64, into: i64) -> Result<(), StoreError> {
        if from == into {
            return Ok(());
        }
        let tx = Savepoint::begin(&self.conn)?;
        tx.execute(
            "UPDATE segments SET episode_id = ?2 WHERE episode_id = ?1",
            [from, into],
        )?;
        tx.execute(
            "UPDATE events SET source_episode_id = ?2 WHERE source_episode_id = ?1",
            [from, into],
        )?;
        tx.execute(
            "UPDATE episodes SET
                started_at = MIN(started_at, (SELECT started_at FROM episodes WHERE id = ?1)),
                ended_at = MAX(ended_at, (SELECT ended_at FROM episodes WHERE id = ?1))
             WHERE id = ?2",
            [from, into],
        )?;
        tx.execute("DELETE FROM episodes WHERE id = ?1", [from])?;
        tx.commit()?;
        Ok(())
    }

    // ---- events and notes -----------------------------------------------------

    /// Insert an event, or update the one with the same `dedupe_key`. Returns the row.
    pub fn upsert_event(&self, e: &NewEvent, now: i64) -> Result<Event, StoreError> {
        use rusqlite::OptionalExtension;
        let source = e.source.unwrap_or(Origin::Interpretation).as_str();
        let existing: Option<i64> = match &e.dedupe_key {
            Some(k) => self
                .conn
                .query_row("SELECT id FROM events WHERE dedupe_key = ?1", [k], |r| {
                    r.get(0)
                })
                .optional()?,
            None => None,
        };
        let id = match existing {
            Some(id) => {
                self.conn.execute(
                    "UPDATE events SET when_start = ?2, when_end = ?3, all_day = ?4, title = ?5,
                         details = ?6, node_id = COALESCE(?7, node_id), confidence = ?8,
                         source_episode_id = COALESCE(?9, source_episode_id), updated_at = ?10
                     WHERE id = ?1",
                    rusqlite::params![
                        id,
                        e.when_start,
                        e.when_end,
                        i64::from(e.all_day),
                        e.title,
                        e.details,
                        e.node_id,
                        e.confidence,
                        e.source_episode_id,
                        now
                    ],
                )?;
                id
            }
            None => {
                self.conn.execute(
                    "INSERT INTO events(when_start, when_end, all_day, title, details, node_id,
                         source, source_segment_id, source_episode_id, confidence, dedupe_key,
                         created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
                    rusqlite::params![
                        e.when_start,
                        e.when_end,
                        i64::from(e.all_day),
                        e.title,
                        e.details,
                        e.node_id,
                        source,
                        e.source_segment_id,
                        e.source_episode_id,
                        e.confidence,
                        e.dedupe_key,
                        now
                    ],
                )?;
                self.conn.last_insert_rowid()
            }
        };
        Ok(self.conn.query_row(
            &format!("SELECT {EVENT_COLUMNS} FROM events WHERE id = ?1"),
            [id],
            row_to_event,
        )?)
    }

    fn query_events(&self, sql: &str, args: &[i64]) -> Result<Vec<Event>, StoreError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), row_to_event)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Events starting at or after `from`, soonest first.
    pub fn events_from(&self, from: i64, limit: i64) -> Result<Vec<Event>, StoreError> {
        self.query_events(
            &format!(
                "SELECT {EVENT_COLUMNS} FROM events WHERE when_start >= ?1
                 ORDER BY when_start, id LIMIT ?2"
            ),
            &[from, limit],
        )
    }

    /// Events starting before `before`, most recent first.
    pub fn events_before(&self, before: i64, limit: i64) -> Result<Vec<Event>, StoreError> {
        self.query_events(
            &format!(
                "SELECT {EVENT_COLUMNS} FROM events WHERE when_start < ?1
                 ORDER BY when_start DESC, id DESC LIMIT ?2"
            ),
            &[before, limit],
        )
    }

    /// Events produced by an episode's interpretation.
    pub fn events_of_episode(&self, episode_id: i64) -> Result<Vec<Event>, StoreError> {
        self.query_events(
            &format!(
                "SELECT {EVENT_COLUMNS} FROM events WHERE source_episode_id = ?1 ORDER BY when_start, id"
            ),
            &[episode_id],
        )
    }

    pub fn delete_event(&self, id: i64) -> Result<(), StoreError> {
        let tx = Savepoint::begin(&self.conn)?;
        tx.execute(
            "UPDATE reminders SET event_id = NULL WHERE event_id = ?1",
            [id],
        )?;
        tx.execute("DELETE FROM events WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    // ---- todos (reminders are todos with a due time) ----------------------------------

    /// Insert a todo (last in the manual order), or update the one with the same
    /// `dedupe_key`: text, due time, event and node; a changed due time clears the snooze.
    /// A completed todo stays completed (re-dictating it doesn't reopen it).
    pub fn add_todo(&self, t: &NewTodo, now: i64) -> Result<Todo, StoreError> {
        use rusqlite::OptionalExtension;
        let source = t.source.unwrap_or(Origin::User).as_str();
        let text = t.text.trim();
        let existing: Option<i64> = match &t.dedupe_key {
            Some(k) => self
                .conn
                .query_row("SELECT id FROM reminders WHERE dedupe_key = ?1", [k], |r| {
                    r.get(0)
                })
                .optional()?,
            None => None,
        };
        let id = match existing {
            Some(id) => {
                self.conn.execute(
                    "UPDATE reminders SET text = ?2,
                         snoozed_until = CASE WHEN due IS ?3 THEN snoozed_until END,
                         due = ?3, event_id = COALESCE(?4, event_id),
                         node_id = COALESCE(?5, node_id)
                     WHERE id = ?1",
                    rusqlite::params![id, text, t.due, t.event_id, t.node_id],
                )?;
                id
            }
            None => {
                self.conn.execute(
                    "INSERT INTO reminders(event_id, due, text, source, source_segment_id,
                         node_id, position, dedupe_key, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                         (SELECT COALESCE(MAX(position), 0) + 1 FROM reminders), ?7, ?8)",
                    rusqlite::params![
                        t.event_id,
                        t.due,
                        text,
                        source,
                        t.source_segment_id,
                        t.node_id,
                        t.dedupe_key,
                        now
                    ],
                )?;
                self.conn.last_insert_rowid()
            }
        };
        self.todo(id)?
            .ok_or_else(|| StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
    }

    pub fn todo(&self, id: i64) -> Result<Option<Todo>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {TODO_COLUMNS} FROM reminders WHERE id = ?1"),
                [id],
                row_to_todo,
            )
            .optional()?)
    }

    fn query_todos(&self, sql: &str, args: &[i64]) -> Result<Vec<Todo>, StoreError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), row_to_todo)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The todo list in display order: open ones with a due time (soonest first), then the
    /// undated open ones (manual order), then the completed ones (most recently completed
    /// first).
    pub fn todos(&self, filter: TodoFilter) -> Result<Vec<Todo>, StoreError> {
        let filter = match filter {
            TodoFilter::Open => "WHERE completed_at IS NULL",
            TodoFilter::Completed => "WHERE completed_at IS NOT NULL",
            TodoFilter::All => "",
        };
        self.query_todos(
            &format!(
                "SELECT {TODO_COLUMNS} FROM reminders {filter}
                 ORDER BY completed_at IS NOT NULL,
                     CASE WHEN completed_at IS NULL THEN due IS NULL END,
                     CASE WHEN completed_at IS NULL THEN due END,
                     CASE WHEN completed_at IS NULL THEN position END,
                     completed_at DESC, id"
            ),
            &[],
        )
    }

    /// Open todos whose pop-up is due at `now`: the snooze is over, or (not snoozed) the due
    /// time has come. Soonest first.
    pub fn due_todos(&self, now: i64) -> Result<Vec<Todo>, StoreError> {
        self.query_todos(
            &format!(
                "SELECT {TODO_COLUMNS} FROM reminders
                 WHERE completed_at IS NULL AND COALESCE(snoozed_until, due) <= ?1
                 ORDER BY COALESCE(snoozed_until, due), id"
            ),
            &[now],
        )
    }

    /// The next moment after `now` an open todo comes due (or its snooze ends) — when the UI
    /// should wake to show it.
    pub fn next_due_at(&self, now: i64) -> Result<Option<i64>, StoreError> {
        Ok(self.conn.query_row(
            "SELECT MIN(COALESCE(snoozed_until, due)) FROM reminders
             WHERE completed_at IS NULL AND COALESCE(snoozed_until, due) > ?1",
            [now],
            |r| r.get(0),
        )?)
    }

    /// Mark done at `at` (the snooze goes with it).
    pub fn complete_todo(&self, id: i64, at: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE reminders SET completed_at = ?2, done = 1, snoozed_until = NULL
             WHERE id = ?1 AND completed_at IS NULL",
            rusqlite::params![id, at],
        )?;
        Ok(())
    }

    /// Open again (a past due time pops up again at once).
    pub fn reopen_todo(&self, id: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE reminders SET completed_at = NULL, done = 0, snoozed_until = NULL
             WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// Don't pop up before `until` (an undated todo then pops up at `until`).
    pub fn snooze_todo(&self, id: i64, until: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE reminders SET snoozed_until = ?2 WHERE id = ?1",
            rusqlite::params![id, until],
        )?;
        Ok(())
    }

    pub fn clear_todo_snooze(&self, id: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE reminders SET snoozed_until = NULL WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    pub fn set_todo_text(&self, id: i64, text: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE reminders SET text = ?2 WHERE id = ?1",
            rusqlite::params![id, text.trim()],
        )?;
        Ok(())
    }

    pub fn delete_todo(&self, id: i64) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM reminders WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn insert_note(
        &self,
        node_id: Option<i64>,
        text: &str,
        source: Origin,
        source_segment_id: Option<i64>,
        now: i64,
    ) -> Result<Note, StoreError> {
        self.conn.execute(
            "INSERT INTO notes(node_id, text, source, source_segment_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                node_id,
                text.trim(),
                source.as_str(),
                source_segment_id,
                now
            ],
        )?;
        Ok(Note {
            id: self.conn.last_insert_rowid(),
            node_id,
            text: text.trim().to_owned(),
            source,
            source_segment_id,
            created_at: now,
        })
    }

    /// Newest first.
    pub fn notes(&self, limit: i64) -> Result<Vec<Note>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, node_id, text, source, source_segment_id, created_at FROM notes
             ORDER BY created_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |row| {
                Ok(Note {
                    id: row.get(0)?,
                    node_id: row.get(1)?,
                    text: row.get(2)?,
                    source: Origin::parse(&row.get::<_, String>(3)?),
                    source_segment_id: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- brain runs and changes -------------------------------------------------------

    pub fn begin_run(
        &self,
        tier: i64,
        kind: &str,
        model: &str,
        started_at: i64,
    ) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO brain_runs(tier, kind, started_at, model) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![tier, kind, started_at, model],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_run(&self, id: i64, outcome: &RunOutcome) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE brain_runs SET finished_at = ?2, model = COALESCE(?3, model), cost_usd = ?4,
                 duration_ms = ?5, ok = ?6, error = ?7, summary = ?8
             WHERE id = ?1",
            rusqlite::params![
                id,
                outcome.finished_at,
                outcome.model,
                outcome.cost_usd,
                outcome.duration_ms,
                i64::from(outcome.ok),
                outcome.error,
                outcome.summary
            ],
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn runs(&self, limit: i64) -> Result<Vec<BrainRun>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM brain_runs ORDER BY started_at DESC, id DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map([limit], row_to_run)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The most recent finished run of a kind.
    pub fn last_run_of(&self, kind: &str) -> Result<Option<BrainRun>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM brain_runs WHERE kind = ?1 AND finished_at IS NOT NULL
                     ORDER BY started_at DESC, id DESC LIMIT 1"
                ),
                [kind],
                row_to_run,
            )
            .optional()?)
    }

    /// Summed `cost_usd` of runs started at or after `since` (the daily budget).
    pub fn cost_since(&self, since: i64) -> Result<f64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM brain_runs WHERE started_at >= ?1",
            [since],
            |r| r.get(0),
        )?)
    }

    /// Runs of one tier started at or after `since` (tier 0's own hourly cap).
    pub fn run_count_of_tier_since(&self, tier: i64, since: i64) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM brain_runs WHERE tier = ?1 AND started_at >= ?2",
            [tier, since],
            |r| r.get(0),
        )?)
    }

    /// Runs started at or after `since` (the per-hour budget guard).
    pub fn run_count_since(&self, since: i64) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM brain_runs WHERE started_at >= ?1",
            [since],
            |r| r.get(0),
        )?)
    }

    pub fn record_change(
        &self,
        run_id: Option<i64>,
        kind: &str,
        description: &str,
        before: &serde_json::Value,
        after: &serde_json::Value,
        now: i64,
    ) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO changes(run_id, kind, description, before_json, after_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                run_id,
                kind,
                description,
                serde_json::to_string(before)?,
                serde_json::to_string(after)?,
                now
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Newest first.
    pub fn changes(&self, limit: i64) -> Result<Vec<Change>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, kind, description, before_json, after_json, undone, created_at
             FROM changes ORDER BY created_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], row_to_change)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn change(&self, id: i64) -> Result<Option<Change>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT id, run_id, kind, description, before_json, after_json, undone, created_at
                 FROM changes WHERE id = ?1",
                [id],
                row_to_change,
            )
            .optional()?)
    }

    pub fn mark_change_undone(&self, id: i64) -> Result<(), StoreError> {
        self.conn
            .execute("UPDATE changes SET undone = 1 WHERE id = ?1", [id])?;
        Ok(())
    }
}

impl Store {
    /// Episodes whose topics include `topic` (case-insensitive, whole topic), newest first.
    pub fn episodes_with_topic(&self, topic: &str) -> Result<Vec<Episode>, StoreError> {
        let sql = format!(
            "SELECT {EPISODE_COLUMNS} FROM episodes
             WHERE EXISTS (SELECT 1 FROM json_each(episodes.topics_json)
                           WHERE lower(value) = lower(?1))
             ORDER BY started_at DESC, id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([topic], row_to_episode)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = Result<(), StoreError>;

    fn version(s: &Store) -> Result<String, StoreError> {
        Ok(s.conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )?)
    }

    #[test]
    fn dangling_sessions_are_closed_at_their_last_segment_or_start() -> Result<(), StoreError> {
        let s = Store::open_in_memory()?;
        let with_seg = s.begin_session(1_000, "t")?;
        s.insert_segment(with_seg, 1_500, 2_500, "hello there", None)?;
        let empty = s.begin_session(3_000, "t")?;
        let done = s.begin_session(4_000, "t")?;
        s.end_session(done, 4_100)?;
        let live = s.begin_session(9_000, "t")?;
        assert_eq!(s.close_dangling_sessions(5_000)?, 2);
        let end = |id: i64| -> Result<Option<i64>, StoreError> {
            Ok(s.conn
                .query_row("SELECT ended_at FROM sessions WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?)
        };
        assert_eq!(end(with_seg)?, Some(2_500));
        assert_eq!(end(empty)?, Some(3_000));
        assert_eq!(end(done)?, Some(4_100));
        assert_eq!(end(live)?, None);
        Ok(())
    }

    #[test]
    fn search_finds_text_in_all_three_places_and_skips_echoes() -> R {
        let s = Store::open_in_memory()?;
        let sess = s.begin_session(0, "t")?;
        let a = s.insert_segment_from(
            sess,
            1000,
            2000,
            "The Roadmap is tight",
            None,
            AudioSourceKind::System,
            None,
        )?;
        s.insert_segment_from(
            sess,
            3000,
            4000,
            "the roadmap is tight",
            None,
            AudioSourceKind::Mic,
            Some(a.id),
        )?;
        s.insert_segment_from(
            sess,
            5000,
            6000,
            "more ROADMAP talk",
            None,
            AudioSourceKind::Mic,
            None,
        )?;
        s.insert_segment_from(
            sess,
            7000,
            8000,
            "unrelated",
            None,
            AudioSourceKind::Mic,
            None,
        )?;
        s.insert_episode(
            0,
            10,
            &EpisodeContent {
                title: "Planning".into(),
                summary: "nothing".into(),
                decisions: vec!["Cut the roadmap".into()],
                ..Default::default()
            },
            1,
        )?;
        s.insert_episode(
            20,
            30,
            &EpisodeContent {
                title: "Other".into(),
                summary: "Reviewed the ROADMAP".into(),
                ..Default::default()
            },
            1,
        )?;
        s.insert_episode(
            40,
            50,
            &EpisodeContent {
                title: "No".into(),
                ..Default::default()
            },
            1,
        )?;
        s.upsert_event(
            &NewEvent {
                when_start: 5,
                title: "Roadmap review".into(),
                ..Default::default()
            },
            1,
        )?;
        s.upsert_event(
            &NewEvent {
                when_start: 9,
                title: "x".into(),
                details: "about the roadmap".into(),
                dedupe_key: Some("k".into()),
                ..Default::default()
            },
            1,
        )?;
        let r = s.search("  roadmap ", 200)?;
        let texts: Vec<&str> = r.segments.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, ["more ROADMAP talk", "The Roadmap is tight"]);
        let titles: Vec<&str> = r.episodes.iter().map(|x| x.title.as_str()).collect();
        assert_eq!(titles, ["Other", "Planning"]);
        assert_eq!(r.events.len(), 2);
        assert_eq!(r.events[0].title, "x");
        assert_eq!(s.search("roadmap", 1)?.segments.len(), 1);
        Ok(())
    }

    #[test]
    fn search_blank_query_finds_nothing_and_wildcards_are_literal() -> R {
        let s = Store::open_in_memory()?;
        let sess = s.begin_session(0, "t")?;
        s.insert_segment_from(
            sess,
            1000,
            2000,
            "fifty percent",
            None,
            AudioSourceKind::Mic,
            None,
        )?;
        s.insert_segment_from(
            sess,
            3000,
            4000,
            "100% done_now",
            None,
            AudioSourceKind::Mic,
            None,
        )?;
        assert_eq!(s.search("", 10)?, SearchResults::default());
        assert_eq!(s.search("   ", 10)?, SearchResults::default());
        assert_eq!(s.search("%", 10)?.segments.len(), 1);
        assert_eq!(s.search("_", 10)?.segments.len(), 1);
        assert_eq!(s.search("f_fty", 10)?.segments.len(), 0);
        Ok(())
    }

    #[test]
    fn user_name_round_trips_and_clears() -> R {
        let s = Store::open_in_memory()?;
        assert_eq!(s.user_name()?, None);
        s.set_user_name("  Alex ")?;
        assert_eq!(s.user_name()?.as_deref(), Some("Alex"));
        s.set_user_name("   ")?;
        assert_eq!(s.user_name()?, None);
        Ok(())
    }

    #[test]
    fn seeds_default_categories_and_schema_version() -> R {
        let s = Store::open_in_memory()?;
        let names: Vec<String> = s.categories()?.into_iter().map(|c| c.name).collect();
        assert_eq!(names, DEFAULT_CATEGORIES);
        assert_eq!(version(&s)?, "5");
        assert!(s.categories()?.iter().all(|c| c.kind == NodeKind::Category));
        Ok(())
    }

    /// The exact v1 schema (as shipped) with data in it.
    const V1: &str = "
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
        INSERT INTO sessions VALUES (1, 0, 10, 'Null');
        INSERT INTO categories VALUES (1, 'Work', 0), (2, 'Admin', 1), (7, 'Ideas', 2);
        INSERT INTO segments VALUES (1, 1, 1, 2, 'filed', 0.9, 7), (2, 1, 3, 4, 'loose', NULL, NULL);
    ";

    #[test]
    fn migrates_v1_to_v2() -> R {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(V1)?;
        let s = Store::init(conn)?;
        assert_eq!(version(&s)?, "5");
        let cats = s.categories()?;
        assert_eq!(
            cats.iter()
                .map(|c| (c.id, c.name.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "Work"), (2, "Admin"), (7, "Ideas")]
        );
        let rows = s.segments(View::All, 0)?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].node_id, Some(7));
        assert!(rows[0].categorised);
        assert_eq!(rows[0].confidence, Some(0.9));
        assert_eq!(rows[1].node_id, None);
        assert!(!rows[1].categorised);
        assert_eq!(rows[1].interpreted_at, None);
        // the old table is gone, the new ones work, the migration doesn't run twice
        assert!(s.conn.prepare("SELECT * FROM categories").is_err());
        assert!(s.episodes(10)?.is_empty());
        assert_eq!(s.segments(View::Node(7), 0)?.len(), 1);
        assert!(
            rows.iter()
                .all(|r| r.source == AudioSourceKind::Mic && r.echo_of.is_none())
        );
        Ok(())
    }

    /// A v2 database (no `source` / `echo_of`): every row, filing and episode link survives,
    /// existing segments read as microphone segments, and the new columns work.
    #[test]
    fn migrates_v2_to_v3() -> R {
        let conn = rusqlite::Connection::open_in_memory()?;
        let v2_segments = "CREATE TABLE segments (
            id             INTEGER PRIMARY KEY,
            session_id     INTEGER NOT NULL REFERENCES sessions(id),
            started_at     INTEGER NOT NULL,
            ended_at       INTEGER NOT NULL,
            text           TEXT NOT NULL,
            confidence     REAL,
            node_id        INTEGER REFERENCES nodes(id),
            episode_id     INTEGER REFERENCES episodes(id),
            interpreted_at INTEGER,
            categorised    INTEGER NOT NULL DEFAULT 0
        );";
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(v2_segments)?;
        conn.execute_batch(SCHEMA_REST)?;
        conn.execute_batch(
            "INSERT INTO meta VALUES ('schema_version', '2');
             INSERT INTO nodes(id, parent_id, kind, name, position) VALUES (1, NULL, 'category', 'Work', 0);
             INSERT INTO sessions VALUES (1, 1000, 9000, 'engine');
             INSERT INTO episodes(id, started_at, ended_at, title, summary, created_at, last_revised_at)
                 VALUES (5, 1000, 9000, 'Standup', '- done', 1, 1);
             INSERT INTO segments VALUES (1, 1, 1000, 2000, 'filed and interpreted', 0.9, 1, 5, 3000, 1);
             INSERT INTO segments VALUES (2, 1, 4000, 5000, 'loose', NULL, NULL, NULL, NULL, 0);",
        )?;
        let s = Store::init(conn)?;
        assert_eq!(version(&s)?, "5");
        let rows = s.segments(View::All, 0)?;
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].node_id, rows[0].episode_id, rows[0].interpreted_at),
            (Some(1), Some(5), Some(3000))
        );
        assert!(rows[0].categorised && !rows[1].categorised);
        assert_eq!(rows[0].confidence, Some(0.9));
        assert!(
            rows.iter()
                .all(|r| r.source == AudioSourceKind::Mic && r.echo_of.is_none())
        );
        assert_eq!(s.segments_of_episode(5)?.len(), 1);
        assert_eq!(s.episodes(10)?.len(), 1);
        // the new columns work on the migrated table, and reopening doesn't migrate again
        let sys =
            s.insert_segment_from(1, 6000, 7000, "them", None, AudioSourceKind::System, None)?;
        assert_eq!(
            s.segment(sys.id)?.map(|r| r.source),
            Some(AudioSourceKind::System)
        );
        s.init_locked()?;
        assert_eq!(s.segments(View::All, 0)?.len(), 3);
        Ok(())
    }

    /// Lanes interleave by `started_at`; an echo is stored, linked, and left out of every
    /// listing (views, counts, the brain's queues) but kept for audit; deleting the system
    /// segment takes its echoes along.
    #[test]
    fn sources_interleave_and_echoes_are_hidden() -> R {
        let s = Store::open_in_memory()?;
        let session = s.begin_session(0, "e")?;
        let me = s.insert_segment(session, 1_000, 2_000, "me first", None)?;
        let them = s.insert_segment_from(
            session,
            1_500,
            4_000,
            "them talking",
            None,
            AudioSourceKind::System,
            None,
        )?;
        let echo = s.insert_segment_from(
            session,
            1_600,
            4_200,
            "them talking",
            None,
            AudioSourceKind::Mic,
            Some(them.id),
        )?;
        let me2 = s.insert_segment(session, 5_000, 6_000, "me again", None)?;
        let ids = |rows: Vec<TranscriptRow>| rows.iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(s.segments(View::All, 0)?), vec![me.id, them.id, me2.id]);
        assert_eq!(
            ids(s.segments(View::Today, 0)?),
            vec![me.id, them.id, me2.id]
        );
        assert_eq!(
            ids(s.segments_between(0, 10_000)?),
            vec![me.id, them.id, me2.id]
        );
        assert_eq!(
            ids(s.uninterpreted_segments(10)?),
            vec![me.id, them.id, me2.id]
        );
        assert_eq!(
            ids(s.uncategorised_segments(10)?),
            vec![me.id, them.id, me2.id]
        );
        assert_eq!(s.uninterpreted_count()?, 3);
        assert_eq!(s.segment_count_since(0)?, 3);
        assert_eq!(ids(s.echoes_of(them.id)?), vec![echo.id]);
        assert_eq!(s.segment(echo.id)?.and_then(|r| r.echo_of), Some(them.id));
        // the echo-rule candidates: system rows reaching into the window, no echoes
        assert_eq!(
            ids(s.segments_near(AudioSourceKind::System, 3_900, 9_000)?),
            vec![them.id]
        );
        assert!(
            s.segments_near(AudioSourceKind::System, 4_100, 9_000)?
                .is_empty()
        );
        assert_eq!(
            ids(s.segments_near(AudioSourceKind::Mic, 0, 10_000)?),
            vec![me.id, me2.id]
        );
        // a later system segment can claim a stored mic row
        s.set_segment_echo(me2.id, Some(them.id))?;
        assert_eq!(ids(s.segments(View::All, 0)?), vec![me.id, them.id]);
        s.delete_segment(them.id)?;
        assert_eq!(ids(s.segments(View::All, 0)?), vec![me.id]);
        assert!(s.segment(echo.id)?.is_none() && s.segment(me2.id)?.is_none());
        Ok(())
    }

    #[test]
    fn refuses_a_newer_schema() {
        let conn = match rusqlite::Connection::open_in_memory() {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO meta VALUES ('schema_version', '99');",
        );
        assert!(matches!(
            Store::init(conn),
            Err(StoreError::SchemaTooNew(99))
        ));
    }

    #[test]
    fn sessions_begin_and_end() -> R {
        let s = Store::open_in_memory()?;
        let id = s.begin_session(1000, "Null")?;
        s.end_session(id, 2000)?;
        let session = s.conn.query_row(
            "SELECT id, started_at, ended_at, engine FROM sessions WHERE id = ?1",
            [id],
            |r| {
                Ok(Session {
                    id: r.get(0)?,
                    started_at: r.get(1)?,
                    ended_at: r.get(2)?,
                    engine: r.get(3)?,
                })
            },
        )?;
        assert_eq!(
            session,
            Session {
                id,
                started_at: 1000,
                ended_at: Some(2000),
                engine: "Null".into()
            }
        );
        Ok(())
    }

    #[test]
    fn segments_views_and_counts() -> R {
        let s = Store::open_in_memory()?;
        let sid = s.begin_session(0, "e")?;
        let old = s.insert_segment(sid, 100, 200, "yesterday", Some(0.5))?;
        let new = s.insert_segment(sid, 5_000, 6_000, "today", None)?;
        let earlier = s.insert_segment(sid, 4_000, 4_500, "today early", None)?;
        assert_eq!(old.confidence, Some(0.5));
        assert_eq!(old.node_id, None);

        let all = s.segments(View::All, 1_000)?;
        assert_eq!(all, vec![old.clone(), earlier.clone(), new.clone()]);
        let today = s.segments(View::Today, 1_000)?;
        assert_eq!(today, vec![earlier.clone(), new.clone()]);
        assert_eq!(s.segment_count_since(1_000)?, 2);
        assert_eq!(s.segment_count_since(0)?, 3);
        assert_eq!(s.segments_between(4_000, 5_000)?, vec![earlier.clone()]);
        assert!(s.segments(View::Notes, 0)?.is_empty());

        let work = s.categories()?[0].id;
        s.set_segment_node(new.id, Some(work))?;
        let filed = s.segments(View::Node(work), 0)?;
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].id, new.id);
        assert_eq!(filed[0].node_id, Some(work));
        assert!(filed[0].categorised);
        s.set_segment_node(new.id, None)?;
        assert!(s.segments(View::Node(work), 0)?.is_empty());
        // the user's choice beats the automatic pass
        assert_eq!(s.uncategorised_segments(10)?.len(), 2);
        s.apply_categorisation(&[(new.id, Some(work)), (old.id, Some(work))])?;
        assert_eq!(s.segment(new.id)?.and_then(|r| r.node_id), None);
        assert_eq!(s.segment(old.id)?.and_then(|r| r.node_id), Some(work));
        assert_eq!(s.uncategorised_segments(10)?.len(), 1);

        s.delete_segment(old.id)?;
        assert_eq!(s.segments(View::All, 0)?.len(), 2);
        Ok(())
    }

    #[test]
    fn node_tree_subtrees_merge_and_delete() -> R {
        let s = Store::open_in_memory()?;
        let work = s.categories()?[0].id;
        let proj = s.add_node(
            Some(work),
            NodeKind::Project,
            " Export rewrite ",
            "the exporter",
            Origin::Auto,
        )?;
        assert_eq!(proj.name, "Export rewrite");
        assert_eq!(proj.kind, NodeKind::Project);
        assert_eq!(proj.parent_id, Some(work));
        assert_eq!(proj.created_by, Origin::Auto);
        // case/whitespace-insensitive dedupe
        let again = s.add_node(
            Some(work),
            NodeKind::Project,
            "export  REWRITE",
            "",
            Origin::Auto,
        )?;
        assert_eq!(again.id, proj.id);
        let topic = s.add_node(Some(proj.id), NodeKind::Topic, "CSV", "", Origin::Auto)?;
        assert_eq!(s.subtree_ids(work)?, vec![work, proj.id, topic.id]);
        let tree = s.nodes()?;
        let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            ["Work", "Export rewrite", "CSV", "Admin", "Personal"]
        );

        let sid = s.begin_session(0, "e")?;
        let seg = s.insert_segment(sid, 1, 2, "x", None)?;
        s.set_segment_node(seg.id, Some(topic.id))?;
        assert_eq!(s.segments(View::Node(work), 0)?.len(), 1);
        assert_eq!(s.segments(View::Node(proj.id), 0)?.len(), 1);
        assert!(s.segments(View::Node(s.categories()?[1].id), 0)?.is_empty());

        // archive + revive through add_node
        s.set_node_archived(topic.id, true)?;
        assert!(s.node(topic.id)?.is_some_and(|n| n.archived));
        assert!(
            !s.add_node(Some(proj.id), NodeKind::Topic, "csv", "", Origin::Auto)?
                .archived
        );

        // merge: the segment follows, the child re-parents
        let other = s.add_node(
            Some(work),
            NodeKind::Project,
            "Exporter v2",
            "",
            Origin::Auto,
        )?;
        s.merge_nodes(proj.id, other.id)?;
        assert!(s.node(proj.id)?.is_none());
        assert_eq!(s.node(topic.id)?.and_then(|n| n.parent_id), Some(other.id));
        assert_eq!(s.segments(View::Node(other.id), 0)?.len(), 1);
        // a move under its own subtree is refused
        s.move_node(other.id, Some(topic.id))?;
        assert_eq!(s.node(other.id)?.and_then(|n| n.parent_id), Some(work));
        let admin = s.categories()?[1].id;
        s.move_node(other.id, Some(admin))?;
        assert_eq!(s.node(other.id)?.and_then(|n| n.parent_id), Some(admin));

        s.rename_category(work, " Office ")?;
        assert_eq!(s.categories()?[0].name, "Office");
        s.delete_node(admin)?;
        assert!(s.node(topic.id)?.is_none());
        assert_eq!(s.segments(View::All, 0)?[0].node_id, None);
        assert_eq!(s.categories()?.len(), 2);
        // positions keep growing after a delete
        assert_eq!(s.add_category("Next")?.position, 3);
        Ok(())
    }

    #[test]
    fn episodes_revise_reset_and_merge() -> R {
        let s = Store::open_in_memory()?;
        let sid = s.begin_session(0, "e")?;
        let a = s.insert_segment(sid, 1_000, 2_000, "a", None)?;
        let b = s.insert_segment(sid, 3_000, 4_000, "b", None)?;
        assert_eq!(s.uninterpreted_count()?, 2);
        let content = EpisodeContent {
            title: "Standup".into(),
            summary: "- stuff".into(),
            decisions: vec!["ship".into()],
            action_items: vec![ActionItem {
                text: "write tests".into(),
                owner: Some("Sam".into()),
                due: None,
            }],
            topics: vec!["tests".into()],
            model: "haiku".into(),
            cost_usd: Some(0.01),
            ..Default::default()
        };
        let ep = s.insert_episode(1_000, 2_000, &content, 5_000)?;
        assert_eq!(ep.revision, 1);
        assert_eq!(ep.action_items[0].owner.as_deref(), Some("Sam"));
        s.mark_segments_interpreted(&[a.id], ep.id, 5_000)?;
        assert_eq!(s.uninterpreted_count()?, 1);
        assert_eq!(s.uninterpreted_segments(10)?[0].id, b.id);
        assert_eq!(s.segments_of_episode(ep.id)?.len(), 1);

        s.revise_episode(
            ep.id,
            1_000,
            4_000,
            &EpisodeContent {
                cost_usd: Some(0.02),
                ..content.clone()
            },
            6_000,
        )?;
        s.mark_segments_interpreted(&[b.id], ep.id, 6_000)?;
        let ep2 = s
            .episode(ep.id)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        assert_eq!(ep2.revision, 2);
        assert_eq!(ep2.ended_at, 4_000);
        assert_eq!(ep2.last_revised_at, 6_000);
        assert!((ep2.cost_usd.unwrap_or(0.0) - 0.03).abs() < 1e-9);
        assert_eq!(s.episodes_between(3_500, 10_000)?.len(), 1);
        assert!(s.episodes_between(4_001, 10_000)?.is_empty());

        let other = s.insert_episode(7_000, 8_000, &content, 9_000)?;
        s.merge_episodes(other.id, ep.id)?;
        assert!(s.episode(other.id)?.is_none());
        assert_eq!(s.episode(ep.id)?.map(|e| e.ended_at), Some(8_000));

        s.reset_episode(ep.id)?;
        assert!(s.episode(ep.id)?.is_none());
        assert_eq!(s.uninterpreted_count()?, 2);
        Ok(())
    }

    #[test]
    fn events_reminders_notes_dedupe() -> R {
        let s = Store::open_in_memory()?;
        let e = NewEvent {
            when_start: 100,
            all_day: true,
            title: "Property taxes due".into(),
            source: Some(Origin::Hotword),
            source_segment_id: Some(3),
            dedupe_key: Some("hotword:3:property taxes due:2026-12-12".into()),
            ..Default::default()
        };
        let first = s.upsert_event(&e, 1)?;
        assert_eq!(first.source, Origin::Hotword);
        let second = s.upsert_event(
            &NewEvent {
                title: "Property taxes".into(),
                ..e.clone()
            },
            2,
        )?;
        assert_eq!(first.id, second.id);
        assert_eq!(second.title, "Property taxes");
        assert_eq!(second.updated_at, 2);
        assert_eq!(s.events_from(0, 10)?.len(), 1);
        assert!(s.events_from(101, 10)?.is_empty());
        assert_eq!(s.events_before(101, 10)?.len(), 1);

        let r = s.add_todo(
            &NewTodo {
                event_id: Some(first.id),
                due: Some(50),
                text: "Property taxes in a week".into(),
                source: Some(Origin::Hotword),
                dedupe_key: Some("k".into()),
                ..Default::default()
            },
            1,
        )?;
        let r2 = s.add_todo(
            &NewTodo {
                due: Some(60),
                text: "x".into(),
                dedupe_key: Some("k".into()),
                ..Default::default()
            },
            2,
        )?;
        assert_eq!(r.id, r2.id);
        assert_eq!(r2.due, Some(60));
        assert_eq!(r2.source, Origin::Hotword, "the first source stays");
        assert_eq!(s.todos(TodoFilter::Open)?.len(), 1);
        s.complete_todo(r.id, 70)?;
        assert!(s.todos(TodoFilter::Open)?.is_empty());
        assert_eq!(s.todos(TodoFilter::All)?.len(), 1);
        s.delete_event(first.id)?;
        assert_eq!(s.todos(TodoFilter::All)?[0].event_id, None);

        let n = s.insert_note(None, " remember milk ", Origin::Hotword, Some(3), 5)?;
        assert_eq!(n.text, "remember milk");
        assert_eq!(s.notes(10)?[0].id, n.id);
        Ok(())
    }

    /// The v3 `reminders` table (due NOT NULL, a done flag) is rebuilt as the todos: ids,
    /// texts, due times and event links survive, done rows become completed, the new columns
    /// work, and reopening doesn't migrate again.
    #[test]
    fn migrates_v3_to_v4_reminders_become_todos() -> R {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(SCHEMA_REST)?;
        conn.execute_batch(
            "CREATE TABLE reminders (
                id                INTEGER PRIMARY KEY,
                event_id          INTEGER REFERENCES events(id),
                due               INTEGER NOT NULL,
                text              TEXT NOT NULL,
                done              INTEGER NOT NULL DEFAULT 0,
                source            TEXT NOT NULL,
                source_segment_id INTEGER,
                dedupe_key        TEXT UNIQUE,
                created_at        INTEGER NOT NULL
            );
            INSERT INTO meta VALUES ('schema_version', '3');
            INSERT INTO events(id, when_start, title, source, created_at, updated_at)
                VALUES (4, 1000, 'Taxes', 'hotword', 1, 1);
            INSERT INTO reminders VALUES (7, 4, 500, 'taxes in a week', 0, 'hotword', 9, 'k7', 10);
            INSERT INTO reminders VALUES (8, NULL, 600, 'old one', 1, 'hotword', NULL, NULL, 20);",
        )?;
        let s = Store::init(conn)?;
        assert_eq!(version(&s)?, "5");
        let all = s.todos(TodoFilter::All)?;
        assert_eq!(all.len(), 2);
        let open = &all[0];
        assert_eq!(
            (open.id, open.event_id, open.due, open.text.as_str()),
            (7, Some(4), Some(500), "taxes in a week")
        );
        assert_eq!(open.source, Origin::Hotword);
        assert_eq!(open.source_segment_id, Some(9));
        assert_eq!(open.dedupe_key.as_deref(), Some("k7"));
        assert!(!open.done());
        assert_eq!(all[1].id, 8);
        assert_eq!(
            all[1].completed_at,
            Some(20),
            "done → completed at creation"
        );
        // undated todos work on the rebuilt table, ids continue
        let t = s.add_todo(
            &NewTodo {
                text: "undated".into(),
                ..Default::default()
            },
            30,
        )?;
        assert!(t.id > 8 && t.due.is_none());
        assert_eq!(t.source, Origin::User);
        s.init_locked()?;
        assert_eq!(s.todos(TodoFilter::All)?.len(), 3);
        Ok(())
    }

    /// A v4 database (no `episodes.user_edited_at`) gains the column in place, keeps its rows,
    /// and the edit method stamps it without touching `revision`.
    #[test]
    fn migrates_v4_to_v5_and_edits_stamp_user_edited_at() -> R {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(SCHEMA_REST)?;
        conn.execute_batch(
            "ALTER TABLE episodes DROP COLUMN user_edited_at;
             INSERT INTO meta VALUES ('schema_version', '4');
             INSERT INTO episodes(id, started_at, ended_at, title, summary, revision, created_at,
                 last_revised_at) VALUES (3, 10, 20, 'Old note', 'old summary', 2, 5, 6);",
        )?;
        let s = Store::init(conn)?;
        assert_eq!(version(&s)?, "5");
        let e = s.episode(3)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        assert_eq!(
            (e.title.as_str(), e.revision, e.user_edited_at),
            ("Old note", 2, None)
        );
        s.update_episode_content(
            3,
            &EpisodeContent {
                title: "Mine".into(),
                summary: "- fixed".into(),
                decisions: vec!["d".into()],
                action_items: vec![ActionItem {
                    text: "a".into(),
                    owner: Some("Me".into()),
                    due: None,
                }],
                topics: vec!["t".into()],
                node_id: Some(999),
                low_signal: true,
                ..Default::default()
            },
            777,
        )?;
        let e = s.episode(3)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        assert_eq!((e.title.as_str(), e.summary.as_str()), ("Mine", "- fixed"));
        assert_eq!((e.user_edited_at, e.revision), (Some(777), 2));
        assert_eq!(
            (e.node_id, e.low_signal),
            (None, false),
            "structure untouched"
        );
        assert_eq!(e.action_items[0].owner.as_deref(), Some("Me"));
        s.init_locked()?;
        assert_eq!(
            s.episode(3)?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?
                .user_edited_at,
            Some(777)
        );
        Ok(())
    }

    /// Display order, completion, reopening, snoozes and the pop-up queries.
    #[test]
    fn todos_order_complete_snooze_and_come_due() -> R {
        let s = Store::open_in_memory()?;
        let add = |text: &str, due: Option<i64>| {
            s.add_todo(
                &NewTodo {
                    text: format!(" {text} "),
                    due,
                    ..Default::default()
                },
                1,
            )
        };
        let milk = add("buy oat milk", None)?;
        let late = add("later", Some(5_000))?;
        let soon = add("soon", Some(1_000))?;
        let bread = add("bread", None)?;
        let done = add("done one", Some(500))?;
        assert_eq!(milk.text, "buy oat milk", "trimmed");
        assert!(bread.position > milk.position, "new ones go last");
        s.complete_todo(done.id, 900)?;
        let ids = |f| -> Result<Vec<i64>, StoreError> {
            Ok(s.todos(f)?.into_iter().map(|t| t.id).collect())
        };
        // dated open soonest first, then undated by position, then completed
        assert_eq!(
            ids(TodoFilter::All)?,
            vec![soon.id, late.id, milk.id, bread.id, done.id]
        );
        assert_eq!(ids(TodoFilter::Completed)?, vec![done.id]);
        assert_eq!(ids(TodoFilter::Open)?.len(), 4);
        // completing twice keeps the first time; most recently completed first
        s.complete_todo(done.id, 999)?;
        assert_eq!(s.todo(done.id)?.and_then(|t| t.completed_at), Some(900));
        s.complete_todo(milk.id, 950)?;
        assert_eq!(ids(TodoFilter::Completed)?, vec![milk.id, done.id]);
        s.reopen_todo(milk.id)?;
        assert_eq!(ids(TodoFilter::Completed)?, vec![done.id]);

        // pop-ups: nothing before 1000; at 1000 `soon`; the next wake is `late`
        assert!(s.due_todos(999)?.is_empty());
        assert_eq!(s.next_due_at(999)?, Some(1_000));
        assert_eq!(
            s.due_todos(1_000)?.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![soon.id]
        );
        assert_eq!(s.next_due_at(1_000)?, Some(5_000));
        // snoozed: hidden until the snooze ends, which is the next wake
        s.snooze_todo(soon.id, 3_000)?;
        assert!(s.due_todos(2_000)?.is_empty());
        assert_eq!(s.next_due_at(2_000)?, Some(3_000));
        assert_eq!(s.due_todos(3_000)?.len(), 1);
        s.clear_todo_snooze(soon.id)?;
        assert_eq!(s.due_todos(2_000)?.len(), 1);
        // a snoozed undated todo pops up when the snooze ends
        s.snooze_todo(bread.id, 4_000)?;
        assert_eq!(s.next_due_at(3_500)?, Some(4_000));
        // completing clears the snooze and the pop-up
        s.complete_todo(soon.id, 2_500)?;
        assert!(s.due_todos(2_600)?.is_empty());
        assert_eq!(s.todo(soon.id)?.and_then(|t| t.snoozed_until), None);
        // a dedupe-key update moving the due time drops the snooze
        let k = s.add_todo(
            &NewTodo {
                text: "keyed".into(),
                due: Some(10_000),
                dedupe_key: Some("key".into()),
                ..Default::default()
            },
            1,
        )?;
        s.snooze_todo(k.id, 20_000)?;
        let same = s.add_todo(
            &NewTodo {
                text: "keyed".into(),
                due: Some(10_000),
                dedupe_key: Some("key".into()),
                ..Default::default()
            },
            2,
        )?;
        assert_eq!(same.snoozed_until, Some(20_000), "same due: snooze kept");
        let moved = s.add_todo(
            &NewTodo {
                text: "keyed".into(),
                due: Some(15_000),
                dedupe_key: Some("key".into()),
                ..Default::default()
            },
            3,
        )?;
        assert_eq!((moved.id, moved.snoozed_until), (k.id, None));
        // edit and delete
        s.set_todo_text(bread.id, "  rye bread ")?;
        assert_eq!(
            s.todo(bread.id)?.map(|t| t.text).as_deref(),
            Some("rye bread")
        );
        s.delete_todo(bread.id)?;
        assert!(s.todo(bread.id)?.is_none());
        Ok(())
    }

    /// A todo filed under a node: deleting the node unfiles it, merging moves it.
    #[test]
    fn todos_follow_node_deletes_and_merges() -> R {
        let s = Store::open_in_memory()?;
        let work = s.categories()?[0].id;
        let p = s.add_node(Some(work), NodeKind::Project, "P", "", Origin::Auto)?;
        let q = s.add_node(Some(work), NodeKind::Project, "Q", "", Origin::Auto)?;
        let t = s.add_todo(
            &NewTodo {
                text: "filed".into(),
                node_id: Some(p.id),
                ..Default::default()
            },
            1,
        )?;
        s.merge_nodes(p.id, q.id)?;
        assert_eq!(s.todo(t.id)?.and_then(|t| t.node_id), Some(q.id));
        s.delete_node(q.id)?;
        assert_eq!(s.todo(t.id)?.and_then(|t| t.node_id), None);
        Ok(())
    }

    #[test]
    fn runs_cost_and_changes() -> R {
        let s = Store::open_in_memory()?;
        let id = s.begin_run(1, "interpret", "sonnet", 1_000)?;
        assert_eq!(s.run_count_since(0)?, 1);
        assert!(s.last_run_of("interpret")?.is_none());
        s.finish_run(
            id,
            &RunOutcome {
                finished_at: 2_000,
                model: Some("claude-sonnet-4-5".into()),
                cost_usd: Some(0.25),
                duration_ms: Some(1_000),
                ok: true,
                error: None,
                summary: "1 episode".into(),
            },
        )?;
        let run = s
            .last_run_of("interpret")?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        assert_eq!(run.model, "claude-sonnet-4-5");
        assert!(run.ok);
        assert!((s.cost_since(0)? - 0.25).abs() < 1e-9);
        assert_eq!(s.cost_since(1_001)?, 0.0);
        assert_eq!(s.runs(10)?.len(), 1);

        let c = s.record_change(
            Some(id),
            "node_rename",
            "Renamed 'Exp' to 'Export rewrite'",
            &serde_json::json!({"name": "Exp"}),
            &serde_json::json!({"name": "Export rewrite"}),
            3_000,
        )?;
        assert_eq!(
            s.changes(10)?[0].description,
            "Renamed 'Exp' to 'Export rewrite'"
        );
        s.mark_change_undone(c)?;
        assert!(s.change(c)?.is_some_and(|c| c.undone));
        s.set_meta("tier1_last", "42")?;
        assert_eq!(s.meta("tier1_last")?.as_deref(), Some("42"));
        assert_eq!(s.meta("nope")?, None);
        Ok(())
    }

    #[test]
    fn concurrent_opens_seed_the_default_categories_once() -> R {
        let dir = std::env::temp_dir().join(format!(
            "oats-store-race-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let path = dir.join("oats.sqlite");
        let threads: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || Store::open(&path).map(|_| ()))
            })
            .collect();
        for t in threads {
            if let Ok(r) = t.join() {
                r?;
            }
        }
        let s = Store::open(&path)?;
        assert_eq!(s.categories()?.len(), DEFAULT_CATEGORIES.len());
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn open_creates_file_and_reopens() -> R {
        let dir = std::env::temp_dir().join(format!(
            "oats-store-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let path = dir.join("nested").join("oats.sqlite");
        {
            let s = Store::open(&path)?;
            let sid = s.begin_session(0, "e")?;
            s.insert_segment(sid, 1, 2, "kept", None)?;
        }
        let s = Store::open(&path)?;
        assert_eq!(s.segments(View::All, 0)?[0].text, "kept");
        assert_eq!(s.categories()?.len(), 3);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn database_and_new_directory_are_owner_only() -> R {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "oats-store-perm-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let data = dir.join("data");
        let path = data.join("oats.sqlite");
        {
            let s = Store::open(&path)?;
            let sid = s.begin_session(0, "e")?;
            s.insert_segment(sid, 1, 2, "secret", None)?;
        }
        let mode = |p: &Path| -> Result<u32, StoreError> {
            Ok(std::fs::metadata(p)?.permissions().mode() & 0o777)
        };
        assert_eq!(mode(&data)?, 0o700);
        assert_eq!(mode(&path)?, 0o600);
        // an existing file with a loose mode is tightened on open
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        let s = Store::open(&path)?;
        assert_eq!(mode(&path)?, 0o600);
        let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
        if wal.exists() {
            assert_eq!(mode(&wal)?, 0o600);
        }
        let secure: i64 = s.conn.query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
        assert_eq!(secure, 1);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn with_savepoint_rolls_back_everything_on_error_and_nests() -> R {
        let s = Store::open_in_memory()?;
        let sid = s.begin_session(0, "e")?;
        let r: Result<(), StoreError> = s.with_savepoint(|s| {
            s.insert_segment(sid, 1, 2, "half", None)?;
            // a nested transaction-using call commits only its savepoint
            s.apply_categorisation(&[(1, None)])?;
            Err(StoreError::Io(std::io::Error::other("fail mid-way")))
        });
        assert!(r.is_err());
        assert!(s.segments(View::All, 0)?.is_empty(), "nothing half-applied");
        assert!(s.conn.is_autocommit(), "no transaction left open");
        s.with_savepoint(|s| {
            s.insert_segment(sid, 1, 2, "kept", None)?;
            let inner: Result<(), StoreError> = s.with_savepoint(|s| {
                s.insert_segment(sid, 3, 4, "inner, rolled back", None)?;
                Err(StoreError::Io(std::io::Error::other("inner fails")))
            });
            assert!(inner.is_err());
            Ok(())
        })?;
        let rows = s.segments(View::All, 0)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "kept");
        Ok(())
    }

    /// A database whose `episodes` table predates AUTOINCREMENT is rebuilt in place on open
    /// (same rows, same schema version) and never hands out an id something still points at.
    #[test]
    fn an_old_episodes_table_is_upgraded_so_ids_are_never_reused() -> R {
        let dir = std::env::temp_dir().join(format!(
            "oats-store-autoinc-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let path = dir.join("oats.sqlite");
        let content = EpisodeContent {
            title: "t".into(),
            ..Default::default()
        };
        {
            let s = Store::open(&path)?;
            let a = s.insert_episode(0, 10, &content, 1)?;
            let b = s.insert_episode(20, 30, &content, 1)?;
            s.upsert_event(
                &NewEvent {
                    when_start: 5,
                    title: "from b".into(),
                    source_episode_id: Some(b.id),
                    ..Default::default()
                },
                1,
            )?;
            // simulate the old table: plain INTEGER PRIMARY KEY, b's row gone
            s.conn.execute_batch(&format!(
                "PRAGMA foreign_keys=OFF;
                 CREATE TABLE old AS SELECT * FROM episodes WHERE id = {};
                 DROP TABLE episodes;
                 CREATE TABLE episodes (id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL,
                    ended_at INTEGER NOT NULL, title TEXT NOT NULL, summary TEXT NOT NULL,
                    node_id INTEGER, decisions_json TEXT NOT NULL DEFAULT '[]',
                    action_items_json TEXT NOT NULL DEFAULT '[]',
                    topics_json TEXT NOT NULL DEFAULT '[]', low_signal INTEGER NOT NULL DEFAULT 0,
                    model TEXT NOT NULL DEFAULT '', cost_usd REAL,
                    revision INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL,
                    last_revised_at INTEGER NOT NULL);
                 INSERT INTO episodes SELECT id, started_at, ended_at, title, summary, node_id,
                    decisions_json, action_items_json, topics_json, low_signal, model, cost_usd,
                    revision, created_at, last_revised_at FROM old;
                 DROP TABLE old;
                 DELETE FROM sqlite_sequence WHERE name = 'episodes';",
                a.id
            ))?;
        }
        let s = Store::open(&path)?;
        assert_eq!(version(&s)?, "5");
        assert_eq!(s.episodes(10)?.len(), 1, "rows survive the rebuild");
        let c = s.insert_episode(40, 50, &content, 2)?;
        let events = s.events_from(i64::MIN, 10)?;
        assert!(
            events.iter().all(|e| e.source_episode_id != Some(c.id)),
            "new episode {} adopted {events:?}",
            c.id
        );
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn counts_runs_per_tier() -> R {
        let s = Store::open_in_memory()?;
        s.begin_run(0, "command", "haiku", 100)?;
        s.begin_run(0, "categorise", "haiku", 200)?;
        s.begin_run(1, "interpret", "sonnet", 300)?;
        assert_eq!(s.run_count_of_tier_since(0, 0)?, 2);
        assert_eq!(s.run_count_of_tier_since(0, 150)?, 1);
        assert_eq!(s.run_count_of_tier_since(1, 0)?, 1);
        assert_eq!(s.run_count_since(0)?, 3);
        Ok(())
    }

    #[test]
    fn episodes_with_topic_matches_case_insensitively() -> R {
        let s = Store::open_in_memory()?;
        let mk = |title: &str, topics: &[&str]| EpisodeContent {
            title: title.into(),
            summary: String::new(),
            node_id: None,
            decisions: vec![],
            action_items: vec![],
            topics: topics.iter().map(|t| t.to_string()).collect(),
            low_signal: false,
            model: "m".into(),
            cost_usd: None,
        };
        s.insert_episode(10, 20, &mk("old", &["Q4 roadmap", "x"]), 1)?;
        s.insert_episode(30, 40, &mk("new", &["q4 ROADMAP"]), 2)?;
        s.insert_episode(50, 60, &mk("other", &["Q4 roadmap extra", "50%_"]), 3)?;
        let hits = s.episodes_with_topic("Q4 Roadmap")?;
        assert_eq!(
            hits.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(),
            vec!["new", "old"]
        );
        assert!(s.episodes_with_topic("nothing")?.is_empty());
        assert!(s.episodes_with_topic("Q4")?.is_empty(), "whole topic only");
        Ok(())
    }
}
