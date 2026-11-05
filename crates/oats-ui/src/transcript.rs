//! The transcript pane: the date helpers (local midnight, day headings, row times — pure, so
//! they are tested with a fixed time zone) and the list itself (one row per stored segment,
//! grouped under a heading whenever the local day changes).

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use egui::{Align2, Rect, Sense, Vec2, pos2, vec2};
use oats_core::{AudioSourceKind, TranscriptRow};

use crate::move_to::{MoveDraft, move_field};
use oats_brain::MoveTarget;

use crate::theme::Tokens;

/// What the user did in the list this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptAction {
    /// File a segment under a node (`None` = unfiled).
    Move {
        segment: i64,
        category: Option<i64>,
    },
    /// "Move to…" on a row: the text typed in its field.
    MoveText {
        segment: i64,
        text: String,
    },
    Delete {
        segment: i64,
    },
    /// "Speaker ▸ Name…": name a voice (empty clears the name).
    SpeakerName {
        speaker: i64,
        name: String,
    },
    /// "Speaker ▸ This is me".
    SpeakerMe {
        speaker: i64,
    },
    /// "Speaker ▸ Merge into…": the voice the user typed (resolved by the app).
    SpeakerMerge {
        speaker: i64,
        into: String,
    },
    /// "Speaker ▸ Forget this voice".
    SpeakerForget {
        speaker: i64,
    },
    /// "This line was ▸" (one line) / "These N lines were ▸" (the selection): who said them.
    SpeakerReassign {
        segments: Vec<i64>,
        target: oats_core::SpeakerTarget,
    },
    /// Load the page of rows before the first loaded one.
    LoadEarlier,
    /// Load the page of rows after the last loaded one (only after a jump: otherwise the
    /// loaded rows already end at the newest).
    LoadLater,
}

/// The rows picked for a batch action (⌥-click toggles a row, ⇧-click selects the range from
/// the anchor; a plain click or Escape clears it), kept in egui's memory: the Transcripts view
/// and a node's "Show transcript" list draw the same rows. Ids no longer in the list being
/// drawn are dropped, and the app clears it on every view change.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowSelection {
    pub selected: std::collections::BTreeSet<i64>,
    pub anchor: Option<i64>,
}

fn selection_id() -> egui::Id {
    egui::Id::new("oats-row-selection")
}

/// The current row selection.
pub fn row_selection(ctx: &egui::Context) -> RowSelection {
    ctx.data(|d| d.get_temp::<RowSelection>(selection_id()))
        .unwrap_or_default()
}

fn set_row_selection(ctx: &egui::Context, sel: RowSelection) {
    ctx.data_mut(|d| d.insert_temp(selection_id(), sel));
}

/// Drop the row selection (a view change, a batch action done).
pub fn clear_row_selection(ctx: &egui::Context) {
    set_row_selection(ctx, RowSelection::default());
}

/// The selection after a click on row `id` in `order` (the list's display order): ⌥ toggles
/// it (and makes it the anchor), ⇧ selects the range from the anchor (from the row itself
/// without one), a plain click clears. Pure.
pub fn click_selection(
    mut sel: RowSelection,
    id: i64,
    order: &[i64],
    alt: bool,
    shift: bool,
) -> RowSelection {
    if shift {
        let anchor = sel.anchor.unwrap_or(id);
        let at = |x: i64| order.iter().position(|r| *r == x);
        match (at(anchor), at(id)) {
            (Some(a), Some(b)) => {
                let (lo, hi) = (a.min(b), a.max(b));
                sel.selected.extend(order[lo..=hi].iter().copied());
            }
            _ => {
                sel.selected.insert(id);
            }
        }
        sel.anchor = Some(anchor);
    } else if alt {
        if !sel.selected.remove(&id) {
            sel.selected.insert(id);
        }
        sel.anchor = Some(id);
    } else {
        sel = RowSelection::default();
    }
    sel
}

