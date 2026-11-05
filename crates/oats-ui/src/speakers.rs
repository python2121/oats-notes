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
use oats_core::{
    AudioSourceKind, NameSource, Speaker, SpeakerTarget, TranscriptRow, speaker_label,
};

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
        tentative: false,
    }
}

/// A row's speaker tag.
#[derive(Clone, Debug, PartialEq)]
pub struct RowLabel {
    pub text: String,
    /// The voice's colour (`None` = the lane tag's accent).
    pub voice: Option<i64>,
    pub hover: String,
    /// A loose or ambiguous match that taught the voice nothing
    /// (`TranscriptRow::speaker_sure == Some(false)`): the name without its pill.
    pub tentative: bool,
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
            tentative: false,
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
            let tentative = row.speaker_sure == Some(false);
            let mut hover = if s.brain_named() {
                format!("{text} — named from the conversation — right-click to correct")
            } else if s.name.is_some() || s.is_me {
                format!("{text} ({lane})")
            } else {
                format!("{text} — an unnamed voice ({lane}); right-click ▸ Speaker to name it")
            };
            if tentative {
                hover.push_str(" — a tentative match (too loose to learn from); right-click ▸ This line was to confirm");
            }
            Some(RowLabel {
                text,
                voice: Some(s.id),
                hover,
                tentative,
            })
        }
        None => match row.source {
            AudioSourceKind::Mic => None,
            AudioSourceKind::System => Some(RowLabel {
                text: "them".to_owned(),
                voice: None,
                hover: "them — system audio (what the computer played)".to_owned(),
                tentative: false,
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
    // a tentative match keeps the name and colour but loses the pill
    if !label.tentative {
        p.rect_filled(rect, 3.0, colour.gamma_multiply(0.16));
    }
    p.galley(at + vec2(4.0, 1.0), galley, colour);
}

/// What an inline field under a row is for.
#[derive(Clone, Debug, PartialEq)]
pub enum DraftMode {
    /// "Name…" the voice `speaker`.
    Name { speaker: i64 },
    /// "Merge into…": the voice `speaker` into the one typed.
    Merge { speaker: i64 },
    /// "This line was ▸ Someone new…": these lines are a new person (an optional name).
    New { segments: Vec<i64> },
}

/// The open inline field (one at a time), kept in egui's memory.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerDraft {
    /// The row it is drawn under.
    pub segment: i64,
    pub mode: DraftMode,
    pub text: String,
    pub focus: bool,
}

fn open_draft(ui: &egui::Ui, d: SpeakerDraft) {
    ui.ctx().data_mut(|m| m.insert_temp(draft_id(), Some(d)));
}

/// At most this many voices are listed in "This line was ▸" (type to narrow the rest).
pub const PICKER_ROWS: usize = 8;

/// The voices "This line was ▸" lists for the search `text`: a case-insensitive substring of
/// the name or the label ("them 3", "voice 4", "me"; empty = all), the most recently heard
/// first. `source` is the clicked line's lane (an anonymous voice's label follows it).
pub fn picker_voices<'a>(
    map: &'a SpeakerMap,
    text: &str,
    source: AudioSourceKind,
) -> Vec<&'a Speaker> {
    let q = text.trim().to_lowercase();
    let mut v: Vec<&Speaker> = map
        .values()
        .filter(|s| {
            q.is_empty()
                || speaker_label(Some(s), source).to_lowercase().contains(&q)
                || s.name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(&q))
        })
        .collect();
    v.sort_by(|a, b| b.last_heard_at.cmp(&a.last_heard_at).then(a.id.cmp(&b.id)));
    v
}

/// The picker's search text, kept while the submenu is open (and the pass it was last drawn
/// in, so a newly opened submenu starts empty with the field focused).
#[derive(Clone, Debug, Default)]
struct PickerState {
    text: String,
    drawn_pass: u64,
    /// The list's height in rows, fixed when the submenu opened (the unfiltered count, at
    /// most [`PICKER_ROWS`]): a submenu that changes height is re-anchored by egui and jumps
    /// about under the pointer, and a list that reserves rows it never fills reads as blank
    /// space. While a filter leaves fewer rows, the rest stays empty.
    rows: usize,
    /// More voices than [`PICKER_ROWS`] when it opened: a "type to narrow" line is kept.
    more: bool,
}

fn picker_id() -> egui::Id {
    egui::Id::new("oats-speaker-picker")
}

