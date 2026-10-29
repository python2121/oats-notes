//! The Calendar view (⌘4): a month grid — ‹ month year › and Today; weeks Sunday to Saturday;
//! days outside the month dimmed, today outlined in the accent; each day lists up to three
//! items as one-line chips (events in the accent tint, dated todos in the button grey,
//! completed todos crossed through) and "+N" when there are more. Clicking a day selects it;
//! the panel under the grid lists that day's events and todos in full (time, title, source,
//! node) with their right-click actions (delete an event; complete / snooze / delete a
//! todo). Clicking an item (a chip or a panel row) shows where it was said in the
//! transcript.
//!
//! Events and todos come from the brain's interpretation (significant dates) or from voice
//! commands; the source is shown so the user can audit. Under them the panel lists what the
//! computer played that day ("Watched: <title> 10:12–10:40", from `media_plays`); a click
//! opens the video in the browser, and the right-click "Mark as a call" relabels a play that
//! was really a call ("Call via Discord 10:12–10:40").

use std::collections::BTreeMap;

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use egui::{Align2, Rect, RichText, Sense, Stroke, pos2, vec2};
use oats_core::{Event, MediaClass, MediaPlay, Node, Todo};

use crate::notes::{chip, node_path};
use crate::theme::Tokens;
use crate::todos::{TodoAction, source_label, todo_menu};
use crate::transcript::{clock, day_heading};

#[derive(Clone, Debug, PartialEq)]
pub enum CalendarAction {
    DeleteEvent(i64),
    /// Show where the event was said in the transcript.
    ShowEvent(i64),
    Todo(TodoAction),
    /// "Mark as a call" on a media play (`media_plays.id`).
    MarkPlayAsCall(i64),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CalendarState {
    /// The first day of the month shown (`None` = this month).
    pub month: Option<NaiveDate>,
    /// The day whose items the panel lists (`None` = today).
    pub selected: Option<NaiveDate>,
}

/// The first day of `d`'s month.
pub fn month_of(d: NaiveDate) -> NaiveDate {
    d.with_day(1).unwrap_or(d)
}

/// The first day of the month `n` months from `first` (negative = earlier).
pub fn add_months(first: NaiveDate, n: i32) -> NaiveDate {
    let m = first.year() * 12 + first.month0() as i32 + n;
    NaiveDate::from_ymd_opt(m.div_euclid(12), m.rem_euclid(12) as u32 + 1, 1).unwrap_or(first)
}

/// The days the grid shows for the month starting at `first`: whole weeks, Sunday to Saturday,
/// from the week containing the 1st to the week containing the last day (28–42 days).
pub fn grid_days(first: NaiveDate) -> Vec<NaiveDate> {
    let first = month_of(first);
    let start = first - chrono::Days::new(u64::from(first.weekday().num_days_from_sunday()));
    let last = add_months(first, 1).pred_opt().unwrap_or(first);
    let end = last + chrono::Days::new(u64::from(6 - last.weekday().num_days_from_sunday()));
    start.iter_days().take_while(|d| *d <= end).collect()
}

/// Something on a day.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DayItem<'a> {
    Event(&'a Event),
    Todo(&'a Todo),
}

impl DayItem<'_> {
    fn title(&self) -> &str {
        match self {
            DayItem::Event(e) => &e.title,
            DayItem::Todo(t) => &t.text,
        }
    }
}

/// Events (on their start day) and dated todos (on their due day), per local day: events
/// first (all-day, then by time), then todos by due time. A voice command's event and its own
/// todo for the day (same title, linked) would read twice in a cell: only the event is kept
/// in the grid; the day panel lists both (`dedupe = false`).
pub fn items_by_day<'a, Tz: TimeZone>(
    events: &'a [Event],
    todos: &'a [Todo],
    tz: &Tz,
    dedupe: bool,
) -> BTreeMap<NaiveDate, Vec<DayItem<'a>>> {
    let day = |ms: i64| tz.timestamp_millis_opt(ms).single().map(|d| d.date_naive());
    let mut out: BTreeMap<NaiveDate, Vec<DayItem<'a>>> = BTreeMap::new();
    let mut evs: Vec<&Event> = events.iter().collect();
    evs.sort_by_key(|e| (!e.all_day, e.when_start, e.id));
    for e in evs {
        if let Some(d) = day(e.when_start) {
            out.entry(d).or_default().push(DayItem::Event(e));
        }
    }
    let mut tds: Vec<&Todo> = todos.iter().filter(|t| t.due.is_some()).collect();
    tds.sort_by_key(|t| (t.due, t.id));
    for t in tds {
        let Some(d) = t.due.and_then(day) else {
            continue;
        };
        let items = out.entry(d).or_default();
        let twin = items.iter().any(|i| {
            matches!(i, DayItem::Event(e) if Some(e.id) == t.event_id
                && oats_core::store::name_key(&e.title) == oats_core::store::name_key(&t.text))
        });
        if !(dedupe && twin) {
            items.push(DayItem::Todo(t));
        }
    }
    out
}

