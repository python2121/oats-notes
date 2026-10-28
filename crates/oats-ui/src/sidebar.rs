//! The left sidebar: the "Views" group (Today, All transcripts) and the "Categories" group (the
//! store's categories as 22 pt hover rows, a "+" header icon that reveals an inline field to add
//! one, right-click → Rename / Delete with an inline rename).

use egui::{Key, Rect, Sense, Stroke, pos2, vec2};
use oats_core::{Category, View};

use crate::app::{Command, with_shortcut};
use crate::theme::Tokens;

/// Inline editing state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SidebarState {
    /// The "new category" field's text while it is open.
    pub adding: Option<String>,
    /// The category being renamed and the field's text.
    pub renaming: Option<(i64, String)>,
    /// Give the open field keyboard focus this frame.
    pub focus: bool,
}

/// What the user did in the sidebar this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum SidebarAction {
    Select(View),
    Add(String),
    Rename(i64, String),
    Delete(i64),
    ToggleSidebar,
}

pub fn sidebar_panel(
    ui: &mut egui::Ui,
    state: &mut SidebarState,
    view: View,
    categories: &[Category],
) -> Vec<SidebarAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();

    // header: the "hide sidebar" icon, right-aligned, so the panel can be closed without the menu
    ui.horizontal(|ui| {
        ui.set_height(26.0);
        group_label(ui, &t, "Views");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(2.0);
            if header_icon(ui, HeaderIcon::HideSidebar)
                .on_hover_text(with_shortcut(
                    ui.ctx(),
                    "Hide sidebar",
                    Command::ToggleSidebar,
                ))
                .clicked()
            {
                actions.push(SidebarAction::ToggleSidebar);
            }
        });
    });
    for (label, v) in [("Today", View::Today), ("All transcripts", View::All)] {
        if hover_row(ui, &t, label, view == v).clicked() {
            actions.push(SidebarAction::Select(v));
        }
    }

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.set_height(26.0);
        group_label(ui, &t, "Categories");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(2.0);
            if header_icon(ui, HeaderIcon::Plus)
                .on_hover_text(with_shortcut(
                    ui.ctx(),
                    "New category",
                    Command::NewCategory,
                ))
                .clicked()
            {
                state.adding = Some(String::new());
                state.renaming = None;
                state.focus = true;
            }
        });
    });

    egui::ScrollArea::vertical()
        .id_salt("sidebar-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for c in categories {
                if let Some((id, text)) = &mut state.renaming
                    && *id == c.id
                {
                    let id = *id;
                    match inline_field(ui, text, &mut state.focus) {
                        Field::Commit => {
                            let name = text.trim().to_owned();
                            if !name.is_empty() && name != c.name {
                                actions.push(SidebarAction::Rename(id, name));
                            }
                            state.renaming = None;
                        }
                        Field::Cancel => state.renaming = None,
                        Field::Editing => {}
                    }
                    continue;
                }
                let resp = hover_row(ui, &t, &c.name, view == View::Category(c.id));
                if resp.clicked() {
                    actions.push(SidebarAction::Select(View::Category(c.id)));
                }
                resp.context_menu(|ui| {
                    if ui.button("Rename…").clicked() {
                        state.renaming = Some((c.id, c.name.clone()));
                        state.adding = None;
                        state.focus = true;
                        ui.close();
                    }
                    if ui.button("Delete").clicked() {
                        actions.push(SidebarAction::Delete(c.id));
                        ui.close();
                    }
                });
            }
            if let Some(text) = &mut state.adding {
                match inline_field(ui, text, &mut state.focus) {
                    Field::Commit => {
                        let name = text.trim().to_owned();
                        if !name.is_empty() {
                            actions.push(SidebarAction::Add(name));
                        }
                        state.adding = None;
                    }
                    Field::Cancel => state.adding = None,
                    Field::Editing => {}
                }
            }
        });
    actions
}

