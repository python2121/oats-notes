//! Pop-ups in the window's upper right, over whatever view is showing (rounded cards, the
//! popup shadow), stacked newest on top:
//!
//! - **Confirmations** ("Todo added: …", "Reminder set: … — Thu 12 Dec, 09:00", "Event
//!   added: …", "Note filed: …", "Project created: …"): one per thing a voice command created,
//!   at most [`MAX_CONFIRMATIONS`]; each closes by itself after [`CONFIRM_SECS`] (one
//!   scheduled repaint, no animation loop; hovering holds it open) or with its ×.
//! - **Reminders**: every open todo whose due time (or snooze end) has come — a bell, the
//!   text, when it was due, Snooze 1h · 1d · 1w, and Done. The × in the corner is Done too:
//!   dismissing a reminder marks it complete (the user's rule — one non-snooze action), and
//!   it moves to the crossed-through end of the todo list. At most [`MAX_REMINDERS`] show,
//!   then "and N more".
//!
//! Nothing here repaints on its own: the app schedules one repaint for the next due time
//! (`Store::next_due_at`), and one per confirmation deadline.

use chrono::Local;
use egui::{Align2, Color32, Order, RichText, Sense, Stroke, pos2, vec2};
use oats_core::Todo;

use crate::theme::Tokens;
use crate::todos::{SNOOZES, absolute};

/// How long a confirmation stays up.
pub const CONFIRM_SECS: f64 = 4.0;
/// …and one with a button ("Move them too"): long enough to read and decide.
pub const ACTION_SECS: f64 = 10.0;
/// Confirmations shown at once (the oldest goes).
pub const MAX_CONFIRMATIONS: usize = 3;
/// Reminder cards shown at once ("and N more" below).
pub const MAX_REMINDERS: usize = 3;
/// Card width.
pub const WIDTH: f32 = 340.0;
/// The narrowest a card gets (in a content column narrower than `WIDTH` + 2 × `INSET`).
pub const MIN_WIDTH: f32 = 240.0;
/// Inset from the window's edge (right) and from the top of the content (below the bars).
pub const INSET: f32 = 12.0;
/// The column the × takes at a card's right edge (its glyph is ~7 pt wide).
const CLOSE_W: f32 = 12.0;

/// The cards' width in a content column `content_w` wide.
pub fn card_width(content_w: f32) -> f32 {
    (content_w - 2.0 * INSET).clamp(MIN_WIDTH, WIDTH)
}

/// A confirmation on screen.
#[derive(Clone, Debug, PartialEq)]
pub struct Confirmation {
    pub id: u64,
    pub text: String,
    /// egui time (seconds) it closes at.
    pub expires: f64,
    /// A failure: the warning colour and a "!" instead of the check.
    pub error: bool,
    /// A button under the text (its label and what it does); clicking it closes the card.
    pub action: Option<(String, ToastAction)>,
}

/// The confirmations on screen.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToastState {
    pub confirmations: Vec<Confirmation>,
    next_id: u64,
}

impl ToastState {
    /// Show `text` from `now` (egui time) for [`CONFIRM_SECS`]; newest first, at most
    /// [`MAX_CONFIRMATIONS`].
    pub fn confirm(&mut self, text: String, now: f64) {
        self.confirm_as(text, now, false);
    }

    /// [`ToastState::confirm`], as an error when `error`.
    pub fn confirm_as(&mut self, text: String, now: f64, error: bool) {
        self.next_id += 1;
        self.confirmations.insert(
            0,
            Confirmation {
                id: self.next_id,
                text,
                expires: now + CONFIRM_SECS,
                error,
                action: None,
            },
        );
        self.confirmations.truncate(MAX_CONFIRMATIONS);
    }