/// Escape (no text field focused) clears a selection; ids not among `rows` are dropped.
fn tend_selection(ui: &egui::Ui, rows: &[TranscriptRow]) {
    let mut sel = row_selection(ui.ctx());
    if sel.selected.is_empty() {
        return;
    }
    let before = sel.clone();
    if !ui.ctx().egui_wants_keyboard_input()
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        sel = RowSelection::default();
    }
    let ids: std::collections::HashSet<i64> = rows.iter().map(|r| r.id).collect();
    sel.selected.retain(|id| ids.contains(id));
    if sel.anchor.is_some_and(|a| !ids.contains(&a)) {
        sel.anchor = None;
    }
    if sel != before {
        set_row_selection(ui.ctx(), sel);
    }
}

/// Rows per page of the Transcripts view.
pub const PAGE_ROWS: usize = 1000;

/// Start loading the next page when its edge is within this many viewport heights of the
/// visible area, so the rows are there before the user scrolls to them.
pub const PREFETCH_VIEWPORTS: f32 = 1.5;

/// Whether `rect` (a list edge marker, in screen coordinates) is within the prefetch distance
/// of the visible `viewport`.
pub fn near_viewport(rect: Rect, viewport: Rect) -> bool {
    viewport
        .expand2(vec2(0.0, PREFETCH_VIEWPORTS * viewport.height()))
        .intersects(rect)
}

/// The Transcripts view's data: ONE contiguous range of rows, oldest first, grown a page at a
/// time at either end, plus the bookkeeping that keeps the scroll position still when rows are
/// inserted above the visible part. There is no eviction: a row is a line of text.
#[derive(Debug, Default)]
pub struct TranscriptPages {
    pub rows: Vec<TranscriptRow>,
    /// Older rows exist beyond the first loaded one (`false`: "Beginning of history").
    pub has_earlier: bool,
    /// Newer rows exist beyond the last loaded one (only after a jump). While `false`, live
    /// segments are appended and the list sticks to the bottom.
    pub has_later: bool,
    /// The row at (or just above) the viewport top and its y in content coordinates, as of the
    /// last layout without a pending insert.
    anchor: Option<(i64, f32)>,
    /// Rows were inserted above since `anchor` was recorded: the next layout measures how far
    /// the anchor moved and scrolls by that much.
    armed: bool,
    /// The viewport was at the end of the content at the last layout (egui keeps it there by
    /// itself, so there is nothing to correct).
    at_end: bool,
    /// "Jump to now": the next layout puts the viewport at the end of the content (and the
    /// stick-to-bottom keeps it there).
    pub jump_to_end: bool,
}

impl TranscriptPages {
    /// Replace everything (the initial load, a jump).
    pub fn set(&mut self, rows: Vec<TranscriptRow>, has_earlier: bool, has_later: bool) {
        self.rows = rows;
        self.has_earlier = has_earlier;
        self.has_later = has_later;
        self.anchor = None;
        self.armed = false;
    }

    /// Put older rows (oldest first) in front.
    pub fn prepend(&mut self, mut older: Vec<TranscriptRow>, has_earlier: bool) {
        self.has_earlier = has_earlier;
        if !older.is_empty() {
            older.append(&mut self.rows);
            self.rows = older;
            self.armed = true;
        }
    }

    /// Put newer rows (oldest first) at the end.
    pub fn append(&mut self, newer: Vec<TranscriptRow>, has_later: bool) {
        self.has_later = has_later;
        self.rows.extend(newer);
    }

    /// The viewport showed the end of the content at the last layout.
    pub fn at_end(&self) -> bool {
        self.at_end
    }

    pub fn contains_row(&self, id: i64) -> bool {
        self.rows.iter().any(|r| r.id == id)
    }

    /// Drop a row (an echo that was marked, a deletion). `true` when it was loaded.
    pub fn remove_row(&mut self, id: i64) -> bool {
        let before = self.rows.len();
        self.rows.retain(|r| r.id != id);
        self.rows.len() < before
    }

    /// `(started_at, id)` of the first / last loaded row, the paging keys.
    pub fn first_key(&self) -> Option<(i64, i64)> {
        self.rows.first().map(|r| (r.started_at, r.id))
    }

