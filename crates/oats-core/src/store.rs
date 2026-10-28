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
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum View {
    /// Segments since local midnight.
    Today,
    /// Everything.
    All,
    /// Segments filed under one category.
    Category(i64),
}

impl Default for View {
    fn default() -> Self {
        Self::Today
    }
}

/// The default categories, in sidebar order.
pub const DEFAULT_CATEGORIES: [&str; 3] = ["Work", "Admin", "Personal"];

pub struct Store {
    conn: rusqlite::Connection,
}

impl Store {
    /// Open (creating the file, its parent directory and the schema as needed).
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let _ = path;
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    /// An in-memory store (tests).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    pub fn begin_session(&self, started_at: i64, engine: &str) -> Result<i64, StoreError> {
        let _ = (started_at, engine, &self.conn);
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    pub fn end_session(&self, id: i64, ended_at: i64) -> Result<(), StoreError> {
        let _ = (id, ended_at);
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
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
        let _ = (session_id, started_at, ended_at, text, confidence);
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    /// Segments for a view, oldest first. `today_start` is local midnight in Unix ms (the UI
    /// computes it; the store stays timezone-free).
    pub fn segments(&self, view: View, today_start: i64) -> Result<Vec<TranscriptRow>, StoreError> {
        let _ = (view, today_start);
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    pub fn set_segment_category(
        &self,
        segment_id: i64,
        category_id: Option<i64>,
    ) -> Result<(), StoreError> {
        let _ = (segment_id, category_id);
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    pub fn delete_segment(&self, segment_id: i64) -> Result<(), StoreError> {
        let _ = segment_id;
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    /// Categories in sidebar order.
    pub fn categories(&self) -> Result<Vec<Category>, StoreError> {
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    /// Append a category (trimmed; a duplicate name returns the existing one).
    pub fn add_category(&self, name: &str) -> Result<Category, StoreError> {
        let _ = name;
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    pub fn rename_category(&self, id: i64, name: &str) -> Result<(), StoreError> {
        let _ = (id, name);
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    /// Delete a category; its segments go back to uncategorised.
    pub fn delete_category(&self, id: i64) -> Result<(), StoreError> {
        let _ = id;
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }

    /// Count of segments whose `started_at` ≥ `since` (the status line's "N today").
    pub fn segment_count_since(&self, since: i64) -> Result<i64, StoreError> {
        let _ = since;
        Err(StoreError::Sqlite(rusqlite::Error::InvalidQuery))
    }
}
