//! The Calendar view: upcoming events grouped by day (no month grid yet), the past ones behind
//! a toggle, and reminders with a done checkbox. Events and reminders come from the brain's
//! interpretation (significant dates) or from a hotword command; the source is shown so the
//! user can audit.

use chrono::Local;
use egui::{Align2, RichText, Sense};
use oats_core::{Event, Node, Origin, Reminder};

use crate::notes::{chip, node_path};
use crate::theme::Tokens;
use crate::transcript::{clock, day_heading, day_of};

#[derive(Clone, Debug, PartialEq)]
pub enum CalendarAction {
    SetReminderDone(i64, bool),
    DeleteEvent(i64),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CalendarState {
    pub show_past: bool,
    pub show_done: bool,
}

/// `(past, upcoming)` split at `now_ms`, both soonest-first.
pub fn split_events(events: &[Event], now_ms: i64) -> (Vec<&Event>, Vec<&Event>) {
    let mut all: Vec<&Event> = events.iter().collect();
    all.sort_by_key(|e| (e.when_start, e.id));
    let (past, upcoming): (Vec<&Event>, Vec<&Event>) = all
        .into_iter()
        .partition(|e| e.when_end.unwrap_or(e.when_start + 86_400_000) < now_ms);
    (past, upcoming)
}

fn source_label(s: Origin) -> &'static str {
    match s {
        Origin::Hotword => "voice command",
        Origin::Interpretation | Origin::Auto => "from transcript",
        Origin::User => "you",
    }
}

fn event_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    e: &Event,
    nodes: &[Node],
    actions: &mut Vec<CalendarAction>,
) {
    let resp = ui
        .horizontal(|ui| {
            ui.add_space(12.0);
            let when = if e.all_day {
                "all day".to_owned()
            } else {
                clock(e.when_start, &Local)
            };
            ui.label(
                RichText::new(when)
                    .font(t.mono(10.5))
                    .color(t.text_tertiary),
            );
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(&e.title).color(t.text).size(13.0)).truncate());
            if let Some(n) = e.node_id {
                chip(ui, t, &node_path(nodes, n), true);
            }
            chip(ui, t, source_label(e.source), false);
            if let Some(c) = e.confidence.filter(|c| *c < 0.7) {
                chip(ui, t, &format!("{:.0}% sure", c * 100.0), false);
            }
        })
        .response;
    let resp = ui.interact(resp.rect, ui.id().with(("event", e.id)), Sense::click());
    resp.context_menu(|ui| {
        if ui.button("Delete").clicked() {
            actions.push(CalendarAction::DeleteEvent(e.id));
            ui.close();
        }
    });
    if !e.details.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(60.0);
            ui.add(
                egui::Label::new(RichText::new(&e.details).color(t.text_secondary).size(11.5))
                    .wrap(),
            );
        });
    }
}

fn day_groups(
    ui: &mut egui::Ui,
    t: &Tokens,
    events: &[&Event],
    nodes: &[Node],
    this_year: i32,
    actions: &mut Vec<CalendarAction>,
) {
    let mut last = None;
    for e in events {
        let day = day_of(e.when_start, &Local);
        if last != Some(day) {
            last = Some(day);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(
                    RichText::new(
                        day.map(|d| day_heading(d, this_year))
                            .unwrap_or_else(|| "Unknown date".to_owned()),
                    )
                    .font(crate::theme::font_medium(ui.ctx(), 13.0))
                    .color(t.text),
                );
            });
        }
        event_row(ui, t, e, nodes, actions);
    }
}

