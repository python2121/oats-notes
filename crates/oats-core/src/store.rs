//! The transcript store: one SQLite file (WAL mode), opened once per thread that uses it (the
//! pipeline writes, the UI reads). Times are Unix milliseconds (UTC); the UI formats them in
//! local time.
//!
//! Schema (created on open; `schema_version` in `meta` for later migrations):
//! - `sessions(id, started_at, ended_at NULL, engine)` — one per listen start/stop.
//! - `segments(id, session_id, started_at, ended_at, text, confidence NULL, category_id NULL)`
//!   — one per transcribed speech segment, indexed on `started_at`.
//! - `categories(id, name UNIQUE, position)` — the sidebar's buckets; seeded with
//!   Work / Admin / Personal when empty.

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub engine: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptRow {
    pub id: i64,
    pub session_id: i64,
    pub started_at: i64,
    pub ended_at: i64,
    pub text: String,
    pub confidence: Option<f32>,
    pub category_id: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub position: i64,
}

/// What the transcript list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum View {
    /// Segments since local midnight.
    #[default]
    Today,
    /// Everything.
    All,
    /// Segments filed under one category.
    Category(i64),
}

/// The default categories, in sidebar order.
pub const DEFAULT_CATEGORIES: [&str; 3] = ["Work", "Admin", "Personal"];

pub struct Store {
    conn: rusqlite::Connection,
}

const SCHEMA_VERSION: i64 = 1;

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
CREATE TABLE IF NOT EXISTS categories (
    id       INTEGER PRIMARY KEY,
    name     TEXT NOT NULL UNIQUE,
    position INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS segments (
    id          INTEGER PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions(id),
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER NOT NULL,
    text        TEXT NOT NULL,
    confidence  REAL,
    category_id INTEGER REFERENCES categories(id)
);
CREATE INDEX IF NOT EXISTS segments_started_at ON segments(started_at);
CREATE INDEX IF NOT EXISTS segments_category ON segments(category_id);
";

const SEGMENT_COLUMNS: &str = "id, session_id, started_at, ended_at, text, confidence, category_id";

fn row_to_segment(row: &rusqlite::Row<'_>) -> rusqlite::Result<TranscriptRow> {
    Ok(TranscriptRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        text: row.get(4)?,
        confidence: row.get::<_, Option<f64>>(5)?.map(|c| c as f32),
        category_id: row.get(6)?,
    })
}

fn row_to_category(row: &rusqlite::Row<'_>) -> rusqlite::Result<Category> {
    Ok(Category {
        id: row.get(0)?,
        name: row.get(1)?,
        position: row.get(2)?,
    })
}

