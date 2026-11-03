//! The left sidebar: the "Views" group (Transcripts, Conversations, Calendar, Todos, Media) and the tree
//! group — the store's categories as 22 pt hover rows, with the projects the brain created
//! under them and topics under those, collapsible per node (a disclosure triangle on rows with
//! children, indented SwiftInvert-style); a "+" header icon that reveals an inline field to add
//! a category; right-click → Rename / Move to… (re-parent, resolved in the tree) / Delete with
//! an inline field. Selecting a node shows its notes (see `notes::node_view`). A project's
//! note rows have the card's context menu (Move to…, Mark as a call / Mark as media) and are
//! drag sources and drop zones for merging one note into another (`merge`).

use std::collections::{HashMap, HashSet};

use egui::{Key, Rect, Sense, Stroke, pos2, vec2};
use oats_core::{Node, NodeKind, View};

use crate::app::{Command, with_shortcut};
use crate::theme::Tokens;

/// Inline editing and collapse state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SidebarState {
    /// The "new category" field's text while it is open.
    pub adding: Option<String>,
    /// The node being renamed and the field's text.
    pub renaming: Option<(i64, String)>,
    /// The node being moved ("Move to…": a new parent typed by name) and the field's text.
    pub reparenting: Option<(i64, String)>,
    /// Give the open field keyboard focus this frame.
    pub focus: bool,
    /// Nodes whose children are hidden.
    pub collapsed: HashSet<i64>,
    /// Projects whose layer (recent notes, then topics) is open; closed by default. The app
    /// mirrors this into `AppPrefs::sidebar_episodes_open` when it changes.
    pub episodes_open: HashSet<i64>,
    /// A row a key press just selected: the panel scrolls it into view once it is drawn
    /// selected, then clears this.
    pub reveal_selected: Option<SidebarRow>,
}

/// One note listed under a project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarEpisode {
    pub id: i64,
    pub title: String,
    pub started_at: i64,
    /// The node the note is filed under (the project or a topic below it).
    pub node_id: Option<i64>,
    /// A media note ("Mark as a call" is offered).
    pub is_media: bool,
    /// A conversation heard from an app classed Media ("Mark as media" is offered).
    pub convertible: bool,
}

/// How many notes a project lists in the sidebar.
pub const SIDEBAR_EPISODES: usize = 5;

/// What the user did in the sidebar this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum SidebarAction {
    Select(View),
    /// A note under a project: `(episode, project)`.
    SelectEpisode(i64, i64),
    /// A project's layer was opened or closed (the app persists it).
    EpisodesToggled,
    Add(String),
    Rename(i64, String),
    /// "Move to…" on a project / topic: the new parent's name as typed.
    Reparent(i64, String),
    Delete(i64),
    ToggleSidebar,
    /// "Move to…" on a note row: the text typed in its field.
    MoveEpisode {
        episode: i64,
        text: String,
    },
    MarkAsCall(i64),
    MarkAsMedia(i64),
    /// A note dropped onto another note's row (see `NotesAction::MergeDrop`).
    MergeDrop {
        dragged: i64,
        into: i64,
        at: egui::Pos2,
    },
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

/// A row the keyboard can land on, in draw order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarRow {
    View(View),
    Node(i64),
    /// A note listed under a project.
    Episode {
        episode: i64,
        project: i64,
    },
}

/// The arrow keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavKey {
    Down,
    Up,
    Left,
    Right,
}

/// What an arrow key does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavOutcome {
    Nothing,
    /// Select (and navigate to) a row.
    Select(SidebarRow),
    /// Expand a collapsed category / topic.
    Expand(i64),
    /// Collapse an open category / topic.
    Collapse(i64),
    /// Open a project's note layer.
    OpenLayer(i64),
    /// Close a project's note layer.
    CloseLayer(i64),
}

