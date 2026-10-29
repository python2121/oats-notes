//! Echo / duplication between the two lanes, at the text level (the cheap layer screenpipe
//! uses, tightened): on built-in speakers the microphone hears what the computer plays, so a
//! call produces the same sentence twice — once from the system-audio tap ("them"), once from
//! the room ("me"). A microphone segment that overlaps a system segment in time (within
//! [`ECHO_WINDOW_MS`]) and says the same thing is an *echo*: it is stored, linked to the system
//! segment (`segments.echo_of`), and left out of the default views and of the brain, so
//! nothing is lost and the rule stays auditable.
//!
//! Same text = word-Jaccard **or** containment ≥ [`ECHO_THRESHOLD`] over normalised words
//! (lower case, punctuation and apostrophes dropped); when either side has fewer than
//! [`MIN_FUZZY_WORDS`] words only an exact (normalised) match counts — "yes" / "okay" are
//! said by both sides all the time.
//!
//! [`DedupMode`] (`OATS_SYSTEM_DEDUP=auto|always|never`, default auto) decides whether the
//! rule runs at all; in auto it runs only while the computer plays through speakers the
//! microphone can hear (on headphones there is no echo to remove).

use std::collections::HashSet;

use crate::store::TranscriptRow;

/// A system segment within this many ms of a microphone segment's span is a candidate.
pub const ECHO_WINDOW_MS: i64 = 45_000;
/// Jaccard or containment at or above this is the same text.
pub const ECHO_THRESHOLD: f32 = 0.8;
/// Below this many words on either side, only an exact match counts.
pub const MIN_FUZZY_WORDS: usize = 4;

/// Whether the echo rule runs (`OATS_SYSTEM_DEDUP`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DedupMode {
    /// Only while the output is a speaker the microphone can hear (decided by the audio
    /// capture from the default output device).
    #[default]
    Auto,
    Always,
    Never,
}

impl DedupMode {
    /// `auto` | `always` | `never` (case-insensitive; `on`/`off` are accepted too).
    pub fn parse(s: &str) -> Option<DedupMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(DedupMode::Auto),
            "always" | "on" | "text" => Some(DedupMode::Always),
            "never" | "off" => Some(DedupMode::Never),
            _ => None,
        }
    }

    /// From `OATS_SYSTEM_DEDUP` (unset or unknown → auto; unknown is logged).
    pub fn from_env() -> DedupMode {
        match std::env::var("OATS_SYSTEM_DEDUP") {
            Ok(v) => DedupMode::parse(&v).unwrap_or_else(|| {
                log::warn!("OATS_SYSTEM_DEDUP={v:?} is not auto|always|never; using auto");
                DedupMode::Auto
            }),
            Err(_) => DedupMode::Auto,
        }
    }

    /// Whether the rule runs, given whether the default output is a speaker the microphone
    /// can hear.
    pub fn active(self, output_is_speakers: bool) -> bool {
        match self {
            DedupMode::Auto => output_is_speakers,
            DedupMode::Always => true,
            DedupMode::Never => false,
        }
    }
}

/// Lower-cased words, punctuation dropped, apostrophes removed inside words ("don't" →
/// "dont"), so recogniser differences in punctuation and casing don't matter.
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’'))
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// max(word-Jaccard, containment) of two texts, 0…1 (containment = shared distinct words over
/// the smaller side's distinct words).
pub fn similarity(a: &str, b: &str) -> f32 {
    let wa = words(a);
    let wb = words(b);
    similarity_of(&wa, &wb)
}

fn similarity_of(wa: &[String], wb: &[String]) -> f32 {
    let sa: HashSet<&str> = wa.iter().map(String::as_str).collect();
    let sb: HashSet<&str> = wb.iter().map(String::as_str).collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let shared = sa.intersection(&sb).count() as f32;
    let union = sa.union(&sb).count() as f32;
    let jaccard = shared / union;
    let containment = shared / sa.len().min(sb.len()) as f32;
    jaccard.max(containment)
}

/// Whether `mic` says the same thing as `system` (the rule in the module docs).
pub fn is_echo(mic: &str, system: &str) -> bool {
    let wm = words(mic);
    let ws = words(system);
    if wm.is_empty() || ws.is_empty() {
        return false;
    }
    if wm.len() < MIN_FUZZY_WORDS || ws.len() < MIN_FUZZY_WORDS {
        return wm == ws;
    }
    similarity_of(&wm, &ws) >= ECHO_THRESHOLD
}

/// Whether two spans `[a0, a1]` and `[b0, b1]` (ms) come within [`ECHO_WINDOW_MS`].
pub fn within_window(a0: i64, a1: i64, b0: i64, b1: i64) -> bool {
    b1 >= a0 - ECHO_WINDOW_MS && b0 <= a1 + ECHO_WINDOW_MS
}

