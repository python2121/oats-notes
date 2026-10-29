//! The Notes view: the brain's interpreted episodes, newest first — title, time range, the
//! node chip (category › project › topic), the summary (a small markdown subset: headings,
//! bullets, **bold**), decisions, action items as a checklist-looking list, topics, a
//! "Re-interpret" button — with the brain's control strip (Interpret now / Review now /
//! Reorganise…, last run, today's spend) and the recent-changes log (what the review and
//! reorganisation passes did, with Undo where possible) at the top.

use chrono::Local;
use egui::{Align2, RichText, Sense, vec2};
use oats_core::{BrainRun, Change, Episode, Node, TranscriptRow};

use crate::theme::Tokens;
use crate::transcript::{TranscriptAction, clock, day_heading, day_of, transcript_rows};

/// What the user did in the Notes view this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum NotesAction {
    InterpretNow,
    ReviewNow,
    Reorganise,
    Reinterpret(i64),
    Undo(i64),
    /// Drill into a node (a project chip in a category's view).
    SelectNode(i64),
}

/// What the strip shows about the brain.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BrainStatus {
    /// `Some(Ok(version))` once the CLI answered, `Some(Err(why))` when it can't be used.
    pub availability: Option<Result<String, String>>,
    /// The job running right now ("interpret", "review", …).
    pub running: Option<String>,
    pub last_run: Option<BrainRun>,
    pub spend_today_usd: f64,
    /// The latest `BrainEvent::Backlog`: segments and voice commands waiting for claude.
    pub backlog: Option<(i64, usize)>,
}

impl BrainStatus {
    /// "last run: interpret · 2 of 2 chunks · $0.03 · 4.1 s" (or the error).
    pub fn last_run_line(&self) -> String {
        match &self.last_run {
            None => "no brain run yet".to_owned(),
            Some(r) => {
                let what = if r.ok {
                    if r.summary.is_empty() {
                        "ok".to_owned()
                    } else {
                        r.summary.clone()
                    }
                } else {
                    format!("failed: {}", r.error.clone().unwrap_or_default())
                };
                let cost = r
                    .cost_usd
                    .map(|c| format!(" · ${c:.3}"))
                    .unwrap_or_default();
                let secs = r
                    .duration_ms
                    .map(|d| format!(" · {:.1} s", d as f64 / 1000.0))
                    .unwrap_or_default();
                format!("last run: {} ({}) · {what}{cost}{secs}", r.kind, r.model)
            }
        }
    }

    pub fn available(&self) -> bool {
        matches!(self.availability, Some(Ok(_)))
    }

    /// "claude unavailable (not logged in) — 42 segments waiting" while claude can't be
    /// used, "catching up — 12 segments waiting" while the backlog is worked off, else
    /// `None`.
    pub fn backlog_line(&self) -> Option<String> {
        let (segments, commands) = self.backlog.unwrap_or((0, 0));
        let mut waiting = format!(
            "{segments} segment{} waiting",
            if segments == 1 { "" } else { "s" }
        );
        if commands > 0 {
            waiting.push_str(&format!(
                ", {commands} voice command{}",
                if commands == 1 { "" } else { "s" }
            ));
        }
        match &self.availability {
            Some(Err(why)) => Some(format!("claude unavailable ({why}) — {waiting}")),
            Some(Ok(_)) if segments > 0 || commands > 0 => Some(format!("catching up — {waiting}")),
            _ => None,
        }
    }
}

/// Per-frame UI state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NotesState {
    pub show_changes: bool,
    /// Low-signal episodes expanded by the user.
    pub expanded: Vec<i64>,
    /// A node view's raw transcript section is open (collapsed by default).
    pub show_transcript: bool,
}

/// "Work › Export rewrite › CSV" for a node.
pub fn node_path(nodes: &[Node], id: i64) -> String {
    let mut parts = Vec::new();
    let mut cur = nodes.iter().find(|n| n.id == id);
    while let Some(n) = cur {
        parts.push(n.name.clone());
        cur = n.parent_id.and_then(|p| nodes.iter().find(|x| x.id == p));
        if parts.len() > 8 {
            break;
        }
    }
    parts.reverse();
    parts.join(" › ")
}

/// A line of the summary, classified for the simple renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdLine {
    Heading(String),
    Bullet(String),
    Text(String),
    Blank,
}