/// The navigable rows in exactly the order `sidebar_panel` draws them: the Views, then the
/// visible tree, with an open project's notes between it and its topics.
pub fn navigable_rows(
    nodes: &[Node],
    collapsed: &HashSet<i64>,
    episodes_open: &HashSet<i64>,
    episodes: &HashMap<i64, Vec<SidebarEpisode>>,
) -> Vec<SidebarRow> {
    let mut out: Vec<SidebarRow> = VIEW_ROWS
        .iter()
        .map(|(_, v)| SidebarRow::View(*v))
        .collect();
    // a project folds with its notes layer, not with `collapsed`
    let mut folded = collapsed.clone();
    for n in nodes {
        if n.kind == NodeKind::Project && !episodes_open.contains(&n.id) {
            folded.insert(n.id);
        }
    }
    for (n, _) in visible_rows(nodes, &folded) {
        out.push(SidebarRow::Node(n.id));
        if n.kind == NodeKind::Project && episodes_open.contains(&n.id) {
            for ep in episodes.get(&n.id).into_iter().flatten() {
                out.push(SidebarRow::Episode {
                    episode: ep.id,
                    project: n.id,
                });
            }
        }
    }
    out
}

/// The index of the row drawn selected, if it is in the list.
pub fn current_row(rows: &[SidebarRow], view: View, focus_episode: Option<i64>) -> Option<usize> {
    if let Some(ep) = focus_episode {
        return rows
            .iter()
            .position(|r| matches!(r, SidebarRow::Episode { episode, .. } if *episode == ep));
    }
    let want = match view {
        View::Node(id) => SidebarRow::Node(id),
        v => SidebarRow::View(v),
    };
    rows.iter().position(|r| *r == want)
}

/// ↓: the next row (the first when nothing is selected); no wrap.
pub fn next_row(rows: &[SidebarRow], cur: Option<usize>) -> Option<SidebarRow> {
    let i = cur.map_or(0, |i| i + 1);
    rows.get(i).cloned()
}

/// ↑: the previous row (the last when nothing is selected); no wrap.
pub fn prev_row(rows: &[SidebarRow], cur: Option<usize>) -> Option<SidebarRow> {
    match cur {
        None => rows.last().cloned(),
        Some(0) => None,
        Some(i) => rows.get(i - 1).cloned(),
    }
}

/// ←: the parent row of the current one.
pub fn parent_row(nodes: &[Node], row: &SidebarRow) -> Option<SidebarRow> {
    match row {
        SidebarRow::View(_) => None,
        SidebarRow::Episode { project, .. } => Some(SidebarRow::Node(*project)),
        SidebarRow::Node(id) => nodes
            .iter()
            .find(|n| n.id == *id)
            .and_then(|n| n.parent_id)
            .map(SidebarRow::Node),
    }
}

/// What one arrow key does, given the rows and the fold state.
pub fn nav_decide(
    key: NavKey,
    rows: &[SidebarRow],
    cur: Option<usize>,
    nodes: &[Node],
    collapsed: &HashSet<i64>,
    episodes_open: &HashSet<i64>,
    episodes: &HashMap<i64, Vec<SidebarEpisode>>,
) -> NavOutcome {
    let select = |r: Option<SidebarRow>| r.map_or(NavOutcome::Nothing, NavOutcome::Select);
    match key {
        NavKey::Down => return select(next_row(rows, cur)),
        NavKey::Up => return select(prev_row(rows, cur)),
        _ => {}
    }
    let Some(i) = cur else {
        return NavOutcome::Nothing;
    };
    let SidebarRow::Node(id) = rows[i] else {
        // a note row or a Views row: only ← (to the project) does anything
        return if key == NavKey::Left {
            select(parent_row(nodes, &rows[i]))
        } else {
            NavOutcome::Nothing
        };
    };
    let is_project = nodes
        .iter()
        .any(|n| n.id == id && n.kind == NodeKind::Project);
    let has_content = has_children(nodes, id) || episodes.get(&id).is_some_and(|e| !e.is_empty());
    // `Some(open)` for a row with a disclosure triangle
    let open = if !has_content {
        None
    } else if is_project {
        Some(episodes_open.contains(&id))
    } else {
        Some(!collapsed.contains(&id))
    };
    match (key, open) {
        (NavKey::Right, Some(false)) if is_project => NavOutcome::OpenLayer(id),
        (NavKey::Right, Some(false)) => NavOutcome::Expand(id),
        (NavKey::Right, Some(true)) => select(rows.get(i + 1).cloned().filter(|r| match r {
            SidebarRow::Episode { project, .. } => *project == id,
            SidebarRow::Node(n) => nodes.iter().any(|x| x.id == *n && x.parent_id == Some(id)),
            SidebarRow::View(_) => false,
        })),
        (NavKey::Left, Some(true)) if is_project => NavOutcome::CloseLayer(id),
        (NavKey::Left, Some(true)) => NavOutcome::Collapse(id),
        (NavKey::Left, _) => select(parent_row(nodes, &rows[i])),
        _ => NavOutcome::Nothing,
    }
}

