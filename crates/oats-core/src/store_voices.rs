//! The store's per-line voices (schema v11): each transcribed line's own speaker embedding,
//! kept for [`crate::speakers::VOICE_EMBEDDING_KEEP_DAYS`] so the user can correct who said
//! it ("This line was ▸") and the correction teaches the voiceprints.
//!
//! - `segment_voices(segment_id INTEGER PRIMARY KEY REFERENCES segments(id) ON DELETE CASCADE,
//!   embedding BLOB /* f32 LE, 256 */, speech_ms, created_at)` — written by the pipeline right
//!   after the line's insert (the embedding of the span that became the line's voice, else
//!   its longest span), only while speaker recognition is on. Deleting a line deletes its row
//!   (foreign keys are on for every connection after `init`). Pruned at open and at each
//!   session end.
//!
//! Corrections: [`Store::correct_segment_speaker`] (one line) and
//! [`Store::correct_segments_speaker`] (the "Move them too" group): the label moves; when the
//! line's embedding is kept (and learning is on) it is taken out of the old voice's centroid
//! ([`SpeakerBook::unlearn`]) and learned into the new one with
//! [`crate::speakers::CORRECTION_WEIGHT`]. A `segment_speaker` / `segment_speakers` change
//! records it; its undo restores the labels only.

use serde_json::json;

use super::speakers::{centroid_from_blob, centroid_to_blob};
use super::{Store, StoreError};
use crate::engine::AudioSourceKind;
use crate::speakers::{
    CORRECTION_WEIGHT, NameSource, SpeakerBook, VOICE_EMBEDDING_KEEP_DAYS, cosine_distance,
    normalise, speaker_label,
};

const DAY_MS: i64 = 24 * 3_600_000;

/// Who a corrected line was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeakerTarget {
    /// A known voice.
    Voice(i64),
    /// Someone new: a voice is enrolled from the line (named by the user when given).
    New { name: Option<String> },
    /// Nobody: the line's voice is cleared (it reads as its lane again).
    None,
}

/// What [`Store::correct_segment_speaker`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Correction {
    /// The line's voice before.
    pub from: Option<i64>,
    /// …and now.
    pub to: Option<i64>,
    /// The line's embedding was kept and moved the voiceprints (unlearned from the old voice,
    /// learned into the new one); `false` = relabelled only.
    pub learned: bool,
    /// The voice enrolled for [`SpeakerTarget::New`].
    pub new_voice: Option<i64>,
}

/// What [`Store::correct_segments_speaker`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinesCorrection {
    pub to: Option<i64>,
    /// Lines relabelled (missing ones are skipped).
    pub moved: usize,
    /// …of which moved the voiceprints too.
    pub learned: usize,
    pub new_voice: Option<i64>,
}

/// Where [`Store::segments_like`] looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineScope {
    /// The lines of a note.
    Episode(i64),
    /// The lines of a listening session (a line not yet in a note).
    Session(i64),
}

/// One line's correction inside [`Store::correct_lines`].
struct Line {
    segment: i64,
    from: Option<i64>,
    learned: bool,
}

impl Store {
    /// v11's table, added in place when missing (idempotent; inside `init`'s transaction).
    pub(super) fn ensure_voices_schema(conn: &rusqlite::Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS segment_voices (
                 segment_id INTEGER PRIMARY KEY REFERENCES segments(id) ON DELETE CASCADE,
                 embedding  BLOB NOT NULL,
                 speech_ms  INTEGER NOT NULL,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS segment_voices_created ON segment_voices(created_at);",
        )?;
        Ok(())
    }

    /// Keep a line's speaker embedding (`speech_ms` of speech; replaces an earlier one).
    pub fn insert_segment_voice(
        &self,
        segment_id: i64,
        embedding: &[f32],
        speech_ms: i64,
        now: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO segment_voices(segment_id, embedding, speech_ms, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![segment_id, centroid_to_blob(embedding), speech_ms, now],
        )?;
        Ok(())
    }