/// Grid metrics.
const MARGIN: f32 = 14.0;
const CELL_H: f32 = 78.0;
const CHIP_H: f32 = 15.0;
const MAX_CHIPS: usize = 3;

pub fn calendar_view(
    ui: &mut egui::Ui,
    state: &mut CalendarState,
    events: &[Event],
    todos: &[Todo],
    nodes: &[Node],
    media_plays: &[MediaPlay],
) -> Vec<CalendarAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    let now = Local::now();
    let today = now.date_naive();
    let month = state.month.unwrap_or_else(|| month_of(today));
    let selected = state.selected.unwrap_or(today);
    let grid_items = items_by_day(events, todos, &Local, true);
    egui::ScrollArea::vertical()
        .id_salt("calendar-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(8.0);
            header(ui, &t, state, month, today);
            ui.add_space(6.0);
            let width = (ui.available_width() - 2.0 * MARGIN).max(7.0 * 40.0);
            let cell_w = width / 7.0;
            // weekday names
            let (names, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
            for (i, name) in ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
                .iter()
                .enumerate()
            {
                ui.painter().text(
                    pos2(
                        names.left() + MARGIN + cell_w * (i as f32 + 0.5),
                        names.center().y,
                    ),
                    Align2::CENTER_CENTER,
                    *name,
                    t.font(11.0),
                    t.text_tertiary,
                );
            }
            let days = grid_days(month);
            for week in days.chunks(7) {
                let (row, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), CELL_H), Sense::hover());
                for (i, day) in week.iter().enumerate() {
                    let cell = Rect::from_min_size(
                        pos2(row.left() + MARGIN + cell_w * i as f32, row.top()),
                        vec2(cell_w, CELL_H),
                    );
                    let items = grid_items.get(day).map(Vec::as_slice).unwrap_or(&[]);
                    day_cell(
                        ui,
                        &t,
                        state,
                        cell,
                        *day,
                        DayLook {
                            in_month: day.month() == month.month(),
                            today: *day == today,
                            selected: *day == selected,
                        },
                        items,
                        &mut actions,
                    );
                }
            }
            ui.add_space(12.0);
            let panel_items = items_by_day(events, todos, &Local, false);
            // the host loads the selected day's plays; a day just clicked lists none until
            // they are loaded (the next frame)
            let day_plays: Vec<MediaPlay> = media_plays
                .iter()
                .filter(|p| {
                    Local
                        .timestamp_millis_opt(p.started_at)
                        .single()
                        .is_some_and(|d| d.date_naive() == selected)
                })
                .cloned()
                .collect();
            day_panel(
                ui,
                &t,
                selected,
                panel_items.get(&selected).map(Vec::as_slice).unwrap_or(&[]),
                nodes,
                &day_plays,
                &mut actions,
            );
            ui.add_space(10.0);
        });
    actions
}

fn header(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut CalendarState,
    month: NaiveDate,
    today: NaiveDate,
) {
    ui.horizontal(|ui| {
        ui.add_space(MARGIN - 4.0);
        let arrow = |ui: &mut egui::Ui, s: &str, tip: &str| {
            ui.add(
                egui::Button::new(RichText::new(s).size(15.0).color(t.text_secondary))
                    .frame(false)
                    .min_size(vec2(22.0, 22.0)),
            )
            .on_hover_text(tip)
            .clicked()
        };
        if arrow(ui, "‹", "Previous month") {
            state.month = Some(add_months(month, -1));
        }
        ui.label(
            RichText::new(month.format("%B %Y").to_string())
                .font(crate::theme::font_medium(ui.ctx(), 15.0))
                .color(t.text),
        );
        if arrow(ui, "›", "Next month") {
            state.month = Some(add_months(month, 1));
        }
        ui.add_space(8.0);
        if ui
            .add_enabled(
                month != month_of(today) || state.selected.is_some_and(|d| d != today),
                egui::Button::new("Today"),
            )
            .clicked()
        {
            state.month = None;
            state.selected = None;
        }
    });
}

#[derive(Clone, Copy)]
struct DayLook {
    in_month: bool,
    today: bool,
    selected: bool,
}

