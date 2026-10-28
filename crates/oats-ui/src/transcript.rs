//! The transcript pane: the date helpers (local midnight, day headings, row times — pure, so
//! they are tested with a fixed time zone) and the list itself (one row per stored segment,
//! grouped under a heading whenever the local day changes).

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use egui::{Align2, Rect, Sense, pos2, vec2};
use oats_core::{Node, TranscriptRow};

use crate::notes::node_path;

use crate::theme::Tokens;

/// What the user did in the list this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptAction {
    /// File a segment under a node (`None` = unfiled).
    Move {
        segment: i64,
        category: Option<i64>,
    },
    Delete {
        segment: i64,
    },
}

/// Unix ms → a time in `tz` (`None` for an out-of-range value).
pub fn at<Tz: TimeZone>(ms: i64, tz: &Tz) -> Option<DateTime<Tz>> {
    tz.timestamp_millis_opt(ms).single()
}

/// The start of the day containing `now`, in Unix ms (the store's "today" boundary). Falls back
/// to `now` minus the time of day if midnight doesn't exist locally (a DST jump at 00:00).
pub fn midnight_ms<Tz: TimeZone>(now: &DateTime<Tz>) -> i64 {
    let tz = now.timezone();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|m| tz.from_local_datetime(&m).earliest())
        .map(|m| m.timestamp_millis())
        .unwrap_or_else(|| {
            now.timestamp_millis() - i64::from(now.time().num_seconds_from_midnight()) * 1000
        })
}

/// Local midnight today, in Unix ms.
pub fn local_midnight_ms() -> i64 {
    midnight_ms(&Local::now())
}

/// Milliseconds from `now` until just after the next midnight in its zone (a second past, so
/// a repaint then sees the new day). At least one second; DST-safe (the next day's first
/// instant, which may be 01:00 where midnight is skipped).
pub fn ms_until_next_midnight<Tz: TimeZone>(now: &DateTime<Tz>) -> i64 {
    let tz = now.timezone();
    let next = now
        .date_naive()
        .succ_opt()
        .and_then(|d| {
            (0..=8).find_map(|q| {
                d.and_hms_opt(0, 0, 0)
                    .map(|m| m + chrono::Duration::minutes(15 * q))
                    .and_then(|m| tz.from_local_datetime(&m).earliest())
            })
        })
        .map(|m| m.timestamp_millis())
        .unwrap_or_else(|| now.timestamp_millis() + 86_400_000);
    (next - now.timestamp_millis() + 1_000).max(1_000)
}

/// [`ms_until_next_midnight`] from the local now, as a `Duration`.
pub fn until_local_midnight() -> std::time::Duration {
    std::time::Duration::from_millis(ms_until_next_midnight(&Local::now()) as u64)
}

/// The local calendar day of a row.
pub fn day_of<Tz: TimeZone>(ms: i64, tz: &Tz) -> Option<NaiveDate> {
    at(ms, tz).map(|d| d.date_naive())
}

/// "Tuesday, 28 October" (with the year appended when it isn't `this_year`).
pub fn day_heading(day: NaiveDate, this_year: i32) -> String {
    if day.year() == this_year {
        day.format("%A, %-d %B").to_string()
    } else {
        day.format("%A, %-d %B %Y").to_string()
    }
}

/// "14:05".
pub fn clock<Tz: TimeZone>(ms: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    at(ms, tz)
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_owned())
}

/// Indices into `rows` where a new day starts (always includes 0 for a non-empty list), with
/// that day. `rows` are oldest first.
pub fn day_breaks<Tz: TimeZone>(
    rows: &[TranscriptRow],
    tz: &Tz,
) -> Vec<(usize, Option<NaiveDate>)> {
    let mut out = Vec::new();
    let mut prev: Option<Option<NaiveDate>> = None;
    for (i, r) in rows.iter().enumerate() {
        let day = day_of(r.started_at, tz);
        if prev != Some(day) {
            out.push((i, day));
            prev = Some(day);
        }
    }
    out
}

/// The list. The scroll area sticks to the bottom: while the user is at the end, a new segment
/// scrolls into view; once they scroll up it stays where they left it until they return.
pub fn transcript_list(
    ui: &mut egui::Ui,
    rows: &[TranscriptRow],
    nodes: &[Node],
) -> Vec<TranscriptAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    if rows.is_empty() {
        let rect = ui.available_rect_before_wrap();
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Nothing yet. Start listening and OatsNotes will transcribe what it hears.",
            t.font(12.5),
            t.text_tertiary,
        );
        return actions;
    }
    let breaks = day_breaks(rows, &Local);
    let this_year = Local::now().year();
    egui::ScrollArea::vertical()
        .id_salt("transcript-scroll")
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(6.0);
            let mut next_break = breaks.iter().peekable();
            for (i, row) in rows.iter().enumerate() {
                if let Some((_, day)) = next_break.next_if(|(at, _)| *at == i) {
                    if i > 0 {
                        ui.add_space(10.0);
                    }
                    let heading = day
                        .map(|d| day_heading(d, this_year))
                        .unwrap_or_else(|| "Unknown date".to_owned());
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        ui.label(
                            egui::RichText::new(heading)
                                .font(crate::theme::font_medium(ui.ctx(), 13.5))
                                .color(t.text),
                        );
                    });
                    ui.add_space(4.0);
                }
                transcript_row(ui, &t, row, nodes, &mut actions);
            }
            ui.add_space(8.0);
        });
    actions
}

/// Width of the time column.
const TIME_W: f32 = 44.0;