    /// A confirmation with a button (`label`, sending `action`), up for [`ACTION_SECS`].
    pub fn confirm_action(&mut self, text: String, now: f64, label: &str, action: ToastAction) {
        self.next_id += 1;
        self.confirmations.insert(
            0,
            Confirmation {
                id: self.next_id,
                text,
                expires: now + ACTION_SECS,
                error: false,
                action: Some((label.to_owned(), action)),
            },
        );
        self.confirmations.truncate(MAX_CONFIRMATIONS);
    }

    /// Drop the expired ones; the time the next one expires, if any are left.
    pub fn expire(&mut self, now: f64) -> Option<f64> {
        self.confirmations.retain(|c| c.expires > now);
        self.confirmations
            .iter()
            .map(|c| c.expires)
            .min_by(f64::total_cmp)
    }
}

/// What the user did in a pop-up.
#[derive(Clone, Debug, PartialEq)]
pub enum ToastAction {
    /// Done (or the ×): complete the todo.
    Done(i64),
    Snooze {
        id: i64,
        until: i64,
    },
    /// "and N more": open the Todos view.
    ShowTodos,
    /// "Move them too": these lines were the same person as the line just corrected.
    ReassignLines {
        segments: Vec<i64>,
        target: oats_core::SpeakerTarget,
    },
}

/// The reminder cards to show: newest due first.
pub fn reminder_order(due: &[Todo]) -> Vec<&Todo> {
    let mut v: Vec<&Todo> = due.iter().collect();
    v.sort_by_key(|t| (std::cmp::Reverse(t.alert_at()), std::cmp::Reverse(t.id)));
    v
}

/// Draw the stack at the top right of `content` (the content column, below the bars):
/// [`WIDTH`] wide, narrower when the column is (never under [`MIN_WIDTH`]), [`INSET`] from
/// its edges. Schedules the repaint for the next confirmation deadline.
pub fn toasts(
    ctx: &egui::Context,
    content: egui::Rect,
    due: &[Todo],
    state: &mut ToastState,
) -> Vec<ToastAction> {
    let mut actions = Vec::new();
    let now = ctx.input(|i| i.time);
    if let Some(next) = state.expire(now) {
        ctx.request_repaint_after(std::time::Duration::from_secs_f64((next - now).max(0.0)));
    }
    if due.is_empty() && state.confirmations.is_empty() {
        return actions;
    }
    let t = Tokens::get(ctx);
    let shadow = ctx.global_style().visuals.popup_shadow;
    let mut dismissed = Vec::new();
    let width = card_width(content.width());
    let right = ctx.content_rect().right() - content.right().min(ctx.content_rect().right());
    egui::Area::new(egui::Id::new("oats-toasts"))
        .order(Order::Foreground)
        .anchor(
            Align2::RIGHT_TOP,
            vec2(-INSET - right, content.top().max(0.0) + INSET),
        )
        .constrain(true)
        .interactable(true)
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 8.0;
            for c in &mut state.confirmations {
                let hovered =
                    confirmation_card(ui, &t, shadow, width, c, &mut dismissed, &mut actions);
                if hovered {
                    // hovering holds it open; re-checked when the pointer leaves
                    c.expires = c.expires.max(now + 1.5);
                }
            }
            let order = reminder_order(due);
            for todo in order.iter().take(MAX_REMINDERS) {
                reminder_card(ui, &t, shadow, width, todo, &mut actions);
            }
            if order.len() > MAX_REMINDERS {
                let more = order.len() - MAX_REMINDERS;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    let resp = ui
                        .add(
                            egui::Label::new(
                                RichText::new(format!("and {more} more due"))
                                    .color(t.text_secondary)
                                    .size(11.5),
                            )
                            .sense(Sense::click()),
                        )
                        .on_hover_text("Show the todo list");
                    if resp.clicked() {
                        actions.push(ToastAction::ShowTodos);
                    }
                });
            }
        });
    state.confirmations.retain(|c| !dismissed.contains(&c.id));
    actions
}

