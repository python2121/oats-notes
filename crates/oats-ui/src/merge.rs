//! Drag a note onto another note to merge them — the manual twin of the review tier's
//! `merge_episodes`. A note card's title, a project page's note row and a sidebar note row are
//! drag sources (payload [`DraggedNote`]); the same places are drop zones that draw an accent
//! outline while a note hovers over them. A drop never merges at once: the app asks first
//! (a merge is irreversible), then [`apply_merge`] folds the DRAGGED note into the target and
//! the app queues a re-interpretation of the target so its text covers both.

use egui::{Rect, Response};
use oats_core::Store;

use crate::theme::Tokens;

/// The drag-and-drop payload: the note (episode id) being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DraggedNote(pub i64);

/// A merge the user confirmed: `dragged` goes INTO `into`; `reinterpret`: queue a
/// re-interpretation of `into` afterwards (declined only to keep hand-edited text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeAction {
    pub dragged: i64,
    pub into: i64,
    pub reinterpret: bool,
}

/// A drop waiting for the user's answer (shown as a small Yes / No pop-up at the drop point).
#[derive(Clone, Debug, PartialEq)]
pub struct MergeConfirm {
    pub dragged: i64,
    pub into: i64,
    pub dragged_title: String,
    pub into_title: String,
    /// Either note was edited by hand: the second question ("Replace your edits?") follows.
    pub edited: bool,
    /// The first answer was Yes and the edits question is showing.
    pub asking_edits: bool,
    /// Where the note was dropped (the pop-up's anchor).
    pub at: egui::Pos2,
}

/// Fold `a.dragged` into `a.into` in one savepoint: its segments and events re-pointed to the
/// target, the target's span extended, the dragged row deleted, an `episode_merge` change
/// recorded (`by: user`; merges are recorded but not undoable). Refuses a note onto itself, a
/// missing note, and media onto conversation (or the reverse). Returns the target's title.
pub fn apply_merge(store: &Store, a: &MergeAction, now_ms: i64) -> Result<String, String> {
    if a.dragged == a.into {
        return Err("A note can't be merged into itself".to_owned());
    }
    let (Some(from), Some(into)) = (
        store.episode(a.dragged).map_err(|e| e.to_string())?,
        store.episode(a.into).map_err(|e| e.to_string())?,
    ) else {
        return Err("That note is gone".to_owned());
    };
    if from.is_media() != into.is_media() {
        return Err("Media and conversations aren't merged".to_owned());
    }
    store
        .with_savepoint(|s| {
            s.merge_episodes(from.id, into.id)?;
            s.record_change(
                None,
                "episode_merge",
                &format!(
                    "Merged note '{}' into '{}' (by you)",
                    from.title, into.title
                ),
                &serde_json::json!({
                    "from": from.id,
                    "into": into.id,
                    "from_title": from.title,
                    "by": "user",
                }),
                &serde_json::json!({"into": into.id}),
                now_ms,
            )?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    Ok(into.title)
}

/// Make `resp` (sensing drags) a drag source for note `id`: the payload is set when a drag
/// starts, and the title follows the pointer while it lasts.
pub fn drag_source(ui: &egui::Ui, t: &Tokens, resp: &Response, id: i64, title: &str) {
    resp.dnd_set_drag_payload(DraggedNote(id));
    if resp.dragged()
        && let Some(pos) = ui.ctx().pointer_latest_pos()
    {
        egui::Area::new(egui::Id::new(("note-drag", id)))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .fixed_pos(pos + egui::vec2(12.0, 8.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(format!("Merge “{title}” into…"))
                            .size(11.5)
                            .color(t.text_secondary),
                    );
                });
            });
    }
}

/// `rect` as a drop zone for notes other than `target`: an accent outline while one hovers
/// over it; the dragged note's id when it is released here this frame.
pub fn drop_zone(ui: &egui::Ui, t: &Tokens, rect: Rect, target: i64) -> Option<i64> {
    let payload = egui::DragAndDrop::payload::<DraggedNote>(ui.ctx())?;
    if payload.0 == target || !ui.rect_contains_pointer(rect) {
        return None;
    }
    ui.painter().rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.5, t.accent),
        egui::StrokeKind::Inside,
    );
    if ui.input(|i| i.pointer.any_released()) {
        egui::DragAndDrop::clear_payload(ui.ctx());
        return Some(payload.0);
    }
    None
}

