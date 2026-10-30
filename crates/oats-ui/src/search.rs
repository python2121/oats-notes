//! Search (⌘F): the bar under the control bar and the results view that replaces the current
//! view while it is open — Tags first (the topics on notes; a chip opens the Topic view), then
//! Notes (episode cards), Calendar (event rows), Media (plays; a row opens the Media view) and
//! Transcript (lines with day headings) — with the matched text marked in the accent colour. The query runs in
//! the store (`Store::search`); this module only draws and reports what was clicked.

use std::ops::Range;

use chrono::{Datelike, Local};
use egui::text::{CCursor, CCursorRange, LayoutJob};
use egui::{Align2, Id, RichText, Sense, TextFormat, pos2, vec2};
use oats_core::{Event, MediaPlay, Node, SearchResults, TranscriptRow};

use crate::notes::{NotesAction, NotesState, chip, episode_card, node_path};
use crate::sidebar::{HeaderIcon, header_icon};
use crate::theme::Tokens;
use crate::transcript::{clock, day_heading, day_of, source_label, source_tag};

/// Hits per section.
pub const SEARCH_LIMIT: i64 = 200;
/// Height of the search bar.
pub const BAR_H: f32 = 36.0;

const FIELD_ID: &str = "search-field";

/// The open search: the text, what was last queried, the hits.
#[derive(Clone, Debug, Default)]
pub struct SearchState {
    pub query: String,
    /// The query `results` belong to.
    pub queried: String,
    pub results: SearchResults,
    /// Focus the field on the next frame.
    pub focus: bool,
    /// ...and select its text (⌘F again).
    pub select_all: bool,
}

impl SearchState {
    pub fn opened() -> Self {
        Self {
            focus: true,
            ..Self::default()
        }
    }

    /// The query changed since the results were fetched.
    pub fn stale(&self) -> bool {
        self.query != self.queried
    }

    /// "12 notes · 48 transcript lines · 2 events" ("" for a blank query, "No matches").
    pub fn count_label(&self) -> String {
        if self.query.trim().is_empty() {
            return String::new();
        }
        let r = &self.results;
        let n = |len: usize, one: &str, many: &str| {
            let num = if len as i64 >= SEARCH_LIMIT {
                format!("{len}+")
            } else {
                len.to_string()
            };
            format!("{num} {}", if len == 1 { one } else { many })
        };
        let mut parts = Vec::new();
        if !r.tags.is_empty() {
            parts.push(n(r.tags.len(), "tag", "tags"));
        }
        if !r.episodes.is_empty() {
            parts.push(n(r.episodes.len(), "note", "notes"));
        }
        if !r.segments.is_empty() {
            parts.push(n(r.segments.len(), "transcript line", "transcript lines"));
        }
        if !r.events.is_empty() {
            parts.push(n(r.events.len(), "event", "events"));
        }
        if !r.media.is_empty() {
            parts.push(n(r.media.len(), "play", "plays"));
        }
        if parts.is_empty() {
            "No matches".to_owned()
        } else {
            parts.join(" · ")
        }
    }
}

/// What the user did in the results this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum SearchAction {
    Close,
    /// Show this episode (in its node's view).
    OpenEpisode(i64),
    /// Show this transcript segment (id) in the Transcripts view.
    OpenSegment(i64),
    OpenEvent(i64),
    /// Open the Topic view for this tag.
    OpenTag(String),
    /// Show this play (`media_plays.id`) in the Media view.
    OpenMedia(i64),
    /// A notes-card action (Re-interpret, …).
    Notes(NotesAction),
}

/// Byte ranges of `text` that match `needle` case-insensitively (non-overlapping, in order).
/// Chars are compared by their first lower-case char, so byte offsets always stay on char
/// boundaries of `text`.
pub fn match_ranges(text: &str, needle: &str) -> Vec<Range<usize>> {
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let needle: Vec<char> = needle.trim().chars().map(fold).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let hay: Vec<(usize, char)> = text.char_indices().map(|(i, c)| (i, fold(c))).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()]
            .iter()
            .map(|(_, c)| *c)
            .eq(needle.iter().copied())
        {
            let start = hay[i].0;
            let last = i + needle.len();
            let end = hay.get(last).map_or(text.len(), |(b, _)| *b);
            out.push(start..end);
            i = last;
        } else {
            i += 1;
        }
    }
    out
}

