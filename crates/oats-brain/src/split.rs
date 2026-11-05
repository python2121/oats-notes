//! "Split…": the user says, in their own words, what part of a note belongs in a note of its
//! own. One tier-1 request ([`split_request`], `prompts/split.*.md`) returns both notes'
//! content and the transcript lines (numbered, [`render_transcript_numbered`]) that move to
//! the new note; [`apply_split`] rewrites the original in place and creates the second note
//! from those lines, in one savepoint. No events, no speakers, no new nodes.

use chrono::Local;
use oats_core::{Episode, EpisodeContent, Node, NodeKind, Store, StoreError, TranscriptRow};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::claude_cli::Request;
use crate::episodes::{Chunk, numbered_rows, render_transcript_numbered};
use crate::interpret::{
    ProposedAction, ProposedDecision, action_item_schema, action_items_of, clock,
    decision_item_schema, decisions_of, render, render_tree, strip_delimiters,
};
use crate::moveto::node_path;

pub const SPLIT_SYSTEM: &str = include_str!("../prompts/split.system.md");
pub const SPLIT_USER: &str = include_str!("../prompts/split.user.md");

/// The failure when the model's lines don't separate anything (none, or every line).
pub const NOT_SEPARATED: &str = "the model didn't separate the transcript lines";

/// Longest instruction kept from the user (characters).
const MAX_TEXT_CHARS: usize = 2000;

/// One of the two notes as the model returns it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct NoteContent {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<ProposedDecision>,
    #[serde(default)]
    pub action_items: Vec<ProposedAction>,
    #[serde(default)]
    pub topics: Vec<String>,
    /// For the NEW note: an existing project / topic (`None` = the original's home).
    /// Ignored on `keep`.
    #[serde(default)]
    pub node_id: Option<i64>,
}

/// The model's answer.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct SplitResult {
    #[serde(default)]
    pub keep: NoteContent,
    #[serde(default)]
    pub new: NoteContent,
    /// 1-based line numbers of the numbered transcript that go to the new note.
    #[serde(default)]
    pub new_lines: Vec<i64>,
}

fn note_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": { "type": "string" },
            "summary": { "type": "string", "description": "markdown" },
            "decisions": { "type": "array", "items": decision_item_schema() },
            "action_items": { "type": "array", "items": action_item_schema() },
            "topics": { "type": "array", "items": { "type": "string" } },
            "node_id": {
                "type": ["integer", "null"],
                "description": "the NEW note's existing project/topic; null = the original's home (ignored on keep)"
            }
        },
        "required": ["title", "summary", "decisions", "action_items", "topics", "node_id"],
        "additionalProperties": false
    })
}

pub fn split_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "keep": note_schema(),
            "new": note_schema(),
            "new_lines": {
                "type": "array",
                "description": "1-based line numbers of the transcript lines that go to the NEW note",
                "items": { "type": "integer" }
            }
        },
        "required": ["keep", "new", "new_lines"],
        "additionalProperties": false
    })
}

pub fn parse_split(v: &Value) -> Result<SplitResult, serde_json::Error> {
    serde_json::from_value(v.clone())
}

/// The note as the prompt shows it: title, summary, decisions and action items with their
/// `(project: …)` and `(at: HH:MM)`, topics.
fn render_note(e: &Episode, tree: &[Node]) -> String {
    let refs = |node: Option<i64>, at: Option<i64>| {
        let mut r = String::new();
        if let Some(n) = node.and_then(|id| tree.iter().find(|n| n.id == id)) {
            r.push_str(&format!(" (project: {})", n.name));
        }
        if let Some(at) = at {
            r.push_str(&format!(" (at: {})", clock(at, &Local)));
        }
        r
    };
    let mut s = format!("Title: {}\n\nSummary:\n{}\n", e.title, e.summary.trim());
    if !e.decisions.is_empty() {
        s.push_str("\nDecisions:\n");
        for d in &e.decisions {
            s.push_str(&format!("- {}{}\n", d.text, refs(d.node_id, d.at_ms)));
        }
    }
    if !e.action_items.is_empty() {
        s.push_str("\nAction items:\n");
        for a in &e.action_items {
            s.push_str(&format!(
                "- {}{}{}{}\n",
                a.text,
                a.owner
                    .as_ref()
                    .map(|o| format!(" (owner: {o})"))
                    .unwrap_or_default(),
                a.due
                    .as_ref()
                    .map(|d| format!(" (due: {d})"))
                    .unwrap_or_default(),
                refs(a.node_id, a.at_ms)
            ));
        }
    }
    if !e.topics.is_empty() {
        s.push_str(&format!("\nTopics: {}\n", e.topics.join(", ")));
    }
    s
}

