//! Cutting an all-day transcript into episodes ("meetings" / conversations) before any prompt
//! sees it — pure logic. The only signal we have is the transcript itself, so the rule is the
//! one meeting detectors fall back to (screenpipe ends a meeting on silence): a gap of more
//! than [`EpisodeRule::gap`] between segments starts a new episode, and an episode is capped
//! in length and segment count so a prompt stays a sane size.

use std::time::Duration;

use chrono::{DateTime, TimeZone};
use oats_core::TranscriptRow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EpisodeRule {
    /// Silence longer than this starts a new episode.
    pub gap: Duration,
    /// An episode never spans more than this.
    pub max_len: Duration,
    /// … nor more segments than this.
    pub max_segments: usize,
}

impl Default for EpisodeRule {
    fn default() -> Self {
        EpisodeRule {
            gap: Duration::from_secs(10 * 60),
            max_len: Duration::from_secs(3 * 60 * 60),
            max_segments: 400,
        }
    }
}

/// A run of segments that belong together.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub started_at: i64,
    pub ended_at: i64,
    pub segments: Vec<TranscriptRow>,
}

impl Chunk {
    pub fn segment_ids(&self) -> Vec<i64> {
        self.segments.iter().map(|s| s.id).collect()
    }

    /// Whether more segments may still join this chunk: its last segment ended less than a gap
    /// ago.
    pub fn is_open(&self, now_ms: i64, rule: &EpisodeRule) -> bool {
        now_ms - self.ended_at < rule.gap.as_millis() as i64
    }

    pub fn text_len(&self) -> usize {
        self.segments.iter().map(|s| s.text.len()).sum()
    }
}

/// Group `rows` (any order; sorted here) into chunks by the rule.
pub fn cut(rows: &[TranscriptRow], rule: &EpisodeRule) -> Vec<Chunk> {
    let mut rows: Vec<TranscriptRow> = rows.to_vec();
    rows.sort_by_key(|r| (r.started_at, r.id));
    let gap = rule.gap.as_millis() as i64;
    let max_len = rule.max_len.as_millis() as i64;
    let mut out: Vec<Chunk> = Vec::new();
    for row in rows {
        let new_chunk = match out.last() {
            None => true,
            Some(c) => {
                row.started_at - c.ended_at > gap
                    || row.ended_at - c.started_at > max_len
                    || c.segments.len() >= rule.max_segments
            }
        };
        if new_chunk {
            out.push(Chunk {
                started_at: row.started_at,
                ended_at: row.ended_at,
                segments: vec![row],
            });
        } else if let Some(c) = out.last_mut() {
            c.ended_at = c.ended_at.max(row.ended_at);
            c.segments.push(row);
        }
    }
    out
}

/// The transcript as the prompts see it: one `[HH:MM] text` line per segment (anarlog renders
/// `speaker: text`; we have no speakers yet).
pub fn render_transcript<Tz: TimeZone>(rows: &[TranscriptRow], tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let mut out = String::new();
    for r in rows {
        let stamp = tz
            .timestamp_millis_opt(r.started_at)
            .single()
            .map(|d: DateTime<Tz>| d.format("%H:%M").to_string())
            .unwrap_or_else(|| "--:--".to_owned());
        out.push('[');
        out.push_str(&stamp);
        out.push_str("] ");
        out.push_str(r.text.trim());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, start_s: i64, len_s: i64) -> TranscriptRow {
        TranscriptRow {
            id,
            session_id: 1,
            started_at: start_s * 1000,
            ended_at: (start_s + len_s) * 1000,
            text: format!("segment {id}"),
            confidence: None,
            node_id: None,
            episode_id: None,
            interpreted_at: None,
            categorised: false,
        }
    }

    #[test]
    fn gap_starts_a_new_episode() {
        let rule = EpisodeRule {
            gap: Duration::from_secs(600),
            ..Default::default()
        };
        let rows = [
            row(3, 800, 5), // out of order on purpose
            row(1, 0, 10),
            row(2, 300, 10),   // 290 s after the first ends: same
            row(4, 1_300, 10), // 495 s after #3 ends: same (gap is > 600)
            row(5, 2_000, 10), // 690 s: new
        ];
        let chunks = cut(&rows, &rule);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].segment_ids(), vec![1, 2, 3, 4]);
        assert_eq!(chunks[0].started_at, 0);
        assert_eq!(chunks[0].ended_at, 1_310_000);
        assert_eq!(chunks[1].segment_ids(), vec![5]);
        assert!(chunks[1].is_open(2_500_000, &rule));
        assert!(!chunks[1].is_open(2_700_000, &rule));
        assert!(cut(&[], &rule).is_empty());
    }

    #[test]
    fn caps_on_length_and_count() {
        let rule = EpisodeRule {
            gap: Duration::from_secs(600),
            max_len: Duration::from_secs(1_000),
            max_segments: 3,
        };
        let rows: Vec<TranscriptRow> = (0..7).map(|i| row(i, i * 100, 10)).collect();
        let chunks = cut(&rows, &rule);
        assert_eq!(
            chunks.iter().map(Chunk::segment_ids).collect::<Vec<_>>(),
            vec![vec![0, 1, 2], vec![3, 4, 5], vec![6]]
        );
        let long: Vec<TranscriptRow> = (0..5).map(|i| row(i, i * 300, 10)).collect();
        let rule = EpisodeRule {
            max_segments: 100,
            ..rule
        };
        let chunks = cut(&long, &rule);
        // 0, 300, 600, 900 (ending 910) fit in 1000 s; 1200 doesn't
        assert_eq!(
            chunks.iter().map(Chunk::segment_ids).collect::<Vec<_>>(),
            vec![vec![0, 1, 2, 3], vec![4]]
        );
    }

    #[test]
    fn renders_stamped_lines() {
        let tz = chrono::FixedOffset::east_opt(0).unwrap();
        let rows = [row(1, 1_793_106_300, 5), row(2, 1_793_106_360, 5)];
        assert_eq!(
            render_transcript(&rows, &tz),
            "[13:05] segment 1\n[13:06] segment 2\n"
        );
    }
}
