//! The Todos view (⌘5): todos and reminders in one list — a reminder is a todo with a due
//! time. Open ones first (dated soonest first, then the undated ones in their manual order),
//! each with a checkbox, its text, when it is due ("in 2 days", "Thu 12 Dec, 09:00", overdue
//! in red), a snooze indicator and its node; the completed ones at the bottom, crossed
//! through, with the day they were done. Two fields at the top add one by hand: the text and
//! a "due" (parsed by `oats_core::when`; the text also takes a trailing ` @ tomorrow 9am`).
//! Right-click: Edit text, Set due…, Clear due, Snooze ▸ 1 hour / 1 day / 1 week, Clear
//! snooze, Delete; the due label opens the same inline due editor; clicking a dictated
//! todo's text shows where it was said in the transcript.
//!
//! The helpers here (due labels, the snooze choices, the right-click menu) are shared with
//! the calendar's day panel and the reminder pop-ups.

use chrono::{DateTime, Datelike, Local, TimeZone};
use egui::{Key, RichText, Sense};
use oats_core::{Node, Origin, Todo, when};

use crate::notes::{chip, node_path};
use crate::theme::Tokens;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// The snooze choices: label, short label (the pop-up's buttons), length in ms.
pub const SNOOZES: [(&str, &str, i64); 3] = [
    ("1 hour", "1h", HOUR),
    ("1 day", "1d", DAY),
    ("1 week", "1w", 7 * DAY),
];

/// What the user did to a todo (the Todos view, the calendar's day panel, a pop-up).
#[derive(Clone, Debug, PartialEq)]
pub enum TodoAction {
    /// Add a todo typed by hand (`due` is the parsed due time, if any).
    Add {
        text: String,
        due: Option<i64>,
    },
    /// Set (`Some`) or clear (`None`) the due time.
    SetDue(i64, Option<i64>),
    SetDone(i64, bool),
    Snooze {
        id: i64,
        until: i64,
    },
    ClearSnooze(i64),
    SetText(i64, String),
    Delete(i64),
    /// Show where it was said (its source segment) in the transcript.
    Show(i64),
}

/// Which add field holds the unreadable date.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AddError {
    DueField,
    TextSuffix,
}

/// What Enter in the add fields does: nothing (empty text), an unreadable date, or add.
#[derive(Debug, PartialEq)]
pub enum AddOutcome {
    Nothing,
    Bad(AddError),
    Add { text: String, due: Option<i64> },
}

/// Interpret the add fields: the due field wins over a ` @ <date>` suffix of the text.
pub fn parse_add<Tz: TimeZone>(text: &str, due_text: &str, now: &DateTime<Tz>) -> AddOutcome {
    let due_text = due_text.trim();
    let (body, suffix) = when::split_at_suffix(text);
    if body.is_empty() {
        return AddOutcome::Nothing;
    }
    if !due_text.is_empty() {
        return match when::parse(due_text, now) {
            Some(d) => AddOutcome::Add {
                text: body.to_owned(),
                due: Some(d.timestamp_millis()),
            },
            None => AddOutcome::Bad(AddError::DueField),
        };
    }
    match suffix {
        Some(s) => match when::parse(s, now) {
            Some(d) => AddOutcome::Add {
                text: body.to_owned(),
                due: Some(d.timestamp_millis()),
            },
            None => AddOutcome::Bad(AddError::TextSuffix),
        },
        None => AddOutcome::Add {
            text: body.to_owned(),
            due: None,
        },
    }
}

/// Inline editing state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TodosState {
    /// The "+ Add todo" field.
    pub new_text: String,
    /// The "due" field next to it.
    pub new_due: String,
    /// The add fields hold a date that couldn't be read.
    pub add_error: Option<AddError>,
    /// The todo whose due time is being edited and the field's text.
    pub editing_due: Option<(i64, String)>,
    /// The due editor's text couldn't be read.
    pub due_error: bool,
    pub focus_due: bool,
    /// The todo being edited and the field's text.
    pub editing: Option<(i64, String)>,
    /// Give the open edit field keyboard focus this frame.
    pub focus_edit: bool,
}