    pub fn last_key(&self) -> Option<(i64, i64)> {
        self.rows.last().map(|r| (r.started_at, r.id))
    }
}

/// The jump-to-a-line mechanism (a calendar item or todo → where it was said): the row to
/// scroll to and flash when the list lays it out (consumed there), and the flash in progress.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptFocus {
    pub pending: Option<i64>,
    /// The row flashing right now and the egui time the flash started.
    pub flash: Option<(i64, f64)>,
}

/// How long a jumped-to row flashes.
const FLASH_SECS: f64 = 1.5;

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

/// "14:05" or "2:05 pm" (the clock preference; see `timefmt`).
pub fn clock<Tz: TimeZone>(ms: i64, tz: &Tz) -> String {
    crate::timefmt::clock(ms, tz)
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

/// A thin muted line at the top / bottom of the list; returns its rect.
fn edge_marker(ui: &mut egui::Ui, t: &Tokens, text: &str) -> Rect {
    let r = ui
        .horizontal(|ui| {
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new(text)
                    .font(t.font(11.5))
                    .color(t.text_tertiary),
            );
        })
        .response
        .rect;
    ui.add_space(4.0);
    r
}

/// The Transcripts list: one contiguous range of rows, oldest at the top, a day heading wherever
/// the local day changes between consecutive rows (so an empty day never shows). The scroll
/// area sticks to the bottom while the range ends at the newest row: a new segment scrolls into
/// view while the user is at the end; once they scroll up it stays where they left it.
///
/// Paging: the top marker ("Loading earlier…" / "Beginning of history") and, after a jump, the
/// bottom marker ask for their page ([`TranscriptAction::LoadEarlier`] / `LoadLater`) as soon as
/// they come within [`PREFETCH_VIEWPORTS`] viewport heights of the visible area. Rows inserted
/// above the viewport shift everything below them down: the row that was at the viewport top is
/// remembered and the scroll offset is moved by however far it went, in the same frame (the
/// pass is discarded and redone, so nothing is painted at the old offset).
pub fn transcript_list(
    ui: &mut egui::Ui,
    pages: &mut TranscriptPages,
    draft: &mut Option<MoveDraft>,
    focus: &mut TranscriptFocus,
) -> Vec<TranscriptAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    if pages.rows.is_empty() && !pages.has_earlier && !pages.has_later {
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
    let this_year = Local::now().year();
    let was_armed = pages.armed;
    let at_end_before = pages.at_end;
    // a jump scrolls to its row (a smooth scroll toward an absolute offset): load nothing
    // meanwhile, rows inserted above would move the row out from under it
    let jumping = focus.pending.is_some() || focus.flash.is_some();
    // the first layout of a new range has not stuck to the bottom yet (egui needs one pass to
    // know the content size): looking at the edges then would load a page nobody needs
    let settled = pages.anchor.is_some();
    let jump_end = std::mem::take(&mut pages.jump_to_end);
    let emit = !was_armed && !jumping && settled && !jump_end;
    if was_armed || !settled || jump_end {
        ui.ctx().request_repaint();
    }
    let mut correction = 0.0_f32;
    let mut new_anchor: Option<(i64, f32)> = None;
    let anchor = pages.anchor;
    let rows = &pages.rows;
    tend_selection(ui, rows);
    let (has_earlier, has_later) = (pages.has_earlier, pages.has_later);
    let breaks = day_breaks(rows, &Local);
    let out = egui::ScrollArea::vertical()
        .id_salt("transcript-scroll")
        .auto_shrink([false, false])
        // a jump scrolls to its row instead of the end; with newer rows still unloaded the end
        // is not the end of the transcript
        .stick_to_bottom(!jumping && !has_later)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let origin = ui.min_rect().top();
            let view = ui.clip_rect();
            ui.add_space(6.0);
            let r = edge_marker(
                ui,
                &t,
                if has_earlier {
                    "Loading earlier…"
                } else {
                    "Beginning of history"
                },
            );
            if emit && has_earlier && near_viewport(r, view) {
                actions.push(TranscriptAction::LoadEarlier);
            }
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
                let rect = transcript_row(ui, &t, row, rows, draft, &mut actions);
                focus_row(ui, &t, focus, row.id, rect);
                let y = rect.top() - origin;
                if was_armed {
                    if let Some((id, old)) = anchor
                        && id == row.id
                    {
                        correction = y - old;
                    }
                } else if new_anchor.is_none() || rect.top() <= view.top() + 1.0 {
                    new_anchor = Some((row.id, y));
                }
            }
            if has_later {
                ui.add_space(10.0);
                let r = edge_marker(ui, &t, "Loading later…");
                if emit && near_viewport(r, view) {
                    actions.push(TranscriptAction::LoadLater);
                }
            }
            ui.add_space(8.0);
        });
    pages.at_end = out.state.offset.y + out.inner_rect.height() >= out.content_size.y - 2.0;
    if jump_end && !pages.at_end {
        // "Jump to now": straight to the end (no animation, so no page loads on the way)
        let mut state = out.state;
        state.offset.y = (out.content_size.y - out.inner_rect.height()).max(0.0);
        state.store(ui.ctx(), out.id);
        pages.at_end = true;
        pages.anchor = None;
        ui.ctx()
            .request_discard("jump to the newest transcript row");
        return actions;
    }
    if was_armed {
        pages.armed = false;
        if correction.abs() > 0.5 && !at_end_before {
            let mut state = out.state;
            state.offset.y = (state.offset.y + correction).max(0.0);
            state.store(ui.ctx(), out.id);
            if !jumping {
                ui.ctx()
                    .request_discard("transcript rows were inserted above");
            }
        }
    } else {
        pages.anchor = new_anchor;
    }
    if jumping && focus.pending.is_none() && focus.flash.is_none() {
        // the jump is over: look at the edges once more
        ui.ctx().request_repaint();
    }
    actions
}

