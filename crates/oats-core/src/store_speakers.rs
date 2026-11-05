//! The store's speaker part (schema v10): one voiceprint per person heard, kept across
//! sessions, and the voice of every segment.
//!
//! - `speakers(id INTEGER PRIMARY KEY AUTOINCREMENT, name NULL, name_source NULL /* user |
//!   brain */, name_confidence NULL, is_me, centroid BLOB /* f32 little-endian, unit length
//!   */, speech_secs, mic_secs, created_at, last_heard_at)` — written by the pipeline (a new
//!   voice at once; centroid / seconds / last heard in batches, at most once a minute and at
//!   session end), named by the user or the brain. AUTOINCREMENT: a forgotten voice's id (its
//!   "them N" label) is never handed to another person.
//! - `segments.speaker_id INTEGER NULL` (indexed) — the dominant voice of the segment, set in
//!   the segment's own insert.
//!
//! The user's corrections: [`Store::set_speaker_name`], [`Store::set_speaker_me`],
//! [`Store::merge_speakers`] ("same person"), [`Store::forget_speaker`].

use super::{Store, StoreError};
use crate::speakers::{NameSource, Speaker, SpeakerEntry, normalise};

const SPEAKER_COLUMNS: &str = "id, name, name_source, name_confidence, is_me, centroid, speech_secs, mic_secs, created_at, last_heard_at";

/// f32 little-endian.
pub fn centroid_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

pub fn centroid_from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn row_to_speaker(r: &rusqlite::Row<'_>) -> rusqlite::Result<Speaker> {
    Ok(Speaker {
        id: r.get(0)?,
        name: r.get(1)?,
        name_source: r
            .get::<_, Option<String>>(2)?
            .as_deref()
            .and_then(NameSource::parse),
        name_confidence: r.get::<_, Option<f64>>(3)?.map(|c| c as f32),
        is_me: r.get::<_, i64>(4)? != 0,
        centroid: centroid_from_blob(&r.get::<_, Vec<u8>>(5)?),
        speech_secs: r.get(6)?,
        mic_secs: r.get(7)?,
        created_at: r.get(8)?,
        last_heard_at: r.get(9)?,
    })
}