/// Read ↑ ↓ ← → for the sidebar and apply the result: fold changes happen here, navigation
/// is returned as the actions a click would give. Call only when the sidebar is visible, no
/// text field has focus and no modal is open.
pub fn key_nav(
    ctx: &egui::Context,
    state: &mut SidebarState,
    nodes: &[Node],
    episodes: &HashMap<i64, Vec<SidebarEpisode>>,
    view: View,
    focus_episode: Option<i64>,
) -> Vec<SidebarAction> {
    let key = ctx.input_mut(|i| {
        let none = egui::Modifiers::NONE;
        [
            (Key::ArrowDown, NavKey::Down),
            (Key::ArrowUp, NavKey::Up),
            (Key::ArrowLeft, NavKey::Left),
            (Key::ArrowRight, NavKey::Right),
        ]
        .into_iter()
        .find(|(k, _)| i.consume_key(none, *k))
        .map(|(_, n)| n)
    });
    let Some(key) = key else {
        return Vec::new();
    };
    let rows = navigable_rows(nodes, &state.collapsed, &state.episodes_open, episodes);
    let cur = current_row(&rows, view, focus_episode);
    let outcome = nav_decide(
        key,
        &rows,
        cur,
        nodes,
        &state.collapsed,
        &state.episodes_open,
        episodes,
    );
    let mut actions = Vec::new();
    match outcome {
        NavOutcome::Nothing => {}
        NavOutcome::Select(row) => {
            actions.push(match &row {
                SidebarRow::View(v) => SidebarAction::Select(*v),
                SidebarRow::Node(id) => SidebarAction::Select(View::Node(*id)),
                SidebarRow::Episode { episode, project } => {
                    SidebarAction::SelectEpisode(*episode, *project)
                }
            });
            state.reveal_selected = Some(row);
        }
        NavOutcome::Expand(id) => {
            state.collapsed.remove(&id);
        }
        NavOutcome::Collapse(id) => {
            state.collapsed.insert(id);
        }
        NavOutcome::OpenLayer(id) => {
            state.episodes_open.insert(id);
            actions.push(SidebarAction::EpisodesToggled);
        }
        NavOutcome::CloseLayer(id) => {
            state.episodes_open.remove(&id);
            actions.push(SidebarAction::EpisodesToggled);
        }
    }
    // the key was an input event, but the result is applied after this frame's draw
    ctx.request_repaint();
    actions
}

fn has_children(nodes: &[Node], id: i64) -> bool {
    nodes.iter().any(|n| n.parent_id == Some(id) && !n.archived)
}

/// What the sidebar shows in pure white: where today's notes went.
#[derive(Default)]
pub struct TodayMarks {
    /// Local midnight (ms): a note that started at or after it is "today's".
    pub start: i64,
    /// Nodes holding (or, for ancestors, containing) a note from today.
    pub nodes: HashSet<i64>,
    /// An unfiled, non-media note started today (the Conversations row).
    pub conversations: bool,
}

/// The nodes that hold a note from today, plus every ancestor of those. Pure.
pub fn today_nodes(
    nodes: &[Node],
    episodes: &HashMap<i64, Vec<SidebarEpisode>>,
    today_start: i64,
) -> HashSet<i64> {
    let mut out = HashSet::new();
    for (project, eps) in episodes {
        for ep in eps.iter().filter(|e| e.started_at >= today_start) {
            let mut cur = Some(ep.node_id.unwrap_or(*project));
            while let Some(id) = cur {
                if !out.insert(id) {
                    break;
                }
                cur = nodes.iter().find(|n| n.id == id).and_then(|n| n.parent_id);
            }
            // the project row itself (the list's key) even when the note sits on a topic
            let mut cur = Some(*project);
            while let Some(id) = cur {
                if !out.insert(id) {
                    break;
                }
                cur = nodes.iter().find(|n| n.id == id).and_then(|n| n.parent_id);
            }
        }
    }
    out
}