/// The confirmation pop-up at the drop point: "Merge “A” into “B”? Undo not available." with
/// Yes / No; for a hand-edited note then "Replace your edits?" (Yes: re-interpret the merged
/// note; Keep my text: merge without; Cancel). Escape cancels. Returns the confirmed merge;
/// clears `confirm` on any answer.
pub fn confirm_popup(
    ctx: &egui::Context,
    t: &Tokens,
    confirm: &mut Option<MergeConfirm>,
) -> Option<MergeAction> {
    let c = confirm.as_mut()?;
    let mut out = None;
    let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    egui::Area::new(egui::Id::new("merge-confirm"))
        .order(egui::Order::Foreground)
        .fixed_pos(c.at)
        .constrain(true)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(320.0);
                if !c.asking_edits {
                    ui.label(
                        egui::RichText::new(format!(
                            "Merge “{}” into “{}”?",
                            c.dragged_title, c.into_title
                        ))
                        .size(12.0)
                        .color(t.text),
                    );
                    ui.label(
                        egui::RichText::new("Undo not available.")
                            .size(11.0)
                            .color(t.text_tertiary),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Yes").clicked() {
                            if c.edited {
                                c.asking_edits = true;
                            } else {
                                out = Some(MergeAction {
                                    dragged: c.dragged,
                                    into: c.into,
                                    reinterpret: true,
                                });
                            }
                        }
                        if ui.button("No").clicked() {
                            close = true;
                        }
                    });
                } else {
                    ui.label(
                        egui::RichText::new("Replace your edits?")
                            .size(12.0)
                            .color(t.text),
                    );
                    ui.label(
                        egui::RichText::new(
                            "You edited one of these notes. Yes re-interprets the merged note \
                             (your text is replaced); Keep my text merges without.",
                        )
                        .size(11.0)
                        .color(t.text_tertiary),
                    );
                    ui.horizontal(|ui| {
                        for (label, reinterpret) in [("Yes", true), ("Keep my text", false)] {
                            if ui.button(label).clicked() {
                                out = Some(MergeAction {
                                    dragged: c.dragged,
                                    into: c.into,
                                    reinterpret,
                                });
                            }
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
            });
        });
    if out.is_some() || close {
        *confirm = None;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oats_core::EpisodeContent;

    fn store() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "oats-ui-merge-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir.join("oats.sqlite")).unwrap(), dir)
    }

    fn episode(s: &Store, at: i64, title: &str) -> (i64, i64) {
        let sid = s.begin_session(at, "t").unwrap();
        let seg = s
            .insert_segment(sid, at, at + 5_000, "words said", None)
            .unwrap();
        let e = s
            .insert_episode(
                at,
                at + 5_000,
                &EpisodeContent {
                    title: title.into(),
                    ..Default::default()
                },
                at,
            )
            .unwrap();
        s.mark_segments_interpreted(&[seg.id], e.id, at).unwrap();
        (e.id, seg.id)
    }

    #[test]
    fn a_merge_folds_the_dragged_note_into_the_target_and_records_it() {
        let (s, dir) = store();
        let (a, seg_a) = episode(&s, 1_000_000, "Standup part 1");
        let (b, seg_b) = episode(&s, 2_000_000, "Standup part 2");
        let merge = MergeAction {
            dragged: b,
            into: a,
            reinterpret: true,
        };
        assert_eq!(
            apply_merge(&s, &merge, 3_000_000).unwrap(),
            "Standup part 1"
        );
        assert!(s.episode(b).unwrap().is_none(), "the dragged note is gone");
        let target = s.episode(a).unwrap().unwrap();
        assert_eq!(target.ended_at, 2_005_000, "span extended");
        let segs: Vec<i64> = s
            .segments_of_episode(a)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(segs, vec![seg_a, seg_b]);
        let change = s
            .changes(5)
            .unwrap()
            .into_iter()
            .find(|c| c.kind == "episode_merge")
            .unwrap();
        assert!(change.description.contains("Standup part 2"));
        // onto itself, or a note that is gone: refused, nothing changes
        assert!(
            apply_merge(
                &s,
                &MergeAction {
                    dragged: a,
                    into: a,
                    reinterpret: true
                },
                0
            )
            .is_err()
        );
        assert!(apply_merge(&s, &merge, 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn media_and_conversation_are_never_merged() {
        let (s, dir) = store();
        let (a, _) = episode(&s, 1_000_000, "A call");
        let (b, _) = episode(&s, 2_000_000, "A video");
        s.set_episode_kind(b, "media").unwrap();
        let r = apply_merge(
            &s,
            &MergeAction {
                dragged: b,
                into: a,
                reinterpret: true,
            },
            0,
        );
        assert!(r.is_err());
        assert!(s.episode(b).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
