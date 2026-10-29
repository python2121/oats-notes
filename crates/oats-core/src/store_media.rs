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
             CREATE INDEX IF NOT EXISTS media_plays_started ON media_plays(started_at);",
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