/// "Thu 12 Dec, 09:00" (the year added when it isn't `now`'s).
pub fn absolute<Tz: TimeZone>(ms: i64, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let tz = now.timezone();
    match tz.timestamp_millis_opt(ms).single() {
        Some(d) if d.year() == now.year() => {
            format!("{}, {}", d.format("%a %-d %b"), crate::timefmt::hm(&d))
        }
        Some(d) => format!("{}, {}", d.format("%a %-d %b %Y"), crate::timefmt::hm(&d)),
        None => "?".to_owned(),
    }
}

/// When a todo is due, relative to `now`: "in 20 min", "today, 15:00", "tomorrow, 09:00",
/// "in 3 days", else the date ("Thu 12 Dec, 09:00"); past due times are the date too, and
/// `true` (overdue).
pub fn due_label<Tz: TimeZone>(due: i64, now: &DateTime<Tz>) -> (String, bool)
where
    Tz::Offset: std::fmt::Display,
{
    let now_ms = now.timestamp_millis();
    if due <= now_ms {
        return (absolute(due, now), true);
    }
    let Some(d) = now.timezone().timestamp_millis_opt(due).single() else {
        return ("?".to_owned(), false);
    };
    let days = (d.date_naive() - now.date_naive()).num_days();
    let label = if due - now_ms < HOUR {
        format!("in {} min", ((due - now_ms) + MINUTE - 1) / MINUTE)
    } else if days == 0 {
        format!("today, {}", crate::timefmt::hm(&d))
    } else if days == 1 {
        format!("tomorrow, {}", crate::timefmt::hm(&d))
    } else if days < 7 {
        format!("in {days} days")
    } else {
        absolute(due, now)
    };
    (label, false)
}

/// "snoozed until 10:30" (the date too when not today), when snoozed past `now`.
pub fn snooze_label<Tz: TimeZone>(t: &Todo, now: &DateTime<Tz>) -> Option<String>
where
    Tz::Offset: std::fmt::Display,
{
    let until = t.snoozed_until.filter(|u| *u > now.timestamp_millis())?;
    let d = now.timezone().timestamp_millis_opt(until).single()?;
    Some(if d.date_naive() == now.date_naive() {
        format!("snoozed until {}", crate::timefmt::hm(&d))
    } else {
        format!("snoozed until {}", absolute(until, now))
    })
}

fn now_ms() -> i64 {
    Local::now().timestamp_millis()
}

/// The right-click menu of a todo: complete / reopen, snooze, clear snooze, (edit), delete.
/// Returns whether "Edit Text" / "Set due…" were picked (only offered with `edit`, i.e. where
/// an inline editor exists); "Clear due" is always there.
pub fn todo_menu(
    ui: &mut egui::Ui,
    t: &Todo,
    edit: bool,
    actions: &mut Vec<TodoAction>,
) -> (bool, bool) {
    let mut editing = false;
    let mut set_due = false;
    let done_label = if t.done() {
        "Mark Not Done"
    } else {
        "Mark Done"
    };
    if ui.button(done_label).clicked() {
        actions.push(TodoAction::SetDone(t.id, !t.done()));
        ui.close();
    }
    if edit && ui.button("Edit Text").clicked() {
        editing = true;
        ui.close();
    }
    if !t.done() {
        if edit && ui.button("Set due…").clicked() {
            set_due = true;
            ui.close();
        }
        if ui
            .add_enabled(t.due.is_some(), egui::Button::new("Clear due"))
            .clicked()
        {
            actions.push(TodoAction::SetDue(t.id, None));
            ui.close();
        }
        ui.menu_button("Snooze", |ui| {
            for (label, _, len) in SNOOZES {
                if ui.button(label).clicked() {
                    actions.push(TodoAction::Snooze {
                        id: t.id,
                        until: now_ms() + len,
                    });
                    ui.close();
                }
            }
        });
        if ui
            .add_enabled(t.snoozed_until.is_some(), egui::Button::new("Clear Snooze"))
            .clicked()
        {
            actions.push(TodoAction::ClearSnooze(t.id));
            ui.close();
        }
    }
    if t.source_segment_id.is_some() && ui.button("Show in Transcript").clicked() {
        actions.push(TodoAction::Show(t.id));
        ui.close();
    }
    ui.separator();
    if ui.button("Delete").clicked() {
        actions.push(TodoAction::Delete(t.id));
        ui.close();
    }
    (editing, set_due)
}

