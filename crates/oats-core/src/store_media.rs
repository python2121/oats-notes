//! The store's media part (schema v7): what played on the system lane.
//!
//! - `segments.media_kind TEXT NULL` / `segments.media_ref TEXT NULL` — the stamp a system
//!   segment got when it closed ([`crate::media::MediaClass`] spelling; the URL).
//! - `episodes.kind TEXT NOT NULL DEFAULT 'conversation'` — `media` for an episode made of
//!   YouTube / music (filed under the auto-created **Media** category).
//! - `media_plays(id, started_at, ended_at, kind, url, title, producer, segments_count)` —
//!   one row per contiguous play of the same media (closed when it changes or stops), written
//!   by the pipeline while the policy logs plays.
//!
//! Schema v8 adds the per-app classes:
//! - `producer_overrides(bundle_id PRIMARY KEY, class, transcribe_anyway)` — the user's
//!   [`ProducerClass`] per app (a missing row is Auto) and, for a media app, whether it is
//!   transcribed anyway under Ignore / Log only;
//! - `producers_seen(bundle_id PRIMARY KEY, name, last_seen)` — every app the media watcher
//!   saw playing sound (the Preferences "Apps" table);
//! - `segments.media_app TEXT NULL` — the bundle id of the app that played a system segment.
//!
//! And the user's relabels: [`Store::mark_episode_as_call`] / [`Store::mark_episode_as_media`]
//! / [`Store::mark_play_as_call`].

use std::collections::HashMap;

use super::{NodeKind, Origin, Store, StoreError};
use crate::media::{
    MediaClass, MediaPlay, MediaStamp, ProducerClass, ProducerOverride, ProducerSeen,
};
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

/// The `media_kind` values that are media proper (see [`MediaClass::is_media`]).
const MEDIA_KINDS_SQL: &str = "('youtube', 'music', 'app')";

/// What a relabel ("Mark as a call", "Mark as media") changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Relabel {
    /// The episodes whose kind changed (each is worth a re-interpretation).
    pub episodes: Vec<i64>,
    /// The system segments re-stamped.
    pub segments: usize,
}

impl Relabel {
    /// Something was transcribed: there are words to re-interpret.
    pub fn transcribed(&self) -> bool {
        self.segments > 0
    }
}

/// Consecutive plays of the same URL at most this far apart are one row of the Media view.
pub const MEDIA_COLLAPSE_MS: i64 = 10 * 60_000;

/// One row of the Media view (the watch history): a play, or consecutive plays of the same URL
/// at most [`MEDIA_COLLAPSE_MS`] apart folded together (a pause, a reload, a tab switch away
/// and back).
#[derive(Clone, Debug, PartialEq)]
pub struct MediaEntry {
    /// The newest play of the run: its title, URL, kind and app are what the row shows.
    pub play: MediaPlay,
    /// Every play folded in, newest first.
    pub ids: Vec<i64>,
    /// The earliest start and the latest end.
    pub started_at: i64,
    pub ended_at: i64,
    /// The plays' own durations summed (the gaps between them are not counted).
    pub duration_ms: i64,
    /// Transcribed segments over all its plays.
    pub segments_count: i64,
}

impl MediaEntry {
    fn of(play: MediaPlay) -> Self {
        MediaEntry {
            ids: vec![play.id],
            started_at: play.started_at,
            ended_at: play.ended_at,
            duration_ms: (play.ended_at - play.started_at).max(0),
            segments_count: play.segments_count,
            play,
        }
    }

    /// `older` (a play that started before this row) continues it: the same media (the URL;
    /// for music the URL and the title, see [`MediaPlay::same_media`]), at most
    /// [`MEDIA_COLLAPSE_MS`] before it.
    fn continues(&self, older: &MediaPlay) -> bool {
        older.same_media(&self.play) && self.started_at - older.ended_at <= MEDIA_COLLAPSE_MS
    }
}

/// Append `plays` (newest first, all older than the rows already in `entries`) to `entries`,
/// folding each into the last row when it continues it ([`MediaEntry`]). Paging calls it once
/// per page, so a run split by a page boundary still becomes one row.
pub fn collapse_plays(entries: &mut Vec<MediaEntry>, plays: impl IntoIterator<Item = MediaPlay>) {
    for p in plays {
        match entries.last_mut() {
            Some(last) if last.continues(&p) => {
                last.ids.push(p.id);
                last.started_at = last.started_at.min(p.started_at);
                last.ended_at = last.ended_at.max(p.ended_at);
                last.duration_ms += (p.ended_at - p.started_at).max(0);
                last.segments_count += p.segments_count;
            }
            _ => entries.push(MediaEntry::of(p)),
        }
    }
}