/// The system segment a microphone segment (`[started_at, ended_at]`, `text`) echoes, if any:
/// among `system` rows within the window (rows of other sources and rows that are themselves
/// echoes are ignored), the most similar one that passes [`is_echo`]. When no single row
/// passes, the window's rows joined in time order are tried too (the microphone's segmenter
/// can merge two system segments into one); the match then points at the row sharing the
/// most words.
pub fn find_echo_of(
    started_at: i64,
    ended_at: i64,
    text: &str,
    system: &[TranscriptRow],
) -> Option<i64> {
    let mut near: Vec<&TranscriptRow> = system
        .iter()
        .filter(|r| r.source == crate::AudioSourceKind::System && r.echo_of.is_none())
        .filter(|r| within_window(started_at, ended_at, r.started_at, r.ended_at))
        .collect();
    if near.is_empty() {
        return None;
    }
    near.sort_by_key(|r| (r.started_at, r.id));
    let best = near
        .iter()
        .filter(|r| is_echo(text, &r.text))
        .map(|r| (similarity(text, &r.text), r.id))
        .max_by(|a, b| a.0.total_cmp(&b.0));
    if let Some((_, id)) = best {
        return Some(id);
    }
    if near.len() > 1 {
        let joined = near
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if is_echo(text, &joined) {
            let mic: HashSet<String> = words(text).into_iter().collect();
            return near
                .iter()
                .max_by_key(|r| words(&r.text).iter().filter(|w| mic.contains(*w)).count())
                .map(|r| r.id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioSourceKind;

    fn sys(id: i64, start: i64, end: i64, text: &str) -> TranscriptRow {
        TranscriptRow {
            id,
            started_at: start,
            ended_at: end,
            text: text.to_owned(),
            source: AudioSourceKind::System,
            ..TranscriptRow::default()
        }
    }

    #[test]
    fn words_are_normalised() {
        assert_eq!(
            words("Don't  PANIC, it's — fine!  42x"),
            ["dont", "panic", "its", "fine", "42x"]
        );
        assert!(words(" … ").is_empty());
    }

    #[test]
    fn identical_and_near_identical_text_is_an_echo() {
        let s = "The quarterly numbers look good and we should ship on Friday.";
        assert!(is_echo(s, s));
        // recogniser differences: casing, punctuation, one word
        assert!(is_echo(
            "the quarterly numbers look good, and we should ship Friday",
            s
        ));
        // the mic heard only the second half: containment
        assert!(is_echo("and we should ship on Friday", s));
        // a longer mic segment containing the whole system sentence
        assert!(is_echo(
            "The quarterly numbers look good and we should ship on Friday. Yeah.",
            s
        ));
    }

    #[test]
    fn different_text_is_not_an_echo() {
        assert!(!is_echo(
            "I'll send the estimate to Priya tomorrow morning",
            "The quarterly numbers look good and we should ship on Friday."
        ));
        // half the words in common is not enough
        assert!(!is_echo(
            "we should ship the export rewrite before the holidays",
            "we should ship on Friday because the numbers look good"
        ));
        assert!(!is_echo("", "anything at all here"));
    }

    #[test]
    fn short_text_needs_an_exact_match() {
        assert!(is_echo("Okay.", "okay"));
        assert!(is_echo("Thank you very", "thank you, very"));
        assert!(!is_echo("Yes", "Yes, I think we should go"));
        assert!(!is_echo("okay thanks", "okay"));
        // 3 words against a long text: containment would be 1.0, but it is too short
        assert!(!is_echo(
            "ship on Friday",
            "we should ship on Friday after all"
        ));
    }

    #[test]
    fn similarity_is_the_max_of_jaccard_and_containment() {
        assert!((similarity("a b c d", "a b c d") - 1.0).abs() < 1e-6);
        // shared 2, union 6 → Jaccard 1/3; containment 2/4
        assert!((similarity("a b c d", "a b e f") - 0.5).abs() < 1e-6);
        assert_eq!(similarity("", "a"), 0.0);
    }

    #[test]
    fn find_echo_respects_the_window_and_the_source() {
        let text = "the deploy goes out on Thursday after the review";
        let rows = [
            sys(1, 0, 4_000, text),
            sys(2, 200_000, 204_000, text),
            TranscriptRow {
                source: AudioSourceKind::Mic,
                ..sys(3, 10_000, 14_000, text)
            },
        ];
        // overlapping in time
        assert_eq!(find_echo_of(500, 4_800, text, &rows), Some(1));
        // 40 s after the system segment ended: still inside the window
        assert_eq!(find_echo_of(44_000, 48_000, text, &rows), Some(1));
        // 60 s after: outside (and far from #2)
        assert_eq!(find_echo_of(64_000, 68_000, text, &rows), None);
        // only system rows count (row 3 is a mic row)
        assert_eq!(
            find_echo_of(10_000, 14_000, "something else entirely now", &rows),
            None
        );
        // an echo row is never the target
        let echoed = [TranscriptRow {
            echo_of: Some(9),
            ..sys(4, 0, 4_000, text)
        }];
        assert_eq!(find_echo_of(0, 4_000, text, &echoed), None);
        assert_eq!(find_echo_of(0, 4_000, text, &[]), None);
    }

    #[test]
    fn find_echo_picks_the_best_match_and_joins_split_segments() {
        let rows = [
            sys(1, 0, 3_000, "Welcome everyone to the planning meeting"),
            sys(
                2,
                3_500,
                7_000,
                "today we decide the launch date for the new app",
            ),
        ];
        assert_eq!(
            find_echo_of(
                3_400,
                7_200,
                "today we decide the launch date for the new app",
                &rows
            ),
            Some(2)
        );
        // the mic merged both system segments into one
        let merged = "Welcome everyone to the planning meeting. Today we decide the launch \
                      date for the new app.";
        assert!(find_echo_of(0, 7_200, merged, &rows).is_some());
    }

    #[test]
    fn dedup_mode_parses_and_decides() {
        assert_eq!(DedupMode::parse("AUTO"), Some(DedupMode::Auto));
        assert_eq!(DedupMode::parse("always"), Some(DedupMode::Always));
        assert_eq!(DedupMode::parse("never"), Some(DedupMode::Never));
        assert_eq!(DedupMode::parse("off"), Some(DedupMode::Never));
        assert_eq!(DedupMode::parse("sometimes"), None);
        assert!(DedupMode::Auto.active(true));
        assert!(!DedupMode::Auto.active(false));
        assert!(DedupMode::Always.active(false));
        assert!(!DedupMode::Never.active(true));
    }
}