/// Who made it, for the small source chip.
pub fn source_label(s: Origin) -> &'static str {
    match s {
        Origin::Hotword => "voice command",
        Origin::Interpretation | Origin::Auto => "from transcript",
        Origin::User => "you",
    }
}

/// The view. `todos` in display order (open, then completed: `Store::todos`).
pub fn todos_view(
    ui: &mut egui::Ui,
    state: &mut TodosState,
    todos: &[Todo],
    nodes: &[Node],
) -> Vec<TodoAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    let now = Local::now();
    let open = todos.iter().filter(|x| !x.done()).count();
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(
            RichText::new("Todos")
                .font(crate::theme::font_medium(ui.ctx(), 15.0))
                .color(t.text),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!("{open} open"))
                .color(t.text_tertiary)
                .size(11.5),
        );
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        let width = (ui.available_width() - 14.0).min(680.0);
        let due_width = 270.0_f32.min(width * 0.45);
        let bad = |on: bool, ui: &mut egui::Ui| {
            if on {
                ui.visuals_mut().override_text_color = Some(t.danger);
            }
        };
        let text_resp = ui
            .scope(|ui| {
                bad(state.add_error == Some(AddError::TextSuffix), ui);
                ui.add(
                    egui::TextEdit::singleline(&mut state.new_text)
                        .hint_text("+ Add todo")
                        .desired_width(width - due_width - 8.0),
                )
            })
            .inner;
        let due_resp = ui
            .scope(|ui| {
                bad(state.add_error == Some(AddError::DueField), ui);
                ui.add(
                    egui::TextEdit::singleline(&mut state.new_due)
                        .hint_text("e.g. tomorrow 9am, in 3 days, nov 4")
                        .desired_width(due_width),
                )
            })
            .inner;
        if text_resp.changed() || due_resp.changed() {
            state.add_error = None;
        }
        let enter = ui.input(|i| i.key_pressed(Key::Enter));
        if (text_resp.lost_focus() || due_resp.lost_focus()) && enter {
            match parse_add(&state.new_text, &state.new_due, &now) {
                AddOutcome::Nothing => {}
                AddOutcome::Bad(e) => {
                    state.add_error = Some(e);
                    if e == AddError::DueField {
                        due_resp.request_focus();
                    } else {
                        text_resp.request_focus();
                    }
                }
                AddOutcome::Add { text, due } => {
                    actions.push(TodoAction::Add { text, due });
                    state.new_text.clear();
                    state.new_due.clear();
                    state.add_error = None;
                    text_resp.request_focus();
                }
            }
        }
    });
    ui.add_space(6.0);
    if todos.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.label(
                RichText::new(
                    "Nothing to do. Say “hey computer, remind me in 2 days to call the landlord” \
                     or type one above.",
                )
                .color(t.text_tertiary)
                .size(12.5),
            );
        });
        return actions;
    }
    egui::ScrollArea::vertical()
        .id_salt("todos-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut completed_heading = false;
            for todo in todos {
                if todo.done() && !completed_heading {
                    completed_heading = true;
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        ui.label(
                            RichText::new(format!(
                                "Completed ({})",
                                todos.len().saturating_sub(open)
                            ))
                            .font(crate::theme::font_medium(ui.ctx(), 13.0))
                            .color(t.text_secondary),
                        );
                    });
                    ui.add_space(2.0);
                }
                todo_row(ui, &t, state, todo, nodes, &now, &mut actions);
            }
            ui.add_space(10.0);
        });
    actions
}