/// The markdown subset: `#` headings, `-`/`*`/`•` bullets (any indent), everything else text.
pub fn md_lines(summary: &str) -> Vec<MdLine> {
    summary
        .lines()
        .map(|raw| {
            let l = raw.trim_end();
            let t = l.trim_start();
            if t.is_empty() {
                MdLine::Blank
            } else if let Some(h) = t.strip_prefix('#') {
                MdLine::Heading(h.trim_start_matches('#').trim().to_owned())
            } else if let Some(b) = t
                .strip_prefix("- ")
                .or_else(|| t.strip_prefix("* "))
                .or_else(|| t.strip_prefix("• "))
            {
                MdLine::Bullet(b.trim().to_owned())
            } else {
                MdLine::Text(t.to_owned())
            }
        })
        .collect()
}

/// Split `**bold**` runs: `(text, is_bold)`.
pub fn bold_spans(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut bold = false;
    for (i, part) in text.split("**").enumerate() {
        if !part.is_empty() {
            out.push((part.to_owned(), bold));
        }
        if i < text.matches("**").count() {
            bold = !bold;
        }
    }
    out
}

fn rich_line(ui: &mut egui::Ui, t: &Tokens, text: &str, size: f32, color: egui::Color32, hl: &str) {
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = ui.available_width();
    for (span, bold) in bold_spans(text) {
        let font = if bold {
            crate::theme::font_medium(ui.ctx(), size)
        } else {
            t.font(size)
        };
        crate::search::append_marked(
            &mut job,
            t,
            &span,
            hl,
            egui::TextFormat {
                font_id: font,
                color,
                ..Default::default()
            },
        );
    }
    ui.add(egui::Label::new(job).wrap().selectable(false));
}

fn summary_block(ui: &mut egui::Ui, t: &Tokens, summary: &str, hl: &str) {
    for line in md_lines(summary) {
        match line {
            MdLine::Blank => ui.add_space(3.0),
            MdLine::Heading(h) => {
                ui.add_space(3.0);
                ui.label(
                    RichText::new(h)
                        .font(crate::theme::font_medium(ui.ctx(), 12.5))
                        .color(t.text),
                );
            }
            MdLine::Bullet(b) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new("•").color(t.text_secondary).size(12.5));
                    rich_line(ui, t, &b, 12.5, t.text, hl);
                });
            }
            MdLine::Text(x) => rich_line(ui, t, &x, 12.5, t.text, hl),
        }
    }
}

/// A small rounded chip.
pub fn chip(ui: &mut egui::Ui, t: &Tokens, text: &str, accent: bool) {
    let font = t.font(10.5);
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        font,
        if accent { t.accent } else { t.text_secondary },
    );
    let size = vec2(galley.size().x + 12.0, 16.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let fill = if accent {
            t.accent_alpha(0.18)
        } else {
            t.hover
        };
        ui.painter().rect_filled(rect, 8.0, fill);
        ui.painter().galley(
            rect.center() - galley.size() / 2.0,
            galley,
            if accent { t.accent } else { t.text_secondary },
        );
    }
}

fn small_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> bool {
    ui.add_enabled(enabled, egui::Button::new(RichText::new(label).size(11.5)))
        .clicked()
}

