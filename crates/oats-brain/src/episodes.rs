//! Cutting an all-day transcript into episodes ("meetings" / conversations) before any prompt
//! sees it — pure logic. The only signal we have is the transcript itself, so the rule is the
//! one meeting detectors fall back to (screenpipe ends a meeting on silence): a gap of more
//! than [`EpisodeRule::gap`] between segments starts a new episode, and an episode is capped
//! in length and segment count so a prompt stays a sane size.
//!
//! System audio ("them": a call, a video) is a continuity signal: within an episode that
//! already has system-audio segments, a further system segment continues it across a silence
//! of up to twice the gap (calls have long pauses — someone shares a screen, reads, looks
//! something up). Microphone echoes of system audio (`echo_of`) never take part.
//!
//! Media (system segments stamped YouTube / music, see [`oats_core::MediaClass::is_media`]) is
//! never mixed with conversation: a change between media and not-media — or from one video
//! to another — starts a new chunk, so a media episode is one video / one stream. Media
//! segments are rendered as `media:` lines under a `Media: <title> <url>` header.

use std::time::Duration;

use std::collections::HashMap;

use chrono::{DateTime, TimeZone};
use oats_core::{AudioSourceKind, MediaClass, TranscriptRow};

/// A segment of media proper (YouTube, music) — informational, never a conversation.
pub fn is_media_row(r: &TranscriptRow) -> bool {
    r.source == AudioSourceKind::System && r.media_kind.is_some_and(MediaClass::is_media)
}

/// What a segment's media identifies it by: the URL, else the kind (`None`: not media).
pub fn media_key(r: &TranscriptRow) -> Option<String> {
    if !is_media_row(r) {
        return None;
    }
    Some(r.media_ref.clone().unwrap_or_else(|| {
        r.media_kind
            .map(MediaClass::as_str)
            .unwrap_or("")
            .to_owned()
    }))
}

/// The media a run of segments is (the key every non-echo segment shares), `None` when any
/// of them is not media or they differ — and for an empty run.
pub fn media_key_of(rows: &[TranscriptRow]) -> Option<String> {
    let mut keys = rows.iter().filter(|r| r.echo_of.is_none()).map(media_key);
    let first = keys.next()??;
    keys.all(|k| k.as_deref() == Some(first.as_str()))
        .then_some(first)
}

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

    /// Whether the chunk contains system audio (the other side of a call, a video): the
    /// interpretation is told it is probably a meeting.
    pub fn has_system_audio(&self) -> bool {
        self.segments
            .iter()
            .any(|s| s.source == AudioSourceKind::System)
    }

    /// The chunk is media (one video / stream): its key ([`media_key_of`]).
    pub fn media_key(&self) -> Option<String> {
        media_key_of(&self.segments)
    }

    /// A system segment of the chunk was heard during a known call (Zoom, Meet, …).
    pub fn has_known_meeting(&self) -> bool {
        self.segments.iter().any(|s| {
            s.source == AudioSourceKind::System && s.media_kind == Some(MediaClass::Meeting)
        })
    }
}