fn card_frame(t: &Tokens, shadow: egui::epaint::Shadow, light: bool) -> egui::Frame {
    egui::Frame::new()
        .fill(t.chrome)
        .stroke(Stroke::new(1.0, t.button_border))
        .corner_radius(10)
        .shadow(if light {
            egui::epaint::Shadow {
                blur: shadow.blur / 2,
                ..shadow
            }
        } else {
            shadow
        })
        .inner_margin(if light {
            // The message's line box has ~1.5 pt of leading above the capitals and the
            // descender's room below: 9 / 7 puts the capitals (and the icon and × centred on
            // them) on the card's centre line.
            egui::Margin {
                left: 12,
                right: 12,
                top: 9,
                bottom: 7,
            }
        } else {
            // The title's line box has ~3 pt of leading above the capitals, the button row
            // none below: 10 / 13 makes the visible gaps (border → title capitals, Done →
            // border) equal.
            egui::Margin {
                left: 12,
                right: 12,
                top: 10,
                bottom: 13,
            }
        })
}

/// The × in a card's top-right corner, its ink centred on the capitals of the first line of
/// text set in `line` that starts at the row's top (the card's title / message).
/// The × of a card. `salt` is the card's own id: sibling cards share the child ui's stable
/// `ui.id()`, so a bare `ui.id().with("close")` was one widget id for every card on screen.
fn close_button(
    ui: &mut egui::Ui,
    t: &Tokens,
    tip: &str,
    line: &egui::FontId,
    salt: impl std::hash::Hash + std::fmt::Debug,
) -> bool {
    let top = ui.cursor().top();
    let (slot, _) = ui.allocate_exact_size(vec2(CLOSE_W, 18.0), Sense::hover());
    let center = pos2(slot.center().x, top + crate::widgets::cap_center(ui, line));
    let hit = egui::Rect::from_center_size(center, vec2(18.0, 18.0));
    let resp = ui
        .interact(hit, ui.id().with(("close", salt)), Sense::click())
        .on_hover_text(tip);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "×"));
    crate::widgets::paint_glyph_centered(
        ui,
        center,
        "×",
        &egui::FontId::proportional(15.0),
        t.text_secondary,
    );
    resp.clicked()
}

/// A confirmation; returns whether the pointer is over it.
fn confirmation_card(
    ui: &mut egui::Ui,
    t: &Tokens,
    shadow: egui::epaint::Shadow,
    width: f32,
    c: &Confirmation,
    dismissed: &mut Vec<u64>,
    actions: &mut Vec<ToastAction>,
) -> bool {
    let resp = card_frame(t, shadow, true).show(ui, |ui| {
        ui.set_width(width - 24.0);
        ui.horizontal_top(|ui| {
            let font = egui::FontId::proportional(12.5);
            // The icon is centred on the first line's capitals (the × too), so a one-line
            // message is centred and a wrapped one keeps both on its first line.
            let top = ui.cursor().top();
            let (slot, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
            let r = egui::Rect::from_center_size(
                pos2(slot.center().x, top + crate::widgets::cap_center(ui, &font)),
                vec2(16.0, 16.0),
            );
            let col = if c.error { t.danger } else { t.text };
            if c.error {
                crate::widgets::paint_glyph_centered(ui, r.center(), "!", &font, t.danger);
            } else {
                check_glyph(ui, r, t.accent);
            }
            let w = ui.available_width() - CLOSE_W - ui.spacing().item_spacing.x;
            ui.allocate_ui_with_layout(
                vec2(w, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(w);
                    ui.add(
                        egui::Label::new(RichText::new(&c.text).color(col).font(font.clone()))
                            .wrap(),
                    );
                },
            );
            if close_button(ui, t, "Dismiss", &font, c.id) {
                dismissed.push(c.id);
            }
        });
        if let Some((label, action)) = &c.action {
            ui.add_space(4.0);
            // a row (a right-to-left layout straight in the card would take every free point
            // of the area's height, growing the card each frame)
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // styled like a reminder's Done
                    let b =
                        egui::Button::new(RichText::new(label).size(12.0).color(Color32::WHITE))
                            .fill(t.accent)
                            .corner_radius(5)
                            .min_size(vec2(54.0, 20.0));
                    if ui.add(b).clicked() {
                        actions.push(action.clone());
                        dismissed.push(c.id);
                    }
                });
            });
        }
    });
    resp.response.contains_pointer()
}

