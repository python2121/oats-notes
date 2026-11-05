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
//! computer played that day ("Watched: <title> 10:12–10:40", from `media_plays`, each a
//! hyperlink to its URL with "Open in browser" / "Copy link" in the context menu); a click
//! opens the video in the browser, and the right-click "Mark as a call" relabels a play that
//! was really a call ("Call via Discord 10:12–10:40").

use std::collections::BTreeMap;

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use egui::{Align2, Rect, RichText, Sense, Stroke, pos2, vec2};
use oats_core::{Event, MediaClass, MediaPlay, Node, Todo};

use crate::notes::{chip, chip_max, node_path};
use crate::theme::Tokens;
use crate::todos::{TodoAction, source_label, todo_menu};
use crate::transcript::{clock, day_heading};

#[derive(Clone, Debug, PartialEq)]
pub enum CalendarAction {
    DeleteEvent(i64),
    /// Show where the event was said in the transcript.
    ShowEvent(i64),
    Todo(TodoAction),
    /// "Mark as a call" on media plays (`media_plays.id`; every play of a folded row).
    MarkPlaysAsCall(Vec<i64>),
    /// Delete media plays (every play of a folded row).
    DeletePlays(Vec<i64>),
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
/// The shortest a grid row gets (the day number and one chip).
const MIN_CELL_H: f32 = 40.0;
/// What the day panel under the grid keeps of the view before the grid stops shrinking.
const DAY_PANEL_MIN: f32 = 110.0;
/// The header, the weekday names and the gaps above and below the grid.
const GRID_CHROME_H: f32 = 8.0 + 24.0 + 6.0 + 18.0 + 12.0;
const CHIP_H: f32 = 15.0;
const MAX_CHIPS: usize = 3;
/// The cell's top band (the day number) above its chips.
const CHIPS_TOP: f32 = 18.0;

/// The grid's row height in a view `view_h` tall with `weeks` rows: [`CELL_H`], less when
/// that would leave the day panel under [`DAY_PANEL_MIN`], never under [`MIN_CELL_H`].
pub fn cell_height(view_h: f32, weeks: usize) -> f32 {
    let weeks = weeks.max(1) as f32;
    ((view_h - GRID_CHROME_H - DAY_PANEL_MIN) / weeks).clamp(MIN_CELL_H, CELL_H)
}

/// How many chips fit in a cell `cell_h` tall (at most [`MAX_CHIPS`]).
pub fn chips_fitting(cell_h: f32) -> usize {
    let room = cell_h - 2.0 - CHIPS_TOP - 2.0;
    ((room + 2.0) / (CHIP_H + 2.0))
        .floor()
        .clamp(0.0, MAX_CHIPS as f32) as usize
}

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
    let days = grid_days(month);
    // the grid shrinks (down to MIN_CELL_H a row) so the day panel under it keeps
    // DAY_PANEL_MIN of the view; past that the whole view scrolls
    let cell_h = cell_height(ui.available_height(), days.len() / 7);
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
            for week in days.chunks(7) {
                let (row, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), cell_h), Sense::hover());
                for (i, day) in week.iter().enumerate() {
                    let cell = Rect::from_min_size(
                        pos2(row.left() + MARGIN + cell_w * i as f32, row.top()),
                        vec2(cell_w, cell_h),
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
    let fit = chips_fitting(cell.height());
    if items.len() > fit {
        p.text(
            pos2(inner.right() - 5.0, inner.top() + 3.0),
            Align2::RIGHT_TOP,
            format!("+{}", items.len() - fit),
            t.font(10.5),
            t.text_tertiary,
        );
    }
    let dim = if look.in_month { 1.0 } else { 0.55 };
    for (i, item) in items.iter().take(fit).enumerate() {
        let r = Rect::from_min_size(
            pos2(
                inner.left() + 3.0,
                inner.top() + CHIPS_TOP + i as f32 * (CHIP_H + 2.0),
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

/// Consecutive music plays of one producer this far apart (or closer) are one row.
pub const MUSIC_FOLD_GAP_MS: i64 = 10 * 60_000;

/// One row of the day panel's media section: a play, or consecutive music plays of the same
/// producer and service (a listening session, track after track) folded together.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayFold {
    /// Oldest first.
    pub plays: Vec<MediaPlay>,
}

impl PlayFold {
    pub fn ids(&self) -> Vec<i64> {
        self.plays.iter().map(|p| p.id).collect()
    }

    pub fn started_at(&self) -> i64 {
        self.plays.iter().map(|p| p.started_at).min().unwrap_or(0)
    }

    pub fn ended_at(&self) -> i64 {
        self.plays.iter().map(|p| p.ended_at).max().unwrap_or(0)
    }

    /// The newest play: its URL is what a click opens.
    pub fn last(&self) -> Option<&MediaPlay> {
        self.plays.last()
    }

    /// The distinct tracks (titles) played.
    pub fn tracks(&self) -> usize {
        let mut titles: Vec<&str> = self.plays.iter().map(|p| p.title.trim()).collect();
        titles.sort_unstable();
        titles.dedup();
        titles.len()
    }

    /// "Listened: Spotify" for several tracks, else the play's own label.
    pub fn label(&self) -> String {
        match self.plays.as_slice() {
            [p] => play_label(p),
            [first, ..] if self.tracks() == 1 => play_label(first),
            [first, ..] => format!("Listened: {}", service_of(first)),
            [] => String::new(),
        }
    }

    /// "10:00–10:48 pm" plus " · 14 tracks" for several tracks.
    pub fn when(&self) -> String {
        let range = crate::timefmt::range(self.started_at(), self.ended_at(), &Local);
        match self.tracks() {
            n if n > 1 => format!("{range} · {n} tracks"),
            _ => range,
        }
    }
}

/// The music service a play came from ("Spotify" for a web-player tab in Firefox), else its
/// app.
fn service_of(p: &MediaPlay) -> String {
    match p.url.as_deref() {
        Some(url) => crate::media::music_service(url).to_owned(),
        None if !p.producer.trim().is_empty() => p.producer.trim().to_owned(),
        None => "Music".to_owned(),
    }
}

/// Fold the day's plays (oldest first): consecutive MUSIC plays of the same producer and
/// service at most [`MUSIC_FOLD_GAP_MS`] apart become one row; everything else stays one row
/// per play.
pub fn fold_plays(plays: &[MediaPlay]) -> Vec<PlayFold> {
    let mut out: Vec<PlayFold> = Vec::new();
    for p in plays {
        if p.kind == MediaClass::Music
            && let Some(last) = out.last_mut()
            && let Some(prev) = last.plays.last()
            && prev.kind == MediaClass::Music
            && prev.producer == p.producer
            && service_of(prev) == service_of(p)
            && p.started_at - last.ended_at() <= MUSIC_FOLD_GAP_MS
        {
            last.plays.push(p.clone());
            continue;
        }
        out.push(PlayFold {
            plays: vec![p.clone()],
        });
    }
    out
}

/// "Watched: <title> 10:12–10:40" / "Listened: …" / "Call via …" per play of the day (a
/// music session folded into one row, see [`fold_plays`]); a click opens the URL (of the last
/// track); right-click "Mark as a call" and "Delete" (every play of the row).
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
    for fold in fold_plays(plays) {
        let Some(p) = fold.last() else {
            continue;
        };
        // the label wraps, its time after it (or on the next line)
        crate::widgets::inset_wrapped(ui, MARGIN, MARGIN, |ui| {
            let when = fold.when();
            let text = RichText::new(fold.label()).size(12.5);
            // a real hyperlink to the source (underline on hover, hand cursor, ⌘-click for a
            // new tab, the URL as the tooltip); a play without a URL is plain text
            let resp = match &p.url {
                Some(url) => ui
                    .add(egui::Hyperlink::from_label_and_url(
                        text.color(t.accent),
                        url.clone(),
                    ))
                    .on_hover_text(url.as_str()),
                None => ui.add(
                    egui::Label::new(text.color(t.text))
                        .sense(Sense::click())
                        .wrap(),
                ),
            };
            resp.context_menu(|ui| {
                if let Some(url) = &p.url {
                    if ui.button("Open in browser").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                        ui.close();
                    }
                    if ui.button("Copy link").clicked() {
                        ui.ctx().copy_text(url.clone());
                        ui.close();
                    }
                    ui.separator();
                }
                if p.kind.is_media() {
                    if ui
                        .button("Mark as a call")
                        .on_hover_text("This was a conversation, not media")
                        .clicked()
                    {
                        actions.push(CalendarAction::MarkPlaysAsCall(fold.ids()));
                        ui.close();
                    }
                    ui.separator();
                }
                if ui.button("Delete").clicked() {
                    actions.push(CalendarAction::DeletePlays(fold.ids()));
                    ui.close();
                }
            });
            ui.add(
                egui::Label::new(RichText::new(when).color(t.text_tertiary).size(11.5)).truncate(),
            );
        });
    }
}

/// The rest of a day-panel row after its dot / checkbox and time: a wrapping area (14 pt
/// short of the right edge) whose text wraps and whose chips follow it or take the next line.
fn trailing_wrapped(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let w = (ui.available_width() - MARGIN).max(40.0);
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        vec2(w, h),
        egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
        |ui| {
            ui.set_max_width(w);
            add(ui);
        },
    );
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
        // the title wraps in the rest of the row, the chips flow after it
        trailing_wrapped(ui, |ui| {
            let resp = ui
                .add(
                    egui::Label::new(RichText::new(&e.title).color(t.text).size(13.0))
                        .wrap()
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
                let w = ui.available_width();
                chip_max(ui, t, &node_path(nodes, n), true, w);
            }
            chip(ui, t, source_label(e.source), false);
            if let Some(c) = e.confidence.filter(|c| *c < 0.7) {
                chip(ui, t, &format!("{:.0}% sure", c * 100.0), false);
            }
        });
    });
    if !e.details.is_empty() {
        crate::widgets::inset_wrapped(ui, MARGIN + 14.0, MARGIN, |ui| {
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
        if crate::widgets::check_box(ui, &mut done, &t.font(13.0)).changed() {
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
            text.color(t.text_tertiary)
        } else {
            text.color(t.text)
        };
        trailing_wrapped(ui, |ui| {
            let mut resp = ui.add(egui::Label::new(text).wrap().sense(Sense::click()));
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
                let w = ui.available_width();
                chip_max(ui, t, &node_path(nodes, n), true, w);
            }
            chip(ui, t, source_label(td.source), false);
        });
    });
    ui.add_space(2.0);
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_grid_shrinks_so_the_day_panel_keeps_room_in_a_short_window() {
        // a tall view: the full cell height and three chips
        assert_eq!(cell_height(900.0, 6), CELL_H);
        assert_eq!(chips_fitting(CELL_H), 3);
        // the 520 pt window's ~440 pt list: shorter rows, the day panel keeps its minimum
        let h = cell_height(443.0, 5);
        assert!((MIN_CELL_H..CELL_H).contains(&h));
        assert!(443.0 - GRID_CHROME_H - 5.0 * h >= DAY_PANEL_MIN - 0.01);
        assert!(chips_fitting(h) >= 1);
        // never under the minimum (then the view scrolls)
        assert_eq!(cell_height(200.0, 6), MIN_CELL_H);
        assert_eq!(chips_fitting(MIN_CELL_H), 1);
    }

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

    fn mp(id: i64, kind: MediaClass, start_min: i64, end_min: i64, title: &str) -> MediaPlay {
        MediaPlay {
            id,
            started_at: start_min * 60_000,
            ended_at: end_min * 60_000,
            kind,
            url: Some("https://open.spotify.com/playlist/p".to_owned()),
            title: title.to_owned(),
            producer: "Firefox".to_owned(),
            segments_count: 0,
        }
    }

    #[test]
    fn consecutive_music_plays_of_one_producer_fold_into_one_row() {
        let plays = [
            mp(1, MediaClass::Music, 0, 3, "One — X"),
            mp(2, MediaClass::Music, 3, 7, "Two — Y"),
            // a short pause: same session
            mp(3, MediaClass::Music, 15, 18, "Three — Z"),
            // a video between breaks the run
            MediaPlay {
                url: Some("https://youtu.be/a".to_owned()),
                ..mp(4, MediaClass::YouTube, 19, 25, "A video")
            },
            mp(5, MediaClass::Music, 26, 29, "Four — W"),
            // more than 10 min later: a new row
            mp(6, MediaClass::Music, 40, 43, "Five — V"),
            // the desktop app: another producer
            MediaPlay {
                url: None,
                producer: "Spotify".to_owned(),
                ..mp(7, MediaClass::Music, 43, 46, "Spotify")
            },
            // the same track twice (paused and resumed) is one track
            mp(8, MediaClass::Music, 60, 62, "Six — U"),
            mp(9, MediaClass::Music, 63, 65, "Six — U"),
        ];
        let folds = fold_plays(&plays);
        let ids: Vec<Vec<i64>> = folds.iter().map(PlayFold::ids).collect();
        assert_eq!(
            ids,
            [
                vec![1, 2, 3],
                vec![4],
                vec![5],
                vec![6],
                vec![7],
                vec![8, 9]
            ]
        );
        assert_eq!(folds[0].label(), "Listened: Spotify");
        assert_eq!(folds[0].tracks(), 3);
        assert!(
            folds[0].when().ends_with(" · 3 tracks"),
            "{}",
            folds[0].when()
        );
        assert_eq!(
            folds[0].last().map(|p| p.id),
            Some(3),
            "a click opens the last"
        );
        assert_eq!(
            (folds[0].started_at(), folds[0].ended_at()),
            (0, 18 * 60_000)
        );
        assert_eq!(folds[2].label(), "Listened: Four — W");
        assert_eq!(folds[1].label(), "Watched: A video");
        assert_eq!(folds[5].label(), "Listened: Six — U");
        assert!(!folds[5].when().contains("tracks"));
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