pub fn calendar_view(
    ui: &mut egui::Ui,
    state: &mut CalendarState,
    events: &[Event],
    reminders: &[Reminder],
    nodes: &[Node],
) -> Vec<CalendarAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    let now_ms = Local::now().timestamp_millis();
    let this_year = chrono::Datelike::year(&Local::now());
    let (past, upcoming) = split_events(events, now_ms);
    if events.is_empty() && reminders.is_empty() {
        let rect = ui.available_rect_before_wrap();
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Nothing scheduled. Dates the brain finds significant, and anything you dictate with “hey computer”, land here.",
            t.font(12.5),
            t.text_tertiary,
        );
        return actions;
    }
    egui::ScrollArea::vertical()
        .id_salt("calendar-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(6.0);
            let open: Vec<&Reminder> = reminders
                .iter()
                .filter(|r| state.show_done || !r.done)
                .collect();
            if !reminders.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(
                        RichText::new("Reminders")
                            .font(crate::theme::font_medium(ui.ctx(), 13.0))
                            .color(t.text),
                    );
                    ui.add_space(8.0);
                    let label = if state.show_done {
                        "hide done"
                    } else {
                        "show done"
                    };
                    if ui
                        .add(
                            egui::Label::new(
                                RichText::new(label).color(t.text_tertiary).size(11.0),
                            )
                            .sense(Sense::click()),
                        )
                        .clicked()
                    {
                        state.show_done = !state.show_done;
                    }
                });
                for r in open {
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        let mut done = r.done;
                        if ui.checkbox(&mut done, "").changed() {
                            actions.push(CalendarAction::SetReminderDone(r.id, done));
                        }
                        let col = if r.done { t.text_disabled } else { t.text };
                        ui.label(
                            RichText::new(clock(r.due, &Local))
                                .font(t.mono(10.5))
                                .color(t.text_tertiary),
                        );
                        ui.label(
                            RichText::new(
                                day_of(r.due, &Local)
                                    .map(|d| day_heading(d, this_year))
                                    .unwrap_or_default(),
                            )
                            .color(t.text_tertiary)
                            .size(11.0),
                        );
                        ui.add(
                            egui::Label::new(RichText::new(&r.text).color(col).size(13.0))
                                .truncate(),
                        );
                        chip(ui, &t, source_label(r.source), false);
                    });
                }
                ui.add_space(8.0);
            }
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(
                    RichText::new("Upcoming")
                        .font(crate::theme::font_medium(ui.ctx(), 13.0))
                        .color(t.text),
                );
            });
            if upcoming.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(
                        RichText::new("nothing upcoming")
                            .color(t.text_tertiary)
                            .size(12.0),
                    );
                });
            }
            day_groups(ui, &t, &upcoming, nodes, this_year, &mut actions);
            if !past.is_empty() {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    let label = format!(
                        "{} Past ({})",
                        if state.show_past { "▾" } else { "▸" },
                        past.len()
                    );
                    if ui
                        .add(
                            egui::Label::new(
                                RichText::new(label)
                                    .font(crate::theme::font_medium(ui.ctx(), 13.0))
                                    .color(t.text_secondary),
                            )
                            .sense(Sense::click()),
                        )
                        .clicked()
                    {
                        state.show_past = !state.show_past;
                    }
                });
                if state.show_past {
                    let mut rev: Vec<&Event> = past.clone();
                    rev.reverse();
                    day_groups(ui, &t, &rev, nodes, this_year, &mut actions);
                }
            }
            ui.add_space(8.0);
        });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: i64, start: i64, end: Option<i64>) -> Event {
        Event {
            id,
            when_start: start,
            when_end: end,
            all_day: true,
            title: format!("e{id}"),
            details: String::new(),
            node_id: None,
            source: Origin::Hotword,
            source_segment_id: None,
            source_episode_id: None,
            confidence: None,
            dedupe_key: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn splits_past_and_upcoming() {
        let day = 86_400_000;
        let events = [
            ev(1, 10 * day, None),
            ev(2, 2 * day, None),
            ev(3, 4 * day, Some(6 * day)),
        ];
        let (past, up) = split_events(&events, 5 * day);
        assert_eq!(past.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(
            up.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![3, 1],
            "soonest first; a running multi-day event is upcoming"
        );
        // an all-day event stays "upcoming" until its day is over
        let (past, _) = split_events(&events, 10 * day + 1000);
        assert_eq!(past.len(), 2);
    }
}
