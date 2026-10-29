//! The store's media part (schema v7): what played on the system lane.
//!
//! - `segments.media_kind TEXT NULL` / `segments.media_ref TEXT NULL` — the stamp a system
//!   segment got when it closed ([`crate::media::MediaClass`] spelling; the URL).
//! - `episodes.kind TEXT NOT NULL DEFAULT 'conversation'` — `media` for an episode made of
//!   YouTube / music (filed under the auto-created **Media** category).
//! - `media_plays(id, started_at, ended_at, kind, url, title, producer, segments_count)` —
//!   one row per contiguous play of the same media (closed when it changes or stops), written
//!   by the pipeline while the policy logs plays.

use super::{NodeKind, Origin, Store, StoreError};
use crate::media::{MediaClass, MediaPlay, MediaStamp};
use crate::store::Node;

/// `episodes.kind` of a normal episode.
pub const EPISODE_CONVERSATION: &str = "conversation";
/// `episodes.kind` of an episode made of media.
pub const EPISODE_MEDIA: &str = "media";
/// The top-level category media episodes are filed under.
pub const MEDIA_CATEGORY: &str = "Media";

const PLAY_COLUMNS: &str = "id, started_at, ended_at, kind, url, title, producer, segments_count";

fn row_to_play(row: &rusqlite::Row<'_>) -> rusqlite::Result<MediaPlay> {
    Ok(MediaPlay {
        id: row.get(0)?,
        started_at: row.get(1)?,
        ended_at: row.get(2)?,
        kind: MediaClass::parse(&row.get::<_, String>(3)?).unwrap_or(MediaClass::Other),
        url: row.get(4)?,
        title: row.get(5)?,
        producer: row.get(6)?,
        segments_count: row.get(7)?,
    })
}

fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

impl Store {
    /// v7's columns and table, added in place when missing (idempotent; runs inside `init`'s
    /// transaction, after the episodes rebuild).
    pub(super) fn ensure_media_schema(conn: &rusqlite::Connection) -> Result<(), StoreError> {
        if !has_column(conn, "segments", "media_kind")? {
            conn.execute_batch("ALTER TABLE segments ADD COLUMN media_kind TEXT;")?;
        }
        if !has_column(conn, "segments", "media_ref")? {
            conn.execute_batch("ALTER TABLE segments ADD COLUMN media_ref TEXT;")?;
        }
        if !has_column(conn, "episodes", "kind")? {
            conn.execute_batch(
                "ALTER TABLE episodes ADD COLUMN kind TEXT NOT NULL DEFAULT 'conversation';",
            )?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS media_plays (
                 id             INTEGER PRIMARY KEY,
                 started_at     INTEGER NOT NULL,
                 ended_at       INTEGER NOT NULL,
                 kind           TEXT NOT NULL,
                 url            TEXT,
                 title          TEXT NOT NULL DEFAULT '',
                 producer       TEXT NOT NULL DEFAULT '',
                 segments_count INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS media_plays_started ON media_plays(started_at);",
        )?;
        Ok(())
    }

    // ---- segments ---------------------------------------------------------------------

    /// Stamp a segment with what was playing (`None` clears it).
    pub fn set_segment_media(
        &self,
        id: i64,
        kind: Option<MediaClass>,
        media_ref: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE segments SET media_kind = ?2, media_ref = ?3 WHERE id = ?1",
            rusqlite::params![id, kind.map(MediaClass::as_str), media_ref],
        )?;
        Ok(())
    }

    // ---- media plays ------------------------------------------------------------------

    /// Open a play of `stamp` at `at` (ms); returns its id.
    pub fn begin_media_play(&self, at: i64, stamp: &MediaStamp) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO media_plays(started_at, ended_at, kind, url, title, producer)
             VALUES (?1, ?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                at,
                stamp.class.as_str(),
                stamp.url,
                stamp.title,
                stamp.producer
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Move a play's end to `ended_at` and set its segment count (never moves the end back).
    pub fn update_media_play(
        &self,
        id: i64,
        ended_at: i64,
        segments_count: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE media_plays SET ended_at = MAX(ended_at, ?2), segments_count = ?3
             WHERE id = ?1",
            rusqlite::params![id, ended_at, segments_count],
        )?;
        Ok(())
    }