/// Append `text` to `job` in `base`, with every match of `hl` in the accent colour on a tinted
/// background.
pub fn append_marked(job: &mut LayoutJob, t: &Tokens, text: &str, hl: &str, base: TextFormat) {
    let mut at = 0;
    for r in match_ranges(text, hl) {
        if r.start > at {
            job.append(&text[at..r.start], 0.0, base.clone());
        }
        let mut f = base.clone();
        f.color = t.accent;
        f.background = t.accent_alpha(0.22);
        job.append(&text[r.clone()], 0.0, f);
        at = r.end;
    }
    if at < text.len() {
        job.append(&text[at..], 0.0, base);
    }
}

/// The bar: field, match count, close. Returns `(query_changed, close_clicked)`.
pub fn search_bar(ui: &mut egui::Ui, state: &mut SearchState) -> (bool, bool) {
    let t = Tokens::get(ui.ctx());
    let count = state.count_label();
    let count_w = ui
        .painter()
        .layout_no_wrap(count.clone(), t.font(11.5), t.text_secondary)
        .size()
        .x;
    let id = Id::new(FIELD_ID);
    let mut changed = false;
    let mut close = false;
    ui.horizontal_centered(|ui| {
        ui.add_space(12.0);
        let w = (ui.available_width() - count_w - 24.0 - 12.0 - 8.0 - 8.0).max(120.0);
        if state.select_all {
            state.select_all = false;
            let mut te = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            te.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(0),
                CCursor::new(state.query.chars().count()),
            )));
            te.store(ui.ctx(), id);
        }
        let resp = ui.add(
            egui::TextEdit::singleline(&mut state.query)
                .id(id)
                .hint_text("Search transcripts and notes")
                .desired_width(w),
        );
        if state.focus {
            state.focus = false;
            resp.request_focus();
        }
        changed = resp.changed();
        ui.add_space(8.0);
        ui.label(RichText::new(count).size(11.5).color(t.text_secondary));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            if header_icon(ui, HeaderIcon::Close)
                .on_hover_text("Close search (Esc)")
                .clicked()
            {
                close = true;
            }
        });
    });
    (changed, close)
}

fn heading(ui: &mut egui::Ui, t: &Tokens, text: &str, count: usize) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(
            RichText::new(text)
                .font(crate::theme::font_medium(ui.ctx(), 14.0))
                .color(t.text),
        );
        ui.label(
            RichText::new(count.to_string())
                .font(t.mono(10.5))
                .color(t.text_tertiary),
        );
    });
    ui.add_space(4.0);
}

fn day_label(ui: &mut egui::Ui, t: &Tokens, ms: i64, this_year: i32) {
    let text = day_of(ms, &Local)
        .map(|d| day_heading(d, this_year))
        .unwrap_or_else(|| "Unknown date".to_owned());
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(
            RichText::new(text)
                .font(crate::theme::font_medium(ui.ctx(), 12.5))
                .color(t.text_secondary),
        );
    });
    ui.add_space(3.0);
}

/// A clickable row: hover fill under `add`'s content.
fn clickable_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    id: Id,
    add: impl FnOnce(&mut egui::Ui),
) -> egui::Response {
    let fill = ui.painter().add(egui::Shape::Noop);
    let (left, right) = (ui.max_rect().left(), ui.max_rect().right());
    let inner = ui.horizontal_top(add).response;
    let rect = egui::Rect::from_min_max(
        pos2(left + 4.0, inner.rect.top() - 2.0),
        pos2(right - 4.0, inner.rect.bottom() + 2.0),
    );
    let resp = ui.interact(rect, id, Sense::click());
    if resp.hovered() {
        ui.painter().set(
            fill,
            egui::Shape::rect_filled(rect, 4.0, t.hover.gamma_multiply(0.6)),
        );
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ui.add_space(2.0);
    resp
}

fn marked_label(
    ui: &mut egui::Ui,
    t: &Tokens,
    text: &str,
    hl: &str,
    size: f32,
    color: egui::Color32,
) {
    let mut job = LayoutJob::default();
    append_marked(
        &mut job,
        t,
        text,
        hl,
        TextFormat {
            font_id: t.font(size),
            color,
            ..Default::default()
        },
    );
    ui.add(egui::Label::new(job).wrap().selectable(false));
}

fn transcript_hit(ui: &mut egui::Ui, t: &Tokens, row: &TranscriptRow, hl: &str) -> bool {
    clickable_row(ui, t, ui.id().with(("hit-seg", row.id)), |ui| {
        ui.add_space(8.0);
        let tag = source_label(row);
        let (time_rect, _) = ui.allocate_exact_size(
            vec2(60.0, if tag.is_some() { 32.0 } else { 18.0 }),
            Sense::hover(),
        );
        ui.painter().text(
            pos2(time_rect.left() + 4.0, time_rect.top() + 3.0),
            Align2::LEFT_TOP,
            crate::timefmt::clock_col(row.started_at, &Local),
            t.mono(10.5),
            t.text_tertiary,
        );
        if let Some(tag) = tag {
            source_tag(
                ui,
                t,
                pos2(time_rect.left() + 4.0, time_rect.top() + 18.0),
                tag,
            );
        }
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 44.0).max(60.0));
            ui.add_space(1.0);
            marked_label(ui, t, &row.text, hl, 13.0, t.text);
        });
    })
    .clicked()
}