/// A media episode's summary without the `**Media:** …` header line the media filing wrote.
fn without_media_header(summary: &str) -> String {
    summary
        .lines()
        .filter(|l| !l.trim_start().starts_with("**Media:**"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
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
             CREATE INDEX IF NOT EXISTS media_plays_started ON media_plays(started_at);
             CREATE INDEX IF NOT EXISTS media_plays_url ON media_plays(url, started_at);",
        )?;
        // v8: the per-app classes
        if !has_column(conn, "segments", "media_app")? {
            conn.execute_batch("ALTER TABLE segments ADD COLUMN media_app TEXT;")?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS producer_overrides (
                 bundle_id         TEXT PRIMARY KEY,
                 class             TEXT NOT NULL DEFAULT 'auto',
                 transcribe_anyway INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS producers_seen (
                 bundle_id TEXT PRIMARY KEY,
                 name      TEXT NOT NULL DEFAULT '',
                 last_seen INTEGER NOT NULL
             );",
        )?;
        Ok(())
    }

    // ---- per-app classes ----------------------------------------------------------------

    /// Every stored per-app override (bundle ids as stored, lower case).
    pub fn producer_overrides(&self) -> Result<Vec<ProducerOverride>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT bundle_id, class, transcribe_anyway FROM producer_overrides ORDER BY bundle_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProducerOverride {
                    bundle_id: r.get(0)?,
                    class: ProducerClass::parse(&r.get::<_, String>(1)?).unwrap_or_default(),
                    transcribe_anyway: r.get::<_, i64>(2)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Set an app's class (bundle ids are case-insensitive: stored lower case). Auto without
    /// "transcribe anyway" is no override: the row is removed.
    pub fn set_producer_override(
        &self,
        bundle_id: &str,
        class: ProducerClass,
        transcribe_anyway: bool,
    ) -> Result<(), StoreError> {
        let key = bundle_id.trim().to_ascii_lowercase();
        if class == ProducerClass::Auto && !transcribe_anyway {
            self.conn
                .execute("DELETE FROM producer_overrides WHERE bundle_id = ?1", [key])?;
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO producer_overrides(bundle_id, class, transcribe_anyway) VALUES (?1, ?2, ?3)
             ON CONFLICT(bundle_id) DO UPDATE SET class = ?2, transcribe_anyway = ?3",
            rusqlite::params![key, class.as_str(), i64::from(transcribe_anyway)],
        )?;
        Ok(())
    }

    /// Record the apps seen playing sound at `at` (`(bundle_id, name)`; empty ids skipped).
    pub fn note_producers_seen(
        &self,
        seen: &[(String, String)],
        at: i64,
    ) -> Result<(), StoreError> {
        self.with_savepoint(|s| {
            for (bundle, name) in seen {
                let key = bundle.trim().to_ascii_lowercase();
                if key.is_empty() {
                    continue;
                }
                s.conn.execute(
                    "INSERT INTO producers_seen(bundle_id, name, last_seen) VALUES (?1, ?2, ?3)
                     ON CONFLICT(bundle_id) DO UPDATE SET name = ?2, last_seen = MAX(last_seen, ?3)",
                    rusqlite::params![key, name, at],
                )?;
            }
            Ok(())
        })
    }

    /// The apps seen playing sound, most recently seen first.
    pub fn producers_seen(&self) -> Result<Vec<ProducerSeen>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT bundle_id, name, last_seen FROM producers_seen ORDER BY last_seen DESC, bundle_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ProducerSeen {
                    bundle_id: r.get(0)?,
                    name: r.get(1)?,
                    last_seen: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Per episode, the bundle ids of the apps that played its system segments.
    pub fn episode_media_apps(&self, ids: &[i64]) -> Result<HashMap<i64, Vec<String>>, StoreError> {
        let mut out: HashMap<i64, Vec<String>> = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT media_app FROM segments
             WHERE episode_id = ?1 AND source = 'system' AND media_app IS NOT NULL
               AND media_app != '' AND echo_of IS NULL
             ORDER BY media_app",
        )?;
        for id in ids {
            let apps = stmt
                .query_map([id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            if !apps.is_empty() {
                out.insert(*id, apps);
            }
        }
        Ok(out)
    }

    // ---- relabels -----------------------------------------------------------------------

    /// "Mark as a call" on a media episode: it becomes a conversation (`kind`), leaves the
    /// Media category (unfiled: the Conversations list), loses its `**Media:**` header, and
    /// its system segments are re-stamped as a call (`media_kind = meeting`, so the next
    /// interpretation gets the meeting hint and none of the media rules) and unfiled from
    /// Media; the plays logged over it become calls. `false`: no such media episode.
    /// The caller queues the re-interpretation.
    pub fn mark_episode_as_call(&self, id: i64) -> Result<bool, StoreError> {
        let Some(ep) = self.episode(id)? else {
            return Ok(false);
        };
        if !ep.is_media() {
            return Ok(false);
        }
        self.with_savepoint(|s| {
            let media = s.find_node(None, MEDIA_CATEGORY)?.map(|n| n.id);
            s.conn.execute(
                "UPDATE episodes SET kind = ?2, summary = ?3,
                     node_id = CASE WHEN node_id = ?4 THEN NULL ELSE node_id END
                 WHERE id = ?1",
                rusqlite::params![
                    id,
                    EPISODE_CONVERSATION,
                    without_media_header(&ep.summary),
                    media
                ],
            )?;
            // the plays the episode's media was logged as (same URL, or no URL)
            let refs: Vec<Option<String>> = {
                let mut stmt = s.conn.prepare(&format!(
                    "SELECT DISTINCT media_ref FROM segments
                     WHERE episode_id = ?1 AND source = 'system' AND media_kind IN {MEDIA_KINDS_SQL}"
                ))?;
                stmt.query_map([id], |r| r.get(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for r in &refs {
                s.conn.execute(
                    &format!(
                        "UPDATE media_plays SET kind = 'call'
                         WHERE started_at <= ?2 AND ended_at >= ?1 AND kind IN {MEDIA_KINDS_SQL}
                           AND (url = ?3 OR (url IS NULL AND ?3 IS NULL))"
                    ),
                    rusqlite::params![ep.started_at, ep.ended_at, r],
                )?;
            }
            s.conn.execute(
                "UPDATE segments SET node_id = NULL, categorised = 0
                 WHERE episode_id = ?1 AND node_id IS ?2",
                rusqlite::params![id, media],
            )?;
            s.conn.execute(
                &format!(
                    "UPDATE segments SET media_kind = 'meeting'
                     WHERE episode_id = ?1 AND source = 'system' AND media_kind IN {MEDIA_KINDS_SQL}"
                ),
                [id],
            )?;
            Ok(true)
        })
    }

    /// "Mark as media" on a conversation episode (a call in an app the user counts as media):
    /// it becomes a media episode filed under Media, its system segments stamped as a media
    /// app (`media_kind = app`) and filed under Media; calls logged over it become media
    /// plays again. `false`: no such conversation episode. The caller queues the
    /// re-interpretation (the media rules: no todos, no events).
    pub fn mark_episode_as_media(&self, id: i64) -> Result<bool, StoreError> {
        let Some(ep) = self.episode(id)? else {
            return Ok(false);
        };
        if ep.is_media() {
            return Ok(false);
        }
        self.with_savepoint(|s| {
            let media = s.media_category()?.id;
            s.conn.execute(
                "UPDATE episodes SET kind = ?2, node_id = ?3 WHERE id = ?1",
                rusqlite::params![id, EPISODE_MEDIA, media],
            )?;
            s.conn.execute(
                &format!(
                    "UPDATE segments SET media_kind = 'app', node_id = ?2, categorised = 1
                     WHERE episode_id = ?1 AND source = 'system' AND echo_of IS NULL
                       AND (media_kind IS NULL OR media_kind NOT IN {MEDIA_KINDS_SQL})"
                ),
                rusqlite::params![id, media],
            )?;
            s.conn.execute(
                "UPDATE media_plays SET kind = 'app'
                 WHERE started_at <= ?2 AND ended_at >= ?1 AND kind = 'call'",
                rusqlite::params![ep.started_at, ep.ended_at],
            )?;
            Ok(true)
        })
    }

    /// "Mark as a call" on a logged play: the play becomes a call (`kind = call`; the Calendar
    /// shows "Call via <app>"); whatever was transcribed during it (same URL, or none) is
    /// re-stamped as a call, and the media episodes it belongs to become conversations
    /// ([`Self::mark_episode_as_call`]). Under Log only nothing was transcribed: then only
    /// the label changes (`Relabel::transcribed` is false). `None`: no such play.
    pub fn mark_play_as_call(&self, play_id: i64) -> Result<Option<Relabel>, StoreError> {
        let Some(play) = self.media_play(play_id)? else {
            return Ok(None);
        };
        self.with_savepoint(|s| {
            s.conn.execute(
                "UPDATE media_plays SET kind = 'call' WHERE id = ?1",
                [play_id],
            )?;
            // a segment closes up to its max length after the play's last sound
            let (from, to) = (play.started_at - 1_000, play.ended_at + 30_000);
            let rows: Vec<(i64, Option<i64>)> = {
                let mut stmt = s.conn.prepare(&format!(
                    "SELECT id, episode_id FROM segments
                     WHERE source = 'system' AND echo_of IS NULL
                       AND started_at >= ?1 AND started_at <= ?2
                       AND media_kind IN {MEDIA_KINDS_SQL}
                       AND (media_ref = ?3 OR (media_ref IS NULL AND ?3 IS NULL))"
                ))?;
                stmt.query_map(rusqlite::params![from, to, play.url], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
            };
            let mut out = Relabel {
                segments: rows.len(),
                ..Relabel::default()
            };
            for (_, ep) in &rows {
                if let Some(ep) = ep
                    && !out.episodes.contains(ep)
                    && s.mark_episode_as_call(*ep)?
                {
                    out.episodes.push(*ep);
                }
            }
            // segments not in an episode yet: re-stamped (and unfiled from Media) directly
            let media = s.find_node(None, MEDIA_CATEGORY)?.map(|n| n.id);
            for (seg, ep) in &rows {
                if ep.is_none() {
                    s.conn.execute(
                        "UPDATE segments SET media_kind = 'meeting',
                             node_id = CASE WHEN node_id IS ?2 THEN NULL ELSE node_id END,
                             categorised = CASE WHEN node_id IS ?2 THEN 0 ELSE categorised END
                         WHERE id = ?1",
                        rusqlite::params![seg, media],
                    )?;
                }
            }
            Ok(Some(out))
        })
    }

    // ---- segments ---------------------------------------------------------------------

    /// Stamp a system segment with `stamp`: its class, URL and app (`media_app`).
    pub fn stamp_segment_media(&self, id: i64, stamp: &MediaStamp) -> Result<(), StoreError> {
        let app = Some(stamp.bundle_id.as_str()).filter(|b| !b.is_empty());
        self.conn.execute(
            "UPDATE segments SET media_kind = ?2, media_ref = ?3, media_app = ?4 WHERE id = ?1",
            rusqlite::params![id, stamp.class.as_str(), stamp.url, app],
        )?;
        Ok(())
    }

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

    /// The plays overlapping `from..to` that are the LAST play of their media anywhere in the
    /// history (the Calendar's day panel: a video watched on several days is listed on the
    /// last one only, and once per day). The media is the URL; for music the URL and the
    /// title (a web player's playlist page plays many tracks). Calls are never scrubbed — a
    /// recurring meeting's link repeats by design — and nor are plays without a URL.
    pub fn media_plays_between_latest(
        &self,
        from: i64,
        to: i64,
    ) -> Result<Vec<MediaPlay>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PLAY_COLUMNS} FROM media_plays p
             WHERE p.started_at < ?2 AND p.ended_at >= ?1
               AND (p.kind IN ('call', 'meeting') OR p.url IS NULL OR NOT EXISTS (
                     SELECT 1 FROM media_plays q
                     WHERE q.url = p.url AND q.kind NOT IN ('call', 'meeting')
                       AND (p.kind != 'music' AND q.kind != 'music' OR q.title = p.title)
                       AND (q.started_at > p.started_at
                            OR (q.started_at = p.started_at AND q.id > p.id))))
             ORDER BY p.started_at, p.id"
        ))?;
        let rows = stmt
            .query_map([from, to], row_to_play)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One page of the watch history: the newest `limit` plays strictly before `before`
    /// (`(started_at, id)`; `None` = from the newest), NEWEST first. Calls are left out (they
    /// were never media).
    pub fn media_history_page(
        &self,
        before: Option<(i64, i64)>,
        limit: i64,
    ) -> Result<Vec<MediaPlay>, StoreError> {
        let (t, id) = before.unwrap_or((i64::MAX, i64::MAX));
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PLAY_COLUMNS} FROM media_plays
             WHERE kind != 'call' AND (started_at < ?1 OR (started_at = ?1 AND id < ?2))
             ORDER BY started_at DESC, id DESC LIMIT ?3"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![t, id, limit], row_to_play)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Search: the plays (calls left out) whose title or URL matches the LIKE pattern `pat`
    /// (backslash-escaped), newest first.
    pub(super) fn search_media_plays(
        &self,
        pat: &str,
        limit: i64,
    ) -> Result<Vec<MediaPlay>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PLAY_COLUMNS} FROM media_plays
             WHERE kind != 'call' AND (title LIKE ?1 ESCAPE '\\' OR url LIKE ?1 ESCAPE '\\')
             ORDER BY started_at DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![pat, limit], row_to_play)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The first transcribed (non-echo) system segment of any of these plays — the same
    /// segments "Mark as a call" re-stamps: stamped as media with the play's URL, starting
    /// within the play (up to 30 s after its end, a segment's maximum length).
    pub fn first_segment_of_plays(&self, ids: &[i64]) -> Result<Option<i64>, StoreError> {
        use rusqlite::OptionalExtension;
        let mut best: Option<(i64, i64)> = None;
        for id in ids {
            let Some(play) = self.media_play(*id)? else {
                continue;
            };
            let found: Option<(i64, i64)> = self
                .conn
                .query_row(
                    &format!(
                        "SELECT started_at, id FROM segments
                         WHERE source = 'system' AND echo_of IS NULL
                           AND started_at >= ?1 AND started_at <= ?2
                           AND media_kind IN {MEDIA_KINDS_SQL}
                           AND (media_ref = ?3 OR (media_ref IS NULL AND ?3 IS NULL))
                         ORDER BY started_at, id LIMIT 1"
                    ),
                    rusqlite::params![play.started_at - 1_000, play.ended_at + 30_000, play.url],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some(f) = found
                && best.is_none_or(|b| f < b)
            {
                best = Some(f);
            }
        }
        Ok(best.map(|(_, id)| id))
    }

    /// Delete several plays at once (a folded Media view row).
    pub fn delete_media_plays(&self, ids: &[i64]) -> Result<(), StoreError> {
        self.with_savepoint(|s| {
            for id in ids {
                s.delete_media_play(*id)?;
            }
            Ok(())
        })
    }

    /// The distinct play URLs containing `fragment` (case-sensitive; e.g. a video id, to know
    /// whether a cached thumbnail is still used).
    pub fn media_play_urls_containing(&self, fragment: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT url FROM media_plays WHERE url IS NOT NULL AND instr(url, ?1) > 0",
        )?;
        let rows = stmt
            .query_map([fragment], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    /// Episodes that started before `before` and contain a system segment of the call
    /// `meeting_ref` (the meeting page's URL, `segments.media_ref` of a `meeting`-stamped
    /// segment), newest first, at most `limit`. A link that recurs on earlier days is a
    /// standing meeting (a series).
    pub fn episodes_with_meeting_ref(
        &self,
        meeting_ref: &str,
        before: i64,
        limit: i64,
    ) -> Result<Vec<super::Episode>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM episodes WHERE started_at < ?2 AND id IN (
                 SELECT episode_id FROM segments
                 WHERE episode_id IS NOT NULL AND source = 'system'
                   AND media_kind = 'meeting' AND media_ref = ?1)
             ORDER BY started_at DESC, id DESC LIMIT ?3",
            super::EPISODE_COLUMNS
        ))?;
        let rows = stmt
            .query_map(
                rusqlite::params![meeting_ref, before, limit],
                super::row_to_episode,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Relabel a play (`MediaClass::Call` for "Mark as a call").
    pub fn set_media_play_kind(&self, id: i64, kind: MediaClass) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE media_plays SET kind = ?2 WHERE id = ?1",
            rusqlite::params![id, kind.as_str()],
        )?;
        Ok(())
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
        MediaStamp::new(MediaClass::YouTube, Some(url.into()), title, "Firefox")
    }

    #[test]
    fn the_calendar_lists_a_video_on_its_last_play_only_and_keeps_calls() -> R {
        let s = Store::open_in_memory()?;
        let day = 86_400_000;
        // the same video on day 1 (twice) and day 2; another video on day 1 only
        let a1 = s.begin_media_play(1_000, &stamp("https://youtu.be/a", "A"))?;
        s.update_media_play(a1, 2_000, 0)?;
        let a2 = s.begin_media_play(5_000, &stamp("https://youtu.be/a", "A"))?;
        s.update_media_play(a2, 6_000, 0)?;
        let a3 = s.begin_media_play(day + 1_000, &stamp("https://youtu.be/a", "A"))?;
        s.update_media_play(a3, day + 2_000, 0)?;
        let b = s.begin_media_play(3_000, &stamp("https://youtu.be/b", "B"))?;
        s.update_media_play(b, 4_000, 0)?;
        // a call with a repeating link, both days
        let call = MediaStamp::new(
            MediaClass::Call,
            Some("https://meet.example/team".into()),
            "Team",
            "Firefox",
        );
        let c1 = s.begin_media_play(7_000, &call)?;
        s.update_media_play(c1, 8_000, 0)?;
        let c2 = s.begin_media_play(day + 7_000, &call)?;
        s.update_media_play(c2, day + 8_000, 0)?;
        let ids = |v: Vec<MediaPlay>| v.into_iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(ids(s.media_plays_between_latest(0, day)?), vec![b, c1]);
        assert_eq!(
            ids(s.media_plays_between_latest(day, 2 * day)?),
            vec![a3, c2]
        );
        // the unscrubbed query still lists every play
        assert_eq!(s.media_plays_between(0, day)?.len(), 4);
        Ok(())
    }

    #[test]
    fn the_calendar_keeps_the_last_play_of_each_track_of_one_music_page() -> R {
        let s = Store::open_in_memory()?;
        let day = 86_400_000;
        let page = "https://open.spotify.com/playlist/p";
        let track = |t: &str| MediaStamp::new(MediaClass::Music, Some(page.into()), t, "Firefox");
        let one = s.begin_media_play(1_000, &track("One — X"))?;
        s.update_media_play(one, 2_000, 0)?;
        let two = s.begin_media_play(3_000, &track("Two — Y"))?;
        s.update_media_play(two, 4_000, 0)?;
        // track one again the next day: day one keeps track two only
        let again = s.begin_media_play(day + 1_000, &track("One — X"))?;
        s.update_media_play(again, day + 2_000, 0)?;
        let ids = |v: Vec<MediaPlay>| v.into_iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(ids(s.media_plays_between_latest(0, day)?), vec![two]);
        assert_eq!(
            ids(s.media_plays_between_latest(day, 2 * day)?),
            vec![again]
        );
        Ok(())
    }

    #[test]
    fn a_v7_database_gains_the_per_app_tables_and_column() -> R {
        let dir = std::env::temp_dir().join(format!("oats-v8-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("oats.sqlite");
        {
            let s = Store::open(&path)?;
            let session = s.begin_session(0, "t")?;
            s.insert_segment(session, 0, 1_000, "before", None)?;
            s.set_meta(crate::MediaPolicy::META_KEY, "log")?;
            // back to v7: no per-app tables, no media_app
            s.conn.execute_batch(
                "ALTER TABLE segments DROP COLUMN media_app;
                 DROP TABLE producer_overrides;
                 DROP TABLE producers_seen;
                 UPDATE meta SET value = '7' WHERE key = 'schema_version';",
            )?;
        }
        let s = Store::open(&path)?;
        let version: String = s.conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(version, "8");
        assert!(has_column(&s.conn, "segments", "media_app")?);
        assert!(s.producer_overrides()?.is_empty());
        assert!(s.producers_seen()?.is_empty());
        assert_eq!(s.segments_between(0, 10_000)?.len(), 1);
        // a policy the user chose is kept
        assert_eq!(
            s.meta(crate::MediaPolicy::META_KEY)?.as_deref(),
            Some("log")
        );
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn overrides_and_seen_producers_round_trip() -> R {
        let s = Store::open_in_memory()?;
        s.set_producer_override("com.hnc.Discord", ProducerClass::Media, true)?;
        s.set_producer_override("us.zoom.xos", ProducerClass::Ignore, false)?;
        let o = s.producer_overrides()?;
        assert_eq!(o.len(), 2);
        assert_eq!(o[0].bundle_id, "com.hnc.discord");
        assert!(o[0].transcribe_anyway);
        s.set_producer_override("com.hnc.discord", ProducerClass::Meeting, false)?;
        assert_eq!(s.producer_overrides()?[0].class, ProducerClass::Meeting);
        // auto without transcribe-anyway is no override
        s.set_producer_override("US.ZOOM.XOS", ProducerClass::Auto, false)?;
        assert_eq!(s.producer_overrides()?.len(), 1);

        s.note_producers_seen(&[("com.hnc.Discord".into(), "Discord".into())], 100)?;
        s.note_producers_seen(
            &[
                ("com.spotify.client".into(), "Spotify".into()),
                (String::new(), "nameless".into()),
            ],
            200,
        )?;
        s.note_producers_seen(&[("com.hnc.discord".into(), "Discord".into())], 50)?;
        let seen = s.producers_seen()?;
        assert_eq!(
            seen.iter()
                .map(|p| (p.bundle_id.as_str(), p.last_seen))
                .collect::<Vec<_>>(),
            [("com.spotify.client", 200), ("com.hnc.discord", 100)]
        );
        Ok(())
    }

    /// A Discord media episode: two system segments stamped `app`, filed under Media, a play.
    fn discord_episode(s: &Store) -> Result<(i64, i64, [i64; 2]), StoreError> {
        let session = s.begin_session(0, "t")?;
        let media = s.media_category()?;
        let mut discord = MediaStamp::new(MediaClass::App, None, "Discord", "Discord");
        discord.bundle_id = "com.hnc.discord".into();
        let mut segs = [0; 2];
        for (i, at) in [10_000, 20_000].into_iter().enumerate() {
            let row = s.insert_segment_from(
                session,
                at,
                at + 5_000,
                "we should ship on friday",
                None,
                AudioSourceKind::System,
                None,
            )?;
            s.stamp_segment_media(row.id, &discord)?;
            s.set_segment_node(row.id, Some(media.id))?;
            segs[i] = row.id;
        }
        let play = s.begin_media_play(10_000, &discord)?;
        s.update_media_play(play, 25_000, 2)?;
        let ep = s
            .insert_episode(
                10_000,
                25_000,
                &crate::store::EpisodeContent {
                    title: "Discord".into(),
                    summary: "**Media:** Discord\n\nTalk about shipping.".into(),
                    node_id: Some(media.id),
                    ..Default::default()
                },
                0,
            )?
            .id;
        s.set_episode_kind(ep, EPISODE_MEDIA)?;
        s.mark_segments_interpreted(&segs, ep, 0)?;
        Ok((ep, play, segs))
    }

    #[test]
    fn transcript_pages_can_leave_media_out() -> R {
        let s = Store::open_in_memory()?;
        let (_, _, segs) = discord_episode(&s)?;
        let session = s.begin_session(0, "t")?;
        let mine = s.insert_segment(session, 15_000, 16_000, "my own words", None)?;
        let call = s.insert_segment_from(
            session,
            17_000,
            18_000,
            "their words",
            None,
            AudioSourceKind::System,
            None,
        )?;
        s.stamp_segment_media(
            call.id,
            &MediaStamp::new(MediaClass::Meeting, None, "Zoom", "Zoom"),
        )?;
        let ids =
            |rows: Vec<crate::store::TranscriptRow>| rows.iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(
            ids(s.segments_page_with(None, 10, false)?),
            [mine.id, call.id]
        );
        assert_eq!(s.segments_page_with(None, 10, true)?.len(), 4);
        assert_eq!(
            ids(s.segments_after_with((0, 0), 10, false)?),
            [mine.id, call.id]
        );
        assert_eq!(s.segments_after_with((0, 0), 10, true)?.len(), 4);
        assert!(ids(s.segments_page(None, 10)?).contains(&segs[0]));
        Ok(())
    }

    #[test]
    fn mark_as_call_and_back_to_media() -> R {
        let s = Store::open_in_memory()?;
        let (ep, play, segs) = discord_episode(&s)?;
        assert_eq!(
            s.episode_media_apps(&[ep])?.get(&ep).cloned(),
            Some(vec!["com.hnc.discord".to_string()])
        );
        assert!(
            s.episodes_unfiled(10)?.iter().all(|x| x.id != ep),
            "media is not a conversation"
        );
        assert!(s.mark_episode_as_call(ep)?);
        let e = s
            .episode(ep)?
            .ok_or(StoreError::Io(std::io::Error::other("gone")))?;
        assert_eq!(e.kind, EPISODE_CONVERSATION);
        assert_eq!(e.node_id, None, "out of Media: the Conversations list");
        assert_eq!(e.summary, "Talk about shipping.");
        assert!(s.episodes_unfiled(10)?.iter().any(|x| x.id == ep));
        for id in segs {
            let r = s
                .segment(id)?
                .ok_or(StoreError::Io(std::io::Error::other("gone")))?;
            assert_eq!(r.media_kind, Some(MediaClass::Meeting));
            assert_eq!(r.node_id, None);
        }
        assert_eq!(s.media_play(play)?.map(|p| p.kind), Some(MediaClass::Call));
        assert!(!s.mark_episode_as_call(ep)?, "already a conversation");

        // and back
        assert!(s.mark_episode_as_media(ep)?);
        let e = s
            .episode(ep)?
            .ok_or(StoreError::Io(std::io::Error::other("gone")))?;
        assert!(e.is_media());
        assert_eq!(e.node_id, Some(s.media_category()?.id));
        for id in segs {
            let r = s
                .segment(id)?
                .ok_or(StoreError::Io(std::io::Error::other("gone")))?;
            assert_eq!(r.media_kind, Some(MediaClass::App));
            assert_eq!(r.node_id, Some(s.media_category()?.id));
        }
        assert_eq!(s.media_play(play)?.map(|p| p.kind), Some(MediaClass::App));
        assert!(!s.mark_episode_as_media(ep)?, "already media");
        Ok(())
    }

    #[test]
    fn mark_a_play_as_a_call_with_and_without_a_transcript() -> R {
        let s = Store::open_in_memory()?;
        let (ep, play, _) = discord_episode(&s)?;
        let r = s
            .mark_play_as_call(play)?
            .ok_or(StoreError::Io(std::io::Error::other("gone")))?;
        assert!(r.transcribed());
        assert_eq!(r.episodes, vec![ep]);
        assert_eq!(
            s.episode(ep)?.map(|e| e.kind),
            Some(EPISODE_CONVERSATION.to_string())
        );

        // log only: a play with nothing transcribed — just the label
        let mut slack = MediaStamp::new(MediaClass::App, None, "Slack", "Slack");
        slack.bundle_id = "com.tinyspeck.slackmacgap".into();
        let logged = s.begin_media_play(100_000, &slack)?;
        s.update_media_play(logged, 160_000, 0)?;
        let r = s
            .mark_play_as_call(logged)?
            .ok_or(StoreError::Io(std::io::Error::other("gone")))?;
        assert!(!r.transcribed());
        assert!(r.episodes.is_empty());
        assert_eq!(
            s.media_play(logged)?.map(|p| p.kind),
            Some(MediaClass::Call)
        );
        assert_eq!(s.mark_play_as_call(9_999)?, None);
        Ok(())
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
        assert_eq!(version, "8");
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

    const MIN: i64 = 60_000;

    fn play(
        s: &Store,
        at: i64,
        end: i64,
        url: Option<&str>,
        kind: MediaClass,
    ) -> Result<i64, StoreError> {
        let id = s.begin_media_play(
            at,
            &MediaStamp::new(kind, url.map(str::to_owned), "T", "Firefox"),
        )?;
        s.update_media_play(id, end, 0)?;
        Ok(id)
    }

    #[test]
    fn the_history_pages_newest_first_and_leaves_calls_out() -> R {
        let s = Store::open_in_memory()?;
        let mut ids = Vec::new();
        for i in 0..5 {
            let url = format!("https://youtu.be/v{i}");
            ids.push(play(
                &s,
                i * 60 * MIN,
                i * 60 * MIN + 5 * MIN,
                Some(&url),
                MediaClass::YouTube,
            )?);
        }
        let call = play(&s, 10 * 60 * MIN, 11 * 60 * MIN, None, MediaClass::Call)?;
        let page = s.media_history_page(None, 2)?;
        assert_eq!(
            page.iter().map(|p| p.id).collect::<Vec<_>>(),
            [ids[4], ids[3]]
        );
        let last = page.last().map(|p| (p.started_at, p.id));
        let next = s.media_history_page(last, 2)?;
        assert_eq!(
            next.iter().map(|p| p.id).collect::<Vec<_>>(),
            [ids[2], ids[1]]
        );
        let rest = s.media_history_page(next.last().map(|p| (p.started_at, p.id)), 2)?;
        assert_eq!(rest.iter().map(|p| p.id).collect::<Vec<_>>(), [ids[0]]);
        let all = s.media_history_page(None, 100)?;
        assert!(all.iter().all(|p| p.id != call), "calls are not media");
        // same start: the id breaks the tie, nothing is skipped or repeated
        let twin = play(
            &s,
            4 * 60 * MIN,
            4 * 60 * MIN + MIN,
            Some("https://youtu.be/x"),
            MediaClass::Music,
        )?;
        let first = s.media_history_page(None, 1)?;
        assert_eq!(first[0].id, twin);
        let second = s.media_history_page(Some((first[0].started_at, first[0].id)), 1)?;
        assert_eq!(second[0].id, ids[4]);
        Ok(())
    }

    #[test]
    fn consecutive_plays_of_one_url_within_ten_minutes_collapse() -> R {
        let s = Store::open_in_memory()?;
        let a = Some("https://youtu.be/a");
        // oldest first: a 0–10, a 15–20 (5 min gap: folds), b 21–25, a 26–30 (b between: no),
        // a 45–50 (15 min gap: no)
        let p1 = play(&s, 0, 10 * MIN, a, MediaClass::YouTube)?;
        let p2 = play(&s, 15 * MIN, 20 * MIN, a, MediaClass::YouTube)?;
        let p3 = play(
            &s,
            21 * MIN,
            25 * MIN,
            Some("https://youtu.be/b"),
            MediaClass::YouTube,
        )?;
        let p4 = play(&s, 26 * MIN, 30 * MIN, a, MediaClass::YouTube)?;
        let p5 = play(&s, 45 * MIN, 50 * MIN, a, MediaClass::YouTube)?;
        // no URL: never folded
        let n1 = play(&s, 60 * MIN, 61 * MIN, None, MediaClass::Music)?;
        let n2 = play(&s, 62 * MIN, 63 * MIN, None, MediaClass::Music)?;
        let mut rows = Vec::new();
        collapse_plays(&mut rows, s.media_history_page(None, 100)?);
        let ids: Vec<Vec<i64>> = rows.iter().map(|r| r.ids.clone()).collect();
        assert_eq!(
            ids,
            [
                vec![n2],
                vec![n1],
                vec![p5],
                vec![p4],
                vec![p3],
                vec![p2, p1]
            ]
        );
        let folded = &rows[5];
        assert_eq!((folded.started_at, folded.ended_at), (0, 20 * MIN));
        assert_eq!(folded.duration_ms, 15 * MIN, "the gap is not counted");
        assert_eq!(folded.play.id, p2, "the newest play is shown");

        // paged one play at a time, the run still folds across the page boundary
        let mut paged = Vec::new();
        let mut before = None;
        loop {
            let page = s.media_history_page(before, 1)?;
            let Some(last) = page.last() else { break };
            before = Some((last.started_at, last.id));
            collapse_plays(&mut paged, page);
        }
        assert_eq!(paged, rows);
        Ok(())
    }

    #[test]
    fn music_plays_fold_per_track_not_per_page() -> R {
        let s = Store::open_in_memory()?;
        let page = "https://open.spotify.com/playlist/p";
        let track = |at: i64, end: i64, t: &str| -> Result<i64, StoreError> {
            let id = s.begin_media_play(
                at,
                &MediaStamp::new(MediaClass::Music, Some(page.into()), t, "Firefox"),
            )?;
            s.update_media_play(id, end, 0)?;
            Ok(id)
        };
        // one 0–3, one again 4–5 (paused and resumed: folds), two 5–8 (another track)
        let a = track(0, 3 * MIN, "One — X")?;
        let b = track(4 * MIN, 5 * MIN, "One — X")?;
        let c = track(5 * MIN, 8 * MIN, "Two — Y")?;
        let mut rows = Vec::new();
        collapse_plays(&mut rows, s.media_history_page(None, 100)?);
        let ids: Vec<Vec<i64>> = rows.iter().map(|r| r.ids.clone()).collect();
        assert_eq!(ids, [vec![c], vec![b, a]]);
        Ok(())
    }

    #[test]
    fn a_plays_first_segment_and_deleting_a_folded_row() -> R {
        let s = Store::open_in_memory()?;
        let session = s.begin_session(0, "t")?;
        let url = "https://youtu.be/a";
        let p1 = play(&s, 0, 10 * MIN, Some(url), MediaClass::YouTube)?;
        let p2 = play(&s, 12 * MIN, 20 * MIN, Some(url), MediaClass::YouTube)?;
        assert_eq!(s.first_segment_of_plays(&[p1, p2])?, None);
        let seg = |at: i64| -> Result<i64, StoreError> {
            let row = s.insert_segment_from(
                session,
                at,
                at + 5_000,
                "video words",
                None,
                AudioSourceKind::System,
                None,
            )?;
            s.stamp_segment_media(row.id, &stamp(url, "A"))?;
            Ok(row.id)
        };
        let late = seg(14 * MIN)?;
        let early = seg(2 * MIN)?;
        assert_eq!(s.first_segment_of_plays(&[p2])?, Some(late));
        assert_eq!(s.first_segment_of_plays(&[p2, p1])?, Some(early));
        assert_eq!(
            s.media_play_urls_containing("youtu.be/a")?,
            vec![url.to_string()]
        );
        s.delete_media_plays(&[p1, p2])?;
        assert!(s.media_history_page(None, 10)?.is_empty());
        assert!(s.media_play_urls_containing("youtu.be/a")?.is_empty());
        Ok(())
    }

    #[test]
    fn search_finds_plays_by_title_or_url() -> R {
        let s = Store::open_in_memory()?;
        let a = s.begin_media_play(0, &stamp("https://youtu.be/rust1", "Rust in 100 seconds"))?;
        let b = s.begin_media_play(MIN, &stamp("https://youtu.be/zzz", "Cooking pasta"))?;
        let mut call = MediaStamp::new(MediaClass::Call, None, "Rust standup", "Zoom");
        call.url = Some("https://zoom.us/j/1".into());
        s.begin_media_play(2 * MIN, &call)?;
        let ids = |q: &str| -> Result<Vec<i64>, StoreError> {
            Ok(s.search(q, 50)?.media.iter().map(|p| p.id).collect())
        };
        assert_eq!(ids("rust")?, [a], "calls are left out");
        assert_eq!(ids("ZZZ")?, [b], "the URL matches too");
        assert!(ids("nothing like it")?.is_empty());
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