/// Group `rows` (any order; sorted here) into chunks by the rule.
pub fn cut(rows: &[TranscriptRow], rule: &EpisodeRule) -> Vec<Chunk> {
    let mut rows: Vec<TranscriptRow> = rows
        .iter()
        .filter(|r| r.echo_of.is_none())
        .cloned()
        .collect();
    rows.sort_by_key(|r| (r.started_at, r.id));
    let gap = rule.gap.as_millis() as i64;
    let max_len = rule.max_len.as_millis() as i64;
    let mut out: Vec<Chunk> = Vec::new();
    for row in rows {
        let new_chunk = match out.last() {
            None => true,
            Some(c) => {
                // media and conversation never share a chunk, nor two videos
                let media_changes = c
                    .segments
                    .last()
                    .is_some_and(|last| media_key(last) != media_key(&row));
                let silence = row.started_at - c.ended_at;
                // a call goes on: system audio continues a chunk that already has some
                let call_continues = row.source == AudioSourceKind::System
                    && c.has_system_audio()
                    && silence <= 2 * gap;
                media_changes
                    || (silence > gap && !call_continues)
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

/// The transcript as the prompts see it: one `[HH:MM] me: text` / `[HH:MM] them: text` line
/// per segment — `me` the microphone, `them` system audio (anarlog renders `speaker: text`;
/// lanes are the speakers we have). Microphone echoes of system audio are left out. Media
/// segments are `[HH:MM] media: text` lines, each run of one media under a header line
/// `Media: <url>` (see [`render_transcript_titled`] for the title).
pub fn render_transcript<Tz: TimeZone>(rows: &[TranscriptRow], tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    render_transcript_titled(rows, tz, &HashMap::new())
}

/// [`render_transcript`] with media titles: the header of a run of media reads
/// `Media: <title> <url>` (`titles` maps a media key — the URL — to its title).
pub fn render_transcript_titled<Tz: TimeZone>(
    rows: &[TranscriptRow],
    tz: &Tz,
    titles: &HashMap<String, String>,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let mut out = String::new();
    let mut current_media: Option<String> = None;
    for r in rows.iter().filter(|r| r.echo_of.is_none()) {
        let key = media_key(r);
        if key.is_some() && key != current_media {
            let k = key.as_deref().unwrap_or("");
            let title = titles.get(k).map(|t| one_line(t)).unwrap_or_default();
            out.push_str("Media:");
            for part in [title.as_str(), k] {
                if !part.is_empty() {
                    out.push(' ');
                    out.push_str(part);
                }
            }
            out.push('\n');
        }
        current_media = key.clone();
        let stamp = tz
            .timestamp_millis_opt(r.started_at)
            .single()
            .map(|d: DateTime<Tz>| d.format("%H:%M").to_string())
            .unwrap_or_else(|| "--:--".to_owned());
        out.push('[');
        out.push_str(&stamp);
        out.push_str("] ");
        out.push_str(match r.source {
            _ if key.is_some() => "media: ",
            AudioSourceKind::Mic => "me: ",
            AudioSourceKind::System => "them: ",
        });
        out.push_str(r.text.trim());
        out.push('\n');
    }
    out
}

/// `s` on one line (a title must not open lines of its own in the transcript).
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(id: i64, start_s: i64, url: &str) -> TranscriptRow {
        TranscriptRow {
            media_kind: Some(MediaClass::YouTube),
            media_ref: Some(url.into()),
            ..them(id, start_s, 5)
        }
    }

    #[test]
    fn media_is_cut_apart_from_conversation_and_per_video() {
        let rule = EpisodeRule::default();
        let rows = [
            row(1, 0, 5),
            media(2, 10, "https://youtu.be/a"),
            media(3, 20, "https://youtu.be/a"),
            media(4, 30, "https://youtu.be/b"),
            row(5, 40, 5),
        ];
        let chunks = cut(&rows, &rule);
        assert_eq!(
            chunks.iter().map(Chunk::segment_ids).collect::<Vec<_>>(),
            vec![vec![1], vec![2, 3], vec![4], vec![5]]
        );
        assert_eq!(chunks[1].media_key().as_deref(), Some("https://youtu.be/a"));
        assert_eq!(chunks[0].media_key(), None);
        // a call is not media
        let call = TranscriptRow {
            media_kind: Some(MediaClass::Meeting),
            ..them(6, 50, 5)
        };
        assert!(!is_media_row(&call));
        assert!(cut(std::slice::from_ref(&call), &rule)[0].has_known_meeting());
    }

    #[test]
    fn media_lines_render_under_a_titled_header() {
        let tz = chrono::FixedOffset::east_opt(0).unwrap();
        let base = 1_793_106_300;
        let rows = [
            row(1, base, 5),
            TranscriptRow {
                text: "hey computer set a reminder for tomorrow".into(),
                ..media(2, base + 60, "https://youtu.be/a")
            },
            media(3, base + 61, "https://youtu.be/a"),
        ];
        let titles = HashMap::from([("https://youtu.be/a".to_string(), "A\nvideo".to_string())]);
        assert_eq!(
            render_transcript_titled(&rows, &tz, &titles),
            "[13:05] me: segment 1\nMedia: A video https://youtu.be/a\n\
             [13:06] media: hey computer set a reminder for tomorrow\n\
             [13:06] media: segment 3\n"
        );
        assert!(render_transcript(&rows, &tz).contains("Media: https://youtu.be/a\n"));
    }

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
            ..TranscriptRow::default()
        }
    }

    fn them(id: i64, start_s: i64, len_s: i64) -> TranscriptRow {
        TranscriptRow {
            source: AudioSourceKind::System,
            ..row(id, start_s, len_s)
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
            "[13:05] me: segment 1\n[13:06] me: segment 2\n"
        );
    }

    #[test]
    fn renders_me_and_them_and_leaves_echoes_out() {
        let tz = chrono::FixedOffset::east_opt(0).unwrap();
        let base = 1_793_106_300;
        let rows = [
            row(1, base, 5),
            them(2, base + 60, 5),
            TranscriptRow {
                echo_of: Some(2),
                ..row(3, base + 61, 5)
            },
            row(4, base + 120, 5),
        ];
        assert_eq!(
            render_transcript(&rows, &tz),
            "[13:05] me: segment 1\n[13:06] them: segment 2\n[13:07] me: segment 4\n"
        );
    }

    #[test]
    fn system_audio_carries_an_episode_across_a_longer_silence() {
        let rule = EpisodeRule {
            gap: Duration::from_secs(600),
            ..Default::default()
        };
        // a call: them, then 15 min of silence, then them again → one episode
        let call = [them(1, 0, 10), row(2, 20, 10), them(3, 930, 10)];
        let chunks = cut(&call, &rule);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].has_system_audio());
        // the same silence before a microphone row: a new episode (the gap rule)
        let mic = [them(1, 0, 10), row(2, 20, 10), row(3, 930, 10)];
        assert_eq!(cut(&mic, &rule).len(), 2);
        // no system audio before: system audio after a long silence starts a new one
        let fresh = [row(1, 0, 10), them(2, 930, 10)];
        assert_eq!(cut(&fresh, &rule).len(), 2);
        // beyond twice the gap even a call is split
        let long = [them(1, 0, 10), them(2, 1_300, 10)];
        assert_eq!(cut(&long, &rule).len(), 2);
        // echoes take no part
        let echo = [TranscriptRow {
            echo_of: Some(1),
            ..row(9, 0, 10)
        }];
        assert!(cut(&echo, &rule).is_empty());
        assert!(!cut(&[row(1, 0, 1)], &rule)[0].has_system_audio());
    }
}