fn event_hit(
    ui: &mut egui::Ui,
    t: &Tokens,
    e: &Event,
    nodes: &[Node],
    hl: &str,
    year: i32,
) -> bool {
    clickable_row(ui, t, ui.id().with(("hit-event", e.id)), |ui| {
        ui.add_space(12.0);
        let day = day_of(e.when_start, &Local)
            .map(|d| day_heading(d, year))
            .unwrap_or_default();
        let when = if e.all_day {
            format!("{day} · all day")
        } else {
            format!("{day} · {}", clock(e.when_start, &Local))
        };
        ui.label(
            RichText::new(when)
                .font(t.mono(10.5))
                .color(t.text_tertiary),
        );
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 12.0).max(60.0));
            marked_label(ui, t, &e.title, hl, 13.0, t.text);
            if !e.details.is_empty() {
                marked_label(ui, t, &e.details, hl, 11.5, t.text_secondary);
            }
        });
        if let Some(n) = e.node_id {
            chip(ui, t, &node_path(nodes, n), true);
        }
    })
    .clicked()
}

/// A tag chip: "name · N", clickable (opens the Topic view).
fn tag_chip(ui: &mut egui::Ui, t: &Tokens, name: &str, count: i64, hl: &str) -> egui::Response {
    let mut job = LayoutJob::default();
    let base = TextFormat {
        font_id: t.font(12.0),
        color: t.text,
        ..Default::default()
    };
    // every query word marked (a multi-word query matches tags by any of its words)
    let mut ranges: Vec<Range<usize>> = hl
        .split_whitespace()
        .flat_map(|w| match_ranges(name, w))
        .collect();
    ranges.sort_by_key(|r| r.start);
    let mut at = 0;
    for r in ranges {
        if r.start < at {
            continue;
        }
        if r.start > at {
            job.append(&name[at..r.start], 0.0, base.clone());
        }
        let mut f = base.clone();
        f.color = t.accent;
        job.append(&name[r.clone()], 0.0, f);
        at = r.end;
    }
    if at < name.len() {
        job.append(&name[at..], 0.0, base.clone());
    }
    job.append(
        &format!(" · {count}"),
        0.0,
        TextFormat {
            font_id: t.font(11.0),
            color: t.text_tertiary,
            ..Default::default()
        },
    );
    let galley = ui.painter().layout_job(job);
    let size = vec2(galley.size().x + 18.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let label = format!("{name} · {count}");
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    if ui.is_rect_visible(rect) {
        let fill = if resp.hovered() {
            t.accent_alpha(0.28)
        } else {
            t.accent_alpha(0.14)
        };
        ui.painter().rect(
            rect,
            11.0,
            fill,
            egui::Stroke::new(1.0, t.accent_alpha(0.35)),
            egui::StrokeKind::Inside,
        );
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, t.text);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.on_hover_text(format!("Notes tagged “{name}” ({count})"))
}

fn media_hit(ui: &mut egui::Ui, t: &Tokens, p: &MediaPlay, hl: &str) -> bool {
    clickable_row(ui, t, ui.id().with(("hit-media", p.id)), |ui| {
        ui.add_space(8.0);
        let (time_rect, _) = ui.allocate_exact_size(vec2(60.0, 18.0), Sense::hover());
        ui.painter().text(
            pos2(time_rect.left() + 4.0, time_rect.top() + 3.0),
            Align2::LEFT_TOP,
            crate::timefmt::clock_col(p.started_at, &Local),
            t.mono(10.5),
            t.text_tertiary,
        );
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 44.0).max(60.0));
            ui.add_space(1.0);
            marked_label(ui, t, &crate::media::entry_title(p), hl, 13.0, t.text);
            let mut line = crate::media::source_line(p);
            if let Some(url) = &p.url {
                line.push_str(" · ");
                line.push_str(url);
            }
            marked_label(ui, t, &line, hl, 11.0, t.text_tertiary);
        });
    })
    .clicked()
}