#[allow(clippy::too_many_arguments)]
fn day_cell(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut CalendarState,
    cell: Rect,
    day: NaiveDate,
    look: DayLook,
    items: &[DayItem<'_>],
    actions: &mut Vec<CalendarAction>,
) {
    let resp = ui.interact(cell, ui.id().with(("day", day)), Sense::click());
    if resp.clicked() {
        state.selected = Some(day);
    }
    let p = ui.painter().clone();
    let inner = cell.shrink(1.0);
    let fill = if look.selected {
        t.accent_alpha(0.12)
    } else if !look.in_month {
        t.inset
    } else if resp.hovered() {
        t.hover.gamma_multiply(0.5)
    } else {
        t.canvas
    };
    p.rect_filled(inner, 4.0, fill);
    p.rect_stroke(
        inner,
        4.0,
        Stroke::new(1.0, t.divider),
        egui::StrokeKind::Inside,
    );
    if look.today {
        p.rect_stroke(
            inner,
            4.0,
            Stroke::new(1.5, t.accent),
            egui::StrokeKind::Inside,
        );
    }
    let num_col = match (look.today, look.in_month) {
        (true, _) => t.accent,
        (false, true) => t.text_secondary,
        (false, false) => t.text_disabled,
    };
    p.text(
        pos2(inner.left() + 5.0, inner.top() + 3.0),
        Align2::LEFT_TOP,
        day.day().to_string(),
        t.font(11.0),
        num_col,
    );
    if items.len() > MAX_CHIPS {
        p.text(
            pos2(inner.right() - 5.0, inner.top() + 3.0),
            Align2::RIGHT_TOP,
            format!("+{}", items.len() - MAX_CHIPS),
            t.font(10.5),
            t.text_tertiary,
        );
    }
    let dim = if look.in_month { 1.0 } else { 0.55 };
    for (i, item) in items.iter().take(MAX_CHIPS).enumerate() {
        let r = Rect::from_min_size(
            pos2(
                inner.left() + 3.0,
                inner.top() + 18.0 + i as f32 * (CHIP_H + 2.0),
            ),
            vec2(inner.width() - 6.0, CHIP_H),
        );
        let (fill, col, struck) = match item {
            DayItem::Event(_) => (t.accent_alpha(0.22), t.text, false),
            // (`t.field` is next to invisible on the canvas: the button fill reads as a chip)
            DayItem::Todo(td) if td.done() => (t.button.gamma_multiply(0.6), t.text_tertiary, true),
            DayItem::Todo(_) => (t.button, t.text_secondary, false),
        };
        let chip_resp = ui
            .interact(r, ui.id().with(("chip", day, i)), Sense::click())
            .on_hover_text(format!("{} — Show in transcript", item.title()));
        let fill = if chip_resp.hovered() {
            fill.gamma_multiply(1.4)
        } else {
            fill
        };
        p.rect_filled(r, 4.0, fill.gamma_multiply(dim));
        let mut job = egui::text::LayoutJob::simple_singleline(
            item.title().to_owned(),
            t.font(10.5),
            col.gamma_multiply(dim),
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width(r.width() - 8.0);
        let galley = ui.painter().layout_job(job);
        let at = pos2(r.left() + 4.0, r.center().y - galley.size().y / 2.0);
        let size = galley.size();
        p.galley(at, galley, col);
        if struck {
            p.hline(
                at.x..=at.x + size.x,
                at.y + size.y / 2.0,
                Stroke::new(1.0, t.text_tertiary),
            );
        }
        if chip_resp.clicked() {
            state.selected = Some(day);
            actions.push(match item {
                DayItem::Event(e) => CalendarAction::ShowEvent(e.id),
                DayItem::Todo(td) => CalendarAction::Todo(TodoAction::Show(td.id)),
            });
        }
    }
}

fn day_panel(
    ui: &mut egui::Ui,
    t: &Tokens,
    day: NaiveDate,
    items: &[DayItem<'_>],
    nodes: &[Node],
    media_plays: &[MediaPlay],
    actions: &mut Vec<CalendarAction>,
) {
    let this_year = Local::now().year();
    ui.horizontal(|ui| {
        ui.add_space(MARGIN);
        ui.label(
            RichText::new(day_heading(day, this_year))
                .font(crate::theme::font_medium(ui.ctx(), 13.5))
                .color(t.text),
        );
    });
    ui.add_space(4.0);
    if items.is_empty() && media_plays.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(MARGIN);
            ui.label(
                RichText::new(
                    "Nothing on this day. Dates the brain finds significant, and anything you \
                     dictate with “hey computer”, land here.",
                )
                .color(t.text_tertiary)
                .size(12.0),
            );
        });
        return;
    }
    for item in items {
        match item {
            DayItem::Event(e) => event_row(ui, t, e, nodes, actions),
            DayItem::Todo(td) => todo_row(ui, t, td, nodes, actions),
        }
    }
    media_section(ui, t, media_plays, actions);
}