fn group_label(ui: &mut egui::Ui, t: &Tokens, text: &str) {
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(text)
            .font(crate::theme::font_medium(ui.ctx(), 11.0))
            .color(t.text_tertiary),
    );
}

/// A 22 pt sidebar row: hover fill, the selected row in an accent-alpha fill.
fn hover_row(ui: &mut egui::Ui, t: &Tokens, label: &str, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), t.row_h), Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let fill_rect = rect.shrink2(vec2(2.0, 1.0));
    if selected {
        p.rect_filled(fill_rect, 4.0, t.accent_alpha(0.35));
    } else if resp.hovered() || resp.context_menu_opened() {
        p.rect_filled(fill_rect, 4.0, t.hover);
    }
    let galley = p.layout_no_wrap(label.to_owned(), t.font(12.5), t.text);
    let clip = Rect::from_min_max(
        pos2(rect.left() + 12.0, rect.top()),
        pos2(rect.right() - 6.0, rect.bottom()),
    );
    p.with_clip_rect(clip).galley(
        pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0),
        galley,
        t.text,
    );
    resp
}

enum Field {
    Editing,
    Commit,
    Cancel,
}

/// The inline text field (new category, rename): Enter commits, Escape (or clicking away with
/// nothing typed) cancels.
fn inline_field(ui: &mut egui::Ui, text: &mut String, focus: &mut bool) -> Field {
    let mut out = Field::Editing;
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        let resp = ui.add(
            egui::TextEdit::singleline(text)
                .desired_width(ui.available_width() - 6.0)
                .hint_text("Category name"),
        );
        if *focus {
            resp.request_focus();
            *focus = false;
        }
        if resp.lost_focus() {
            out = if ui.input(|i| i.key_pressed(Key::Escape)) {
                Field::Cancel
            } else if ui.input(|i| i.key_pressed(Key::Enter)) || !text.trim().is_empty() {
                Field::Commit
            } else {
                Field::Cancel
            };
        }
    });
    out
}

#[derive(Clone, Copy)]
enum HeaderIcon {
    Plus,
    HideSidebar,
}

/// A 24 pt frameless icon button: hover/press backdrop, the icon stroked in the secondary text
/// colour (primary on hover).
fn header_icon(ui: &mut egui::Ui, icon: HeaderIcon) -> egui::Response {
    let t = Tokens::get(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    if resp.is_pointer_button_down_on() {
        p.rect_filled(rect, 4.0, t.pressed);
    } else if resp.hovered() {
        p.rect_filled(rect, 4.0, t.hover);
    }
    let col = if resp.hovered() {
        t.text
    } else {
        t.text_secondary
    };
    let stroke = Stroke::new(1.5, col);
    // icon drawn on a 24-unit grid inside a 20 pt box, centred in the button
    let s = 20.0 / 24.0;
    let o = rect.center() - vec2(10.0, 10.0);
    let at = |x: f32, y: f32| pos2(o.x + x * s, o.y + y * s);
    match icon {
        HeaderIcon::Plus => {
            p.line_segment([at(12.0, 6.0), at(12.0, 18.0)], stroke);
            p.line_segment([at(6.0, 12.0), at(18.0, 12.0)], stroke);
        }
        HeaderIcon::HideSidebar => {
            // a window with its left pane (the sidebar) shaded
            let frame = Rect::from_min_max(at(3.0, 4.5), at(21.0, 19.5));
            let pane = Rect::from_min_max(frame.min, pos2(at(9.0, 0.0).x, frame.max.y));
            p.rect_filled(pane, 2.0, col.gamma_multiply(0.35));
            p.rect_stroke(frame, 2.0, stroke, egui::StrokeKind::Middle);
            p.line_segment(
                [pos2(pane.max.x, frame.min.y), pos2(pane.max.x, frame.max.y)],
                stroke,
            );
        }
    }
    resp
}