/// Scroll to the row when it is the pending focus, then flash an accent fill that fades over
/// [`FLASH_SECS`] (repaints are requested only while it fades).
pub(crate) fn focus_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    focus: &mut TranscriptFocus,
    id: i64,
    rect: Rect,
) {
    if focus.pending == Some(id) {
        focus.pending = None;
        ui.scroll_to_rect(rect, Some(egui::Align::Center));
        focus.flash = Some((id, ui.input(|i| i.time)));
    }
    let Some((fid, start)) = focus.flash else {
        return;
    };
    if fid != id {
        return;
    }
    let left = 1.0 - (ui.input(|i| i.time) - start) / FLASH_SECS;
    if left <= 0.0 {
        focus.flash = None;
        return;
    }
    ui.painter()
        .rect_filled(rect, 4.0, t.accent_alpha(0.28 * left as f32));
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(33));
}

/// The rows with day headings, laid out in the caller's scroll area (a node view's collapsible
/// transcript section).
pub fn transcript_rows(
    ui: &mut egui::Ui,
    rows: &[TranscriptRow],
    draft: &mut Option<MoveDraft>,
) -> Vec<TranscriptAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    tend_selection(ui, rows);
    let breaks = day_breaks(rows, &Local);
    let this_year = Local::now().year();
    let mut next_break = breaks.iter().peekable();
    for (i, row) in rows.iter().enumerate() {
        if let Some((_, day)) = next_break.next_if(|(at, _)| *at == i) {
            let heading = day
                .map(|d| day_heading(d, this_year))
                .unwrap_or_else(|| "Unknown date".to_owned());
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new(heading)
                        .font(crate::theme::font_medium(ui.ctx(), 12.5))
                        .color(t.text_secondary),
                );
            });
            ui.add_space(4.0);
        }
        transcript_row(ui, &t, row, rows, draft, &mut actions);
    }
    actions
}

/// Width of the time column.
const TIME_W: f32 = 60.0;