fn todo_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut TodosState,
    todo: &Todo,
    nodes: &[Node],
    now: &DateTime<Local>,
    actions: &mut Vec<TodoAction>,
) {
    ui.horizontal_top(|ui| {
        ui.add_space(12.0);
        let mut done = todo.done();
        if ui
            .checkbox(&mut done, "")
            .on_hover_text(if todo.done() {
                "Mark not done"
            } else {
                "Mark done"
            })
            .changed()
        {
            actions.push(TodoAction::SetDone(todo.id, done));
        }
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 16.0).max(80.0));
            let editing = state.editing.as_ref().is_some_and(|(id, _)| *id == todo.id);
            if editing {
                let mut commit = None;
                let mut cancel = false;
                if let Some((_, text)) = &mut state.editing {
                    let resp = ui
                        .add(egui::TextEdit::singleline(text).desired_width(ui.available_width()));
                    if state.focus_edit {
                        resp.request_focus();
                        state.focus_edit = false;
                    }
                    if ui.input(|i| i.key_pressed(Key::Escape)) {
                        cancel = true;
                    } else if resp.lost_focus() {
                        commit = Some(text.trim().to_owned());
                    }
                }
                if cancel {
                    state.editing = None;
                } else if let Some(text) = commit {
                    if !text.is_empty() && text != todo.text {
                        actions.push(TodoAction::SetText(todo.id, text));
                    }
                    state.editing = None;
                }
            } else {
                let mut text = RichText::new(&todo.text).size(13.0);
                text = if todo.done() {
                    text.strikethrough().color(t.text_tertiary)
                } else {
                    text.color(t.text)
                };
                let shows = todo.source_segment_id.is_some();
                let mut resp = ui.add(egui::Label::new(text).wrap().sense(Sense::click()));
                if shows {
                    resp = resp.on_hover_text("Show in transcript");
                }
                if shows && resp.clicked() {
                    actions.push(TodoAction::Show(todo.id));
                }
                let mut picked = (false, false);
                resp.context_menu(|ui| {
                    picked = todo_menu(ui, todo, true, actions);
                });
                if picked.0 {
                    state.editing = Some((todo.id, todo.text.clone()));
                    state.focus_edit = true;
                }
                if picked.1 {
                    open_due_editor(state, todo);
                }
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if let Some(at) = todo.completed_at {
                    ui.label(
                        RichText::new(format!("done {}", absolute(at, now)))
                            .color(t.text_disabled)
                            .size(11.0),
                    );
                } else if state
                    .editing_due
                    .as_ref()
                    .is_some_and(|(id, _)| *id == todo.id)
                {
                    due_editor(ui, t, state, todo, actions);
                } else if let Some(due) = todo.due {
                    let (label, overdue) = due_label(due, now);
                    let col = if overdue { t.danger } else { t.text_secondary };
                    let resp = ui
                        .add(
                            egui::Label::new(RichText::new(label).color(col).size(11.0))
                                .sense(Sense::click()),
                        )
                        .on_hover_text(if overdue {
                            format!("Overdue — was due {}. Click to change.", absolute(due, now))
                        } else {
                            format!("Due {}. Click to change.", absolute(due, now))
                        });
                    if resp.clicked() {
                        open_due_editor(state, todo);
                    }
                }
                if !todo.done()
                    && let Some(s) = snooze_label(todo, now)
                {
                    ui.label(RichText::new(s).color(t.text_tertiary).size(11.0));
                }
                if let Some(n) = todo.node_id.filter(|n| nodes.iter().any(|x| x.id == *n)) {
                    chip(ui, t, &node_path(nodes, n), true);
                }
                if todo.source != Origin::User {
                    chip(ui, t, source_label(todo.source), false);
                }
            });
        });
    });
    ui.add_space(4.0);
}

fn open_due_editor(state: &mut TodosState, todo: &Todo) {
    let text = todo
        .due
        .and_then(|d| Local.timestamp_millis_opt(d).single())
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    state.editing_due = Some((todo.id, text));
    state.due_error = false;
    state.focus_due = true;
}