impl Store {
    /// Open (creating the file, its parent directory and the schema as needed).
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )?;
        Self::init(conn)
    }

    /// An in-memory store (tests).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Self::init(conn)
    }

    fn init(conn: rusqlite::Connection) -> Result<Self, StoreError> {
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
            [SCHEMA_VERSION.to_string()],
        )?;
        let store = Self { conn };
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM categories", [], |r| r.get(0))?;
        if count == 0 {
            for name in DEFAULT_CATEGORIES {
                store.add_category(name)?;
            }
        }
        Ok(store)
    }

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

    /// Insert a segment; returns the full row.
    pub fn insert_segment(
        &self,
        session_id: i64,
        started_at: i64,
        ended_at: i64,
        text: &str,
        confidence: Option<f32>,
    ) -> Result<TranscriptRow, StoreError> {
        self.conn.execute(
            "INSERT INTO segments(session_id, started_at, ended_at, text, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                session_id,
                started_at,
                ended_at,
                text,
                confidence.map(f64::from)
            ],
        )?;
        Ok(TranscriptRow {
            id: self.conn.last_insert_rowid(),
            session_id,
            started_at,
            ended_at,
            text: text.to_owned(),
            confidence,
            category_id: None,
        })
    }

    /// Segments for a view, oldest first. `today_start` is local midnight in Unix ms (the UI
    /// computes it; the store stays timezone-free).
    pub fn segments(&self, view: View, today_start: i64) -> Result<Vec<TranscriptRow>, StoreError> {
        let order = "ORDER BY started_at, id";
        let (filter, arg): (&str, Option<i64>) = match view {
            View::Today => ("WHERE started_at >= ?1", Some(today_start)),
            View::All => ("", None),
            View::Category(id) => ("WHERE category_id = ?1", Some(id)),
        };
        let sql = format!("SELECT {SEGMENT_COLUMNS} FROM segments {filter} {order}");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = match arg {
            Some(a) => stmt
                .query_map([a], row_to_segment)?
                .collect::<Result<Vec<_>, _>>()?,
            None => stmt
                .query_map([], row_to_segment)?
                .collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    pub fn set_segment_category(
        &self,
        segment_id: i64,
        category_id: Option<i64>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE segments SET category_id = ?2 WHERE id = ?1",
            rusqlite::params![segment_id, category_id],
        )?;
        Ok(())
    }

    pub fn delete_segment(&self, segment_id: i64) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM segments WHERE id = ?1", [segment_id])?;
        Ok(())
    }

    /// Categories in sidebar order.
    pub fn categories(&self) -> Result<Vec<Category>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, position FROM categories ORDER BY position, id")?;
        let rows = stmt
            .query_map([], row_to_category)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Append a category (trimmed; a duplicate name returns the existing one).
    pub fn add_category(&self, name: &str) -> Result<Category, StoreError> {
        let name = name.trim();
        if let Some(existing) = self.category_by_name(name)? {
            return Ok(existing);
        }
        self.conn.execute(
            "INSERT INTO categories(name, position)
             VALUES (?1, (SELECT COALESCE(MAX(position), -1) + 1 FROM categories))",
            [name],
        )?;
        let id = self.conn.last_insert_rowid();
        Ok(self.conn.query_row(
            "SELECT id, name, position FROM categories WHERE id = ?1",
            [id],
            row_to_category,
        )?)
    }

    fn category_by_name(&self, name: &str) -> Result<Option<Category>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, position FROM categories WHERE name = ?1",
                [name],
                row_to_category,
            )
            .optional()?)
    }

    pub fn rename_category(&self, id: i64, name: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE categories SET name = ?2 WHERE id = ?1",
            rusqlite::params![id, name.trim()],
        )?;
        Ok(())
    }

    /// Delete a category; its segments go back to uncategorised.
    pub fn delete_category(&self, id: i64) -> Result<(), StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE segments SET category_id = NULL WHERE category_id = ?1",
            [id],
        )?;
        tx.execute("DELETE FROM categories WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Count of segments whose `started_at` ≥ `since` (the status line's "N today").
    pub fn segment_count_since(&self, since: i64) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE started_at >= ?1",
            [since],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type R = Result<(), StoreError>;

    #[test]
    fn seeds_default_categories_and_schema_version() -> R {
        let s = Store::open_in_memory()?;
        let names: Vec<String> = s.categories()?.into_iter().map(|c| c.name).collect();
        assert_eq!(names, DEFAULT_CATEGORIES);
        let v: String = s.conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(v, "1");
        Ok(())
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
        assert_eq!(old.category_id, None);

        let all = s.segments(View::All, 1_000)?;
        assert_eq!(all, vec![old.clone(), earlier.clone(), new.clone()]);
        let today = s.segments(View::Today, 1_000)?;
        assert_eq!(today, vec![earlier.clone(), new.clone()]);
        assert_eq!(s.segment_count_since(1_000)?, 2);
        assert_eq!(s.segment_count_since(0)?, 3);

        let work = s.categories()?[0].id;
        s.set_segment_category(new.id, Some(work))?;
        let filed = s.segments(View::Category(work), 0)?;
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].id, new.id);
        assert_eq!(filed[0].category_id, Some(work));
        s.set_segment_category(new.id, None)?;
        assert!(s.segments(View::Category(work), 0)?.is_empty());

        s.delete_segment(old.id)?;
        assert_eq!(s.segments(View::All, 0)?.len(), 2);
        Ok(())
    }

    #[test]
    fn add_rename_delete_categories() -> R {
        let s = Store::open_in_memory()?;
        let c = s.add_category("  Ideas ")?;
        assert_eq!(c.name, "Ideas");
        assert_eq!(c.position, 3);
        let again = s.add_category("Ideas")?;
        assert_eq!(again, c);
        assert_eq!(s.categories()?.len(), 4);

        s.rename_category(c.id, " Thoughts ")?;
        assert_eq!(s.categories()?[3].name, "Thoughts");

        let sid = s.begin_session(0, "e")?;
        let seg = s.insert_segment(sid, 1, 2, "x", None)?;
        s.set_segment_category(seg.id, Some(c.id))?;
        s.delete_category(c.id)?;
        assert_eq!(s.categories()?.len(), 3);
        assert_eq!(s.segments(View::All, 0)?[0].category_id, None);

        // positions keep growing after a delete
        assert_eq!(s.add_category("Next")?.position, 3);
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
}