impl Store {
    /// v10's table and column, added in place when missing (idempotent; inside `init`'s
    /// transaction).
    pub(super) fn ensure_speakers_schema(conn: &rusqlite::Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS speakers (
                 id              INTEGER PRIMARY KEY AUTOINCREMENT,
                 name            TEXT,
                 name_source     TEXT,
                 name_confidence REAL,
                 is_me           INTEGER NOT NULL DEFAULT 0,
                 centroid        BLOB NOT NULL,
                 speech_secs     REAL NOT NULL DEFAULT 0,
                 mic_secs        REAL NOT NULL DEFAULT 0,
                 created_at      INTEGER NOT NULL,
                 last_heard_at   INTEGER NOT NULL
             );",
        )?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('segments') WHERE name = 'speaker_id'",
            [],
            |r| r.get(0),
        )?;
        if n == 0 {
            conn.execute_batch("ALTER TABLE segments ADD COLUMN speaker_id INTEGER;")?;
        }
        conn.execute_batch("CREATE INDEX IF NOT EXISTS segments_speaker ON segments(speaker_id);")?;
        Ok(())
    }

    /// Every stored voice, by id.
    pub fn speakers(&self) -> Result<Vec<Speaker>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SPEAKER_COLUMNS} FROM speakers ORDER BY id"
        ))?;
        let rows = stmt
            .query_map([], row_to_speaker)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn speaker(&self, id: i64) -> Result<Option<Speaker>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SPEAKER_COLUMNS} FROM speakers WHERE id = ?1"),
                [id],
                row_to_speaker,
            )
            .optional()?)
    }

    /// A new voice (the pipeline, when a span matches nobody); returns its id.
    pub fn insert_speaker(
        &self,
        centroid: &[f32],
        speech_secs: f64,
        mic_secs: f64,
        now: i64,
    ) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO speakers(centroid, speech_secs, mic_secs, created_at, last_heard_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            rusqlite::params![centroid_to_blob(centroid), speech_secs, mic_secs, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Write back a voice's centroid and counters. `false` when the voice no longer exists
    /// (forgotten or merged away meanwhile: nothing is written).
    pub fn update_speaker_centroid(
        &self,
        id: i64,
        centroid: &[f32],
        speech_secs: f64,
        mic_secs: f64,
        last_heard_at: i64,
    ) -> Result<bool, StoreError> {
        let n = self.conn.execute(
            "UPDATE speakers SET centroid = ?2, speech_secs = ?3, mic_secs = ?4,
                 last_heard_at = MAX(last_heard_at, ?5)
             WHERE id = ?1",
            rusqlite::params![
                id,
                centroid_to_blob(centroid),
                speech_secs,
                mic_secs,
                last_heard_at
            ],
        )?;
        Ok(n > 0)
    }

    /// The pipeline's batched flush: every changed entry in one transaction.
    pub fn flush_speakers(&self, entries: &[SpeakerEntry]) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        self.with_savepoint(|s| {
            for e in entries {
                s.update_speaker_centroid(
                    e.id,
                    &e.centroid,
                    e.speech_secs,
                    e.mic_secs,
                    e.last_heard_at,
                )?;
            }
            Ok(())
        })
    }

    /// Name a voice (`None` or blank clears the name). A `brain` name never replaces a
    /// `user` one, nor a brain name given with higher confidence; returns whether the name
    /// changed.
    pub fn set_speaker_name(
        &self,
        id: i64,
        name: Option<&str>,
        source: NameSource,
        confidence: Option<f32>,
    ) -> Result<bool, StoreError> {
        let Some(current) = self.speaker(id)? else {
            return Ok(false);
        };
        let name = name.map(str::trim).filter(|n| !n.is_empty());
        if source == NameSource::Brain && current.name.is_some() {
            if current.name_source != Some(NameSource::Brain) {
                return Ok(false);
            }
            if current.name_confidence.unwrap_or(0.0) >= confidence.unwrap_or(0.0)
                && current.name.as_deref() != name
            {
                return Ok(false);
            }
        }
        if current.name.as_deref() == name
            && (name.is_none() || current.name_source == Some(source))
        {
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE speakers SET name = ?2, name_source = ?3, name_confidence = ?4 WHERE id = ?1",
            rusqlite::params![
                id,
                name,
                name.map(|_| source.as_str()),
                name.and(confidence).map(f64::from)
            ],
        )?;
        Ok(true)
    }

    /// Restore a voice's name exactly (undo of a brain naming). `false` when it is gone.
    pub fn restore_speaker_name(
        &self,
        id: i64,
        name: Option<&str>,
        source: Option<NameSource>,
        confidence: Option<f32>,
    ) -> Result<bool, StoreError> {
        let n = self.conn.execute(
            "UPDATE speakers SET name = ?2, name_source = ?3, name_confidence = ?4 WHERE id = ?1",
            rusqlite::params![
                id,
                name,
                source.map(NameSource::as_str),
                confidence.map(f64::from)
            ],
        )?;
        Ok(n > 0)
    }

    /// Flag `id` as the user's own voice (and no other). `false` when it doesn't exist.
    pub fn set_speaker_me(&self, id: i64) -> Result<bool, StoreError> {
        self.with_savepoint(|s| {
            if s.speaker(id)?.is_none() {
                return Ok(false);
            }
            s.conn
                .execute("UPDATE speakers SET is_me = (id = ?1)", [id])?;
            Ok(true)
        })
    }

    /// Forget a voice: the row goes (with its voiceprint), its segments lose their voice and
    /// read as their lane again. `false` when it didn't exist.
    pub fn forget_speaker(&self, id: i64) -> Result<bool, StoreError> {
        let gone = self.with_savepoint(|s| {
            s.conn.execute(
                "UPDATE segments SET speaker_id = NULL, speaker_sure = NULL WHERE speaker_id = ?1",
                [id],
            )?;
            Ok(s.conn.execute("DELETE FROM speakers WHERE id = ?1", [id])? > 0)
        })?;
        if gone {
            self.checkpoint();
        }
        Ok(gone)
    }

    /// "Same person": `from`'s segments move to `into`, the centroids are averaged by their
    /// (capped) speech weight, the counters add up, `into` keeps its name (else takes
    /// `from`'s) and is the user's voice if either was; `from` is deleted. `false` when either
    /// is missing or they are the same.
    pub fn merge_speakers(&self, from: i64, into: i64) -> Result<bool, StoreError> {
        if from == into {
            return Ok(false);
        }
        self.with_savepoint(|s| {
            let (Some(a), Some(b)) = (s.speaker(from)?, s.speaker(into)?) else {
                return Ok(false);
            };
            let cap = crate::speakers::MAX_WEIGHT_SECS;
            let (wa, wb) = (a.speech_secs.min(cap) as f32, b.speech_secs.min(cap) as f32);
            let mut centroid = if a.centroid.len() == b.centroid.len() && wa + wb > 0.0 {
                a.centroid
                    .iter()
                    .zip(&b.centroid)
                    .map(|(x, y)| (x * wa + y * wb) / (wa + wb))
                    .collect()
            } else {
                b.centroid.clone()
            };
            normalise(&mut centroid);
            let (name, source, confidence) = if b.name.is_some() {
                (b.name.clone(), b.name_source, b.name_confidence)
            } else {
                (a.name.clone(), a.name_source, a.name_confidence)
            };
            s.conn.execute(
                "UPDATE segments SET speaker_id = ?2 WHERE speaker_id = ?1",
                [from, into],
            )?;
            s.conn.execute(
                "UPDATE speakers SET centroid = ?2, speech_secs = ?3, mic_secs = ?4,
                     last_heard_at = ?5, is_me = ?6, name = ?7, name_source = ?8,
                     name_confidence = ?9, created_at = MIN(created_at, ?10)
                 WHERE id = ?1",
                rusqlite::params![
                    into,
                    centroid_to_blob(&centroid),
                    a.speech_secs + b.speech_secs,
                    a.mic_secs + b.mic_secs,
                    a.last_heard_at.max(b.last_heard_at),
                    a.is_me || b.is_me,
                    name,
                    source.map(NameSource::as_str),
                    confidence.map(f64::from),
                    a.created_at
                ],
            )?;
            s.conn
                .execute("DELETE FROM speakers WHERE id = ?1", [from])?;
            Ok(true)
        })
    }

    /// The voice of each of these segments (`segment id → speaker id`; segments without one
    /// are absent).
    pub fn speakers_of_segments(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, i64>, StoreError> {
        let mut out = std::collections::HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT speaker_id FROM segments WHERE id = ?1 AND speaker_id IS NOT NULL")?;
        for &id in ids {
            if let Some(sp) = stmt
                .query_map([id], |r| r.get::<_, i64>(0))?
                .next()
                .transpose()?
            {
                out.insert(id, sp);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::AudioSourceKind;
    use crate::speakers::NameSource;
    use crate::store::Store;

    type R = Result<(), Box<dyn std::error::Error>>;

    fn unit(seed: f32) -> Vec<f32> {
        let mut v: Vec<f32> = (0..256).map(|i| ((i as f32 + seed) * 0.37).sin()).collect();
        crate::speakers::normalise(&mut v);
        v
    }

    #[test]
    fn speakers_round_trip_and_segments_carry_their_voice() -> R {
        let s = Store::open_in_memory()?;
        let a = s.insert_speaker(&unit(1.0), 3.0, 3.0, 100)?;
        let b = s.insert_speaker(&unit(2.0), 2.0, 0.0, 200)?;
        let all = s.speakers()?;
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].centroid, unit(1.0));
        assert_eq!(all[1].last_heard_at, 200);
        let session = s.begin_session(0, "test")?;
        let row = s.insert_segment_full(
            session,
            0,
            1000,
            "hello",
            None,
            AudioSourceKind::System,
            None,
            Some(b),
            Some(false),
        )?;
        assert_eq!(row.speaker_sure, Some(false));
        assert_eq!(row.speaker_id, Some(b));
        let back = s.segment(row.id)?;
        assert_eq!(back.and_then(|r| r.speaker_id), Some(b));
        assert!(s.update_speaker_centroid(a, &unit(3.0), 10.0, 8.0, 50)?);
        let sa = s.speaker(a)?.ok_or("gone")?;
        assert_eq!(
            (sa.speech_secs, sa.mic_secs, sa.last_heard_at),
            (10.0, 8.0, 100)
        );
        Ok(())
    }

    #[test]
    fn names_follow_the_user_wins_rule() -> R {
        let s = Store::open_in_memory()?;
        let id = s.insert_speaker(&unit(1.0), 3.0, 0.0, 0)?;
        assert!(s.set_speaker_name(id, Some("Cody"), NameSource::Brain, Some(0.8))?);
        // a lower-confidence brain name doesn't replace it, a higher one does
        assert!(!s.set_speaker_name(id, Some("Cory"), NameSource::Brain, Some(0.7))?);
        assert!(s.set_speaker_name(id, Some("Cory"), NameSource::Brain, Some(0.95))?);
        assert!(s.set_speaker_name(id, Some(" Priya "), NameSource::User, None)?);
        assert!(!s.set_speaker_name(id, Some("Cody"), NameSource::Brain, Some(1.0))?);
        let sp = s.speaker(id)?.ok_or("gone")?;
        assert_eq!(sp.name.as_deref(), Some("Priya"));
        assert_eq!(sp.name_source, Some(NameSource::User));
        assert!(s.set_speaker_name(id, None, NameSource::User, None)?);
        assert_eq!(s.speaker(id)?.and_then(|s| s.name), None);
        Ok(())
    }

    #[test]
    fn me_merge_and_forget() -> R {
        let s = Store::open_in_memory()?;
        let a = s.insert_speaker(&unit(1.0), 100.0, 90.0, 10)?;
        let b = s.insert_speaker(&unit(2.0), 50.0, 0.0, 20)?;
        let c = s.insert_speaker(&unit(3.0), 5.0, 0.0, 30)?;
        assert!(s.set_speaker_me(a)?);
        assert!(s.set_speaker_me(b)?);
        let me: Vec<i64> = s
            .speakers()?
            .iter()
            .filter(|x| x.is_me)
            .map(|x| x.id)
            .collect();
        assert_eq!(me, vec![b]);
        assert!(!s.set_speaker_me(999)?);

        let session = s.begin_session(0, "test")?;
        let seg = |sp: i64| {
            s.insert_segment_full(
                session,
                0,
                1,
                "x",
                None,
                AudioSourceKind::Mic,
                None,
                Some(sp),
                None,
            )
        };
        let s1 = seg(a)?;
        let s2 = seg(c)?;
        s.set_speaker_name(c, Some("Priya"), NameSource::User, None)?;
        assert!(s.merge_speakers(c, a)?);
        assert!(s.speaker(c)?.is_none());
        let merged = s.speaker(a)?.ok_or("gone")?;
        assert_eq!(merged.speech_secs, 105.0);
        assert_eq!(merged.name.as_deref(), Some("Priya"));
        assert_eq!(merged.last_heard_at, 30);
        let n: f32 = merged.centroid.iter().map(|x| x * x).sum();
        assert!((n - 1.0).abs() < 1e-4);
        assert_eq!(s.segment(s2.id)?.and_then(|r| r.speaker_id), Some(a));
        assert!(!s.merge_speakers(a, a)?);

        assert!(s.forget_speaker(a)?);
        assert_eq!(s.segment(s1.id)?.and_then(|r| r.speaker_id), None);
        assert!(!s.forget_speaker(a)?);
        // a forgotten id is never reused
        let d = s.insert_speaker(&unit(4.0), 1.0, 0.0, 40)?;
        assert!(d > c);
        Ok(())
    }
}
