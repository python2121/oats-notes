//! The left sidebar: the "Views" group (Today, All transcripts, Notes, Calendar) and the tree
//! group — the store's categories as 22 pt hover rows, with the projects the brain created
//! under them and topics under those, collapsible per node (a disclosure triangle on rows with
//! children, indented SwiftInvert-style); a "+" header icon that reveals an inline field to add
//! a category; right-click → Rename / Delete with an inline rename. Selecting a node shows
//! its notes (see `notes::node_view`).

use std::collections::HashSet;

use egui::{Key, Rect, Sense, Stroke, pos2, vec2};
use oats_core::{Node, View};

use crate::app::{Command, with_shortcut};
use crate::theme::Tokens;

/// Inline editing and collapse state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SidebarState {
    /// The "new category" field's text while it is open.
    pub adding: Option<String>,
    /// The node being renamed and the field's text.
    pub renaming: Option<(i64, String)>,
    /// Give the open field keyboard focus this frame.
    pub focus: bool,
    /// Nodes whose children are hidden.
    pub collapsed: HashSet<i64>,
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

/// The visible rows of the tree in order: `(node, depth)`; archived nodes and the children
/// of collapsed nodes are left out.
pub fn visible_rows<'a>(nodes: &'a [Node], collapsed: &HashSet<i64>) -> Vec<(&'a Node, usize)> {
    fn walk<'a>(
        nodes: &'a [Node],
        parent: Option<i64>,
        depth: usize,
        collapsed: &HashSet<i64>,
        out: &mut Vec<(&'a Node, usize)>,
    ) {
        let mut children: Vec<&Node> = nodes
            .iter()
            .filter(|n| n.parent_id == parent && !n.archived)
            .collect();
        children.sort_by_key(|n| (n.position, n.id));
        for c in children {
            out.push((c, depth));
            if !collapsed.contains(&c.id) && depth < 8 {
                walk(nodes, Some(c.id), depth + 1, collapsed, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(nodes, None, 0, collapsed, &mut out);
    out
}

fn has_children(nodes: &[Node], id: i64) -> bool {
    nodes.iter().any(|n| n.parent_id == Some(id) && !n.archived)
}

pub fn sidebar_panel(
    ui: &mut egui::Ui,
    state: &mut SidebarState,
    view: View,
    nodes: &[Node],
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
    for (label, v) in [
        ("Today", View::Today),
        ("All transcripts", View::All),
        ("Notes", View::Notes),
        ("Calendar", View::Calendar),
    ] {
        if hover_row(ui, &t, label, view == v, 0, None).clicked() {
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
            let rows = visible_rows(nodes, &state.collapsed);
            let mut toggle: Option<i64> = None;
            for (n, depth) in rows {
                if let Some((id, text)) = &mut state.renaming
                    && *id == n.id
                {
                    let id = *id;
                    match inline_field(ui, text, &mut state.focus) {
                        Field::Commit => {
                            let name = text.trim().to_owned();
                            if !name.is_empty() && name != n.name {
                                actions.push(SidebarAction::Rename(id, name));
                            }
                            state.renaming = None;
                        }
                        Field::Cancel => state.renaming = None,
                        Field::Editing => {}
                    }
                    continue;
                }
                let disclosure =
                    has_children(nodes, n.id).then_some(!state.collapsed.contains(&n.id));
                let resp = hover_row(ui, &t, &n.name, view == View::Node(n.id), depth, disclosure);
                if resp.clicked() {
                    // a click on the triangle folds; anywhere else selects
                    let on_triangle = disclosure.is_some()
                        && resp.interact_pointer_pos().is_some_and(|p| {
                            p.x < resp.rect.left() + 12.0 + depth as f32 * 14.0 + 14.0
                        });
                    if on_triangle {
                        toggle = Some(n.id);
                    } else {
                        actions.push(SidebarAction::Select(View::Node(n.id)));
                    }
                }
                if resp.double_clicked() && disclosure.is_some() {
                    toggle = Some(n.id);
                }
                if !n.description.is_empty() {
                    resp.clone().on_hover_text(&n.description);
                }
                resp.context_menu(|ui| {
                    if ui.button("Rename…").clicked() {
                        state.renaming = Some((n.id, n.name.clone()));
                        state.adding = None;
                        state.focus = true;
                        ui.close();
                    }
                    if disclosure.is_some() {
                        let label = if state.collapsed.contains(&n.id) {
                            "Expand"
                        } else {
                            "Collapse"
                        };
                        if ui.button(label).clicked() {
                            toggle = Some(n.id);
                            ui.close();
                        }
                    }
                    if ui.button("Delete").clicked() {
                        actions.push(SidebarAction::Delete(n.id));
                        ui.close();
                    }
                });
                if n.parent_id.is_none() && !has_children(nodes, n.id) {
                    empty_row(ui, &t, "No projects yet");
                }
            }
            if let Some(id) = toggle
                && !state.collapsed.remove(&id)
            {
                state.collapsed.insert(id);
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

/// A muted, non-interactive placeholder row one level under a category.
fn empty_row(ui: &mut egui::Ui, t: &Tokens, label: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), t.row_h), Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().text(
            pos2(rect.left() + 12.0 + 14.0 + 12.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            t.font(11.5),
            t.text_tertiary,
        );
    }
}

fn group_label(ui: &mut egui::Ui, t: &Tokens, text: &str) {
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(text)
            .font(crate::theme::font_medium(ui.ctx(), 11.0))
            .color(t.text_tertiary),
    );
}

/// A 22 pt sidebar row: hover fill, the selected row in an accent-alpha fill, indented by
/// `depth`, with a disclosure triangle (`Some(open)`) for rows that have children.
fn hover_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    label: &str,
    selected: bool,
    depth: usize,
    disclosure: Option<bool>,
) -> egui::Response {
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
    let indent = 12.0 + depth as f32 * 14.0;
    let text_x = if depth > 0 || disclosure.is_some() {
        indent + 12.0
    } else {
        indent
    };
    if let Some(open) = disclosure {
        let c = pos2(rect.left() + indent + 4.0, rect.center().y);
        let col = t.text_secondary;
        let tri = if open {
            [
                pos2(c.x - 3.5, c.y - 2.0),
                pos2(c.x + 3.5, c.y - 2.0),
                pos2(c.x, c.y + 2.5),
            ]
        } else {
            [
                pos2(c.x - 2.0, c.y - 3.5),
                pos2(c.x - 2.0, c.y + 3.5),
                pos2(c.x + 2.5, c.y),
            ]
        };
        p.add(egui::Shape::convex_polygon(tri.to_vec(), col, Stroke::NONE));
    }
    let size = if depth > 0 { 12.0 } else { 12.5 };
    let color = if depth > 0 { t.text_secondary } else { t.text };
    let galley = p.layout_no_wrap(label.to_owned(), t.font(size), color);
    let clip = Rect::from_min_max(
        pos2(rect.left() + text_x, rect.top()),
        pos2(rect.right() - 6.0, rect.bottom()),
    );
    p.with_clip_rect(clip).galley(
        pos2(
            rect.left() + text_x,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        color,
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
                .hint_text("Name"),
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
pub(crate) enum HeaderIcon {
    Plus,
    HideSidebar,
    /// Same panel glyph, for the restore button in the control bar.
    ShowSidebar,
    /// An ×, for closing a bar (search).
    Close,
}

/// A 24 pt frameless icon button: hover/press backdrop, the icon stroked in the secondary text
/// colour (primary on hover).
pub(crate) fn header_icon(ui: &mut egui::Ui, icon: HeaderIcon) -> egui::Response {
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
        HeaderIcon::Close => {
            p.line_segment([at(7.0, 7.0), at(17.0, 17.0)], stroke);
            p.line_segment([at(17.0, 7.0), at(7.0, 17.0)], stroke);
        }
        HeaderIcon::HideSidebar | HeaderIcon::ShowSidebar => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use oats_core::{NodeKind, Origin};

    fn node(id: i64, parent: Option<i64>, name: &str, position: i64, archived: bool) -> Node {
        Node {
            id,
            parent_id: parent,
            kind: if parent.is_none() {
                NodeKind::Category
            } else {
                NodeKind::Project
            },
            name: name.into(),
            description: String::new(),
            created_by: Origin::User,
            position,
            archived,
        }
    }

    #[test]
    fn tree_rows_respect_order_collapse_and_archive() {
        let nodes = vec![
            node(2, None, "Admin", 1, false),
            node(1, None, "Work", 0, false),
            node(10, Some(1), "Exporter", 0, false),
            node(11, Some(10), "CSV", 0, false),
            node(12, Some(1), "Old", 1, true),
            node(13, Some(2), "Apartment", 0, false),
        ];
        let rows = visible_rows(&nodes, &HashSet::new());
        assert_eq!(
            rows.iter().map(|(n, d)| (n.id, *d)).collect::<Vec<_>>(),
            vec![(1, 0), (10, 1), (11, 2), (2, 0), (13, 1)]
        );
        let collapsed: HashSet<i64> = [1].into_iter().collect();
        let rows = visible_rows(&nodes, &collapsed);
        assert_eq!(
            rows.iter().map(|(n, _)| n.id).collect::<Vec<_>>(),
            vec![1, 2, 13]
        );
        assert!(has_children(&nodes, 1));
        assert!(!has_children(&nodes, 13));
        assert!(!has_children(&nodes, 12));
    }
}