/// The control strip above the list.
fn brain_strip(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut NotesState,
    status: &BrainStatus,
    changes: &[Change],
    actions: &mut Vec<NotesAction>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.add_space(8.0);
        let idle = status.running.is_none() && status.available();
        if small_button(ui, "Interpret now", idle) {
            actions.push(NotesAction::InterpretNow);
        }
        if small_button(ui, "Review now", idle) {
            actions.push(NotesAction::ReviewNow);
        }
        if small_button(ui, "Reorganise…", idle) {
            actions.push(NotesAction::Reorganise);
        }
        let line = match (&status.running, &status.availability) {
            (Some(job), _) => match status.backlog_line() {
                Some(b) => format!("running: {job}… · {b}"),
                None => format!("running: {job}…"),
            },
            (None, Some(Err(_))) => status.backlog_line().unwrap_or_default(),
            (None, None) => "checking claude…".to_owned(),
            (None, Some(Ok(_))) => status
                .backlog_line()
                .unwrap_or_else(|| status.last_run_line()),
        };
        ui.add(egui::Label::new(RichText::new(line).color(t.text_secondary).size(11.0)).truncate());
    });
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(
            RichText::new(format!("usage {:.2} today", status.spend_today_usd))
                .color(t.text_tertiary)
                .size(11.0),
        );
        if !changes.is_empty() {
            let label = format!(
                "{} {} change{}",
                if state.show_changes { "▾" } else { "▸" },
                changes.len(),
                if changes.len() == 1 { "" } else { "s" }
            );
            if ui
                .add(
                    egui::Label::new(RichText::new(label).color(t.text_secondary).size(11.0))
                        .sense(Sense::click()),
                )
                .clicked()
            {
                state.show_changes = !state.show_changes;
            }
        }
    });
    if state.show_changes {
        for c in changes.iter().take(30) {
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                let col = if c.undone {
                    t.text_disabled
                } else {
                    t.text_secondary
                };
                let when = clock(c.created_at, &Local);
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("{when}  {}", c.description))
                            .color(col)
                            .size(11.0)
                            .strikethrough_if(c.undone),
                    )
                    .truncate(),
                );
                let undoable = !c.undone
                    && matches!(
                        c.kind.as_str(),
                        "node_rename"
                            | "node_move"
                            | "node_archive"
                            | "node_description"
                            | "episode_node"
                            | "episode_low_signal"
                    );
                if undoable && small_button(ui, "Undo", true) {
                    actions.push(NotesAction::Undo(c.id));
                }
            });
        }
    }
}

trait StrikeIf {
    fn strikethrough_if(self, on: bool) -> Self;
}

impl StrikeIf for RichText {
    fn strikethrough_if(self, on: bool) -> Self {
        if on { self.strikethrough() } else { self }
    }
}

/// One episode card. `hl` is a search string to mark in the text ("" for none); `open`, when
/// given, makes the title clickable and receives the episode's id on a click (search results).
pub(crate) fn episode_card(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut NotesState,
    e: &Episode,
    nodes: &[Node],
    actions: &mut Vec<NotesAction>,
    hl: &str,
    open: Option<&mut Option<i64>>,
) -> egui::Rect {
    let collapsed = e.low_signal && !state.expanded.contains(&e.id);
    let fill = ui.painter().add(egui::Shape::Noop);
    let left = ui.max_rect().left();
    let right = ui.max_rect().right();
    let inner = ui
        .vertical(|ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                let title_col = if e.low_signal {
                    t.text_tertiary
                } else {
                    t.text
                };
                let title = if collapsed {
                    format!("{} · {}", e.title, "low signal")
                } else {
                    e.title.clone()
                };
                let mut title_job = egui::text::LayoutJob::default();
                crate::search::append_marked(
                    &mut title_job,
                    t,
                    &title,
                    hl,
                    egui::TextFormat {
                        font_id: crate::theme::font_medium(ui.ctx(), 13.5),
                        color: title_col,
                        ..Default::default()
                    },
                );
                let mut resp = ui.add(
                    egui::Label::new(title_job)
                        .sense(if e.low_signal || open.is_some() {
                            Sense::click()
                        } else {
                            Sense::hover()
                        })
                        .truncate(),
                );
                if let Some(open) = open {
                    resp = resp
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Show in the notes");
                    if resp.clicked() {
                        *open = Some(e.id);
                    }
                }
                if e.low_signal && resp.clicked() {
                    if let Some(i) = state.expanded.iter().position(|x| *x == e.id) {
                        state.expanded.remove(i);
                    } else {
                        state.expanded.push(e.id);
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(12.0);
                    if small_button(ui, "Re-interpret", true) {
                        actions.push(NotesAction::Reinterpret(e.id));
                    }
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.add_space(12.0);
                ui.label(
                    RichText::new(format!(
                        "{}–{}",
                        clock(e.started_at, &Local),
                        clock(e.ended_at, &Local)
                    ))
                    .font(t.mono(10.5))
                    .color(t.text_tertiary),
                );
                if let Some(n) = e.node_id {
                    chip(ui, t, &node_path(nodes, n), true);
                }
                if e.revision > 1 {
                    chip(ui, t, &format!("rev {}", e.revision), false);
                }
                for topic in e.topics.iter().take(5) {
                    chip(ui, t, topic, false);
                }
            });
            if !collapsed {
                ui.add_space(2.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.set_width((ui.available_width() - 12.0).max(80.0));
                        summary_block(ui, t, &e.summary, hl);
                        if !e.decisions.is_empty() {
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("Decisions")
                                    .font(crate::theme::font_medium(ui.ctx(), 12.0))
                                    .color(t.text_secondary),
                            );
                            for d in &e.decisions {
                                ui.horizontal_top(|ui| {
                                    ui.add_space(6.0);
                                    ui.label(RichText::new("✓").color(t.accent).size(12.0));
                                    rich_line(ui, t, d, 12.5, t.text, hl);
                                });
                            }
                        }
                        if !e.action_items.is_empty() {
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("Action items")
                                    .font(crate::theme::font_medium(ui.ctx(), 12.0))
                                    .color(t.text_secondary),
                            );
                            for a in &e.action_items {
                                ui.horizontal_top(|ui| {
                                    ui.add_space(6.0);
                                    ui.label(RichText::new("☐").color(t.text_secondary).size(12.5));
                                    let mut text = a.text.clone();
                                    if let Some(o) = &a.owner {
                                        text.push_str(&format!(" — **{o}**"));
                                    }
                                    if let Some(d) = &a.due {
                                        text.push_str(&format!(" (due {d})"));
                                    }
                                    rich_line(ui, t, &text, 12.5, t.text, hl);
                                });
                            }
                        }
                    });
                });
            }
            ui.add_space(6.0);
        })
        .response;
    let rect = egui::Rect::from_min_max(
        egui::pos2(left + 6.0, inner.rect.top()),
        egui::pos2(right - 6.0, inner.rect.bottom()),
    );
    ui.painter().set(
        fill,
        egui::Shape::rect_filled(
            rect,
            6.0,
            t.inset.gamma_multiply(if e.low_signal { 0.5 } else { 1.0 }),
        ),
    );
    ui.add_space(6.0);
    rect
}