/// The results view, in `ui`'s rect.
pub fn search_results(
    ui: &mut egui::Ui,
    state: &SearchState,
    notes_state: &mut NotesState,
    nodes: &[Node],
) -> Vec<SearchAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    let r = &state.results;
    if r.segments.is_empty()
        && r.episodes.is_empty()
        && r.events.is_empty()
        && r.tags.is_empty()
        && r.media.is_empty()
    {
        let text = if state.query.trim().is_empty() {
            "Type to search the transcript, the notes and the calendar."
        } else {
            "No matches."
        };
        let rect = ui.available_rect_before_wrap();
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            t.font(12.5),
            t.text_tertiary,
        );
        return actions;
    }
    let hl = state.queried.as_str();
    let year = Local::now().year();
    egui::ScrollArea::vertical()
        .id_salt("search-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !r.tags.is_empty() {
                heading(ui, &t, "Tags", r.tags.len());
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.set_max_width((ui.available_width() - 14.0).max(100.0));
                        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                        for (name, count) in &r.tags {
                            if tag_chip(ui, &t, name, *count, hl).clicked() {
                                actions.push(SearchAction::OpenTag(name.clone()));
                            }
                        }
                    });
                });
                ui.add_space(4.0);
            }
            if !r.episodes.is_empty() {
                heading(ui, &t, "Notes", r.episodes.len());
                let mut last_day = None;
                for e in &r.episodes {
                    let day = day_of(e.started_at, &Local);
                    if last_day != Some(day) {
                        last_day = Some(day);
                        day_label(ui, &t, e.started_at, year);
                    }
                    let mut notes = Vec::new();
                    let mut open = None;
                    episode_card(
                        ui,
                        &t,
                        notes_state,
                        e,
                        nodes,
                        &mut notes,
                        hl,
                        Some(&mut open),
                    );
                    actions.extend(notes.into_iter().map(SearchAction::Notes));
                    if let Some(id) = open {
                        actions.push(SearchAction::OpenEpisode(id));
                    }
                }
            }
            if !r.events.is_empty() {
                heading(ui, &t, "Calendar", r.events.len());
                for e in &r.events {
                    if event_hit(ui, &t, e, nodes, hl, year) {
                        actions.push(SearchAction::OpenEvent(e.id));
                    }
                }
            }
            if !r.media.is_empty() {
                heading(ui, &t, "Media", r.media.len());
                let mut last_day = None;
                for p in &r.media {
                    let day = day_of(p.started_at, &Local);
                    if last_day != Some(day) {
                        last_day = Some(day);
                        day_label(ui, &t, p.started_at, year);
                    }
                    if media_hit(ui, &t, p, hl) {
                        actions.push(SearchAction::OpenMedia(p.id));
                    }
                }
            }
            if !r.segments.is_empty() {
                heading(ui, &t, "Transcript", r.segments.len());
                let mut last_day = None;
                for row in &r.segments {
                    let day = day_of(row.started_at, &Local);
                    if last_day != Some(day) {
                        last_day = Some(day);
                        day_label(ui, &t, row.started_at, year);
                    }
                    if transcript_hit(ui, &t, row, hl) {
                        actions.push(SearchAction::OpenSegment(row.id));
                    }
                }
            }
            ui.add_space(8.0);
        });
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ignore_case_and_cover_every_hit() {
        assert_eq!(
            match_ranges("The Roadmap, the ROADMAP", "roadmap"),
            [4..11, 17..24]
        );
        assert!(match_ranges("nothing", "xyz").is_empty());
        assert!(match_ranges("anything", "  ").is_empty());
        // multi-byte text keeps char boundaries
        let text = "café Ünder";
        for r in match_ranges(text, "ünder") {
            assert_eq!(&text[r], "Ünder");
        }
    }

    #[test]
    fn the_count_label_names_each_kind() {
        let mut s = SearchState::default();
        assert_eq!(s.count_label(), "");
        s.query = "x".into();
        assert_eq!(s.count_label(), "No matches");
    }
}
