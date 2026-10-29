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
}

impl MoveDraft {
    pub fn new(target: MoveTarget, from: Option<i64>) -> Self {
        MoveDraft {
            target,
            text: String::new(),
            focus: true,
            sent: false,
            from,
        }
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
    let d = draft.as_mut().filter(|d| d.target == target)?;
    let mut submit = None;
    let mut cancel = false;
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label(RichText::new("Move to").color(t.text_secondary).size(11.5));
        let r = ui.add_enabled(
            !d.sent,
            egui::TextEdit::singleline(&mut d.text)
                .desired_width(320.0)
                .hint_text(PLACEHOLDER)
                .font(t.font(12.5)),
        );
        if d.sent {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("Moving…").color(t.text_tertiary).size(11.5));
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