/// The view. `episodes` newest first.
pub fn notes_view(
    ui: &mut egui::Ui,
    state: &mut NotesState,
    status: &BrainStatus,
    episodes: &[Episode],
    nodes: &[Node],
    changes: &[Change],
) -> Vec<NotesAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    ui.add_space(6.0);
    brain_strip(ui, &t, state, status, changes, &mut actions);
    ui.add_space(4.0);
    ui.separator();
    if episodes.is_empty() {
        let rect = ui.available_rect_before_wrap();
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "No notes yet. The brain interprets the transcript every 15 minutes while there is something new; Interpret now runs it at once.",
            t.font(12.5),
            t.text_tertiary,
        );
        return actions;
    }
    let this_year = chrono::Datelike::year(&Local::now());
    egui::ScrollArea::vertical()
        .id_salt("notes-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(6.0);
            let mut last_day = None;
            for e in episodes {
                let day = day_of(e.started_at, &Local);
                if last_day != Some(day) {
                    last_day = Some(day);
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        ui.label(
                            RichText::new(
                                day.map(|d| day_heading(d, this_year))
                                    .unwrap_or_else(|| "Unknown date".to_owned()),
                            )
                            .font(crate::theme::font_medium(ui.ctx(), 13.5))
                            .color(t.text),
                        );
                    });
                    ui.add_space(4.0);
                }
                episode_card(ui, &t, state, e, nodes, &mut actions, "", None);
            }
            ui.add_space(8.0);
        });
    actions
}

/// One child of the viewed node, with what lives under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildSummary {
    pub id: i64,
    pub name: String,
    pub episodes: usize,
    /// End of the newest episode under it.
    pub last_activity: Option<i64>,
}

/// Whether `node` is `root` or below it.
pub fn node_in_subtree(nodes: &[Node], node: i64, root: i64) -> bool {
    let mut cur = Some(node);
    for _ in 0..16 {
        match cur {
            Some(id) if id == root => return true,
            Some(id) => cur = nodes.iter().find(|n| n.id == id).and_then(|n| n.parent_id),
            None => return false,
        }
    }
    false
}