fn reminder_card(
    ui: &mut egui::Ui,
    t: &Tokens,
    shadow: egui::epaint::Shadow,
    width: f32,
    todo: &Todo,
    actions: &mut Vec<ToastAction>,
) {
    let now = Local::now();
    let title = crate::theme::font_medium(ui.ctx(), 13.5);
    card_frame(t, shadow, false).show(ui, |ui| {
        ui.set_width(width - 24.0);
        ui.horizontal_top(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
            bell_glyph(ui, r, t.accent);
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.set_width(width - 24.0 - 18.0 - 2.0 - 6.0 * 2.0 - 20.0);
                ui.add(
                    egui::Label::new(RichText::new(&todo.text).font(title.clone()).color(t.text))
                        .wrap(),
                );
                let when = match todo.due {
                    Some(d) => format!("Due {}", absolute(d, &now)),
                    None => "Snoozed reminder".to_owned(),
                };
                ui.label(RichText::new(when).color(t.text_secondary).size(11.5));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if close_button(ui, t, "Done — mark complete", &title, todo.id) {
                    actions.push(ToastAction::Done(todo.id));
                }
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(RichText::new("Snooze").color(t.text_tertiary).size(11.5));
            for (label, short, len) in SNOOZES {
                let b = egui::Button::new(RichText::new(short).size(11.5).color(t.text))
                    .fill(t.button)
                    .stroke(Stroke::new(1.0, t.button_border))
                    .corner_radius(5)
                    .min_size(vec2(30.0, 20.0));
                if ui
                    .add(b)
                    .on_hover_text(format!("Remind me again in {label}"))
                    .clicked()
                {
                    actions.push(ToastAction::Snooze {
                        id: todo.id,
                        until: Local::now().timestamp_millis() + len,
                    });
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let done =
                    egui::Button::new(RichText::new("Done").size(12.0).color(Color32::WHITE))
                        .fill(t.accent)
                        .corner_radius(5)
                        .min_size(vec2(54.0, 20.0));
                if ui
                    .add(done)
                    .on_hover_text("Mark complete (moves to the end of the todo list)")
                    .clicked()
                {
                    actions.push(ToastAction::Done(todo.id));
                }
            });
        });
    });
}

/// A small bell, painted (no glyph needed from the font).
fn bell_glyph(ui: &egui::Ui, r: egui::Rect, col: Color32) {
    let p = ui.painter();
    let c = r.center();
    let s = r.width() / 18.0;
    // the dome and flared skirt
    let pts = vec![
        pos2(c.x - 6.0 * s, c.y + 4.0 * s),
        pos2(c.x - 5.0 * s, c.y - 1.0 * s),
        pos2(c.x - 4.0 * s, c.y - 4.5 * s),
        pos2(c.x - 1.5 * s, c.y - 6.5 * s),
        pos2(c.x + 1.5 * s, c.y - 6.5 * s),
        pos2(c.x + 4.0 * s, c.y - 4.5 * s),
        pos2(c.x + 5.0 * s, c.y - 1.0 * s),
        pos2(c.x + 6.0 * s, c.y + 4.0 * s),
        pos2(c.x + 7.5 * s, c.y + 5.5 * s),
        pos2(c.x - 7.5 * s, c.y + 5.5 * s),
    ];
    p.add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
    // the clapper and the top knob
    p.circle_filled(pos2(c.x, c.y + 7.5 * s), 1.8 * s, col);
    p.circle_filled(pos2(c.x, c.y - 7.3 * s), 1.2 * s, col);
}