/// What a play row reads: "Watched: <title>", "Listened: <title>", "Call via <app>".
pub fn play_label(p: &MediaPlay) -> String {
    let title = if p.title.trim().is_empty() {
        p.url.as_deref().unwrap_or("something")
    } else {
        p.title.as_str()
    };
    match p.kind {
        MediaClass::Call => {
            let via = if p.producer.trim().is_empty() {
                title
            } else {
                p.producer.as_str()
            };
            format!("Call via {via}")
        }
        MediaClass::Music | MediaClass::App => format!("Listened: {title}"),
        _ => format!("Watched: {title}"),
    }
}

/// "Watched: <title> 10:12–10:40" / "Listened: …" / "Call via …" per play of the day; a
/// click opens the URL; right-click "Mark as a call".
fn media_section(
    ui: &mut egui::Ui,
    t: &Tokens,
    plays: &[MediaPlay],
    actions: &mut Vec<CalendarAction>,
) {
    if plays.is_empty() {
        return;
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(MARGIN);
        ui.label(
            RichText::new("Media")
                .font(crate::theme::font_medium(ui.ctx(), 12.0))
                .color(t.text_secondary),
        );
    });
    for p in plays {
        ui.horizontal(|ui| {
            ui.add_space(MARGIN);
            let when = format!(
                "{}–{}",
                clock(p.started_at, &Local),
                clock(p.ended_at, &Local)
            );
            let text = RichText::new(play_label(p)).size(12.5);
            let resp = match &p.url {
                Some(url) => {
                    let link = ui
                        .add(
                            egui::Label::new(text.color(t.accent))
                                .sense(Sense::click())
                                .truncate(),
                        )
                        .on_hover_text(url.as_str());
                    if link.clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                    }
                    link
                }
                None => ui.add(
                    egui::Label::new(text.color(t.text))
                        .sense(Sense::click())
                        .truncate(),
                ),
            };
            if p.kind.is_media() {
                resp.context_menu(|ui| {
                    if ui
                        .button("Mark as a call")
                        .on_hover_text("This was a conversation, not media")
                        .clicked()
                    {
                        actions.push(CalendarAction::MarkPlayAsCall(p.id));
                        ui.close();
                    }
                });
            }
            ui.label(RichText::new(when).color(t.text_tertiary).size(11.5));
        });
    }
}

fn event_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    e: &Event,
    nodes: &[Node],
    actions: &mut Vec<CalendarAction>,
) {
    ui.horizontal(|ui| {
        ui.add_space(MARGIN);
        let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 3.5, t.accent);
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
        let resp = ui
            .add(
                egui::Label::new(RichText::new(&e.title).color(t.text).size(13.0))
                    .truncate()
                    .sense(Sense::click()),
            )
            .on_hover_text("Show in transcript");
        if resp.clicked() {
            actions.push(CalendarAction::ShowEvent(e.id));
        }
        resp.context_menu(|ui| {
            if ui.button("Show in Transcript").clicked() {
                actions.push(CalendarAction::ShowEvent(e.id));
                ui.close();
            }
            ui.separator();
            if ui.button("Delete Event").clicked() {
                actions.push(CalendarAction::DeleteEvent(e.id));
                ui.close();
            }
        });
        if let Some(n) = e.node_id.filter(|n| nodes.iter().any(|x| x.id == *n)) {
            chip(ui, t, &node_path(nodes, n), true);
        }
        chip(ui, t, source_label(e.source), false);
        if let Some(c) = e.confidence.filter(|c| *c < 0.7) {
            chip(ui, t, &format!("{:.0}% sure", c * 100.0), false);
        }
    });
    if !e.details.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(MARGIN + 14.0);
            ui.add(
                egui::Label::new(RichText::new(&e.details).color(t.text_secondary).size(11.5))
                    .wrap(),
            );
        });
    }
    ui.add_space(2.0);
}