/// The non-archived children of `root` (projects of a category, topics of a project), in tree
/// order, each with its episode count and latest activity over `episodes` (the subtree's).
pub fn child_summaries(nodes: &[Node], root: i64, episodes: &[Episode]) -> Vec<ChildSummary> {
    let mut kids: Vec<&Node> = nodes
        .iter()
        .filter(|n| n.parent_id == Some(root) && !n.archived)
        .collect();
    kids.sort_by_key(|n| (n.position, n.id));
    kids.into_iter()
        .map(|k| {
            let under: Vec<&Episode> = episodes
                .iter()
                .filter(|e| e.node_id.is_some_and(|n| node_in_subtree(nodes, n, k.id)))
                .collect();
            ChildSummary {
                id: k.id,
                name: k.name.clone(),
                episodes: under.len(),
                last_activity: under.iter().map(|e| e.ended_at).max(),
            }
        })
        .collect()
}

/// What the user did in a node view this frame.
#[derive(Default)]
pub struct NodeViewOut {
    pub notes: Vec<NotesAction>,
    pub transcript: Vec<TranscriptAction>,
}

/// A category, project or topic selected in the sidebar: its path as the header, the child
/// projects (or topics) as clickable rows, the notes (episodes) of the subtree newest first,
/// and the raw transcript segments filed there behind a toggle (collapsed by default).
pub fn node_view(
    ui: &mut egui::Ui,
    state: &mut NotesState,
    node: &Node,
    episodes: &[Episode],
    rows: &[TranscriptRow],
    nodes: &[Node],
) -> NodeViewOut {
    let t = Tokens::get(ui.ctx());
    let mut out = NodeViewOut::default();
    let children = child_summaries(nodes, node.id, episodes);
    let this_year = chrono::Datelike::year(&Local::now());
    egui::ScrollArea::vertical()
        .id_salt("node-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(node_path(nodes, node.id))
                            .font(crate::theme::font_medium(ui.ctx(), 16.0))
                            .color(t.text),
                    )
                    .truncate(),
                );
            });
            if !node.description.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&node.description)
                                .color(t.text_secondary)
                                .size(11.5),
                        )
                        .truncate(),
                    );
                });
            }
            ui.add_space(8.0);
            let heading = if node.kind == oats_core::NodeKind::Topic {
                None
            } else if node.kind == oats_core::NodeKind::Category {
                Some((
                    "Projects",
                    "No projects yet. The brain creates one when it files notes.",
                ))
            } else {
                Some(("Topics", ""))
            };
            if let Some((title, empty)) = heading
                && (!children.is_empty() || !empty.is_empty())
            {
                section_label(ui, &t, title);
                if children.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        ui.label(RichText::new(empty).color(t.text_tertiary).size(12.0));
                    });
                }
                for c in &children {
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        let resp = ui.add(
                            egui::Label::new(
                                RichText::new(&c.name)
                                    .font(crate::theme::font_medium(ui.ctx(), 13.0))
                                    .color(t.accent),
                            )
                            .sense(Sense::click()),
                        );
                        if resp.clicked() {
                            out.notes.push(NotesAction::SelectNode(c.id));
                        }
                        let last = c
                            .last_activity
                            .and_then(|ms| crate::transcript::at(ms, &Local))
                            .map(|d| format!(" · last {}", d.format("%b %-d, %H:%M")))
                            .unwrap_or_default();
                        ui.label(
                            RichText::new(format!(
                                "{} note{}{last}",
                                c.episodes,
                                if c.episodes == 1 { "" } else { "s" }
                            ))
                            .color(t.text_tertiary)
                            .size(11.0),
                        );
                    });
                    ui.add_space(2.0);
                }
                ui.add_space(8.0);
            }
            section_label(ui, &t, "Notes");
            if episodes.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    ui.label(
                        RichText::new("No notes here yet.")
                            .color(t.text_tertiary)
                            .size(12.0),
                    );
                });
            }
            ui.add_space(4.0);
            let mut last_day = None;
            for e in episodes {
                let day = day_of(e.started_at, &Local);
                if last_day != Some(day) {
                    last_day = Some(day);
                    ui.horizontal(|ui| {
                        ui.add_space(14.0);
                        ui.label(
                            RichText::new(
                                day.map(|d| day_heading(d, this_year))
                                    .unwrap_or_else(|| "Unknown date".to_owned()),
                            )
                            .font(crate::theme::font_medium(ui.ctx(), 12.5))
                            .color(t.text_secondary),
                        );
                    });
                    ui.add_space(4.0);
                }
                episode_card(ui, &t, state, e, nodes, &mut out.notes, "", None);
            }
            ui.add_space(8.0);
            let label = format!(
                "{} Show transcript ({} segment{})",
                if state.show_transcript { "▾" } else { "▸" },
                rows.len(),
                if rows.len() == 1 { "" } else { "s" }
            );
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                if ui
                    .add(
                        egui::Label::new(RichText::new(label).color(t.text_secondary).size(12.0))
                            .sense(Sense::click()),
                    )
                    .clicked()
                {
                    state.show_transcript = !state.show_transcript;
                }
            });
            if state.show_transcript {
                ui.add_space(4.0);
                out.transcript = transcript_rows(ui, rows, nodes);
            }
            ui.add_space(12.0);
        });
    out
}

