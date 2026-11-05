//! "Move to…": the small inline field under a note's title (or a transcript row) where the
//! user types a project or topic name, or describes a new one. Enter submits, Escape cancels.
//! The app resolves the text (`oats_brain::resolve_local` first, the model when that cannot
//! settle it); while the model works the field is disabled with a spinner.

use egui::RichText;
use oats_brain::MoveTarget;

use crate::theme::Tokens;

pub const PLACEHOLDER: &str = "project or topic name, or describe a new one…";

/// The open field: one at a time, for a note or for a transcript segment.
#[derive(Clone, Debug, PartialEq)]
pub struct MoveDraft {
    pub target: MoveTarget,
    pub text: String,
    /// Take the keyboard focus on the next frame.
    pub focus: bool,
    /// Sent to the brain and not settled yet: the field is disabled with a spinner.
    pub sent: bool,
    /// The node the item was filed under when the field opened (a changed home = it moved).
    pub from: Option<i64>,
    /// Opened from the sidebar: the field shows under the sidebar row, not on the card.
    pub sidebar: bool,
    /// The field has been drawn at least once.
    pub shown: bool,
    /// The field was drawn this frame (reset by [`drop_stale`]).
    pub seen: bool,
}

impl MoveDraft {
    pub fn new(target: MoveTarget, from: Option<i64>) -> Self {
        MoveDraft {
            target,
            text: String::new(),
            focus: true,
            sent: false,
            from,
            sidebar: false,
            shown: false,
            seen: false,
        }
    }

    /// The same, opened from a sidebar row.
    pub fn in_sidebar(mut self) -> Self {
        self.sidebar = true;
        self
    }
}

/// The field for `target`, when the open draft is its own. Returns the submitted text on
/// Enter (the draft stays open: the app closes it on a local move, or marks it `sent`).
pub fn move_field(
    ui: &mut egui::Ui,
    t: &Tokens,
    draft: &mut Option<MoveDraft>,
    target: MoveTarget,
) -> Option<String> {
    move_field_in(ui, t, draft, target, false)
}

/// Once per frame, before anything is drawn: a card's (or transcript row's) "Move to" field
/// that was on screen and no longer is — the user navigated away, collapsed the row, the card
/// is gone — is dropped, so it is not still waiting when they come back (after a failed move
/// the field reopens with its text; it must not outlive its card). A field the brain is still
/// answering (`sent`) and a sidebar row's field stay.
pub fn drop_stale(draft: &mut Option<MoveDraft>) {
    if let Some(d) = draft {
        if d.shown && !d.seen && !d.sent && !d.sidebar {
            *draft = None;
        } else {
            d.seen = false;
        }
    }
}

/// [`move_field`] for the card (`sidebar = false`) or for a sidebar row (`true`: narrower,
/// only a draft opened from the sidebar).
pub fn move_field_in(
    ui: &mut egui::Ui,
    t: &Tokens,
    draft: &mut Option<MoveDraft>,
    target: MoveTarget,
    sidebar: bool,
) -> Option<String> {
    let d = draft
        .as_mut()
        .filter(|d| d.target == target && d.sidebar == sidebar)?;
    d.shown = true;
    d.seen = true;
    let mut submit = None;
    let mut cancel = false;
    ui.horizontal(|ui| {
        ui.add_space(if sidebar { 6.0 } else { 12.0 });
        if !sidebar {
            ui.label(RichText::new("Move to").color(t.text_secondary).size(11.5));
        }
        let width = if sidebar {
            (ui.available_width() - if d.sent { 30.0 } else { 6.0 }).max(60.0)
        } else {
            // 320 pt, less in a narrow card (room kept for the spinner and "Moving…")
            let tail = if d.sent { 80.0 } else { 12.0 };
            (ui.available_width() - tail).clamp(80.0, 320.0)
        };
        let r = ui.add_enabled(
            !d.sent,
            egui::TextEdit::singleline(&mut d.text)
                .desired_width(width)
                .hint_text(if sidebar { "Move to…" } else { PLACEHOLDER })
                .font(t.font(12.5)),
        );
        if d.sent {
            ui.add(egui::Spinner::new().size(12.0));
            if !sidebar {
                ui.label(RichText::new("Moving…").color(t.text_tertiary).size(11.5));
            }
        } else {
            if d.focus {
                r.request_focus();
                d.focus = false;
            }
            if r.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                && !d.text.trim().is_empty()
            {
                submit = Some(d.text.trim().to_owned());
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
        }
    });
    if cancel {
        *draft = None;
    }
    submit
}
