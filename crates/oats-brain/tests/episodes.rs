//! Episode cutting edge cases: the exact gap and length boundaries, the segment cap, single and
//! overlapping segments, ordering ties.

use std::time::Duration;

use oats_brain::episodes::{Chunk, EpisodeRule, cut, render_transcript};
use oats_core::{AudioSourceKind, MediaClass, TranscriptRow};

fn row(id: i64, start_ms: i64, end_ms: i64) -> TranscriptRow {
    TranscriptRow {
        id,
        session_id: 1,
        started_at: start_ms,
        ended_at: end_ms,
        text: format!("s{id}"),
        confidence: None,
        node_id: None,
        episode_id: None,
        interpreted_at: None,
        categorised: false,
        ..TranscriptRow::default()
    }
}

fn ids(chunks: &[Chunk]) -> Vec<Vec<i64>> {
    chunks.iter().map(Chunk::segment_ids).collect()
}

const GAP_MS: i64 = 600_000;

fn rule() -> EpisodeRule {
    EpisodeRule {
        gap: Duration::from_millis(GAP_MS as u64),
        max_len: Duration::from_secs(3 * 3600),
        max_segments: 400,
    }
}

#[test]
fn a_single_segment_is_one_chunk() {
    let chunks = cut(&[row(7, 1_000, 4_000)], &rule());
    assert_eq!(chunks.len(), 1);
    let c = &chunks[0];
    assert_eq!((c.started_at, c.ended_at), (1_000, 4_000));
    assert_eq!(c.segment_ids(), vec![7]);
    assert_eq!(c.text_len(), 2);
    assert!(cut(&[], &rule()).is_empty());
}

#[test]
fn the_gap_must_be_exceeded_to_split() {
    let exact = [row(1, 0, 10_000), row(2, 10_000 + GAP_MS, 20_000 + GAP_MS)];
    assert_eq!(
        ids(&cut(&exact, &rule())),
        vec![vec![1, 2]],
        "exactly the gap: same"
    );
    let over = [row(1, 0, 10_000), row(2, 10_001 + GAP_MS, 20_000 + GAP_MS)];
    assert_eq!(
        ids(&cut(&over, &rule())),
        vec![vec![1], vec![2]],
        "1 ms more: split"
    );
    // the gap is measured from the latest end in the chunk, not from the previous start
    let long_then_short = [
        row(1, 0, 900_000),
        row(2, 100_000, 110_000), // inside #1
        row(3, 900_000 + GAP_MS, 900_000 + GAP_MS + 1),
    ];
    let chunks = cut(&long_then_short, &rule());
    assert_eq!(ids(&chunks), vec![vec![1, 2, 3]]);
    assert_eq!(chunks[0].ended_at, 900_000 + GAP_MS + 1);
}

#[test]
fn is_open_is_strictly_within_the_gap() {
    let c = &cut(&[row(1, 0, 10_000)], &rule())[0];
    assert!(c.is_open(10_000, &rule()));
    assert!(c.is_open(10_000 + GAP_MS - 1, &rule()));
    assert!(!c.is_open(10_000 + GAP_MS, &rule()));
    // a clock that went backwards still counts as open
    assert!(c.is_open(0, &rule()));
}

#[test]
fn the_length_cap_allows_exactly_max_len() {
    let r = EpisodeRule {
        max_len: Duration::from_millis(100_000),
        ..rule()
    };
    let exact = [row(1, 0, 10_000), row(2, 90_000, 100_000)];
    assert_eq!(ids(&cut(&exact, &r)), vec![vec![1, 2]]);
    let over = [row(1, 0, 10_000), row(2, 90_000, 100_001)];
    assert_eq!(ids(&cut(&over, &r)), vec![vec![1], vec![2]]);
    // a single segment longer than the cap is still one chunk
    assert_eq!(ids(&cut(&[row(1, 0, 500_000)], &r)), vec![vec![1]]);
}

#[test]
fn the_segment_cap_splits_contiguous_speech() {
    let rows: Vec<TranscriptRow> = (0..5).map(|i| row(i, i * 1_000, i * 1_000 + 500)).collect();
    let r = EpisodeRule {
        max_segments: 2,
        ..rule()
    };
    let chunks = cut(&rows, &r);
    assert_eq!(ids(&chunks), vec![vec![0, 1], vec![2, 3], vec![4]]);
    // the chunks tile the time without overlap
    for w in chunks.windows(2) {
        assert!(w[0].ended_at <= w[1].started_at);
    }
    let one = EpisodeRule {
        max_segments: 1,
        ..rule()
    };
    assert_eq!(cut(&rows, &one).len(), 5);
    // a cap of zero degrades to one segment per chunk rather than looping or panicking
    let zero = EpisodeRule {
        max_segments: 0,
        ..rule()
    };
    assert_eq!(cut(&rows, &zero).len(), 5);
}

