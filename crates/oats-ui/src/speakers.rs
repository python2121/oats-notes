//! Speaker labels in the transcript: who said a line ("Priya", "them 2", "voice 3", "me"),
//! the voice's stable colour, the row's "Speaker ▸" menu and its inline field (Name… / Merge
//! into…).
//!
//! The app loads every stored voice ONCE into a map (`Store::speakers`) and publishes it on the
//! egui context ([`publish`]) whenever voices change (a `SpeakersChanged` pipeline event, a
//! segment naming an unknown voice, a brain change, the user's own edits); rows resolve their
//! label from it ([`row_label`]) — a hash lookup per row, never a query.

use std::sync::Arc;

use egui::{Color32, Rect, RichText, pos2, vec2};
use oats_core::{AudioSourceKind, NameSource, Speaker, TranscriptRow, speaker_label};

use crate::theme::Tokens;
use crate::transcript::TranscriptAction;

pub use oats_core::speakers::SpeakerMap;

fn map_id() -> egui::Id {
    egui::Id::new("oats-speaker-map")
}

fn draft_id() -> egui::Id {
    egui::Id::new("oats-speaker-draft")
}

/// Make `map` the labels every transcript row reads.
pub fn publish(ctx: &egui::Context, map: Arc<SpeakerMap>) {
    ctx.data_mut(|d| d.insert_temp(map_id(), map));
}

/// The published voices (empty before the first [`publish`]).
pub fn map(ctx: &egui::Context) -> Arc<SpeakerMap> {
    ctx.data(|d| d.get_temp::<Arc<SpeakerMap>>(map_id()))
        .unwrap_or_default()
}

/// The tag of the user's own rows (the user's voice's colour when one is known).
pub fn me_label(map: &SpeakerMap) -> RowLabel {
    RowLabel {
        text: "me".to_owned(),
        voice: map.values().find(|s| s.is_me).map(|s| s.id),
        hover: "me (microphone)".to_owned(),
    }
}

/// A row's speaker tag.
#[derive(Clone, Debug, PartialEq)]
pub struct RowLabel {
    pub text: String,
    /// The voice's colour (`None` = the lane tag's accent).
    pub voice: Option<i64>,
    pub hover: String,
}

/// The tag under a row's time: `None` for the user's own lines (microphone, no voice or the
/// "me" voice: they look as they always did). Media rows keep "media"; a row with a known
/// voice gets its label (see [`oats_core::speaker_label`]); else the lane's "them".
pub fn row_label(row: &TranscriptRow, map: &SpeakerMap) -> Option<RowLabel> {
    if let Some(tag) = crate::transcript::source_label(row)
        && tag == "media"
    {
        return Some(RowLabel {
            text: tag.to_owned(),
            voice: None,
            hover: "media — a video or music on system audio".to_owned(),
        });
    }
    let speaker = row.speaker_id.and_then(|id| map.get(&id));
    match speaker {
        Some(s) if s.is_me && row.source == AudioSourceKind::Mic => None,
        Some(s) => {
            let text = speaker_label(Some(s), row.source);
            let lane = match row.source {
                AudioSourceKind::Mic => "microphone",
                AudioSourceKind::System => "system audio",
            };
            let hover = if s.brain_named() {
                format!("{text} — named from the conversation — right-click to correct")
            } else if s.name.is_some() || s.is_me {
                format!("{text} ({lane})")
            } else {
                format!("{text} — an unnamed voice ({lane}); right-click ▸ Speaker to name it")
            };
            Some(RowLabel {
                text,
                voice: Some(s.id),
                hover,
            })
        }
        None => match row.source {
            AudioSourceKind::Mic => None,
            AudioSourceKind::System => Some(RowLabel {
                text: "them".to_owned(),
                voice: None,
                hover: "them — system audio (what the computer played)".to_owned(),
            }),
        },
    }
}

/// The colour of a tag.
pub fn tag_colour(t: &Tokens, voice: Option<i64>) -> Color32 {
    voice.map_or(t.accent, |id| t.speaker_colour(id))
}

/// The small tag, top-left at `at`, cut with … to `max_w`.
pub fn paint_tag(ui: &egui::Ui, t: &Tokens, at: egui::Pos2, label: &RowLabel, max_w: f32) {
    let colour = tag_colour(t, label.voice);
    let p = ui.painter();
    let mut text = label.text.clone();
    let mut galley = p.layout_no_wrap(text.clone(), t.mono(8.5), colour);
    while galley.size().x + 8.0 > max_w && text.chars().count() > 2 {
        text.pop();
        let cut = format!("{}…", text.trim_end());
        galley = p.layout_no_wrap(cut, t.mono(8.5), colour);
    }
    let rect = Rect::from_min_size(at, galley.size() + vec2(8.0, 2.0));
    p.rect_filled(rect, 3.0, colour.gamma_multiply(0.16));
    p.galley(at + vec2(4.0, 1.0), galley, colour);
}