fn todo_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    td: &Todo,
    nodes: &[Node],
    actions: &mut Vec<CalendarAction>,
) {
    ui.horizontal(|ui| {
        ui.add_space(MARGIN - 4.0);
        let mut done = td.done();
        if ui.checkbox(&mut done, "").changed() {
            actions.push(CalendarAction::Todo(TodoAction::SetDone(td.id, done)));
        }
        let when = td.due.map(|d| clock(d, &Local)).unwrap_or_default();
        ui.label(
            RichText::new(when)
                .font(t.mono(10.5))
                .color(t.text_tertiary),
        );
        let mut text = RichText::new(&td.text).size(13.0);
        text = if td.done() {
            text.strikethrough().color(t.text_tertiary)
        } else {
            text.color(t.text)
        };
        let mut resp = ui.add(egui::Label::new(text).truncate().sense(Sense::click()));
        if td.source_segment_id.is_some() {
            resp = resp.on_hover_text("Show in transcript");
            if resp.clicked() {
                actions.push(CalendarAction::Todo(TodoAction::Show(td.id)));
            }
        }
        resp.context_menu(|ui| {
            let mut out = Vec::new();
            todo_menu(ui, td, false, &mut out);
            actions.extend(out.into_iter().map(CalendarAction::Todo));
        });
        if td.snoozed_until.is_some() && !td.done() {
            chip(ui, t, "snoozed", false);
        }
        if let Some(n) = td.node_id.filter(|n| nodes.iter().any(|x| x.id == *n)) {
            chip(ui, t, &node_path(nodes, n), true);
        }
        chip(ui, t, source_label(td.source), false);
    });
    ui.add_space(2.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use oats_core::Origin;

    fn ev(id: i64, start: i64, title: &str) -> Event {
        Event {
            id,
            when_start: start,
            when_end: None,
            all_day: true,
            title: title.to_owned(),
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

    fn todo(id: i64, due: Option<i64>, text: &str, event: Option<i64>) -> Todo {
        Todo {
            id,
            event_id: event,
            text: text.to_owned(),
            due,
            completed_at: None,
            snoozed_until: None,
            source: Origin::Hotword,
            source_segment_id: None,
            node_id: None,
            position: id,
            dedupe_key: None,
            created_at: 0,
        }
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn the_grid_is_whole_weeks_from_sunday() {
        // October 2026 starts on a Thursday and ends on a Saturday
        let days = grid_days(d(2026, 10, 15));
        assert_eq!(days.first(), Some(&d(2026, 9, 27)));
        assert_eq!(days.last(), Some(&d(2026, 10, 31)));
        assert_eq!(days.len(), 35);
        // February 2027 starts on a Monday and ends on a Sunday: five weeks
        let feb = grid_days(d(2027, 2, 1));
        assert_eq!((feb.len(), feb[0]), (35, d(2027, 1, 31)));
        // a six-week month: August 2026 (Sat 1st, Mon 31st)
        assert_eq!(grid_days(d(2026, 8, 1)).len(), 42);
        assert_eq!(add_months(d(2026, 12, 1), 1), d(2027, 1, 1));
        assert_eq!(add_months(d(2026, 1, 1), -1), d(2025, 12, 1));
        assert_eq!(add_months(d(2026, 10, 1), -22), d(2024, 12, 1));
    }

    #[test]
    fn items_land_on_their_local_day_and_twins_merge_in_the_grid() {
        let tz = FixedOffset::east_opt(3600).unwrap();
        let at = |y, m, day, h| {
            tz.with_ymd_and_hms(y, m, day, h, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis()
        };
        let events = [ev(1, at(2026, 12, 12, 0), "Property taxes due")];
        let todos = [
            todo(1, Some(at(2026, 12, 12, 9)), "Property taxes due", Some(1)),
            todo(
                2,
                Some(at(2026, 12, 5, 9)),
                "Property taxes due in one week",
                Some(1),
            ),
            todo(3, None, "undated", None),
            todo(4, Some(at(2026, 12, 12, 8)), "early", None),
        ];
        let grid = items_by_day(&events, &todos, &tz, true);
        let titles = |day: NaiveDate, m: &BTreeMap<NaiveDate, Vec<DayItem<'_>>>| {
            m.get(&day)
                .map(|v| v.iter().map(|i| i.title().to_owned()).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        assert_eq!(
            titles(d(2026, 12, 12), &grid),
            ["Property taxes due", "early"],
            "the event's own todo is folded into it in the grid"
        );
        assert_eq!(
            titles(d(2026, 12, 5), &grid),
            ["Property taxes due in one week"]
        );
        assert_eq!(grid.len(), 2, "undated todos have no day");
        let panel = items_by_day(&events, &todos, &tz, false);
        assert_eq!(
            titles(d(2026, 12, 12), &panel),
            ["Property taxes due", "early", "Property taxes due"],
            "the panel lists the event and both todos (events first, todos by time)"
        );
    }
}