    /// A line's kept embedding and its speech (ms), if any.
    pub fn segment_voice(&self, segment_id: i64) -> Result<Option<(Vec<f32>, i64)>, StoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT embedding, speech_ms FROM segment_voices WHERE segment_id = ?1",
                [segment_id],
                |r| Ok((centroid_from_blob(&r.get::<_, Vec<u8>>(0)?), r.get(1)?)),
            )
            .optional()?)
    }

    /// Drop the embeddings older than [`VOICE_EMBEDDING_KEEP_DAYS`]; how many went.
    pub fn prune_segment_voices(&self, now: i64) -> Result<usize, StoreError> {
        Ok(self.conn.execute(
            "DELETE FROM segment_voices WHERE created_at < ?1",
            [now - VOICE_EMBEDDING_KEEP_DAYS * DAY_MS],
        )?)
    }

    /// Set a line's voice (`None` clears it). `false` when the line is gone.
    pub fn set_segment_speaker(
        &self,
        segment: i64,
        speaker: Option<i64>,
    ) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE segments SET speaker_id = ?2,
                 speaker_sure = CASE WHEN ?2 IS NULL THEN NULL ELSE 1 END
             WHERE id = ?1",
            rusqlite::params![segment, speaker],
        )? > 0)
    }

    /// "This line was ▸": who said `segment`. One savepoint: the label moves; with the line's
    /// embedding kept and `learn` (speaker recognition on) it is unlearned from the old voice
    /// and learned into the new one ([`CORRECTION_WEIGHT`]); [`SpeakerTarget::New`] enrols a
    /// voice from it. A `segment_speaker` change (`by: user`) records it — its undo restores
    /// the label, not the centroids. `None` when the line no longer exists.
    pub fn correct_segment_speaker(
        &self,
        segment: i64,
        to: SpeakerTarget,
        learn: bool,
        now: i64,
    ) -> Result<Option<Correction>, StoreError> {
        self.with_savepoint(|s| {
            let Some(source) = s.segment(segment)?.map(|r| r.source) else {
                return Ok(None);
            };
            let (to_id, lines, new_voice) = s.correct_lines(&[segment], to, learn, now)?;
            let Some(line) = lines.first() else {
                return Ok(None);
            };
            let label = s.voice_label(to_id, source)?;
            s.record_change(
                None,
                "segment_speaker",
                &format!("A line's speaker corrected to {label}"),
                &json!({"segment": segment, "speaker": line.from, "by": "user"}),
                &json!({"segment": segment, "speaker": to_id, "learned": line.learned,
                        "new_voice": new_voice, "by": "user"}),
                now,
            )?;
            Ok(Some(Correction {
                from: line.from,
                to: to_id,
                learned: line.learned,
                new_voice,
            }))
        })
    }

    /// "Move them too": [`Self::correct_segment_speaker`] for several lines at once, ONE
    /// `segment_speakers` change (undoable as a group, labels only).
    pub fn correct_segments_speaker(
        &self,
        segments: &[i64],
        to: SpeakerTarget,
        learn: bool,
        now: i64,
    ) -> Result<LinesCorrection, StoreError> {
        self.with_savepoint(|s| {
            let (to_id, lines, new_voice) = s.correct_lines(segments, to, learn, now)?;
            if lines.is_empty() {
                return Ok(LinesCorrection {
                    to: to_id,
                    new_voice,
                    ..LinesCorrection::default()
                });
            }
            let learned = lines.iter().filter(|l| l.learned).count();
            let label = s.voice_label(to_id, AudioSourceKind::System)?;
            let before: Vec<serde_json::Value> = lines
                .iter()
                .map(|l| json!({"segment": l.segment, "speaker": l.from}))
                .collect();
            let ids: Vec<i64> = lines.iter().map(|l| l.segment).collect();
            s.record_change(
                None,
                "segment_speakers",
                &format!("{} lines' speaker corrected to {label}", lines.len()),
                &json!({"segments": before, "by": "user"}),
                &json!({"segments": ids, "speaker": to_id, "learned": learned,
                        "new_voice": new_voice, "by": "user"}),
                now,
            )?;
            Ok(LinesCorrection {
                to: to_id,
                moved: lines.len(),
                learned,
                new_voice,
            })
        })
    }

    /// The label a voice reads as in the change log.
    fn voice_label(&self, id: Option<i64>, source: AudioSourceKind) -> Result<String, StoreError> {
        Ok(match id {
            Some(id) => match self.speaker(id)? {
                Some(sp) => speaker_label(Some(&sp), source),
                None => format!("voice {id}"),
            },
            None => "nobody".to_owned(),
        })
    }

    /// The shared body of the corrections (inside the caller's savepoint): resolve the target
    /// (enrolling a new voice), relabel each existing line, move the voiceprints.
    fn correct_lines(
        &self,
        segments: &[i64],
        to: SpeakerTarget,
        learn: bool,
        now: i64,
    ) -> Result<(Option<i64>, Vec<Line>, Option<i64>), StoreError> {
        let mut rows = Vec::new();
        for &id in segments {
            if let Some(row) = self.segment(id)? {
                let voice = if learn { self.segment_voice(id)? } else { None };
                rows.push((row, voice));
            }
        }
        // the voice a new person is enrolled from: the first line with a kept embedding (a
        // relabel-only enrolment starts with no voiceprint; the first learned line gives it one)
        let mut seed = None;
        let (to_id, new_voice) = match to {
            SpeakerTarget::Voice(id) => (Some(id), None),
            SpeakerTarget::None => (None, None),
            SpeakerTarget::New { name } => {
                let first = rows.iter().find_map(|(row, v)| {
                    v.as_ref().and_then(|(e, ms)| {
                        let mut c = e.clone();
                        normalise(&mut c).then_some((row.id, c, *ms, row.source))
                    })
                });
                let id = match first {
                    Some((seg, centroid, ms, source)) => {
                        seed = Some(seg);
                        let secs = ms as f64 / 1000.0;
                        let mic = if source == AudioSourceKind::Mic {
                            secs
                        } else {
                            0.0
                        };
                        self.insert_speaker(&centroid, secs, mic, now)?
                    }
                    None => self.insert_speaker(&[], 0.0, 0.0, now)?,
                };
                if let Some(n) = name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                    self.set_speaker_name(id, Some(n), NameSource::User, None)?;
                }
                (Some(id), Some(id))
            }
        };
        let mut book: Option<SpeakerBook> = None;
        let mut lines = Vec::with_capacity(rows.len());
        for (row, voice) in rows {
            self.set_segment_speaker(row.id, to_id)?;
            let mut learned = false;
            if let Some((embedding, ms)) = voice
                && row.speaker_id != to_id
            {
                if book.is_none() {
                    book = Some(SpeakerBook::new(
                        self.speakers()?.iter().map(|s| s.entry()).collect(),
                    ));
                }
                if let Some(book) = book.as_mut() {
                    let secs = ms as f64 / 1000.0;
                    let mic = row.source == AudioSourceKind::Mic;
                    if let Some(from) = row.speaker_id {
                        learned |= book.unlearn(from, &embedding, secs, mic);
                    }
                    if seed == Some(row.id) {
                        learned = true;
                    } else if let Some(to) = to_id {
                        learned |= book.learn(to, &embedding, secs, CORRECTION_WEIGHT, mic);
                    }
                }
            }
            lines.push(Line {
                segment: row.id,
                from: row.speaker_id,
                learned,
            });
        }
        if let Some(mut book) = book {
            self.flush_speakers(&book.take_dirty())?;
        }
        Ok((to_id, lines, new_voice))
    }

    /// Lines that sound like `segment` (its kept embedding within `max_distance`, cosine):
    /// among the lines of `among`, the line itself left out, only those whose voice is `from`
    /// (the corrected line's old voice) or none — and not already `to`. Nearest first; empty
    /// when the line has no kept embedding.
    pub fn segments_like(
        &self,
        segment: i64,
        among: LineScope,
        from: Option<i64>,
        to: Option<i64>,
        max_distance: f32,
    ) -> Result<Vec<(i64, f32)>, StoreError> {
        let Some((target, _)) = self.segment_voice(segment)? else {
            return Ok(Vec::new());
        };
        let (column, id) = match among {
            LineScope::Episode(id) => ("episode_id", id),
            LineScope::Session(id) => ("session_id", id),
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT s.id, s.speaker_id, v.embedding FROM segments s
               JOIN segment_voices v ON v.segment_id = s.id
             WHERE s.{column} = ?1 AND s.id != ?2 AND s.echo_of IS NULL
               AND (s.speaker_id IS NULL OR s.speaker_id IS ?3)"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![id, segment, from], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    centroid_from_blob(&r.get::<_, Vec<u8>>(2)?),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out: Vec<(i64, f32)> = rows
            .into_iter()
            .filter(|(_, voice, _)| *voice != to)
            .map(|(id, _, e)| (id, cosine_distance(&target, &e)))
            .filter(|(_, d)| *d <= max_distance)
            .collect();
        out.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speakers::{SIMILAR_LINE_THRESHOLD, cosine_distance};

    type R = Result<(), Box<dyn std::error::Error>>;

    fn unit(seed: f32) -> Vec<f32> {
        let mut v: Vec<f32> = (0..256).map(|i| ((i as f32 + seed) * 0.37).sin()).collect();
        normalise(&mut v);
        v
    }

    /// `v` nudged a little (still within the similar-line threshold).
    fn nudged(v: &[f32], by: f32) -> Vec<f32> {
        let mut out: Vec<f32> = v
            .iter()
            .enumerate()
            .map(|(i, x)| x + by * ((i as f32) * 1.3).cos() / 64.0)
            .collect();
        normalise(&mut out);
        out
    }

    fn line(s: &Store, session: i64, source: AudioSourceKind, voice: Option<i64>) -> i64 {
        s.insert_segment_full(session, 0, 5000, "words", None, source, None, voice, None)
            .map(|r| r.id)
            .unwrap_or(-1)
    }

    #[test]
    fn a_line_voice_is_kept_and_goes_with_its_line_and_when_old() -> R {
        let s = Store::open_in_memory()?;
        let session = s.begin_session(0, "t")?;
        let a = line(&s, session, AudioSourceKind::Mic, None);
        let b = line(&s, session, AudioSourceKind::Mic, None);
        s.insert_segment_voice(a, &unit(1.0), 4200, 1_000)?;
        s.insert_segment_voice(b, &unit(2.0), 3000, 1_000 + 20 * DAY_MS)?;
        let (e, ms) = s.segment_voice(a)?.ok_or("no voice")?;
        assert_eq!((e, ms), (unit(1.0), 4200));
        s.delete_segment(a)?;
        assert!(s.segment_voice(a)?.is_none(), "deleted with its line");
        // the purge deletes lines too
        s.conn
            .execute("UPDATE segments SET interpreted_at = 1 WHERE id = ?1", [b])?;
        let c = line(&s, session, AudioSourceKind::Mic, None);
        s.insert_segment_voice(c, &unit(3.0), 3000, 1_000)?;
        s.purge_transcripts(0, i64::MAX)?;
        assert!(s.segment_voice(b)?.is_none());
        // retention: 30 days
        assert_eq!(s.prune_segment_voices(1_000 + 29 * DAY_MS)?, 0);
        assert!(s.segment_voice(c)?.is_some());
        assert_eq!(s.prune_segment_voices(1_000 + 31 * DAY_MS)?, 1);
        assert!(s.segment_voice(c)?.is_none());
        Ok(())
    }

    #[test]
    fn correcting_a_line_moves_its_label_and_its_voiceprint() -> R {
        let s = Store::open_in_memory()?;
        let session = s.begin_session(0, "t")?;
        let priya = s.insert_speaker(&unit(1.0), 100.0, 0.0, 10)?;
        let cody = s.insert_speaker(&unit(5.0), 100.0, 0.0, 10)?;
        let embedding = nudged(&unit(1.0), 4.0);
        let seg = line(&s, session, AudioSourceKind::System, Some(cody));
        s.insert_segment_voice(seg, &embedding, 6000, 10)?;
        let c = s
            .correct_segment_speaker(seg, SpeakerTarget::Voice(priya), true, 50)?
            .ok_or("gone")?;
        assert_eq!(
            c,
            Correction {
                from: Some(cody),
                to: Some(priya),
                learned: true,
                new_voice: None
            }
        );
        assert_eq!(s.segment(seg)?.and_then(|r| r.speaker_id), Some(priya));
        let get = |id| s.speaker(id).ok().flatten().ok_or("gone");
        assert!(
            cosine_distance(&get(priya)?.centroid, &embedding)
                < cosine_distance(&unit(1.0), &embedding),
            "the new voice learned it"
        );
        assert!(
            cosine_distance(&get(cody)?.centroid, &embedding)
                > cosine_distance(&unit(5.0), &embedding),
            "the old one unlearned it"
        );
        assert_eq!(get(priya)?.speech_secs, 106.0);
        assert_eq!(get(cody)?.speech_secs, 94.0);
        let ch = s.changes(1)?.into_iter().next().ok_or("no change")?;
        assert_eq!(ch.kind, "segment_speaker");
        let before: serde_json::Value = serde_json::from_str(&ch.before_json)?;
        let after: serde_json::Value = serde_json::from_str(&ch.after_json)?;
        assert_eq!(before["speaker"], json!(cody));
        assert_eq!(after["speaker"], json!(priya));
        assert_eq!(after["learned"], json!(true));

        // Nobody clears it
        let c = s
            .correct_segment_speaker(seg, SpeakerTarget::None, true, 60)?
            .ok_or("gone")?;
        assert_eq!((c.from, c.to), (Some(priya), None));
        assert_eq!(s.segment(seg)?.and_then(|r| r.speaker_id), None);

        // Someone new enrols a voice from the line, named
        let c = s
            .correct_segment_speaker(
                seg,
                SpeakerTarget::New {
                    name: Some(" Dana ".into()),
                },
                true,
                70,
            )?
            .ok_or("gone")?;
        let dana = c.new_voice.ok_or("no voice")?;
        assert_eq!(c.to, Some(dana));
        assert!(c.learned);
        let d = get(dana)?;
        assert_eq!(d.name.as_deref(), Some("Dana"));
        assert_eq!((d.speech_secs, d.mic_secs), (6.0, 0.0));
        assert!(cosine_distance(&d.centroid, &embedding) < 1e-5);

        // a line without a kept embedding is relabelled only
        let bare = line(&s, session, AudioSourceKind::Mic, Some(cody));
        let before_c = get(cody)?.centroid;
        let c = s
            .correct_segment_speaker(bare, SpeakerTarget::Voice(priya), true, 80)?
            .ok_or("gone")?;
        assert!(!c.learned);
        assert_eq!(s.segment(bare)?.and_then(|r| r.speaker_id), Some(priya));
        assert_eq!(get(cody)?.centroid, before_c);
        // and so is any line while learning is off (speaker recognition off)
        let p_before = get(priya)?.centroid;
        let c = s
            .correct_segment_speaker(seg, SpeakerTarget::Voice(priya), false, 90)?
            .ok_or("gone")?;
        assert!(!c.learned);
        assert_eq!(get(priya)?.centroid, p_before);
        // a missing line
        assert!(
            s.correct_segment_speaker(9999, SpeakerTarget::None, true, 1)?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn similar_lines_stay_in_scope_and_under_the_threshold() -> R {
        let s = Store::open_in_memory()?;
        let session = s.begin_session(0, "t")?;
        let other_session = s.begin_session(0, "t")?;
        let cody = s.insert_speaker(&unit(5.0), 100.0, 0.0, 10)?;
        let priya = s.insert_speaker(&unit(1.0), 100.0, 0.0, 10)?;
        let ep = s.conn.query_row(
            "INSERT INTO episodes(started_at, ended_at, title, summary, revision, created_at, last_revised_at)
             VALUES (0, 1, 'E', '', 1, 0, 0) RETURNING id",
            [],
            |r| r.get::<_, i64>(0),
        )?;
        let voice = unit(1.0);
        let mk = |sess, sp, e: &[f32]| -> Result<i64, StoreError> {
            let id = line(&s, sess, AudioSourceKind::System, sp);
            s.insert_segment_voice(id, e, 4000, 0)?;
            Ok(id)
        };
        let corrected = mk(session, Some(cody), &voice)?;
        let close = mk(session, Some(cody), &nudged(&voice, 2.0))?;
        let unvoiced = mk(session, None, &nudged(&voice, 1.0))?;
        let far = mk(session, Some(cody), &unit(9.0))?;
        let other_voice = mk(session, Some(priya), &voice)?;
        let elsewhere = mk(other_session, Some(cody), &voice)?;
        let echo =
            s.insert_segment_from(session, 0, 1, "x", None, AudioSourceKind::Mic, Some(close))?;
        s.insert_segment_voice(echo.id, &voice, 4000, 0)?;
        assert!(cosine_distance(&voice, &nudged(&voice, 2.0)) <= SIMILAR_LINE_THRESHOLD);
        assert!(cosine_distance(&voice, &unit(9.0)) > SIMILAR_LINE_THRESHOLD);

        let ids = |v: Vec<(i64, f32)>| v.into_iter().map(|(i, _)| i).collect::<Vec<_>>();
        let found = s.segments_like(
            corrected,
            LineScope::Session(session),
            Some(cody),
            Some(priya),
            SIMILAR_LINE_THRESHOLD,
        )?;
        assert_eq!(ids(found), vec![unvoiced, close], "nearest first");
        let _ = (far, other_voice, elsewhere);
        // only the note's lines with an Episode scope
        s.conn.execute(
            "UPDATE segments SET episode_id = ?1 WHERE id IN (?2, ?3)",
            [ep, corrected, close],
        )?;
        let found = s.segments_like(
            corrected,
            LineScope::Episode(ep),
            Some(cody),
            Some(priya),
            0.45,
        )?;
        assert_eq!(ids(found), vec![close]);
        // a tighter threshold
        assert!(
            s.segments_like(
                corrected,
                LineScope::Session(session),
                Some(cody),
                None,
                1e-4
            )?
            .is_empty()
        );
        // a line with no kept embedding finds nothing
        let bare = line(&s, session, AudioSourceKind::System, Some(cody));
        assert!(
            s.segments_like(bare, LineScope::Session(session), Some(cody), None, 2.0)?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn moving_similar_lines_is_one_change() -> R {
        let s = Store::open_in_memory()?;
        let session = s.begin_session(0, "t")?;
        let cody = s.insert_speaker(&unit(5.0), 100.0, 0.0, 10)?;
        let priya = s.insert_speaker(&unit(1.0), 100.0, 0.0, 10)?;
        let a = line(&s, session, AudioSourceKind::System, Some(cody));
        let b = line(&s, session, AudioSourceKind::System, None);
        let c = line(&s, session, AudioSourceKind::System, Some(cody));
        s.insert_segment_voice(a, &unit(1.0), 3000, 0)?;
        s.insert_segment_voice(b, &unit(1.0), 3000, 0)?;
        let before = s.speaker(priya)?.ok_or("gone")?.speech_secs;
        let r =
            s.correct_segments_speaker(&[a, b, c, 999], SpeakerTarget::Voice(priya), true, 5)?;
        assert_eq!((r.moved, r.learned, r.to), (3, 2, Some(priya)));
        for id in [a, b, c] {
            assert_eq!(s.segment(id)?.and_then(|r| r.speaker_id), Some(priya));
        }
        assert_eq!(s.speaker(priya)?.ok_or("gone")?.speech_secs, before + 6.0);
        let changes = s.changes(5)?;
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "segment_speakers");
        Ok(())
    }
}