fn transcript_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    row: &TranscriptRow,
    nodes: &[Node],
    actions: &mut Vec<TranscriptAction>,
) {
    // the hover fill goes UNDER the text: reserve its slot first, fill it once the row's height
    // is known
    let fill = ui.painter().add(egui::Shape::Noop);
    let left = ui.max_rect().left();
    let right = ui.max_rect().right();
    let inner = ui
        .horizontal_top(|ui| {
            ui.add_space(8.0);
            let (time_rect, _) = ui.allocate_exact_size(vec2(TIME_W, 18.0), Sense::hover());
            ui.painter().text(
                pos2(time_rect.left() + 4.0, time_rect.top() + 3.0),
                Align2::LEFT_TOP,
                clock(row.started_at, &Local),
                t.mono(10.5),
                t.text_tertiary,
            );
            let w = (ui.available_width() - 44.0).max(60.0);
            ui.vertical(|ui| {
                ui.set_width(w);
                ui.add_space(1.0);
                ui.add(
                    egui::Label::new(egui::RichText::new(&row.text).color(t.text).size(13.0))
                        .wrap()
                        .selectable(false),
                );
            });
        })
        .response;
    let rect = Rect::from_min_max(
        pos2(left + 4.0, inner.rect.top() - 2.0),
        pos2(right - 4.0, inner.rect.bottom() + 2.0),
    );
    let resp = ui.interact(rect, ui.id().with(("segment", row.id)), Sense::click());
    if resp.hovered() || resp.context_menu_opened() {
        ui.painter().set(
            fill,
            egui::Shape::rect_filled(rect, 4.0, t.hover.gamma_multiply(0.6)),
        );
        if let Some(c) = row.confidence {
            ui.painter().text(
                pos2(rect.right() - 8.0, rect.top() + 5.0),
                Align2::RIGHT_TOP,
                format!("{:.0}%", c * 100.0),
                t.mono(9.5),
                t.text_tertiary,
            );
        }
    }
    resp.context_menu(|ui| {
        ui.menu_button("Move to", |ui| {
            for c in nodes.iter().filter(|n| !n.archived) {
                let here = row.node_id == Some(c.id);
                let name = node_path(nodes, c.id);
                let label = if here { format!("✔ {name}") } else { name };
                if ui.add_enabled(!here, egui::Button::new(label)).clicked() {
                    actions.push(TranscriptAction::Move {
                        segment: row.id,
                        category: Some(c.id),
                    });
                    ui.close();
                }
            }
            ui.separator();
            let none = row.node_id.is_none();
            if ui
                .add_enabled(
                    !none,
                    egui::Button::new(if none { "✔ None" } else { "None" }),
                )
                .clicked()
            {
                actions.push(TranscriptAction::Move {
                    segment: row.id,
                    category: None,
                });
                ui.close();
            }
        });
        if ui.button("Copy Text").clicked() {
            ui.ctx().copy_text(row.text.clone());
            ui.close();
        }
        ui.separator();
        if ui.button("Delete").clicked() {
            actions.push(TranscriptAction::Delete { segment: row.id });
            ui.close();
        }
    });
    ui.add_space(2.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn the_next_midnight_is_a_second_after_the_day_ends() {
        let tz = FixedOffset::east_opt(3600).unwrap();
        let now = tz
            .with_ymd_and_hms(2026, 10, 28, 23, 59, 0)
            .single()
            .unwrap();
        assert_eq!(ms_until_next_midnight(&now), 61_000);
        let morning = tz.with_ymd_and_hms(2026, 10, 28, 6, 0, 0).single().unwrap();
        assert_eq!(ms_until_next_midnight(&morning), 18 * 3_600_000 + 1_000);
    }

    fn row(id: i64, started_at: i64) -> TranscriptRow {
        TranscriptRow {
            id,
            session_id: 1,
            started_at,
            ended_at: started_at + 1000,
            text: format!("row {id}"),
            confidence: None,
            node_id: None,
            episode_id: None,
            interpreted_at: None,
            categorised: false,
        }
    }

    #[test]
    fn headings_and_clock() {
        let tz = FixedOffset::east_opt(3600).unwrap();
        // 2026-10-27 13:05 UTC = 14:05 at +01:00, a Tuesday
        let ms = 1_793_106_300_000;
        let day = day_of(ms, &tz);
        assert_eq!(day, NaiveDate::from_ymd_opt(2026, 10, 27));
        assert_eq!(clock(ms, &tz), "14:05");
        let d = NaiveDate::from_ymd_opt(2026, 10, 27);
        assert_eq!(
            d.map(|d| day_heading(d, 2026)).as_deref(),
            Some("Tuesday, 27 October")
        );
        assert_eq!(
            d.map(|d| day_heading(d, 2027)).as_deref(),
            Some("Tuesday, 27 October 2026")
        );
    }

    #[test]
    fn midnight_is_start_of_local_day() {
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        let now = at(1_793_106_300_000, &tz).unwrap();
        let midnight = at(midnight_ms(&now), &tz).unwrap();
        assert_eq!(midnight.date_naive(), now.date_naive());
        assert_eq!(midnight.format("%H:%M:%S").to_string(), "00:00:00");
    }

    #[test]
    fn rows_group_by_local_day() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let day = 86_400_000;
        let base = 1_793_059_200_000; // 2026-10-27 00:00 UTC
        let rows = [
            row(1, base + 1000),
            row(2, base + 2000),
            row(3, base + day + 5),
            row(4, base + 2 * day + 5),
            row(5, base + 2 * day + 50),
        ];
        let b = day_breaks(&rows, &tz);
        assert_eq!(b.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 2, 3]);
        assert_eq!(b[1].1, NaiveDate::from_ymd_opt(2026, 10, 28));
        assert!(day_breaks(&[], &tz).is_empty());
    }
}