/// The inline due field: Enter saves (empty clears), Escape or leaving the field cancels, an
/// unreadable date stays open in red.
fn due_editor(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut TodosState,
    todo: &Todo,
    actions: &mut Vec<TodoAction>,
) {
    let error = state.due_error;
    let focus = std::mem::take(&mut state.focus_due);
    let Some((_, text)) = &mut state.editing_due else {
        return;
    };
    if error {
        ui.visuals_mut().override_text_color = Some(t.danger);
    }
    let resp = ui.add(
        egui::TextEdit::singleline(text)
            .hint_text("tomorrow 9am, in 3 days, nov 4")
            .font(egui::TextStyle::Small)
            .desired_width(180.0),
    );
    if focus {
        resp.request_focus();
    }
    if resp.changed() {
        state.due_error = false;
    }
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        state.editing_due = None;
        state.due_error = false;
    } else if resp.lost_focus() {
        if ui.input(|i| i.key_pressed(Key::Enter)) {
            let trimmed = text.trim().to_owned();
            if trimmed.is_empty() {
                actions.push(TodoAction::SetDue(todo.id, None));
                state.editing_due = None;
            } else if let Some(d) = when::parse(&trimmed, &Local::now()) {
                actions.push(TodoAction::SetDue(todo.id, Some(d.timestamp_millis())));
                state.editing_due = None;
            } else {
                state.due_error = true;
                state.focus_due = true;
            }
        } else {
            state.editing_due = None;
            state.due_error = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn now() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 29, 14, 0, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn due_labels_read_naturally() {
        let n = now();
        let at = n.timestamp_millis();
        assert_eq!(due_label(at + 20 * MINUTE, &n), ("in 20 min".into(), false));
        assert_eq!(due_label(at + 61, &n), ("in 1 min".into(), false));
        assert_eq!(
            due_label(at + 3 * HOUR, &n),
            ("today, 5:00 pm".into(), false)
        );
        assert_eq!(
            due_label(at + 19 * HOUR, &n),
            ("tomorrow, 9:00 am".into(), false)
        );
        assert_eq!(due_label(at + 2 * DAY, &n), ("in 2 days".into(), false));
        assert_eq!(
            due_label(at + 44 * DAY - 5 * HOUR, &n),
            ("Sat 12 Dec, 9:00 am".into(), false)
        );
        assert_eq!(
            due_label(at + 400 * DAY, &n),
            ("Fri 3 Dec 2027, 2:00 pm".into(), false)
        );
        assert_eq!(
            due_label(at - HOUR, &n),
            ("Thu 29 Oct, 1:00 pm".into(), true)
        );
        assert!(due_label(at, &n).1, "due now is due");
    }

    #[test]
    fn the_add_fields_parse_the_due_from_the_field_or_the_suffix() {
        let n = now();
        let ms = |s: &str| when::parse(s, &n).map(|d| d.timestamp_millis());
        assert_eq!(parse_add("  ", "", &n), AddOutcome::Nothing);
        assert_eq!(
            parse_add("buy milk", "", &n),
            AddOutcome::Add {
                text: "buy milk".into(),
                due: None
            }
        );
        assert_eq!(
            parse_add("call landlord @ tomorrow 9am", "", &n),
            AddOutcome::Add {
                text: "call landlord".into(),
                due: ms("tomorrow 9am")
            }
        );
        assert_eq!(
            parse_add("call landlord @ tomorrow 9am", "nov 4", &n),
            AddOutcome::Add {
                text: "call landlord".into(),
                due: ms("nov 4")
            }
        );
        assert_eq!(
            parse_add("x", "someday", &n),
            AddOutcome::Bad(AddError::DueField)
        );
        assert_eq!(
            parse_add("x @ whenever", "", &n),
            AddOutcome::Bad(AddError::TextSuffix)
        );
    }

    #[test]
    fn snooze_label_only_while_snoozed() {
        let n = now();
        let mut t = Todo {
            id: 1,
            event_id: None,
            text: "x".into(),
            due: None,
            completed_at: None,
            snoozed_until: Some(n.timestamp_millis() + 30 * MINUTE),
            source: Origin::User,
            source_segment_id: None,
            node_id: None,
            position: 1,
            dedupe_key: None,
            created_at: 0,
        };
        assert_eq!(
            snooze_label(&t, &n).as_deref(),
            Some("snoozed until 2:30 pm")
        );
        t.snoozed_until = Some(n.timestamp_millis() + DAY);
        assert_eq!(
            snooze_label(&t, &n).as_deref(),
            Some("snoozed until Fri 30 Oct, 2:00 pm")
        );
        t.snoozed_until = Some(n.timestamp_millis() - 1);
        assert_eq!(snooze_label(&t, &n), None);
    }
}