pub fn sidebar_panel(
    ui: &mut egui::Ui,
    state: &mut SidebarState,
    view: View,
    nodes: &[Node],
    episodes: &HashMap<i64, Vec<SidebarEpisode>>,
    focus_episode: Option<i64>,
    move_draft: &mut Option<crate::move_to::MoveDraft>,
    today: &TodayMarks,
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
    for (label, v) in VIEW_ROWS {
        let resp = hover_row(
            ui,
            &t,
            label,
            view == v,
            0,
            None,
            v == View::Notes && today.conversations,
        );
        reveal(ui, state, &SidebarRow::View(v), &resp, view == v);
        if resp.clicked() {
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
            // a project folds with its notes layer (closed by default), not with `collapsed`
            let mut folded = state.collapsed.clone();
            for n in nodes {
                if n.kind == NodeKind::Project && !state.episodes_open.contains(&n.id) {
                    folded.insert(n.id);
                }
            }
            let rows = visible_rows(nodes, &folded);
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
                if let Some((id, text)) = &mut state.reparenting
                    && *id == n.id
                {
                    let id = *id;
                    match inline_field_hint(ui, text, &mut state.focus, "Move to (name)") {
                        Field::Commit => {
                            let to = text.trim().to_owned();
                            if !to.is_empty() {
                                actions.push(SidebarAction::Reparent(id, to));
                            }
                            state.reparenting = None;
                        }
                        Field::Cancel => state.reparenting = None,
                        Field::Editing => {}
                    }
                    continue;
                }
                let is_project = n.kind == NodeKind::Project;
                let project_eps = episodes.get(&n.id).map_or(&[][..], Vec::as_slice);
                let disclosure = if is_project {
                    (has_children(nodes, n.id) || !project_eps.is_empty())
                        .then_some(state.episodes_open.contains(&n.id))
                } else {
                    has_children(nodes, n.id).then_some(!state.collapsed.contains(&n.id))
                };
                let selected = view == View::Node(n.id) && focus_episode.is_none();
                let resp = hover_row(
                    ui,
                    &t,
                    &n.name,
                    selected,
                    depth,
                    disclosure,
                    today.nodes.contains(&n.id),
                );
                let kind = match n.kind {
                    NodeKind::Category => "Category",
                    NodeKind::Project => "Project",
                    NodeKind::Topic => "Topic",
                };
                reveal(ui, state, &SidebarRow::Node(n.id), &resp, selected);
                resp.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        format!("{kind}: {}", n.name),
                    )
                });
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
                        state.reparenting = None;
                        state.adding = None;
                        state.focus = true;
                        ui.close();
                    }
                    if n.parent_id.is_some() && ui.button("Move to…").clicked() {
                        state.reparenting = Some((n.id, String::new()));
                        state.renaming = None;
                        state.adding = None;
                        state.focus = true;
                        ui.close();
                    }
                    if disclosure.is_some() {
                        let closed = match disclosure {
                            Some(open) => !open,
                            None => false,
                        };
                        let label = if closed { "Expand" } else { "Collapse" };
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
                if is_project && state.episodes_open.contains(&n.id) {
                    for ep in project_eps {
                        let r = episode_row(
                            ui,
                            &t,
                            ep,
                            focus_episode == Some(ep.id),
                            depth + 1,
                            today.start,
                        );
                        let row = SidebarRow::Episode {
                            episode: ep.id,
                            project: n.id,
                        };
                        reveal(ui, state, &row, &r, focus_episode == Some(ep.id));
                        if r.clicked() {
                            actions.push(SidebarAction::SelectEpisode(ep.id, n.id));
                        }
                        crate::merge::drag_source(ui, &t, &r, ep.id, &ep.title);
                        if let Some(dragged) = crate::merge::drop_zone(ui, &t, r.rect, ep.id) {
                            let at = ui.ctx().pointer_latest_pos().unwrap_or(r.rect.center());
                            actions.push(SidebarAction::MergeDrop {
                                dragged,
                                into: ep.id,
                                at,
                            });
                        }
                        r.context_menu(|ui| {
                            episode_menu(ui, ep, move_draft, &mut actions);
                        });
                        let target = oats_brain::MoveTarget::Episode(ep.id);
                        if let Some(text) =
                            crate::move_to::move_field_in(ui, &t, move_draft, target, true)
                        {
                            actions.push(SidebarAction::MoveEpisode {
                                episode: ep.id,
                                text,
                            });
                        }
                    }
                }
            }
            if let Some(id) = toggle {
                let is_project = nodes
                    .iter()
                    .any(|n| n.id == id && n.kind == NodeKind::Project);
                if is_project {
                    if !state.episodes_open.remove(&id) {
                        state.episodes_open.insert(id);
                    }
                    actions.push(SidebarAction::EpisodesToggled);
                } else if !state.collapsed.remove(&id) {
                    state.collapsed.insert(id);
                }
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

/// Scroll a row into view when a key just selected it and it is now drawn selected.
fn reveal(
    ui: &egui::Ui,
    state: &mut SidebarState,
    row: &SidebarRow,
    resp: &egui::Response,
    selected: bool,
) {
    if selected && state.reveal_selected.as_ref() == Some(row) {
        ui.scroll_to_rect(resp.rect, None);
        state.reveal_selected = None;
    }
}

/// A note row's context menu, as on the card's title: "Move to…" (the field opens under the
/// row), and "Mark as a call" / "Mark as media" when they apply.
fn episode_menu(
    ui: &mut egui::Ui,
    ep: &SidebarEpisode,
    move_draft: &mut Option<crate::move_to::MoveDraft>,
    actions: &mut Vec<SidebarAction>,
) {
    if ui.button("Move to…").clicked() {
        *move_draft = Some(
            crate::move_to::MoveDraft::new(oats_brain::MoveTarget::Episode(ep.id), ep.node_id)
                .in_sidebar(),
        );
        ui.close();
    }
    if ep.is_media {
        ui.separator();
        if ui
            .button("Mark as a call")
            .on_hover_text(
                "This was a conversation, not media: file it with your conversations and \
                 re-interpret it as a meeting",
            )
            .clicked()
        {
            actions.push(SidebarAction::MarkAsCall(ep.id));
            ui.close();
        }
    } else if ep.convertible {
        ui.separator();
        if ui
            .button("Mark as media")
            .on_hover_text(
                "Not a conversation of yours: make it a media note (no todos, no events)",
            )
            .clicked()
        {
            actions.push(SidebarAction::MarkAsMedia(ep.id));
            ui.close();
        }
    }
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

/// A note under a project: a 22 pt row, the title clipped to the column (full title and date
/// in the tooltip), the date ("29 Oct") right-aligned in the tertiary colour.
fn episode_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    ep: &SidebarEpisode,
    selected: bool,
    depth: usize,
    today_start: i64,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), t.row_h), Sense::click_and_drag());
    let date = crate::notes::short_date(ep.started_at);
    let is_today = ep.started_at >= today_start;
    let resp = resp.on_hover_text(format!(
        "{} · {date}{}",
        ep.title,
        if is_today { " · today" } else { "" }
    ));
    let (title_col, date_col) = if is_today {
        (egui::Color32::WHITE, egui::Color32::WHITE)
    } else {
        (t.text_secondary, t.text_tertiary)
    };
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            format!("Note: {}", ep.title),
        )
    });
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
    let text_x = 12.0 + depth as f32 * 14.0 + 12.0;
    let d = p.layout_no_wrap(date, t.font(10.5), date_col);
    let dw = d.size().x;
    p.galley(
        pos2(rect.right() - 8.0 - dw, rect.center().y - d.size().y / 2.0),
        d,
        date_col,
    );
    let galley = p.layout_no_wrap(ep.title.clone(), t.font(12.0), title_col);
    let clip = Rect::from_min_max(
        pos2(rect.left() + text_x, rect.top()),
        pos2(rect.right() - 8.0 - dw - 6.0, rect.bottom()),
    );
    p.with_clip_rect(clip).galley(
        pos2(
            rect.left() + text_x,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        title_col,
    );
    resp
}