/// A check mark, painted.
fn check_glyph(ui: &egui::Ui, r: egui::Rect, col: Color32) {
    let c = r.center();
    ui.painter().add(egui::Shape::line(
        vec![
            pos2(c.x - 5.0, c.y),
            pos2(c.x - 1.5, c.y + 3.5),
            pos2(c.x + 5.0, c.y - 4.0),
        ],
        Stroke::new(2.0, col),
    ));
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_cards_shrink_in_a_narrow_column() {
        assert_eq!(card_width(800.0), WIDTH);
        assert_eq!(card_width(300.0), 300.0 - 2.0 * INSET);
        assert_eq!(card_width(100.0), MIN_WIDTH);
    }

    use super::*;
    use oats_core::Origin;

    fn todo(id: i64, due: Option<i64>, snoozed: Option<i64>) -> Todo {
        Todo {
            id,
            event_id: None,
            text: format!("t{id}"),
            due,
            completed_at: None,
            snoozed_until: snoozed,
            source: Origin::User,
            source_segment_id: None,
            node_id: None,
            position: id,
            dedupe_key: None,
            created_at: 0,
        }
    }

    #[test]
    fn confirmations_stack_newest_first_and_expire_after_four_seconds() {
        let mut s = ToastState::default();
        for i in 0..5 {
            s.confirm(format!("c{i}"), f64::from(i));
        }
        let texts: Vec<&str> = s.confirmations.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["c4", "c3", "c2"], "at most three, newest on top");
        assert_eq!(s.expire(5.5), Some(6.0), "c2 (shown at 2) closes at 6");
        assert_eq!(s.expire(6.0), Some(7.0));
        assert_eq!(s.confirmations.len(), 2);
        assert_eq!(s.expire(8.0), None);
        assert!(s.confirmations.is_empty());
    }

    /// Runs one frame of the stack with egui's id-clash check on and returns whether egui
    /// painted a "Double use of widget ID" warning (anything on its debug layer).
    fn clash_painted(due: &[Todo], confirmations: usize) -> bool {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        ctx.options_mut(|o| o.warn_on_id_clash = true);
        let mut state = ToastState::default();
        for i in 0..confirmations {
            state.confirm(format!("Moved to Work › Project {i}"), 0.0);
        }
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                vec2(1100.0, 720.0),
            )),
            ..Default::default()
        };
        ctx.begin_pass(raw);
        let content = egui::Rect::from_min_max(pos2(260.0, 60.0), pos2(1100.0, 700.0));
        toasts(&ctx, content, due, &mut state);
        let painted =
            ctx.graphics(|g| g.get(egui::LayerId::debug()).is_some_and(|l| !l.is_empty()));
        ctx.end_pass().textures_delta.clear();
        painted
    }

    #[test]
    fn several_cards_at_once_do_not_reuse_a_widget_id() {
        // the × of every card used the child ui's stable id (the same for sibling cards), so
        // a second confirmation or a reminder under a confirmation drew egui's red
        // "Double use of widget ID" at the × — off the right edge of the window
        assert!(
            !clash_painted(&[], 2),
            "two confirmations share a close-button id"
        );
        let due = [todo(1, Some(100), None), todo(2, Some(300), None)];
        assert!(
            !clash_painted(&due, 1),
            "a confirmation over reminders shares a close-button id"
        );
        assert!(!clash_painted(&due, 0), "two reminders share ids");
    }

    #[test]
    fn reminders_newest_due_first() {
        let due = [
            todo(1, Some(100), None),
            todo(2, Some(300), None),
            todo(3, Some(50), Some(200)),
        ];
        let ids: Vec<i64> = reminder_order(&due).iter().map(|t| t.id).collect();
        assert_eq!(ids, [2, 3, 1]);
    }
}