/// The tier-1 request: the tree, the note, its numbered transcript (`rows` =
/// `Store::segments_of_episode`) and the user's instruction.
pub fn split_request(
    tree: &[Node],
    ep: &Episode,
    rows: &[TranscriptRow],
    text: &str,
    speakers: &oats_core::speakers::SpeakerMap,
    model: Option<String>,
) -> Request {
    let current = match ep.node_id {
        Some(id) if tree.iter().any(|n| n.id == id) => format!("{} [{id}]", node_path(tree, id)),
        _ => "nothing (unfiled)".to_owned(),
    };
    let text: String = text.trim().chars().take(MAX_TEXT_CHARS).collect();
    Request {
        system_prompt: SPLIT_SYSTEM.to_owned(),
        user_prompt: render(
            SPLIT_USER,
            &[
                ("tree", &render_tree(tree, None)),
                ("current", &current),
                ("note", &strip_delimiters(&render_note(ep, tree))),
                (
                    "transcript",
                    &strip_delimiters(&render_transcript_numbered(rows, &Local, speakers)),
                ),
                ("text", &strip_delimiters(&text)),
            ],
        ),
        json_schema: Some(split_schema()),
        model,
        max_turns: None,
    }
}

/// The segment ids `new_lines` names in `rows` (the rows the request numbered): numbers out
/// of range dropped, duplicates once. `Err(NOT_SEPARATED)` when that leaves nothing, or
/// every line.
pub fn lines_to_segments(rows: &[TranscriptRow], new_lines: &[i64]) -> Result<Vec<i64>, String> {
    let numbered = numbered_rows(rows);
    let mut ids: Vec<i64> = new_lines
        .iter()
        .filter(|n| **n >= 1 && (**n as usize) <= numbered.len())
        .map(|n| numbered[*n as usize - 1].id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() || ids.len() >= numbered.len() {
        return Err(NOT_SEPARATED.to_owned());
    }
    Ok(ids)
}

/// What a split did.
#[derive(Clone, Debug, PartialEq)]
pub struct Split {
    /// The original, rewritten.
    pub original: Episode,
    /// The note split off.
    pub new: Episode,
    /// Segments moved to it.
    pub moved: usize,
}

impl Split {
    /// The pop-up text: "Split off “<new title>”".
    pub fn confirmation(&self) -> String {
        format!("Split off “{}”", self.new.title)
    }
}

/// A stored note's content from the model's: title (else "Untitled"), summary, items resolved
/// against the note's own segments (`at` → `segment_id`) and `home`, topics.
fn content_of(
    tree: &[Node],
    note: &NoteContent,
    rows: &[TranscriptRow],
    home: Option<i64>,
    previous: &[oats_core::ActionItem],
    low_signal: bool,
    model: &str,
    cost_usd: Option<f64>,
) -> EpisodeContent {
    let chunk = Chunk {
        started_at: rows.first().map(|r| r.started_at).unwrap_or(0),
        ended_at: rows.iter().map(|r| r.ended_at).max().unwrap_or(0),
        segments: rows.to_vec(),
    };
    let title = note.title.trim();
    EpisodeContent {
        title: if title.is_empty() {
            "Untitled".to_owned()
        } else {
            title.chars().take(200).collect()
        },
        summary: note.summary.trim().to_owned(),
        node_id: home,
        decisions: decisions_of(tree, &chunk, &note.decisions, home),
        action_items: action_items_of(tree, &chunk, &note.action_items, home, Some(previous)),
        topics: note
            .topics
            .iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect(),
        low_signal,
        model: model.to_owned(),
        cost_usd,
    }
}

/// Apply a split in ONE savepoint: re-read the note and its segments (the segments `moving`
/// still in it — new ones may have been revised in since the request; ids no longer there are
/// dropped, and with nothing left, or nothing left to keep, it fails with [`NOT_SEPARATED`]),
/// insert the new note (conversation, its lines' span, `new.node_id` when it is an existing,
/// unarchived project / topic, else the original's node), move the segments, rewrite the
/// original with `keep` (its edits replaced: `user_edited_at` cleared, revision bumped), each
/// note's items resolved against its own segments (a tick kept on an unchanged item),
/// recompute both spans, and record an `episode_split` change (`by: user`; not undoable).
/// Events stay on the original. `Ok(Err(why))` = refused, nothing written.
#[allow(clippy::too_many_arguments)]
pub fn apply_split(
    store: &Store,
    tree: &[Node],
    episode_id: i64,
    result: &SplitResult,
    moving: &[i64],
    model: &str,
    cost_usd: Option<f64>,
    run_id: Option<i64>,
    now: i64,
) -> Result<Split, String> {
    let r = store.with_savepoint(|s| -> Result<Result<Split, String>, StoreError> {
        let Some(orig) = s.episode(episode_id)? else {
            return Ok(Err("the note no longer exists".to_owned()));
        };
        if orig.is_media() {
            return Ok(Err("media notes can't be split".to_owned()));
        }
        let rows = s.segments_of_episode(episode_id)?;
        let (going, staying): (Vec<TranscriptRow>, Vec<TranscriptRow>) =
            rows.into_iter().partition(|r| moving.contains(&r.id));
        if going.is_empty() || staying.is_empty() {
            return Ok(Err(NOT_SEPARATED.to_owned()));
        }
        let home = orig.node_id;
        let new_home = match result.new.node_id {
            Some(id) => s
                .node(id)?
                .filter(|n| !n.archived && n.kind != NodeKind::Category)
                .map(|n| n.id),
            None => None,
        }
        .or(home);
        let new_content = content_of(
            tree,
            &result.new,
            &going,
            new_home,
            &orig.action_items,
            false,
            model,
            cost_usd,
        );
        let start = going
            .iter()
            .map(|r| r.started_at)
            .min()
            .unwrap_or(orig.started_at);
        let end = going
            .iter()
            .map(|r| r.ended_at)
            .max()
            .unwrap_or(orig.ended_at);
        let new = s.insert_episode(start, end, &new_content, now)?;
        let ids: Vec<i64> = going.iter().map(|r| r.id).collect();
        let moved = s.move_segments_to_episode(&ids, orig.id, new.id)?;
        // the cost is the new note's; the original only gets its text rewritten
        let keep = content_of(
            tree,
            &result.keep,
            &staying,
            home,
            &orig.action_items,
            orig.low_signal,
            model,
            None,
        );
        s.revise_episode(orig.id, orig.started_at, orig.ended_at, &keep, now)?;
        s.clear_user_edit(orig.id)?;
        s.recompute_episode_span(orig.id)?;
        s.recompute_episode_span(new.id)?;
        s.record_change(
            run_id,
            "episode_split",
            &format!("Split “{}” off “{}”", new.title, orig.title),
            &json!({
                "id": orig.id,
                "title": orig.title,
                "summary": orig.summary,
                "decisions": orig.decisions,
                "action_items": orig.action_items,
                "topics": orig.topics,
                "node_id": orig.node_id,
                "started_at": orig.started_at,
                "ended_at": orig.ended_at,
                "user_edited_at": orig.user_edited_at,
                "moved_segment_ids": ids,
                "by": "user",
            }),
            &json!({ "episode_id": orig.id, "new_episode_id": new.id }),
            now,
        )?;
        let (Some(original), Some(new)) = (s.episode(orig.id)?, s.episode(new.id)?) else {
            return Err(StoreError::Io(std::io::Error::other(
                "a note vanished while being split",
            )));
        };
        Ok(Ok(Split {
            original,
            new,
            moved,
        }))
    });
    match r {
        Ok(inner) => inner,
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oats_core::ActionItem;

    fn seeded() -> (Store, Episode, Vec<TranscriptRow>) {
        let s = Store::open_in_memory().unwrap();
        let sid = s.begin_session(0, "t").unwrap();
        let t = 1_793_106_300_000;
        let mut ids = Vec::new();
        for i in 0..4 {
            let r = s
                .insert_segment(
                    sid,
                    t + i * 60_000,
                    t + i * 60_000 + 5_000,
                    &format!("line {i}"),
                    None,
                )
                .unwrap();
            ids.push(r.id);
        }
        let e = s
            .insert_episode(
                t,
                t + 185_000,
                &EpisodeContent {
                    title: "Shopping and to-dos".into(),
                    action_items: vec![ActionItem {
                        text: "book the van".into(),
                        done_at: Some(42),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                t,
            )
            .unwrap();
        s.mark_segments_interpreted(&ids, e.id, t).unwrap();
        let rows = s.segments_of_episode(e.id).unwrap();
        (s, e, rows)
    }

    #[test]
    fn lines_out_of_range_are_dropped_and_none_or_all_is_refused() {
        let (_s, _e, rows) = seeded();
        assert_eq!(
            lines_to_segments(&rows, &[3, 4, 9, 0, -1, 4]).unwrap(),
            vec![rows[2].id, rows[3].id]
        );
        assert_eq!(lines_to_segments(&rows, &[]).unwrap_err(), NOT_SEPARATED);
        assert_eq!(
            lines_to_segments(&rows, &[7, 8]).unwrap_err(),
            NOT_SEPARATED
        );
        assert_eq!(
            lines_to_segments(&rows, &[1, 2, 3, 4]).unwrap_err(),
            NOT_SEPARATED
        );
    }

    #[test]
    fn a_split_whose_lines_left_the_note_is_refused_and_writes_nothing() {
        let (s, e, rows) = seeded();
        let result = SplitResult {
            new: NoteContent {
                title: "To-dos".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        // every line of the note: nothing would stay
        let all: Vec<i64> = rows.iter().map(|r| r.id).collect();
        let err = apply_split(&s, &[], e.id, &result, &all, "sonnet", None, None, 1).unwrap_err();
        assert_eq!(err, NOT_SEPARATED);
        // ids that are not in the note (any more)
        let err = apply_split(&s, &[], e.id, &result, &[999], "sonnet", None, None, 1).unwrap_err();
        assert_eq!(err, NOT_SEPARATED);
        assert_eq!(s.episodes(10).unwrap().len(), 1);
        assert!(s.changes(5).unwrap().is_empty());
        assert_eq!(s.segments_of_episode(e.id).unwrap().len(), 4);
    }

    #[test]
    fn the_request_numbers_the_lines_and_carries_the_note_and_the_text() {
        let (_s, e, rows) = seeded();
        let req = split_request(
            &[],
            &e,
            &rows,
            "the to-dos",
            &Default::default(),
            Some("sonnet".into()),
        );
        assert!(req.user_prompt.contains("[1] ["));
        assert!(req.user_prompt.contains("[4] ["));
        assert!(req.user_prompt.contains("Shopping and to-dos"));
        assert!(req.user_prompt.contains("- book the van"));
        assert!(req.user_prompt.contains("the to-dos"));
        assert!(!req.user_prompt.contains("{{"));
        assert!(req.system_prompt.contains("{{user_name_note}}"));
        let schema = req.json_schema.unwrap();
        assert_eq!(
            schema["required"],
            json!(["keep", "new", "new_lines"]),
            "strict schema"
        );
    }
}