/// The open inline field (one at a time), kept in egui's memory.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerDraft {
    pub segment: i64,
    pub speaker: i64,
    pub merge: bool,
    pub text: String,
    pub focus: bool,
}

/// The "Speaker ▸" submenu of a row's context menu (rows with a voice only).
pub fn speaker_menu(
    ui: &mut egui::Ui,
    row: &TranscriptRow,
    map: &SpeakerMap,
) -> Option<TranscriptAction> {
    let id = row.speaker_id?;
    let s = map.get(&id)?;
    let mut action = None;
    let title = format!("Speaker: {}", speaker_label(Some(s), row.source));
    ui.menu_button(title, |ui| {
        let open = |merge: bool| SpeakerDraft {
            segment: row.id,
            speaker: id,
            merge,
            text: if merge {
                String::new()
            } else {
                s.name.clone().unwrap_or_default()
            },
            focus: true,
        };
        if ui.button("Name…").clicked() {
            let d = open(false);
            ui.ctx().data_mut(|m| m.insert_temp(draft_id(), Some(d)));
            ui.close();
        }
        if ui
            .add_enabled(!s.is_me, egui::Button::new("This is me"))
            .clicked()
        {
            action = Some(TranscriptAction::SpeakerMe { speaker: id });
            ui.close();
        }
        if ui
            .add_enabled(map.len() > 1, egui::Button::new("Merge into…"))
            .on_hover_text("Same person as another voice: its lines and voiceprint join that one")
            .clicked()
        {
            let d = open(true);
            ui.ctx().data_mut(|m| m.insert_temp(draft_id(), Some(d)));
            ui.close();
        }
        ui.separator();
        if ui
            .button("Forget this voice")
            .on_hover_text("Deletes the voiceprint; its lines lose the label")
            .clicked()
        {
            action = Some(TranscriptAction::SpeakerForget { speaker: id });
            ui.close();
        }
    });
    action
}

/// The inline Name… / Merge into… field under row `segment`, when it is open there.
pub fn speaker_field(ui: &mut egui::Ui, t: &Tokens, segment: i64) -> Option<TranscriptAction> {
    let mut draft: SpeakerDraft = ui
        .ctx()
        .data(|m| m.get_temp::<Option<SpeakerDraft>>(draft_id()))
        .flatten()
        .filter(|d| d.segment == segment)?;
    let mut out = None;
    let mut close = false;
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label(
            RichText::new(if draft.merge { "Merge into" } else { "Name" })
                .color(t.text_secondary)
                .size(11.5),
        );
        let r = ui.add(
            egui::TextEdit::singleline(&mut draft.text)
                .desired_width((ui.available_width() - 12.0).clamp(80.0, 280.0))
                .hint_text(if draft.merge {
                    "another voice: a name, “them 3”, “voice 3”…"
                } else {
                    "their name (empty clears it)"
                })
                .font(t.font(12.5)),
        );
        if draft.focus {
            r.request_focus();
            draft.focus = false;
        }
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let text = draft.text.trim().to_owned();
            out = Some(if draft.merge {
                TranscriptAction::SpeakerMerge {
                    speaker: draft.speaker,
                    into: text,
                }
            } else {
                TranscriptAction::SpeakerName {
                    speaker: draft.speaker,
                    name: text,
                }
            });
            close = true;
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
    });
    let keep = (!close).then_some(draft);
    ui.ctx().data_mut(|m| m.insert_temp(draft_id(), keep));
    out
}

/// Resolve a typed voice ("Priya", "them 3", "voice 3", "#3", "3") among `map`, excluding
/// `except`: a number names the voice with that id, else a case-insensitive exact name, else
/// a unique name prefix.
pub fn resolve_voice(text: &str, map: &SpeakerMap, except: i64) -> Option<i64> {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    let digits = t
        .trim_start_matches("them")
        .trim_start_matches("voice")
        .trim()
        .trim_start_matches('#');
    if let Ok(id) = digits.parse::<i64>() {
        return map
            .contains_key(&id)
            .then_some(id)
            .filter(|&id| id != except);
    }
    if t == "me" {
        return map
            .values()
            .find(|s| s.is_me && s.id != except)
            .map(|s| s.id);
    }
    let named = |exact: bool| -> Vec<i64> {
        map.values()
            .filter(|s| s.id != except)
            .filter(|s| {
                s.name.as_deref().is_some_and(|n| {
                    let n = n.trim().to_lowercase();
                    if exact { n == t } else { n.starts_with(&t) }
                })
            })
            .map(|s| s.id)
            .collect()
    };
    let exact = named(true);
    if exact.len() == 1 {
        return exact.first().copied();
    }
    let prefix = named(false);
    (prefix.len() == 1).then(|| prefix[0])
}