/// The body of "This line was ▸" / "These N lines were ▸": a search field (focused on open),
/// the matching voices (most recently heard first, the lines' current voice greyed, at most
/// [`PICKER_ROWS`]), "Someone new…" and "Nobody". `segments` are the lines it applies to;
/// `current` their voice when they share one; `row` the clicked row (its lane, where the
/// "Someone new…" field opens).
fn voice_picker(
    ui: &mut egui::Ui,
    row: &TranscriptRow,
    segments: &[i64],
    current: Option<i64>,
    map: &SpeakerMap,
) -> Option<TranscriptAction> {
    let pass = ui.ctx().cumulative_pass_nr();
    let mut st: PickerState = ui
        .ctx()
        .data(|m| m.get_temp::<PickerState>(picker_id()))
        .unwrap_or_default();
    // not drawn in the previous pass: the submenu was just opened
    let opened = st.drawn_pass + 1 < pass;
    if opened {
        st.text.clear();
        let all = picker_voices(map, "", row.source).len();
        st.rows = all.min(PICKER_ROWS);
        st.more = all > PICKER_ROWS;
    }
    st.drawn_pass = pass;
    let mut action = None;
    let field = ui.add(
        egui::TextEdit::singleline(&mut st.text)
            .hint_text("name…")
            .desired_width(160.0),
    );
    // focused on open (and while nothing else has the focus: the submenu's first pass can be
    // a sizing pass that drops the request)
    let entered = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    if !entered && (opened || ui.ctx().memory(|m| m.focused().is_none())) {
        field.request_focus();
    }
    let voices = picker_voices(map, &st.text, row.source);
    let reassign = |target: SpeakerTarget| TranscriptAction::SpeakerReassign {
        segments: segments.to_vec(),
        target,
    };
    if entered {
        let choices: Vec<i64> = voices
            .iter()
            .map(|s| s.id)
            .filter(|id| Some(*id) != current)
            .collect();
        if let [only] = choices.as_slice() {
            action = Some(reassign(SpeakerTarget::Voice(*only)));
            ui.close();
        }
    }
    if field.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        ui.close();
    }
    // the list keeps the height it opened with (`PickerState::rows`)
    let mut row_h = None;
    for s in voices.iter().take(PICKER_ROWS) {
        let label = speaker_label(Some(s), row.source);
        let resp = ui.add_enabled(Some(s.id) != current, egui::Button::new(label));
        row_h = Some(resp.rect.height());
        if resp.clicked() {
            action = Some(reassign(SpeakerTarget::Voice(s.id)));
            ui.close();
        }
    }
    let shown = voices.len().min(PICKER_ROWS);
    if voices.is_empty() {
        ui.label(RichText::new("No voice matches").size(11.0).weak());
    }
    if shown < st.rows {
        let row_h = row_h.unwrap_or_else(|| {
            let text = ui.text_style_height(&egui::TextStyle::Button);
            (text + 2.0 * ui.spacing().button_padding.y).max(ui.spacing().interact_size.y)
        });
        let gap = ui.spacing().item_spacing.y;
        // the "No voice matches" line stands in for one row
        let blank = st.rows - shown - usize::from(voices.is_empty());
        ui.add_space(blank as f32 * (row_h + gap) - gap);
    }
    if st.more {
        let note = if voices.len() > PICKER_ROWS {
            format!("and {} more — type to narrow", voices.len() - PICKER_ROWS)
        } else {
            " ".to_owned() // the line is kept, so the height is
        };
        ui.label(RichText::new(note).size(11.0).weak());
    }
    ui.separator();
    if ui
        .button("Someone new…")
        .on_hover_text("A new voice from these lines (a name is optional)")
        .clicked()
    {
        open_draft(
            ui,
            SpeakerDraft {
                segment: row.id,
                mode: DraftMode::New {
                    segments: segments.to_vec(),
                },
                text: String::new(),
                focus: true,
            },
        );
        ui.close();
    }
    if ui
        .add_enabled(
            current.is_some() || segments.len() > 1,
            egui::Button::new("Nobody"),
        )
        .on_hover_text("Clear the voice: the line reads as its lane again (me / them)")
        .clicked()
    {
        action = Some(reassign(SpeakerTarget::None));
        ui.close();
    }
    ui.ctx().data_mut(|m| m.insert_temp(picker_id(), st));
    action
}

/// The FIRST entry of a row's context menu (rows that aren't media): "This line was ▸" (the
/// voice picker for the row), or, with a multi-row `selection` that contains the row, "These
/// N lines were ▸" for the whole selection. `selection_voice`: the selected lines' voice when
/// they all share one.
pub fn line_was_menu(
    ui: &mut egui::Ui,
    row: &TranscriptRow,
    map: &SpeakerMap,
    selection: &[i64],
    selection_voice: Option<i64>,
) -> Option<TranscriptAction> {
    if row_label(row, map).is_some_and(|l| l.text == "media") {
        return None;
    }
    let mut action = None;
    if selection.len() > 1 && selection.contains(&row.id) {
        ui.menu_button(format!("These {} lines were", selection.len()), |ui| {
            action = voice_picker(ui, row, selection, selection_voice, map);
        });
    } else {
        ui.menu_button("This line was", |ui| {
            action = voice_picker(ui, row, &[row.id], row.speaker_id, map);
        });
    }
    action
}

