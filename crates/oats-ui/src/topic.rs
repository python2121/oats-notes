//! The Topic view: every episode tagged with one topic (a topic chip was clicked), as a
//! concise list — date, time range, title, node path — newest first. Clicking a row jumps to
//! the note. Like the search results it sits over the current view and never changes the saved
//! view pref; Escape or × closes it.

use chrono::Local;
use egui::{Align2, RichText, Sense, pos2, vec2};
use oats_core::{Episode, Node};

use crate::notes::{chip_max, node_path};
use crate::sidebar::{HeaderIcon, header_icon};
use crate::theme::Tokens;
use crate::transcript::at;

/// Height of the header bar.
pub const BAR_H: f32 = 36.0;

/// The open topic view.
#[derive(Clone, Debug, PartialEq)]
pub struct TopicState {
    pub topic: String,
    /// Newest first.
    pub episodes: Vec<Episode>,
}

/// What the user did this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum TopicAction {
    Close,
    /// Jump to this episode's note.
    Open(i64),
}

/// "Tagged “topic”", with the count and the close icon, in `ui`'s rect (a bar).
pub fn topic_header(ui: &mut egui::Ui, state: &TopicState) -> bool {
    let t = Tokens::get(ui.ctx());
    let mut close = false;
    let n = state.episodes.len();
    let count = format!("{n} note{}", if n == 1 { "" } else { "s" });
    ui.horizontal_centered(|ui| {
        ui.add_space(14.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            if header_icon(ui, HeaderIcon::Close)
                .on_hover_text("Close (Esc)")
                .clicked()
            {
                close = true;
            }
            ui.add_space(8.0);
            // the title cut with … so the count after it stays in view
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let count_w = ui
                    .painter()
                    .layout_no_wrap(count.clone(), t.font(11.5), t.text_secondary)
                    .size()
                    .x;
                let title_w = ui.available_width() - count_w - ui.spacing().item_spacing.x;
                let title = crate::widgets::truncated_galley(
                    ui,
                    format!("Tagged “{}”", state.topic),
                    crate::theme::font_medium(ui.ctx(), 13.5),
                    t.text,
                    title_w,
                );
                ui.add(egui::Label::new(title).selectable(false));
                ui.label(RichText::new(count).size(11.5).color(t.text_secondary));
            });
        });
    });
    close
}

/// The list, in `ui`'s rect.
pub fn topic_list(ui: &mut egui::Ui, state: &TopicState, nodes: &[Node]) -> Vec<TopicAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    if state.episodes.is_empty() {
        ui.painter().text(
            ui.available_rect_before_wrap().center(),
            Align2::CENTER_CENTER,
            "No notes carry this topic any more.",
            t.font(12.5),
            t.text_tertiary,
        );
        return actions;
    }
    let year = chrono::Datelike::year(&Local::now());
    egui::ScrollArea::vertical()
        .id_salt("topic-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(6.0);
            for e in &state.episodes {
                let (rect, resp) =
                    ui.allocate_exact_size(vec2(ui.available_width() - 12.0, 26.0), Sense::click());
                let rect = rect.translate(vec2(6.0, 0.0));
                if ui.is_rect_visible(rect) {
                    if resp.hovered() {
                        ui.painter().rect_filled(rect, 6.0, t.hover);
                    }
                    let date = at(e.started_at, &Local)
                        .map(|d| {
                            if chrono::Datelike::year(&d) == year {
                                d.format("%b %-d").to_string()
                            } else {
                                d.format("%b %-d, %Y").to_string()
                            }
                        })
                        .unwrap_or_default();
                    let mut cell = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(egui::Rect::from_min_max(
                                pos2(rect.left() + 8.0, rect.top()),
                                pos2(rect.right() - 8.0, rect.bottom()),
                            ))
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    // right to left: the node chip (at most 40 % of the row, cut with …),
                    // then date and time from the left, the title clipped between them
                    cell.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(n) = e.node_id {
                            let max = (ui.available_width() * 0.4).max(60.0);
                            chip_max(ui, &t, &node_path(nodes, n), true, max);
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.label(RichText::new(date).size(11.5).color(t.text_secondary));
                            ui.label(
                                RichText::new(crate::timefmt::range(
                                    e.started_at,
                                    e.ended_at,
                                    &Local,
                                ))
                                .font(t.mono(10.5))
                                .color(t.text_tertiary),
                            );
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&e.title)
                                        .font(crate::theme::font_medium(ui.ctx(), 13.0))
                                        .color(t.text),
                                )
                                .selectable(false)
                                .truncate(),
                            );
                        });
                    });
                }
                if resp
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Show in the notes")
                    .clicked()
                {
                    actions.push(TopicAction::Open(e.id));
                }
            }
            ui.add_space(8.0);
        });
    actions
}