/// The label a row's lane gets in the list ("them" for system audio, "media" for a YouTube
/// video / music on it, none for the microphone); echoes are never listed.
pub fn source_label(row: &TranscriptRow) -> Option<&'static str> {
    match row.source {
        AudioSourceKind::Mic => None,
        AudioSourceKind::System if row.media_kind.is_some_and(|k| k.is_media()) => Some("media"),
        AudioSourceKind::System => Some("them"),
    }
}

/// One row; returns its rect (the hover / flash area).
fn transcript_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    row: &TranscriptRow,
    rows: &[TranscriptRow],
    draft: &mut Option<MoveDraft>,
    actions: &mut Vec<TranscriptAction>,
) -> Rect {
    // the hover fill goes UNDER the text: reserve its slot first, fill it once the row's height
    // is known
    let fill = ui.painter().add(egui::Shape::Noop);
    let left = ui.max_rect().left();
    let right = ui.max_rect().right();
    let voices = crate::speakers::map(ui.ctx());
    // The row's own click target (hover fill, right-click menu) is registered BEFORE its labels
    // with a placeholder rect, and given its real rect after them (egui keeps a widget's place
    // in the hit-test order on re-registration): the selectable labels are then ON TOP of it,
    // so a press on the text starts a text selection (click_and_drag) that can run across rows,
    // instead of the row taking the press. The labels' right-clicks open the row's menu too.
    let row_id = ui.id().with(("segment", row.id));
    ui.interact(
        Rect::from_min_size(pos2(left + 4.0, ui.cursor().top() - 2.0), Vec2::ZERO),
        row_id,
        Sense::CLICK,
    );
    let mut labels: Option<egui::Response> = None;
    let inner = ui
        .horizontal_top(|ui| {
            ui.add_space(8.0);
            // every row carries its speaker under the time, the user's own lines as "me"
            let tag = crate::speakers::row_label(row, &voices)
                .unwrap_or_else(|| crate::speakers::me_label(&voices));
            let (time_rect, time_resp) = ui.allocate_exact_size(vec2(TIME_W, 32.0), Sense::hover());
            // a real label: in the accessibility tree, and selectable with the row's text
            let time_label = ui.put(
                Rect::from_min_size(
                    pos2(time_rect.left() + 4.0, time_rect.top() + 3.0),
                    vec2(TIME_W - 4.0, 14.0),
                ),
                egui::Label::new(
                    egui::RichText::new(crate::timefmt::clock_col(row.started_at, &Local))
                        .font(t.mono(10.5))
                        .color(t.text_tertiary),
                )
                .selectable(true)
                .wrap_mode(egui::TextWrapMode::Extend),
            );
            let (at, max_w) = crate::speakers::tag_pos(time_rect);
            crate::speakers::paint_tag(ui, t, at, &tag, max_w);
            // the speaker's hover text over the time too (the label sits on top of the slot)
            time_resp.union(time_label.clone()).on_hover_text(tag.hover);
            let w = (ui.available_width() - 44.0).max(60.0);
            ui.vertical(|ui| {
                ui.set_width(w);
                ui.add_space(1.0);
                let text = ui.add(
                    egui::Label::new(egui::RichText::new(&row.text).color(t.text).size(13.0))
                        .wrap()
                        .selectable(true),
                );
                // The label's hit area is widened to the whole text column AND the row's full
                // height (egui keeps the LAST rect registered under an id for the next frame's
                // hit test): a press in the empty space right of a line, or below a line that
                // doesn't wrap (the time column with its speaker tag makes the row taller than
                // one line of text), lands on the label, and egui's cursor mapping clamps it
                // to the end of the text, so dragging from there selects from the line's end,
                // as in any text view. The layout is untouched (nothing is allocated), and the
                // focus bit is left off like the label's own sense.
                // The union keeps the label's own rect inside the wide one: a wrapped label's
                // galley is rounded to whole pixels and can run a fraction past `w`, and a
                // re-registered rect that neither contains nor sits inside the first is what
                // egui's debug "Double use of widget ID" check draws.
                let wide = Rect::from_min_max(
                    pos2(text.rect.left(), time_rect.top() - 2.0),
                    pos2(
                        text.rect.left() + w,
                        text.rect.bottom().max(time_rect.bottom()) + 2.0,
                    ),
                )
                .union(text.rect);
                ui.interact(wide, text.id, Sense::CLICK | Sense::DRAG);
                labels = Some(time_label.union(text));
            });
        })
        .response;
    let rect = Rect::from_min_max(
        pos2(left + 4.0, inner.rect.top() - 2.0),
        pos2(right - 4.0, inner.rect.bottom() + 2.0),
    );
    let mut resp = ui.interact(rect, row_id, Sense::click());
    if let Some(labels) = labels {
        // keeps the row's id (the menu's state) and adds the labels' clicks
        resp = resp.union(labels);
    }
    // ⌥-click / ⇧-click pick rows for a batch action, a plain click drops the pick (the
    // modifiers of the click itself: the frame's may already be released)
    if resp.clicked() {
        let mods = ui.input(|i| {
            i.events
                .iter()
                .rev()
                .find_map(|e| match e {
                    egui::Event::PointerButton {
                        button: egui::PointerButton::Primary,
                        modifiers,
                        ..
                    } => Some(*modifiers),
                    _ => None,
                })
                .unwrap_or(i.modifiers)
        });
        let sel = row_selection(ui.ctx());
        if mods.alt || mods.shift || !sel.selected.is_empty() {
            let order: Vec<i64> = rows.iter().map(|r| r.id).collect();
            set_row_selection(
                ui.ctx(),
                click_selection(sel, row.id, &order, mods.alt, mods.shift),
            );
        }
        if mods.alt || mods.shift {
            // a modified click picks the row; it must not leave a text selection behind
            ui.ctx()
                .plugin::<egui::text_selection::LabelSelectionState>()
                .lock()
                .clear_selection();
        }
    }
    // a right-click outside the selection is about that row alone
    if resp.secondary_clicked() {
        let sel = row_selection(ui.ctx());
        if !sel.selected.is_empty() && !sel.selected.contains(&row.id) {
            clear_row_selection(ui.ctx());
        }
    }
    let selection = row_selection(ui.ctx());
    let selected = selection.selected.contains(&row.id);
    // `contains_pointer`, not `hovered`: over the text the label on top is the hovered widget
    if selected {
        ui.painter().set(
            fill,
            egui::Shape::rect_filled(rect, 4.0, t.accent_alpha(0.18)),
        );
    }
    if resp.contains_pointer() || resp.context_menu_opened() {
        if !selected {
            ui.painter().set(
                fill,
                egui::Shape::rect_filled(rect, 4.0, t.hover.gamma_multiply(0.6)),
            );
        }
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
        if ui.button("Move to…").clicked() {
            *draft = Some(MoveDraft::new(MoveTarget::Segment(row.id), row.node_id));
            ui.close();
        }
        if ui
            .add_enabled(row.node_id.is_some(), egui::Button::new("Unfile"))
            .clicked()
        {
            actions.push(TranscriptAction::Move {
                segment: row.id,
                category: None,
            });
            ui.close();
        }
        if ui.button("Copy Text").clicked() {
            ui.ctx().copy_text(row.text.clone());
            ui.close();
        }
        if row.echo_of.is_none() && !row.media_kind.is_some_and(|k| k.is_media()) {
            ui.separator();
            let picked: Vec<i64> = selection.selected.iter().copied().collect();
            // the picked lines' voice when they all share one (greyed in the picker)
            let mut picked_voices = rows
                .iter()
                .filter(|r| selection.selected.contains(&r.id))
                .map(|r| r.speaker_id);
            let first = picked_voices.next().flatten();
            let shared = picked_voices.all(|v| v == first).then_some(first).flatten();
            if let Some(a) = crate::speakers::speaker_menu(ui, row, &voices, &picked, shared) {
                actions.push(a);
            }
        }
        ui.separator();
        if ui.button("Delete").clicked() {
            actions.push(TranscriptAction::Delete { segment: row.id });
            ui.close();
        }
    });
    if let Some(text) = move_field(ui, t, draft, MoveTarget::Segment(row.id)) {
        actions.push(TranscriptAction::MoveText {
            segment: row.id,
            text,
        });
    }
    if let Some(a) = crate::speakers::speaker_field(ui, t, row.id) {
        actions.push(a);
    }
    ui.add_space(2.0);
    rect
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_and_shift_clicks_pick_rows_and_a_plain_click_drops_them() {
        let order = [10, 11, 12, 13, 14];
        let none = RowSelection::default();
        let a = click_selection(none.clone(), 11, &order, true, false);
        assert_eq!(a.selected.iter().copied().collect::<Vec<_>>(), vec![11]);
        assert_eq!(a.anchor, Some(11));
        let b = click_selection(a.clone(), 13, &order, true, false);
        assert_eq!(b.selected.len(), 2);
        // ⌥ again on a picked row drops it
        let c = click_selection(b.clone(), 11, &order, true, false);
        assert_eq!(c.selected.iter().copied().collect::<Vec<_>>(), vec![13]);
        // ⇧ selects the range from the anchor (both directions)
        let d = click_selection(a.clone(), 14, &order, false, true);
        assert_eq!(
            d.selected.iter().copied().collect::<Vec<_>>(),
            vec![11, 12, 13, 14]
        );
        let e = click_selection(
            click_selection(none.clone(), 13, &order, true, false),
            10,
            &order,
            false,
            true,
        );
        assert_eq!(
            e.selected.iter().copied().collect::<Vec<_>>(),
            vec![10, 11, 12, 13]
        );
        // a plain click clears
        assert_eq!(click_selection(d, 12, &order, false, false), none);
    }
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
            ..TranscriptRow::default()
        }
    }

    #[test]
    fn system_rows_are_tagged_them() {
        assert_eq!(source_label(&row(1, 0)), None);
        let them = TranscriptRow {
            source: AudioSourceKind::System,
            ..row(2, 0)
        };
        assert_eq!(source_label(&them), Some("them"));
        // a YouTube video / music on the system lane is media, a call stays "them"
        let video = TranscriptRow {
            media_kind: Some(oats_core::MediaClass::YouTube),
            ..them.clone()
        };
        assert_eq!(source_label(&video), Some("media"));
        let call = TranscriptRow {
            media_kind: Some(oats_core::MediaClass::Meeting),
            ..them
        };
        assert_eq!(source_label(&call), Some("them"));
    }

    #[test]
    fn headings_and_clock() {
        let tz = FixedOffset::east_opt(3600).unwrap();
        // 2026-10-27 13:05 UTC = 14:05 at +01:00, a Tuesday
        let ms = 1_793_106_300_000;
        let day = day_of(ms, &tz);
        assert_eq!(day, NaiveDate::from_ymd_opt(2026, 10, 27));
        assert_eq!(crate::timefmt::clock_with(ms, &tz, true), "14:05");
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

    #[test]
    fn the_prefetch_distance_is_one_and_a_half_viewports() {
        let view = Rect::from_min_size(pos2(0.0, 1000.0), vec2(400.0, 600.0));
        let marker = |y: f32| Rect::from_min_size(pos2(0.0, y), vec2(400.0, 20.0));
        // inside the viewport, and just under 900 px above / below it (1.5 x 600): near
        assert!(near_viewport(marker(1100.0), view));
        assert!(near_viewport(marker(1000.0 - 890.0), view));
        assert!(near_viewport(marker(1600.0 + 890.0), view));
        // beyond 1.5 viewports: not yet
        assert!(!near_viewport(marker(1000.0 - 900.0 - 21.0), view));
        assert!(!near_viewport(marker(1600.0 + 901.0), view));
    }

    #[test]
    fn pages_grow_at_both_ends_and_arm_the_scroll_correction_only_for_rows_above() {
        let mut p = TranscriptPages::default();
        p.set(vec![row(5, 50), row(6, 60)], true, true);
        assert!(!p.armed);
        p.append(vec![row(7, 70)], false);
        assert!(!p.armed, "rows below shift nothing");
        assert!(!p.has_later);
        p.prepend(vec![row(3, 30), row(4, 40)], true);
        assert!(p.armed);
        assert_eq!(p.first_key(), Some((30, 3)));
        assert_eq!(p.last_key(), Some((70, 7)));
        p.armed = false;
        p.prepend(vec![], false);
        assert!(!p.armed && !p.has_earlier, "a short page ends the history");
        assert!(p.contains_row(6) && p.remove_row(6) && !p.remove_row(6));
        assert_eq!(p.rows.len(), 4);
    }

    /// ⌘C copies the selection; the first row's words come before the second's.
    #[cfg(feature = "selftest")]
    fn assert_copied_in_order(harness: &mut egui_kittest::Harness<'_>) {
        harness.event(egui::Event::Copy);
        harness.step();
        let copied = harness
            .output()
            .platform_output
            .commands
            .iter()
            .find_map(|c| match c {
                egui::OutputCommand::CopyText(s) => Some(s.clone()),
                _ => None,
            })
            .expect("⌘C copied the selection");
        let a = copied
            .find("alpha bravo charlie")
            .expect("first row copied");
        let d = copied
            .find("delta echo foxtrot")
            .expect("second row copied");
        assert!(a < d, "reading order: {copied:?}");
    }

    /// A drag from the first row's text into the second's selects across both rows (the row's
    /// own click target sits under the labels), ⌘C copies them in reading order, and a
    /// right-click on the text still opens the row's menu.
    #[cfg(feature = "selftest")]
    #[test]
    fn transcript_text_is_selectable_across_rows_and_keeps_the_row_menu() {
        use egui_kittest::kittest::Queryable;
        let rows = vec![
            TranscriptRow {
                text: "alpha bravo charlie".to_owned(),
                ..row(1, 1_700_000_000_000)
            },
            TranscriptRow {
                text: "delta echo foxtrot".to_owned(),
                ..row(2, 1_700_000_060_000)
            },
        ];
        let mut draft = None;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(vec2(600.0, 300.0))
            .build_ui(move |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    transcript_rows(ui, &rows, &mut draft);
                });
            });
        harness.run();
        let first = harness.get_by_label("alpha bravo charlie").rect();
        let second = harness.get_by_label("delta echo foxtrot").rect();
        assert!(first.bottom() <= second.top());

        let start = pos2(first.left() + 1.0, first.center().y);
        let end = pos2(second.right() - 1.0, second.center().y);
        harness.hover_at(start);
        harness.run();
        harness.drag_at(start);
        harness.run();
        harness.hover_at(pos2(start.x + 20.0, start.y + 10.0));
        harness.run();
        harness.hover_at(end);
        harness.run();
        harness.drop_at(end);
        harness.run();
        assert_copied_in_order(&mut harness);

        // a press in the empty space RIGHT of the second row's text, dragged up-left into the
        // first row: the selection is anchored at the end of the second line
        let beyond = pos2(second.right() + 60.0, second.center().y);
        harness.hover_at(beyond);
        harness.run();
        harness.drag_at(beyond);
        harness.run();
        harness.hover_at(pos2(beyond.x - 30.0, beyond.y - 10.0));
        harness.run();
        harness.hover_at(start);
        harness.run();
        harness.drop_at(start);
        harness.run();
        assert_copied_in_order(&mut harness);

        // a press in the empty space BELOW the first row's one-line text (beside the speaker
        // tag, inside the row), dragged down to the end of the second row: the selection is
        // anchored at the end of the first line
        let below = pos2(first.center().x, first.bottom() + 10.0);
        harness.hover_at(below);
        harness.run();
        harness.drag_at(below);
        harness.run();
        harness.hover_at(pos2(below.x + 20.0, below.y + 10.0));
        harness.run();
        harness.hover_at(end);
        harness.run();
        harness.drop_at(end);
        harness.run();
        assert_copied_in_order(&mut harness);

        // right-click on the text: the row's menu
        let at = second.center();
        harness.hover_at(at);
        harness.run();
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        harness.run();
        assert!(harness.query_by_label("Copy Text").is_some());
    }
}