#[test]
fn input_order_does_not_matter_and_ties_go_by_id() {
    let rows = [
        row(5, 2_000, 3_000),
        row(3, 1_000, 1_500),
        row(4, 1_000, 1_200),
        row(1, 0, 500),
    ];
    let chunks = cut(&rows, &rule());
    assert_eq!(ids(&chunks), vec![vec![1, 3, 4, 5]]);
    let mut reversed = rows.to_vec();
    reversed.reverse();
    assert_eq!(cut(&reversed, &rule()), chunks);
}

#[test]
fn renders_one_line_per_segment_trimmed() {
    let tz = chrono::FixedOffset::east_opt(3600).unwrap();
    let mut a = row(1, 0, 1_000);
    a.text = "  padded text \n".into();
    let b = row(2, 61_000, 62_000);
    assert_eq!(
        render_transcript(&[a, b], &tz),
        "[01:00] me: padded text\n[01:01] me: s2\n"
    );
    assert_eq!(render_transcript(&[], &tz), "");
}

// ---- meetings bound episodes ------------------------------------------------------------

const MIN: i64 = 60_000;

fn mic(id: i64, at_min: i64) -> TranscriptRow {
    row(id, at_min * MIN, at_min * MIN + 5_000)
}

fn call(id: i64, at_min: i64, url: &str) -> TranscriptRow {
    TranscriptRow {
        source: AudioSourceKind::System,
        media_kind: Some(MediaClass::Meeting),
        media_ref: Some(url.into()),
        ..row(id, at_min * MIN, at_min * MIN + 5_000)
    }
}

#[test]
fn a_call_starting_inside_a_run_of_speech_starts_its_own_episode() {
    // 14:08..14:29 room speech every 3 min, then the call 14:30..14:58
    let mut rows: Vec<TranscriptRow> = (0..8).map(|i| mic(i, i * 3)).collect(); // 0..21
    rows.extend((0..10).map(|i| call(100 + i, 30 + i * 3, "https://meet/a")));
    let chunks = cut(&rows, &rule());
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].segment_ids(), (0..8).collect::<Vec<_>>());
    assert_eq!(chunks[1].started_at, 30 * MIN);
    assert_eq!(chunks[1].meeting_key().as_deref(), Some("https://meet/a"));
    assert_eq!(chunks[0].meeting_key(), None);
}

#[test]
fn a_lead_in_of_mic_speech_joins_the_call() {
    let rows = [
        mic(1, 22),
        mic(2, 27), // 3 min before: stays out
        mic(3, 29), // 1 min before: lead-in
        call(4, 30, "https://meet/a"),
        mic(5, 31),
    ];
    let chunks = cut(&rows, &rule());
    assert_eq!(ids(&chunks), vec![vec![1, 2], vec![3, 4, 5]]);
    assert_eq!(chunks[1].started_at, 29 * MIN);
    // only lead-in: the earlier episode vanishes rather than staying empty
    let only = [mic(1, 29), call(2, 30, "u")];
    assert_eq!(ids(&cut(&only, &rule())), vec![vec![1, 2]]);
}

#[test]
fn room_talk_after_the_call_is_a_new_episode() {
    let rows = [
        call(1, 0, "u"),
        mic(2, 2),
        call(3, 4, "u"),
        mic(4, 6), // 2 min after the last call segment: still the call
        mic(5, 8), // 4 min after: the call is over
        mic(6, 9),
    ];
    let chunks = cut(&rows, &rule());
    assert_eq!(ids(&chunks), vec![vec![1, 2, 3, 4], vec![5, 6]]);
    assert_eq!(chunks[1].meeting_key(), None);
}

#[test]
fn two_different_calls_are_two_episodes() {
    let rows = [call(1, 0, "https://meet/a"), call(2, 1, "https://meet/b")];
    assert_eq!(ids(&cut(&rows, &rule())), vec![vec![1], vec![2]]);
}

#[test]
fn a_pause_inside_a_call_does_not_split() {
    let rows = [call(1, 0, "u"), call(2, 9, "u"), call(3, 18, "u")];
    assert_eq!(ids(&cut(&rows, &rule())), vec![vec![1, 2, 3]]);
}