    /// Update a play's title (the tab title arrives after the URL, or changes).
    pub fn set_media_play_title(&self, id: i64, title: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE media_plays SET title = ?2 WHERE id = ?1",
            rusqlite::params![id, title],
        )?;
        Ok(())
    }

    /// One play.
    pub fn media_play(&self, id: i64) -> Result<Option<MediaPlay>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {PLAY_COLUMNS} FROM media_plays WHERE id = ?1"),
                [id],
                row_to_play,
            )
            .optional()?)
    }

    /// The plays overlapping `[from, to)` (ms), oldest first.
    pub fn media_plays_between(&self, from: i64, to: i64) -> Result<Vec<MediaPlay>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PLAY_COLUMNS} FROM media_plays
             WHERE started_at < ?2 AND ended_at >= ?1
             ORDER BY started_at, id"
        ))?;
        let rows = stmt
            .query_map([from, to], row_to_play)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete a play (the user's "forget this").
    pub fn delete_media_play(&self, id: i64) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM media_plays WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---- episodes ---------------------------------------------------------------------

    /// `episodes.kind`: [`EPISODE_CONVERSATION`] or [`EPISODE_MEDIA`] (`None`: no such
    /// episode).
    pub fn episode_kind(&self, id: i64) -> Result<Option<String>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row("SELECT kind FROM episodes WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Set `episodes.kind`.
    pub fn set_episode_kind(&self, id: i64, kind: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE episodes SET kind = ?2 WHERE id = ?1",
            rusqlite::params![id, kind],
        )?;
        Ok(())
    }

    /// The ids of the media episodes overlapping `[from, to)`.
    pub fn media_episode_ids_between(&self, from: i64, to: i64) -> Result<Vec<i64>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM episodes WHERE kind = 'media' AND started_at < ?2 AND ended_at >= ?1
             ORDER BY started_at, id",
        )?;
        let rows = stmt
            .query_map([from, to], |r| r.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(rows)
    }

    /// The top-level **Media** category, created (`created_by = auto`) on first use.
    pub fn media_category(&self) -> Result<Node, StoreError> {
        self.add_node(None, NodeKind::Category, MEDIA_CATEGORY, "", Origin::Auto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::AudioSourceKind;

    type R = Result<(), StoreError>;

    fn stamp(url: &str, title: &str) -> MediaStamp {
        MediaStamp {
            class: MediaClass::YouTube,
            url: Some(url.into()),
            title: title.into(),
            producer: "Firefox".into(),
        }
    }

    #[test]
    fn a_v6_database_gains_the_media_columns_and_table() -> R {
        let dir = std::env::temp_dir().join(format!("oats-v7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("oats.sqlite");
        {
            let s = Store::open(&path)?;
            let session = s.begin_session(0, "t")?;
            s.insert_segment(session, 0, 1_000, "before", None)?;
            // back to v6: no media columns, no table
            s.conn.execute_batch(
                "ALTER TABLE segments DROP COLUMN media_kind;
                 ALTER TABLE segments DROP COLUMN media_ref;
                 ALTER TABLE episodes DROP COLUMN kind;
                 DROP TABLE media_plays;
                 UPDATE meta SET value = '6' WHERE key = 'schema_version';",
            )?;
        }
        let s = Store::open(&path)?;
        let version: String = s.conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(version, "7");
        let rows = s.segments_between(0, 10_000)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].media_kind, None);
        assert!(s.media_plays_between(0, i64::MAX)?.is_empty());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn segments_are_stamped_and_plays_logged() -> R {
        let s = Store::open_in_memory()?;
        let session = s.begin_session(0, "t")?;
        let row = s.insert_segment_from(
            session,
            1_000,
            2_000,
            "video words",
            None,
            AudioSourceKind::System,
            None,
        )?;
        s.set_segment_media(
            row.id,
            Some(MediaClass::YouTube),
            Some("https://youtu.be/a"),
        )?;
        let back = s
            .segment(row.id)?
            .ok_or(StoreError::Io(std::io::Error::other("gone")))?;
        assert_eq!(back.media_kind, Some(MediaClass::YouTube));
        assert_eq!(back.media_ref.as_deref(), Some("https://youtu.be/a"));

        let a = s.begin_media_play(10_000, &stamp("https://youtu.be/a", "A"))?;
        s.update_media_play(a, 40_000, 3)?;
        s.update_media_play(a, 30_000, 3)?; // never moves back
        let b = s.begin_media_play(50_000, &stamp("https://youtu.be/b", "B"))?;
        s.set_media_play_title(b, "B (live)")?;
        let plays = s.media_plays_between(0, 100_000)?;
        assert_eq!(plays.len(), 2);
        assert_eq!(
            (
                plays[0].started_at,
                plays[0].ended_at,
                plays[0].segments_count
            ),
            (10_000, 40_000, 3)
        );
        assert_eq!(plays[1].title, "B (live)");
        assert_eq!(s.media_plays_between(41_000, 49_000)?.len(), 0);
        assert_eq!(s.media_plays_between(35_000, 45_000)?.len(), 1);
        s.delete_media_play(a)?;
        assert_eq!(s.media_play(a)?, None);
        Ok(())
    }

    #[test]
    fn media_category_and_episode_kind() -> R {
        let s = Store::open_in_memory()?;
        let media = s.media_category()?;
        assert_eq!(media.name, MEDIA_CATEGORY);
        assert_eq!(media.created_by, Origin::Auto);
        assert_eq!(media.parent_id, None);
        assert_eq!(s.media_category()?.id, media.id, "created once");
        let ep = s
            .insert_episode(
                0,
                60_000,
                &crate::store::EpisodeContent {
                    title: "A video".into(),
                    node_id: Some(media.id),
                    ..Default::default()
                },
                0,
            )?
            .id;
        assert_eq!(s.episode_kind(ep)?.as_deref(), Some(EPISODE_CONVERSATION));
        s.set_episode_kind(ep, EPISODE_MEDIA)?;
        assert_eq!(s.episode_kind(ep)?.as_deref(), Some(EPISODE_MEDIA));
        assert_eq!(s.media_episode_ids_between(0, 100_000)?, vec![ep]);
        Ok(())
    }
}