/// The voice's own entry of a row's context menu, "Speaker: <label> ▸" (Name… / This is me /
/// Merge into… / Forget), for a row with a known voice.
pub fn speaker_menu(
    ui: &mut egui::Ui,
    row: &TranscriptRow,
    map: &SpeakerMap,
) -> Option<TranscriptAction> {
    if row_label(row, map).is_some_and(|l| l.text == "media") {
        return None;
    }
    let voice = row.speaker_id.and_then(|id| map.get(&id));
    let mut action = None;
    let title = format!("Speaker: {}", speaker_label(voice, row.source));
    ui.menu_button(title, |ui| {
        if let Some(s) = voice {
            let id = s.id;
            if ui.button("Name…").clicked() {
                open_draft(
                    ui,
                    SpeakerDraft {
                        segment: row.id,
                        mode: DraftMode::Name { speaker: id },
                        text: s.name.clone().unwrap_or_default(),
                        focus: true,
                    },
                );
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
                .on_hover_text(
                    "Same person as another voice: its lines and voiceprint join that one",
                )
                .clicked()
            {
                open_draft(
                    ui,
                    SpeakerDraft {
                        segment: row.id,
                        mode: DraftMode::Merge { speaker: id },
                        text: String::new(),
                        focus: true,
                    },
                );
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
        } else {
            ui.label(
                RichText::new("No voice on this line — use “This line was”")
                    .size(11.0)
                    .weak(),
            );
        }
    });
    action
}

/// The inline Name… / Merge into… / Someone new… field under row `segment`, when it is open
/// there.
pub fn speaker_field(ui: &mut egui::Ui, t: &Tokens, segment: i64) -> Option<TranscriptAction> {
    let mut draft: SpeakerDraft = ui
        .ctx()
        .data(|m| m.get_temp::<Option<SpeakerDraft>>(draft_id()))
        .flatten()
        .filter(|d| d.segment == segment)?;
    let mut out = None;
    let mut close = false;
    let (label, hint) = match &draft.mode {
        DraftMode::Name { .. } => ("Name", "their name (empty clears it)"),
        DraftMode::Merge { .. } => ("Merge into", "another voice: a name, “them 3”, “voice 3”…"),
        DraftMode::New { .. } => ("New voice", "their name (optional)"),
    };
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label(RichText::new(label).color(t.text_secondary).size(11.5));
        let r = ui.add(
            egui::TextEdit::singleline(&mut draft.text)
                .desired_width((ui.available_width() - 12.0).clamp(80.0, 280.0))
                .hint_text(hint)
                .font(t.font(12.5)),
        );
        if draft.focus {
            r.request_focus();
            draft.focus = false;
        }
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let text = draft.text.trim().to_owned();
            out = Some(match &draft.mode {
                DraftMode::Merge { speaker } => TranscriptAction::SpeakerMerge {
                    speaker: *speaker,
                    into: text,
                },
                DraftMode::Name { speaker } => TranscriptAction::SpeakerName {
                    speaker: *speaker,
                    name: text,
                },
                DraftMode::New { segments } => TranscriptAction::SpeakerReassign {
                    segments: segments.clone(),
                    target: SpeakerTarget::New {
                        name: Some(text).filter(|t| !t.is_empty()),
                    },
                },
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
            speaker_sure: None,
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
    fn the_picker_filters_by_name_and_label_newest_first() {
        let mut m = map();
        for (id, at) in [(1, 10), (2, 40), (3, 30), (4, 20)] {
            if let Some(s) = m.get_mut(&id) {
                s.last_heard_at = at;
            }
        }
        let ids = |text: &str, src| {
            picker_voices(&m, text, src)
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("", AudioSourceKind::System), vec![2, 3, 4, 1]);
        assert_eq!(ids("pri", AudioSourceKind::System), vec![2]);
        assert_eq!(ids("PA", AudioSourceKind::System), vec![4]);
        assert_eq!(ids("them 3", AudioSourceKind::System), vec![3]);
        assert_eq!(ids("voice", AudioSourceKind::Mic), vec![3]);
        assert_eq!(ids("me", AudioSourceKind::Mic), vec![1]);
        assert!(ids("zz", AudioSourceKind::Mic).is_empty());
    }

    #[test]
    fn colours_are_stable_per_voice() {
        let t = Tokens::dark();
        assert_eq!(t.speaker_colour(7), t.speaker_colour(7));
        let distinct: std::collections::HashSet<_> = (1..=6).map(|i| t.speaker_colour(i)).collect();
        assert!(distinct.len() >= 4, "{distinct:?}");
    }
}