fn group_label(ui: &mut egui::Ui, t: &Tokens, text: &str) {
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(text)
            .font(crate::theme::font_medium(ui.ctx(), 11.0))
            .color(t.text_tertiary),
    );
}

/// The "Views" group, in order. `View::Notes` is the Conversations view (the variant keeps its
/// name so saved prefs still load).
pub const VIEW_ROWS: [(&str, View); 5] = [
    ("Transcripts", View::Transcripts),
    ("Conversations", View::Notes),
    ("Calendar", View::Calendar),
    ("Todos", View::Todos),
    ("Media", View::Media),
];

/// A 22 pt sidebar row: hover fill, the selected row in an accent-alpha fill, indented by
/// `depth`, with a disclosure triangle (`Some(open)`) for rows that have children.
fn hover_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    label: &str,
    selected: bool,
    depth: usize,
    disclosure: Option<bool>,
    white: bool,
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
    let color = if white {
        egui::Color32::WHITE
    } else if depth > 0 {
        t.text_secondary
    } else {
        t.text
    };
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
    inline_field_hint(ui, text, focus, "Name")
}

/// [`inline_field`] with its own hint text.
fn inline_field_hint(ui: &mut egui::Ui, text: &mut String, focus: &mut bool, hint: &str) -> Field {
    let mut out = Field::Editing;
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        let resp = ui.add(
            egui::TextEdit::singleline(text)
                .desired_width(ui.available_width() - 6.0)
                .hint_text(hint),
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
    /// A magnifying glass (the control bar's Find button).
    Search,
    /// The same, in the accent colour (the search bar is open).
    SearchOn,
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
    let col = if matches!(icon, HeaderIcon::SearchOn) {
        t.accent
    } else if resp.hovered() {
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
        HeaderIcon::Search | HeaderIcon::SearchOn => {
            p.circle_stroke(at(10.5, 10.5), 5.5 * s, stroke);
            p.line_segment([at(14.6, 14.6), at(20.0, 20.0)], stroke);
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
    #[test]
    fn today_marks_cover_the_topic_its_project_and_category_only_for_today() {
        let nodes = vec![
            node(1, None, "Work", 0, false),
            node(2, Some(1), "Proj", 0, false),
            node(3, Some(2), "Topic", 0, false),
            node(4, None, "Admin", 1, false),
            node(5, Some(4), "Other", 0, false),
        ];
        let ep = |node_id, started_at| SidebarEpisode {
            id: 1,
            title: "t".into(),
            started_at,
            node_id,
            is_media: false,
            convertible: false,
        };
        let mut eps = HashMap::new();
        eps.insert(2, vec![ep(Some(3), 1_000)]);
        eps.insert(5, vec![ep(Some(5), 400)]);
        let got = today_nodes(&nodes, &eps, 1_000);
        assert_eq!(got, HashSet::from([1, 2, 3]));
        assert!(today_nodes(&nodes, &eps, 2_000).is_empty());
    }

    #[test]
    fn the_views_group_says_conversations() {
        let labels: Vec<_> = super::VIEW_ROWS.iter().map(|r| r.0).collect();
        assert!(labels.contains(&"Conversations"));
        assert!(!labels.contains(&"Notes"));
        assert_eq!(
            labels,
            ["Transcripts", "Conversations", "Calendar", "Todos", "Media"],
            "no Today, no All transcripts"
        );
    }

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

    fn ep(id: i64) -> SidebarEpisode {
        SidebarEpisode {
            id,
            title: "t".into(),
            started_at: 0,
            node_id: Some(10),
            is_media: false,
            convertible: false,
        }
    }

    /// Work(1) > Exporter(10, project) > CSV(11, topic); Admin(2).
    fn tree() -> Vec<Node> {
        let mut csv = node(11, Some(10), "CSV", 0, false);
        csv.kind = NodeKind::Topic;
        vec![
            node(1, None, "Work", 0, false),
            node(10, Some(1), "Exporter", 0, false),
            csv,
            node(2, None, "Admin", 1, false),
        ]
    }

    fn eps() -> HashMap<i64, Vec<SidebarEpisode>> {
        [(10, vec![ep(100), ep(101)])].into_iter().collect()
    }

    fn views() -> Vec<SidebarRow> {
        VIEW_ROWS
            .iter()
            .map(|(_, v)| SidebarRow::View(*v))
            .collect()
    }

    #[test]
    fn navigable_rows_follow_the_draw_order() {
        let nodes = tree();
        let none = HashSet::new();
        // project closed: its topics and notes are hidden
        let mut want = views();
        want.extend([1, 10, 2].map(SidebarRow::Node));
        assert_eq!(navigable_rows(&nodes, &none, &none, &eps()), want);
        // open: notes sit between the project and its topics
        let open: HashSet<i64> = [10].into_iter().collect();
        let mut want = views();
        want.extend([
            SidebarRow::Node(1),
            SidebarRow::Node(10),
            SidebarRow::Episode {
                episode: 100,
                project: 10,
            },
            SidebarRow::Episode {
                episode: 101,
                project: 10,
            },
            SidebarRow::Node(11),
            SidebarRow::Node(2),
        ]);
        assert_eq!(navigable_rows(&nodes, &none, &open, &eps()), want);
        // a collapsed category hides its whole subtree
        let collapsed: HashSet<i64> = [1].into_iter().collect();
        let mut want = views();
        want.extend([1, 2].map(SidebarRow::Node));
        assert_eq!(navigable_rows(&nodes, &collapsed, &open, &eps()), want);
    }

    #[test]
    fn up_and_down_step_without_wrapping_and_start_from_the_ends() {
        let rows = views();
        assert_eq!(next_row(&rows, None), Some(rows[0].clone()));
        assert_eq!(prev_row(&rows, None), Some(rows[4].clone()));
        assert_eq!(next_row(&rows, Some(1)), Some(rows[2].clone()));
        assert_eq!(prev_row(&rows, Some(1)), Some(rows[0].clone()));
        assert_eq!(prev_row(&rows, Some(0)), None);
        assert_eq!(next_row(&rows, Some(4)), None);
        let cur = current_row(&rows, View::Calendar, None);
        assert_eq!(cur, Some(2));
        assert_eq!(current_row(&rows, View::Node(1), None), None);
        assert_eq!(current_row(&rows, View::Calendar, Some(5)), None);
    }

    #[test]
    fn left_and_right_open_close_and_walk_the_tree() {
        let nodes = tree();
        let eps = eps();
        let decide = |key, collapsed: &[i64], open: &[i64], at: SidebarRow| {
            let collapsed: HashSet<i64> = collapsed.iter().copied().collect();
            let open: HashSet<i64> = open.iter().copied().collect();
            let rows = navigable_rows(&nodes, &collapsed, &open, &eps);
            let cur = rows.iter().position(|r| *r == at);
            nav_decide(key, &rows, cur, &nodes, &collapsed, &open, &eps)
        };
        let n = SidebarRow::Node;
        // → opens a collapsed category, then enters it
        assert_eq!(
            decide(NavKey::Right, &[1], &[], n(1)),
            NavOutcome::Expand(1)
        );
        assert_eq!(
            decide(NavKey::Right, &[], &[], n(1)),
            NavOutcome::Select(n(10))
        );
        // → on a project opens its layer, then enters the first note
        assert_eq!(
            decide(NavKey::Right, &[], &[], n(10)),
            NavOutcome::OpenLayer(10)
        );
        assert_eq!(
            decide(NavKey::Right, &[], &[10], n(10)),
            NavOutcome::Select(SidebarRow::Episode {
                episode: 100,
                project: 10
            })
        );
        // → on a leaf or a note does nothing
        assert_eq!(
            decide(NavKey::Right, &[], &[10], n(11)),
            NavOutcome::Nothing
        );
        assert_eq!(decide(NavKey::Right, &[], &[], n(2)), NavOutcome::Nothing);
        // ← closes an open row, else goes to the parent
        assert_eq!(
            decide(NavKey::Left, &[], &[], n(1)),
            NavOutcome::Collapse(1)
        );
        assert_eq!(decide(NavKey::Left, &[1], &[], n(1)), NavOutcome::Nothing);
        assert_eq!(
            decide(NavKey::Left, &[], &[10], n(10)),
            NavOutcome::CloseLayer(10)
        );
        assert_eq!(
            decide(NavKey::Left, &[], &[], n(10)),
            NavOutcome::Select(n(1))
        );
        assert_eq!(
            decide(NavKey::Left, &[], &[10], n(11)),
            NavOutcome::Select(n(10))
        );
        assert_eq!(
            decide(
                NavKey::Left,
                &[],
                &[10],
                SidebarRow::Episode {
                    episode: 101,
                    project: 10
                }
            ),
            NavOutcome::Select(n(10))
        );
        assert_eq!(
            decide(NavKey::Left, &[], &[], SidebarRow::View(View::Todos)),
            NavOutcome::Nothing
        );
    }
}