fn section_label(ui: &mut egui::Ui, t: &Tokens, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(
            RichText::new(text)
                .font(crate::theme::font_medium(ui.ctx(), 11.0))
                .color(t.text_tertiary),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use oats_core::{NodeKind, Origin};

    #[test]
    fn markdown_subset() {
        let lines = md_lines("# Title\n\n- one\n  * two\n• three\nplain **bold** text\n");
        assert_eq!(
            lines,
            vec![
                MdLine::Heading("Title".into()),
                MdLine::Blank,
                MdLine::Bullet("one".into()),
                MdLine::Bullet("two".into()),
                MdLine::Bullet("three".into()),
                MdLine::Text("plain **bold** text".into()),
            ]
        );
        assert_eq!(
            bold_spans("plain **bold** text"),
            vec![
                ("plain ".to_owned(), false),
                ("bold".to_owned(), true),
                (" text".to_owned(), false)
            ]
        );
        assert_eq!(bold_spans("**all**"), vec![("all".to_owned(), true)]);
        assert_eq!(bold_spans("none"), vec![("none".to_owned(), false)]);
    }

    #[test]
    fn node_paths_and_run_line() {
        let n = |id, parent, name: &str| Node {
            id,
            parent_id: parent,
            kind: NodeKind::Project,
            name: name.into(),
            description: String::new(),
            created_by: Origin::User,
            position: 0,
            archived: false,
        };
        let nodes = vec![
            n(1, None, "Work"),
            n(2, Some(1), "Export"),
            n(3, Some(2), "CSV"),
        ];
        assert_eq!(node_path(&nodes, 3), "Work › Export › CSV");
        assert_eq!(node_path(&nodes, 1), "Work");
        assert_eq!(node_path(&nodes, 9), "");
        let mut s = BrainStatus::default();
        assert_eq!(s.last_run_line(), "no brain run yet");
        assert!(!s.available());
        s.last_run = Some(BrainRun {
            id: 1,
            tier: 1,
            kind: "interpret".into(),
            started_at: 0,
            finished_at: Some(1),
            model: "sonnet".into(),
            cost_usd: Some(0.0312),
            duration_ms: Some(4_100),
            ok: true,
            error: None,
            summary: "2 of 2 chunks".into(),
        });
        assert_eq!(
            s.last_run_line(),
            "last run: interpret (sonnet) · 2 of 2 chunks · $0.031 · 4.1 s"
        );
        s.availability = Some(Ok("2.1.312".into()));
        assert!(s.available());
    }
}

#[cfg(test)]
mod backlog_tests {
    use super::*;

    #[test]
    fn the_backlog_line_names_why_and_how_much_is_waiting() {
        let mut s = BrainStatus {
            availability: Some(Err("not logged in".into())),
            backlog: Some((42, 1)),
            ..Default::default()
        };
        assert_eq!(
            s.backlog_line().as_deref(),
            Some("claude unavailable (not logged in) — 42 segments waiting, 1 voice command")
        );
        s.backlog = None;
        assert_eq!(
            s.backlog_line().as_deref(),
            Some("claude unavailable (not logged in) — 0 segments waiting")
        );
        s.availability = Some(Ok("2.1.312".into()));
        s.backlog = Some((1, 0));
        assert_eq!(
            s.backlog_line().as_deref(),
            Some("catching up — 1 segment waiting")
        );
        s.backlog = Some((0, 0));
        assert_eq!(s.backlog_line(), None);
    }
}