/// The Preferences "Speakers" table's order: the user's voice first, then the most recently
/// heard.
pub fn table_order(map: &SpeakerMap) -> Vec<Speaker> {
    let mut v: Vec<Speaker> = map.values().cloned().collect();
    v.sort_by(|a, b| {
        b.is_me
            .cmp(&a.is_me)
            .then(b.last_heard_at.cmp(&a.last_heard_at))
            .then(a.id.cmp(&b.id))
    });
    v
}

/// "Named by you" / "named from the conversation" / "".
pub fn name_note(s: &Speaker) -> &'static str {
    match (s.name.as_ref(), s.name_source) {
        (Some(_), Some(NameSource::Brain)) => "named from the conversation",
        (Some(_), _) => "named by you",
        _ => "",
    }
}

/// Where a tag goes under a row's time column (`time_rect` top-left), and its width cap.
pub fn tag_pos(time_rect: Rect) -> (egui::Pos2, f32) {
    (
        pos2(time_rect.left() + 4.0, time_rect.top() + 18.0),
        time_rect.width() - 2.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(id: i64, name: Option<&str>, me: bool) -> Speaker {
        Speaker {
            id,
            name: name.map(str::to_owned),
            name_source: name.map(|_| NameSource::User),
            is_me: me,
            ..Speaker::default()
        }
    }

    fn map() -> SpeakerMap {
        [
            sp(1, None, true),
            sp(2, Some("Priya"), false),
            sp(3, None, false),
            sp(4, Some("Pat"), false),
        ]
        .into_iter()
        .map(|s| (s.id, s))
        .collect()
    }

    #[test]
    fn rows_are_labelled_from_the_map() {
        let m = map();
        let row = |source, speaker_id| TranscriptRow {
            source,
            speaker_id,
            ..TranscriptRow::default()
        };
        assert_eq!(row_label(&row(AudioSourceKind::Mic, None), &m), None);
        assert_eq!(row_label(&row(AudioSourceKind::Mic, Some(1)), &m), None);
        let l = |r| row_label(&r, &m).map(|l| l.text);
        assert_eq!(
            l(row(AudioSourceKind::System, None)).as_deref(),
            Some("them")
        );
        assert_eq!(
            l(row(AudioSourceKind::System, Some(2))).as_deref(),
            Some("Priya")
        );
        assert_eq!(
            l(row(AudioSourceKind::System, Some(3))).as_deref(),
            Some("them 3")
        );
        assert_eq!(
            l(row(AudioSourceKind::Mic, Some(3))).as_deref(),
            Some("voice 3")
        );
        // a forgotten voice reads as its lane
        assert_eq!(
            l(row(AudioSourceKind::System, Some(99))).as_deref(),
            Some("them")
        );
    }

    #[test]
    fn typed_voices_resolve_locally() {
        let m = map();
        assert_eq!(resolve_voice("them 3", &m, 2), Some(3));
        assert_eq!(resolve_voice("voice 3", &m, 2), Some(3));
        assert_eq!(resolve_voice("#2", &m, 3), Some(2));
        assert_eq!(resolve_voice("priya", &m, 3), Some(2));
        assert_eq!(resolve_voice("Pri", &m, 3), Some(2));
        assert_eq!(resolve_voice("P", &m, 3), None); // Priya or Pat
        assert_eq!(resolve_voice("me", &m, 3), Some(1));
        assert_eq!(resolve_voice("3", &m, 3), None); // itself
        assert_eq!(resolve_voice("nobody", &m, 3), None);
    }

    #[test]
    fn colours_are_stable_per_voice() {
        let t = Tokens::dark();
        assert_eq!(t.speaker_colour(7), t.speaker_colour(7));
        let distinct: std::collections::HashSet<_> = (1..=6).map(|i| t.speaker_colour(i)).collect();
        assert!(distinct.len() >= 4, "{distinct:?}");
    }
}
