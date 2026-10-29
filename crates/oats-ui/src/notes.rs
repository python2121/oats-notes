//! The Conversations view (`View::Notes`, name kept for the saved pref): the brain's unfiled
//! episodes (not under a project or topic; those live on the project pages), newest first — title, time range, the
//! node chip (category › project › topic), the summary (a small markdown subset: headings,
//! bullets, **bold**), decisions, action items as a checklist-looking list, topics, a
//! "Re-interpret" button — with the brain's control strip (Interpret now / Review now /
//! Reorganise…, last run, today's spend) and the recent-changes log (what the review and
//! reorganisation passes did, with Undo where possible) at the top.

use chrono::Local;
use egui::{Align2, Rect, RichText, Sense, vec2};
use oats_core::{ActionItem, BrainRun, Change, Episode, EpisodeContent, Node, TranscriptRow};

use crate::theme::Tokens;
use crate::transcript::{TranscriptAction, clock, day_heading, day_of, transcript_rows};

/// What the user did in the Conversations view this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum NotesAction {
    InterpretNow,
    ReviewNow,
    Reorganise,
    Reinterpret(i64),
    Undo(i64),
    /// Drill into a node (a project chip in a category's view).
    SelectNode(i64),
    /// A topic chip was clicked: show everything tagged with it.
    OpenTopic(String),
    /// The user saved an edited note (title, summary, decisions, action items, topics).
    SaveEdit(i64, EpisodeContent),
    /// Scroll to this note in the page being viewed and flash it (a next step's source link);
    /// leaves any single-note view.
    GoToNote(i64),
    /// "Mark as a call" on a media note: a conversation again, re-interpreted as a meeting.
    MarkAsCall(i64),
    /// "Mark as media" on a call in an app classed Media: filed under Media.
    MarkAsMedia(i64),
    /// "Summarise" a media note (one model call with the media prompt).
    Summarise(i64),
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
    /// On-demand commands sent and not yet settled (see [`BrainStatus::queue`]).
    pub pending: Vec<PendingOp>,
    /// Kinds whose pending state ran out after [`PENDING_TIMEOUT_S`] (tooltip until re-queued).
    pub timed_out: Vec<String>,
    /// The UI clock (seconds, egui's `input.time`), refreshed every frame.
    pub now: f64,
    /// Where the running job is (the latest `BrainEvent::Progress`).
    pub progress: Option<Progress>,
    /// Kinds that produced a run since they were last queued (a settle without one means
    /// "nothing to do").
    pub ran: Vec<String>,
}

/// A job's position: `step` of `total`, with a short phrase.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub kind: String,
    pub step: usize,
    pub total: usize,
    pub label: String,
}

/// What a finished run says in its pop-up: the text and whether it is an error.
pub fn run_toast(run: &BrainRun) -> (String, bool) {
    let name = match run.kind.as_str() {
        "interpret" => "Interpret",
        "review" => "Review",
        "reorganise" => "Reorganise",
        "summarise" => "Summarise",
        other => other,
    };
    if !run.ok {
        let why = run.error.clone().unwrap_or_default();
        let why: String = why.chars().take(120).collect();
        return (format!("{name} failed: {why}"), true);
    }
    let sum = run.summary.trim();
    let text = match run.kind.as_str() {
        "interpret" if sum.starts_with("0 of") => "Nothing new to interpret".to_owned(),
        "interpret" => format!("Interpreted: {sum}"),
        "review" if sum.starts_with("0 changes") => "Review: nothing to change".to_owned(),
        "review" => format!("Review: {sum}"),
        "reorganise" if sum.is_empty() || sum.starts_with("nothing") => {
            "Nothing to reorganise".to_owned()
        }
        "reorganise" => {
            let short: String = sum.chars().take(110).collect();
            format!("{short} (see the change log)")
        }
        _ if sum.is_empty() => format!("{name}: done"),
        _ => sum.to_owned(),
    };
    (text, false)
}

/// The pop-up for a command the brain settled without running anything.
pub fn nothing_toast(kind: &str) -> Option<String> {
    Some(
        match kind {
            "interpret" | "reinterpret" => "Nothing new to interpret",
            "review" => "Nothing to review",
            "reorganise" => "Nothing to reorganise",
            _ => return None,
        }
        .to_owned(),
    )
}

/// An on-demand brain command the user triggered, waiting for the brain to finish with it.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingOp {
    /// "interpret" | "review" | "reorganise" | "reinterpret" | "summarise".
    pub kind: String,
    /// The episode, for "reinterpret" and "summarise".
    pub id: Option<i64>,
    /// [`BrainStatus::now`] when it was queued.
    pub since: f64,
}

/// A button re-enables this long after the click even when the brain never answers.
pub const PENDING_TIMEOUT_S: f64 = 300.0;

pub const TIMED_OUT_TIP: &str = "Timed out waiting for the brain";

/// The label of a busy action ("Interpreting…"), for a pending / running `kind`.
pub fn busy_label(kind: &str) -> &'static str {
    match kind {
        "interpret" => "Interpreting…",
        "review" => "Reviewing…",
        "reorganise" => "Reorganising…",
        "reinterpret" => "Re-interpreting…",
        "summarise" => "Summarising…",
        _ => "Working…",
    }
}

impl BrainStatus {
    /// The user triggered `kind` (for `id`): pending until [`BrainStatus::settle`].
    pub fn queue(&mut self, kind: &str, id: Option<i64>) {
        self.timed_out.retain(|k| k != kind);
        if !self.pending.iter().any(|p| p.kind == kind && p.id == id) {
            self.pending.push(PendingOp {
                kind: kind.to_owned(),
                id,
                since: self.now,
            });
        }
    }

    /// The brain is done with `kind` (whether or not a run came out of it). A finished
    /// "interpret" also ends the pending re-interpretations (they run as interpret jobs).
    pub fn settle(&mut self, kind: &str, id: Option<i64>) {
        self.pending.retain(|p| {
            !(p.kind == kind && p.id == id || kind == "interpret" && p.kind == "reinterpret")
        });
    }

    /// Give up on commands pending longer than [`PENDING_TIMEOUT_S`].
    pub fn expire(&mut self) {
        let now = self.now;
        let mut gone = Vec::new();
        self.pending.retain(|p| {
            let keep = now - p.since < PENDING_TIMEOUT_S;
            if !keep {
                gone.push(p.kind.clone());
            }
            keep
        });
        for k in gone {
            if !self.timed_out.contains(&k) {
                self.timed_out.push(k);
            }
        }
    }

    /// `kind` (for `id`, when given) is queued or running right now.
    pub fn busy(&self, kind: &str, id: Option<i64>) -> bool {
        self.pending
            .iter()
            .any(|p| p.kind == kind && (id.is_none() || p.id == id))
            || (id.is_none() && self.running.as_deref() == Some(kind))
    }

    /// Why a button for `kind` is disabled or carries a note: the brain is unavailable, or a
    /// wait for it ran out.
    pub fn disabled_reason(&self, kind: &str) -> Option<String> {
        match &self.availability {
            None => Some("Checking claude…".to_owned()),
            Some(Err(_)) => self.backlog_line(),
            Some(Ok(_)) if self.timed_out.iter().any(|k| k == kind) => {
                Some(TIMED_OUT_TIP.to_owned())
            }
            Some(Ok(_)) => None,
        }
    }

    /// "interpret · chunk 2/3" for the running job's latest phase.
    pub fn progress_text(&self) -> Option<String> {
        let p = self.progress.as_ref()?;
        Some(format!("{} · {} {}/{}", p.kind, p.label, p.step, p.total))
    }

    /// The label of a busy button: "Interpreting 2/3…" when the job reports chunks.
    pub fn busy_text(&self, kind: &str) -> String {
        match &self.progress {
            Some(p) if p.kind == kind && p.label == "chunk" && p.total > 1 => {
                let base = busy_label(kind).trim_end_matches('…');
                format!("{base} {}/{}…", p.step, p.total)
            }
            _ => busy_label(kind).to_owned(),
        }
    }

    /// Whether a user-triggered command of `kind` is waiting (a finishing "interpret" also
    /// answers a pending "reinterpret").
    pub fn waiting_for(&self, kind: &str) -> bool {
        self.pending
            .iter()
            .any(|p| p.kind == kind || kind == "interpret" && p.kind == "reinterpret")
    }

    /// "brain: interpret queued…" for the first queued command that is not the running job.
    pub fn queued_line(&self) -> Option<String> {
        let p = self
            .pending
            .iter()
            .find(|p| self.running.as_deref() != Some(p.kind.as_str()))?;
        let kind = if p.kind == "reinterpret" {
            "re-interpret"
        } else {
            &p.kind
        };
        Some(format!("brain: {kind} queued…"))
    }

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
    /// Scroll to this episode's card and flash it when a list lays it out (consumed there).
    pub pending_focus: Option<i64>,
    /// The note being edited by hand and its draft text.
    pub editing: Option<EditDraft>,
    /// A hand-edited note whose "Re-interpret" awaits the "Replace your edits?" answer.
    pub confirm_reinterpret: Option<i64>,
    /// The brain's status, refreshed by the app every frame (the cards' Summarise /
    /// Re-interpret buttons read their busy state from it).
    pub brain: BrainStatus,
    /// Notes expanded on a project page (collapsed by default; not persisted).
    pub open_notes: std::collections::HashSet<i64>,
    /// The project page lists every next step, not just the first [`NEXT_STEPS_CAP`].
    pub show_all_steps: bool,
    /// The card flashing right now and the egui time the flash started.
    pub flash: Option<(i64, f64)>,
    /// Conversation notes heard (partly) from an app classed Media — "Mark as media" is
    /// offered on them.
    pub media_convertible: std::collections::HashSet<i64>,
}

/// The text fields of a note being edited. Decisions: one per line. Action items: one per
/// line as `text | owner | due`. Topics: comma separated.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditDraft {
    pub episode_id: i64,
    pub title: String,
    pub summary: String,
    pub decisions: String,
    pub action_items: String,
    pub topics: String,
    focus: bool,
}

impl EditDraft {
    pub fn from_episode(e: &Episode) -> Self {
        Self {
            episode_id: e.id,
            title: e.title.clone(),
            summary: e.summary.clone(),
            decisions: e.decisions.join("\n"),
            action_items: e
                .action_items
                .iter()
                .map(format_action_item)
                .collect::<Vec<_>>()
                .join("\n"),
            topics: e.topics.join(", "),
            focus: true,
        }
    }

    /// The content to store: trimmed, blank lines dropped, an empty title becomes "Untitled".
    pub fn to_content(&self) -> EpisodeContent {
        let lines = |s: &str| -> Vec<String> {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let title = self.title.trim();
        EpisodeContent {
            title: if title.is_empty() {
                "Untitled".to_owned()
            } else {
                title.to_owned()
            },
            summary: self.summary.trim().to_owned(),
            decisions: lines(&self.decisions),
            action_items: self
                .action_items
                .lines()
                .filter_map(parse_action_item)
                .collect(),
            topics: self
                .topics
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect(),
            ..Default::default()
        }
    }
}

/// `text`, `text | owner`, `text | | due` or `text | owner | due`.
pub fn format_action_item(a: &ActionItem) -> String {
    match (&a.owner, &a.due) {
        (None, None) => a.text.clone(),
        (Some(o), None) => format!("{} | {o}", a.text),
        (None, Some(d)) => format!("{} | | {d}", a.text),
        (Some(o), Some(d)) => format!("{} | {o} | {d}", a.text),
    }
}

/// Lenient inverse of [`format_action_item`]: blank lines give `None`; missing or empty
/// owner / due parts are `None`; extra `|` parts are folded into the due text.
pub fn parse_action_item(line: &str) -> Option<ActionItem> {
    let mut parts = line.splitn(3, '|').map(str::trim);
    let text = parts.next().filter(|t| !t.is_empty())?;
    let some = |p: Option<&str>| p.filter(|x| !x.is_empty()).map(str::to_owned);
    Some(ActionItem {
        text: text.to_owned(),
        owner: some(parts.next()),
        due: some(parts.next()),
    })
}

/// Next steps shown on a project page before "Show all".
pub const NEXT_STEPS_CAP: usize = 12;

/// How long a jumped-to card flashes.
const FLASH_SECS: f64 = 1.5;

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
pub fn chip(ui: &mut egui::Ui, t: &Tokens, text: &str, accent: bool) -> egui::Response {
    let font = t.font(10.5);
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        font,
        if accent { t.accent } else { t.text_secondary },
    );
    let size = vec2(galley.size().x + 12.0, 16.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
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
    resp
}

/// A topic chip you can click (hover fill, pointer cursor).
fn topic_chip(ui: &mut egui::Ui, t: &Tokens, text: &str) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), t.font(10.5), t.text_secondary);
    let size = vec2(galley.size().x + 12.0, 16.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if resp.hovered() { t.pressed } else { t.hover };
        ui.painter().rect_filled(rect, 8.0, fill);
        let col = if resp.hovered() {
            t.text
        } else {
            t.text_secondary
        };
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, col);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, text));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn small_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> bool {
    ui.add_enabled(enabled, egui::Button::new(RichText::new(label).size(11.5)))
        .clicked()
}

/// A brain action button: while `kind` (for `id`) is queued or running it is greyed out with a
/// spinner and "Interpreting…"; while the brain is unavailable it is greyed out with the reason
/// as its tooltip. `true` when clicked (never while disabled).
fn brain_button(
    ui: &mut egui::Ui,
    t: &Tokens,
    status: &BrainStatus,
    kind: &str,
    id: Option<i64>,
    label: &str,
) -> bool {
    let busy = status.busy(kind, id);
    let reason = status.disabled_reason(kind);
    if busy {
        let text = status.busy_text(kind);
        let galley = ui.painter().layout_no_wrap(
            text.clone(),
            egui::FontId::proportional(11.5),
            t.text_tertiary,
        );
        let size = egui::vec2(galley.size().x + 30.0, galley.size().y + 6.0);
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
        ui.painter().rect_stroke(
            rect,
            4.0,
            egui::Stroke::new(1.0, t.button_border),
            egui::StrokeKind::Inside,
        );
        egui::Spinner::new()
            .size(10.0)
            .color(t.text_secondary)
            .paint_at(
                ui,
                egui::Rect::from_center_size(
                    egui::pos2(rect.left() + 13.0, rect.center().y),
                    egui::vec2(10.0, 10.0),
                ),
            );
        ui.painter().galley(
            egui::pos2(rect.left() + 23.0, rect.center().y - galley.size().y / 2.0),
            galley,
            t.text_tertiary,
        );
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, false, &text));
        return false;
    }
    let up = status.available();
    let r = ui.add_enabled(up, egui::Button::new(RichText::new(label).size(11.5)));
    match reason {
        Some(why) if !up => {
            r.on_disabled_hover_text(why);
            false
        }
        Some(why) => r.on_hover_text(why).clicked(),
        None => r.clicked(),
    }
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
        let up = status.available();
        for (kind, label, action) in [
            ("interpret", "Interpret now", NotesAction::InterpretNow),
            ("review", "Review now", NotesAction::ReviewNow),
            ("reorganise", "Reorganise…", NotesAction::Reorganise),
        ] {
            if brain_button(ui, t, status, kind, None, label) && up {
                actions.push(action);
            }
        }
        let line = match (&status.running, &status.availability) {
            (Some(job), _) => {
                let what = status.progress_text().unwrap_or_else(|| job.clone());
                match status.backlog_line() {
                    Some(b) => format!("running: {what}… · {b}"),
                    None => format!("running: {what}…"),
                }
            }
            (None, Some(Ok(_))) if status.queued_line().is_some() => {
                status.queued_line().unwrap_or_default()
            }
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
    if state.editing.as_ref().is_some_and(|d| d.episode_id == e.id) {
        return edit_card(ui, t, state, e, actions);
    }
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
                // clickable for its context menu (Mark as a call / Mark as media / Summarise)
                let mut resp = ui.add(egui::Label::new(title_job).sense(Sense::click()).truncate());
                let convertible = state.media_convertible.contains(&e.id);
                if e.is_media() || convertible {
                    resp = resp.on_hover_text(if e.is_media() {
                        "Right-click: Mark as a call, Summarise"
                    } else {
                        "Right-click: Mark as media"
                    });
                }
                if e.is_media() || convertible {
                    resp.context_menu(|ui| {
                        if e.is_media() {
                            if ui
                                .button("Mark as a call")
                                .on_hover_text(
                                    "This was a conversation, not media: file it with your \
                                 conversations and re-interpret it as a meeting",
                                )
                                .clicked()
                            {
                                actions.push(NotesAction::MarkAsCall(e.id));
                                ui.close();
                            }
                            if ui.button("Summarise").clicked() {
                                actions.push(NotesAction::Summarise(e.id));
                                ui.close();
                            }
                        } else if convertible
                            && ui
                                .button("Mark as media")
                                .on_hover_text(
                                    "Not a conversation of yours: file it under Media (no \
                                     todos, no events)",
                                )
                                .clicked()
                        {
                            actions.push(NotesAction::MarkAsMedia(e.id));
                            ui.close();
                        }
                    });
                }
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
                    if state.confirm_reinterpret == Some(e.id) {
                        // right to left: No, Yes, then the question
                        if small_button(ui, "No", true) {
                            state.confirm_reinterpret = None;
                        }
                        if small_button(ui, "Yes", true) {
                            state.confirm_reinterpret = None;
                            actions.push(NotesAction::Reinterpret(e.id));
                        }
                        ui.label(
                            RichText::new("Replace your edits?")
                                .color(t.text_secondary)
                                .size(11.5),
                        );
                    } else {
                        if e.is_media()
                            && brain_button(
                                ui,
                                t,
                                &state.brain,
                                "summarise",
                                Some(e.id),
                                "Summarise",
                            )
                        {
                            actions.push(NotesAction::Summarise(e.id));
                        }
                        if brain_button(
                            ui,
                            t,
                            &state.brain,
                            "reinterpret",
                            Some(e.id),
                            "Re-interpret",
                        ) {
                            if e.user_edited_at.is_some() {
                                state.confirm_reinterpret = Some(e.id);
                            } else {
                                actions.push(NotesAction::Reinterpret(e.id));
                            }
                        }
                        if small_button(ui, "Edit", true) {
                            state.editing = Some(EditDraft::from_episode(e));
                        }
                    }
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.add_space(12.0);
                ui.label(
                    RichText::new(crate::timefmt::range(e.started_at, e.ended_at, &Local))
                        .font(t.mono(10.5))
                        .color(t.text_tertiary),
                );
                if let Some(n) = e.node_id {
                    chip(ui, t, &node_path(nodes, n), true);
                }
                if e.revision > 1 {
                    chip(ui, t, &format!("rev {}", e.revision), false);
                }
                if let Some(at) = e.user_edited_at {
                    let when = crate::transcript::at(at, &Local)
                        .map(|d| format!("{}, {}", d.format("%b %-d"), crate::timefmt::hm(&d)))
                        .unwrap_or_default();
                    chip(ui, t, "edited", false).on_hover_text(format!(
                        "Edited by you, {when}. The brain leaves its text alone."
                    ));
                }
                for topic in e.topics.iter().take(5) {
                    if topic_chip(ui, t, topic)
                        .on_hover_text(format!("Show everything tagged “{topic}”"))
                        .clicked()
                    {
                        actions.push(NotesAction::OpenTopic(topic.clone()));
                    }
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
    focus_card(ui, t, state, e.id, rect);
    ui.add_space(6.0);
    rect
}

/// The edit form of a card: title, summary (markdown source), decisions, action items,
/// topics, with Save (accent; also ⌘S) and Cancel (also Escape).
fn edit_card(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut NotesState,
    e: &Episode,
    actions: &mut Vec<NotesAction>,
) -> egui::Rect {
    let fill = ui.painter().add(egui::Shape::Noop);
    let left = ui.max_rect().left();
    let right = ui.max_rect().right();
    let mut save = ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S));
    let mut cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
    let inner = ui
        .vertical(|ui| {
            ui.add_space(6.0);
            let Some(d) = state.editing.as_mut() else {
                return;
            };
            let field_label = |ui: &mut egui::Ui, text: &str| {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(text)
                        .font(crate::theme::font_medium(ui.ctx(), 12.0))
                        .color(t.text_secondary),
                );
            };
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                let w = ui.available_width() - 12.0;
                let r = ui.add(
                    egui::TextEdit::singleline(&mut d.title)
                        .desired_width(w)
                        .hint_text("Title")
                        .font(crate::theme::font_medium(ui.ctx(), 13.5)),
                );
                if d.focus {
                    r.request_focus();
                    d.focus = false;
                }
            });
            ui.horizontal_top(|ui| {
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    let w = (ui.available_width() - 12.0).max(80.0);
                    ui.set_width(w);
                    field_label(ui, "Summary (markdown: - bullets, **bold**, # headings)");
                    ui.add(
                        egui::TextEdit::multiline(&mut d.summary)
                            .desired_width(w)
                            .desired_rows(6)
                            .font(t.font(12.5)),
                    );
                    field_label(ui, "Decisions (one per line)");
                    ui.add(
                        egui::TextEdit::multiline(&mut d.decisions)
                            .desired_width(w)
                            .desired_rows(2)
                            .font(t.font(12.5)),
                    );
                    field_label(ui, "Action items (one per line: text | owner | due)");
                    ui.add(
                        egui::TextEdit::multiline(&mut d.action_items)
                            .desired_width(w)
                            .desired_rows(3)
                            .font(t.font(12.5)),
                    );
                    field_label(ui, "Topics (comma separated)");
                    ui.add(
                        egui::TextEdit::singleline(&mut d.topics)
                            .desired_width(w)
                            .font(t.font(12.5)),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let save_btn = egui::Button::new(
                            RichText::new("Save").size(11.5).color(egui::Color32::WHITE),
                        )
                        .fill(t.accent);
                        if ui.add(save_btn).clicked() {
                            save = true;
                        }
                        if small_button(ui, "Cancel", true) {
                            cancel = true;
                        }
                        ui.label(
                            RichText::new("⌘S saves · Esc cancels")
                                .color(t.text_tertiary)
                                .size(11.0),
                        );
                    });
                });
            });
            ui.add_space(6.0);
        })
        .response;
    if save && let Some(d) = state.editing.take() {
        actions.push(NotesAction::SaveEdit(e.id, d.to_content()));
    } else if cancel {
        state.editing = None;
    }
    let rect = egui::Rect::from_min_max(
        egui::pos2(left + 6.0, inner.rect.top()),
        egui::pos2(right - 6.0, inner.rect.bottom()),
    );
    ui.painter()
        .set(fill, egui::Shape::rect_filled(rect, 6.0, t.inset));
    ui.painter().rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.0, t.accent_alpha(0.6)),
        egui::StrokeKind::Inside,
    );
    focus_card(ui, t, state, e.id, rect);
    ui.add_space(6.0);
    rect
}

/// The jump-to-note mechanism, run for every laid-out card: scroll to it when it is the
/// pending focus, then flash an accent fill that fades over [`FLASH_SECS`] (repaints are
/// requested only while it fades).
fn focus_card(ui: &mut egui::Ui, t: &Tokens, state: &mut NotesState, id: i64, rect: egui::Rect) {
    if state.pending_focus == Some(id) {
        state.pending_focus = None;
        ui.scroll_to_rect(rect, Some(egui::Align::Center));
        state.flash = Some((id, ui.input(|i| i.time)));
    }
    let Some((fid, start)) = state.flash else {
        return;
    };
    if fid != id {
        return;
    }
    let left = 1.0 - (ui.input(|i| i.time) - start) / FLASH_SECS;
    if left <= 0.0 {
        state.flash = None;
        return;
    }
    ui.painter()
        .rect_filled(rect, 6.0, t.accent_alpha(0.28 * left as f32));
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(33));
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
    state.brain = status.clone();
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
            "No unfiled conversations. Everything is filed under a project.",
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
            let is_project = node.kind == oats_core::NodeKind::Project;
            if is_project {
                next_steps(ui, &t, state, episodes, &mut out.notes);
                project_notes(ui, &t, state, episodes, nodes, &mut out.notes);
                children_section(ui, &t, node, &children, &mut out);
            } else {
                children_section(ui, &t, node, &children, &mut out);
                flat_notes(ui, &t, state, episodes, nodes, this_year, &mut out);
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

fn children_section(
    ui: &mut egui::Ui,
    t: &Tokens,
    node: &Node,
    children: &[ChildSummary],
    out: &mut NodeViewOut,
) {
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
        section_label(ui, t, title);
        if children.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(14.0);
                ui.label(RichText::new(empty).color(t.text_tertiary).size(12.0));
            });
        }
        for c in children {
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
                    .map(|d| format!(" · last {}, {}", d.format("%b %-d"), crate::timefmt::hm(&d)))
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
}

fn flat_notes(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut NotesState,
    episodes: &[Episode],
    nodes: &[Node],
    this_year: i32,
    out: &mut NodeViewOut,
) {
    section_label(ui, t, "Notes");
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
        episode_card(ui, t, state, e, nodes, &mut out.notes, "", None);
    }
}

/// "29 Oct" in local time.
pub fn short_date(ms: i64) -> String {
    crate::transcript::at(ms, &Local)
        .map(|d| d.format("%-d %b").to_string())
        .unwrap_or_default()
}

/// A next step on a project page: the action item and the note it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextStep {
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
    pub episode_id: i64,
    pub episode_title: String,
    pub started_at: i64,
}

/// The action items of `episodes` (newest first, as given; low-signal notes left out), each
/// tagged with its source note.
pub fn collect_next_steps(episodes: &[Episode]) -> Vec<NextStep> {
    episodes
        .iter()
        .filter(|e| !e.low_signal)
        .flat_map(|e| {
            e.action_items.iter().map(|a| NextStep {
                text: a.text.clone(),
                owner: a.owner.clone(),
                due: a.due.clone(),
                episode_id: e.id,
                episode_title: e.title.clone(),
                started_at: e.started_at,
            })
        })
        .collect()
}

/// A text link (accent colour, pointing hand).
fn link(ui: &mut egui::Ui, t: &Tokens, text: &str, size: f32) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(text).color(t.accent).size(size)).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The project page's "Next steps": action items across its notes, each linking to its source.
fn next_steps(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut NotesState,
    episodes: &[Episode],
    actions: &mut Vec<NotesAction>,
) {
    let steps = collect_next_steps(episodes);
    section_label(ui, t, "Next steps");
    if steps.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.label(
                RichText::new("No next steps yet")
                    .color(t.text_tertiary)
                    .size(12.0),
            );
        });
        ui.add_space(8.0);
        return;
    }
    let shown = if state.show_all_steps {
        steps.len()
    } else {
        steps.len().min(NEXT_STEPS_CAP)
    };
    for s in &steps[..shown] {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(14.0);
            ui.label(RichText::new("☐").color(t.text_secondary).size(12.5));
            let mut text = s.text.clone();
            if let Some(o) = &s.owner {
                text.push_str(&format!(" — **{o}**"));
            }
            if let Some(d) = &s.due {
                text.push_str(&format!(" (due {d})"));
            }
            let mut job = egui::text::LayoutJob::default();
            for (span, bold) in bold_spans(&text) {
                job.append(
                    &span,
                    0.0,
                    egui::TextFormat {
                        font_id: if bold {
                            crate::theme::font_medium(ui.ctx(), 12.5)
                        } else {
                            t.font(12.5)
                        },
                        color: t.text,
                        ..Default::default()
                    },
                );
            }
            let bg = ui.painter().add(egui::Shape::Noop);
            let resp = ui
                .add(
                    egui::Label::new(job)
                        .sense(Sense::click())
                        .selectable(false),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!(
                    "from {} · {}",
                    s.episode_title,
                    short_date(s.started_at)
                ));
            if resp.hovered() {
                ui.painter().set(
                    bg,
                    egui::Shape::rect_filled(resp.rect.expand2(vec2(4.0, 1.0)), 4.0, t.hover),
                );
            }
            if resp.clicked() {
                actions.push(NotesAction::GoToNote(s.episode_id));
            }
        });
        ui.add_space(2.0);
    }
    if steps.len() > NEXT_STEPS_CAP {
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            let label = if state.show_all_steps {
                "Show fewer".to_owned()
            } else {
                format!("Show all {}", steps.len())
            };
            if link(ui, t, &label, 11.5).clicked() {
                state.show_all_steps = !state.show_all_steps;
            }
        });
    }
    ui.add_space(8.0);
}

/// Mark a note open on the project page (a low-signal note also leaves its own collapse).
fn open_note(state: &mut NotesState, e: &Episode) {
    state.open_notes.insert(e.id);
    if e.low_signal && !state.expanded.contains(&e.id) {
        state.expanded.push(e.id);
    }
}

/// The project page's "Notes": every note newest first, one dense row each (disclosure
/// triangle, date, time range, title, topic chips); a click opens the full card inline.
fn project_notes(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut NotesState,
    episodes: &[Episode],
    nodes: &[Node],
    actions: &mut Vec<NotesAction>,
) {
    // a jump to a collapsed note opens it before it is laid out
    if let Some(f) = state.pending_focus
        && let Some(e) = episodes.iter().find(|e| e.id == f)
    {
        open_note(state, e);
    }
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(
            RichText::new("Notes")
                .font(crate::theme::font_medium(ui.ctx(), 11.0))
                .color(t.text_tertiary),
        );
        if !episodes.is_empty() {
            ui.add_space(6.0);
            if link(ui, t, "Expand all", 11.0).clicked() {
                for e in episodes {
                    open_note(state, e);
                }
            }
            ui.label(RichText::new("·").color(t.text_tertiary).size(11.0));
            if link(ui, t, "Collapse all", 11.0).clicked() {
                state.open_notes.clear();
            }
        }
    });
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
    ui.add_space(2.0);
    for e in episodes {
        let open = state.open_notes.contains(&e.id);
        if note_row(ui, t, e, open, actions) {
            if open {
                state.open_notes.remove(&e.id);
            } else {
                open_note(state, e);
            }
        }
        if open {
            episode_card(ui, t, state, e, nodes, actions, "", None);
        }
    }
    ui.add_space(8.0);
}

/// One collapsed-note row (24 pt). `true` when the row (not a chip) was clicked.
fn note_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    e: &Episode,
    open: bool,
    actions: &mut Vec<NotesAction>,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Note: {}", e.title))
    });
    if !ui.is_rect_visible(rect) {
        return resp.clicked();
    }
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect.shrink2(vec2(6.0, 1.0)), 4.0, t.hover);
    }
    let cy = rect.center().y;
    let c = egui::pos2(rect.left() + 18.0, cy);
    let tri = if open {
        [
            egui::pos2(c.x - 3.5, c.y - 2.0),
            egui::pos2(c.x + 3.5, c.y - 2.0),
            egui::pos2(c.x, c.y + 2.5),
        ]
    } else {
        [
            egui::pos2(c.x - 2.0, c.y - 3.5),
            egui::pos2(c.x - 2.0, c.y + 3.5),
            egui::pos2(c.x + 2.5, c.y),
        ]
    };
    p.add(egui::Shape::convex_polygon(
        tri.to_vec(),
        t.text_secondary,
        egui::Stroke::NONE,
    ));
    let mut x = rect.left() + 30.0;
    let date = p.layout_no_wrap(short_date(e.started_at), t.mono(10.5), t.text_tertiary);
    let w = date.size().x;
    p.galley(
        egui::pos2(x, cy - date.size().y / 2.0),
        date,
        t.text_tertiary,
    );
    x += 46.0_f32.max(w + 6.0);
    let range = crate::timefmt::range(e.started_at, e.ended_at, &Local);
    let range = p.layout_no_wrap(range, t.mono(10.5), t.text_tertiary);
    let w = range.size().x;
    p.galley(
        egui::pos2(x, cy - range.size().y / 2.0),
        range,
        t.text_tertiary,
    );
    x += w + 10.0;
    // chips from the right edge, as many as fit in a third of the row
    let topics: Vec<&String> = e.topics.iter().take(3).collect();
    let budget = (rect.width() / 3.0).max(60.0);
    let mut chips_w = 0.0_f32;
    let mut fit = 0;
    for tp in &topics {
        let w = p
            .layout_no_wrap((*tp).clone(), t.font(10.5), t.text_secondary)
            .size()
            .x
            + 12.0
            + 4.0;
        if chips_w + w > budget {
            break;
        }
        chips_w += w;
        fit += 1;
    }
    let chips_left = rect.right() - 14.0 - chips_w;
    let title_col = if e.low_signal {
        t.text_tertiary
    } else {
        t.text
    };
    let title = p.layout_no_wrap(
        e.title.clone(),
        crate::theme::font_medium(ui.ctx(), 12.5),
        title_col,
    );
    let clip = Rect::from_min_max(
        egui::pos2(x, rect.top()),
        egui::pos2((chips_left - 8.0).max(x), rect.bottom()),
    );
    p.with_clip_rect(clip)
        .galley(egui::pos2(x, cy - title.size().y / 2.0), title, title_col);
    if fit > 0 {
        let chips_rect = Rect::from_min_max(
            egui::pos2(chips_left, rect.top()),
            egui::pos2(rect.right() - 14.0, rect.bottom()),
        );
        let mut cui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(chips_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        cui.spacing_mut().item_spacing.x = 4.0;
        for tp in topics.into_iter().take(fit) {
            if topic_chip(&mut cui, t, tp)
                .on_hover_text(format!("Show everything tagged “{tp}”"))
                .clicked()
            {
                actions.push(NotesAction::OpenTopic(tp.clone()));
            }
        }
    }
    resp.clicked()
}

/// The single-note view: a breadcrumb ("Work › Commerce project › 29 Oct", the node names
/// clickable) over the note's full card.
pub fn episode_view(
    ui: &mut egui::Ui,
    state: &mut NotesState,
    e: &Episode,
    nodes: &[Node],
) -> Vec<NotesAction> {
    let t = Tokens::get(ui.ctx());
    let mut actions = Vec::new();
    let mut chain = Vec::new();
    let mut cur = e.node_id.and_then(|n| nodes.iter().find(|x| x.id == n));
    while let Some(n) = cur {
        chain.push((n.id, n.name.clone()));
        cur = n.parent_id.and_then(|p| nodes.iter().find(|x| x.id == p));
        if chain.len() > 8 {
            break;
        }
    }
    chain.reverse();
    egui::ScrollArea::vertical()
        .id_salt("episode-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(14.0);
                ui.spacing_mut().item_spacing.x = 4.0;
                for (id, name) in &chain {
                    if link(ui, &t, name, 12.5).clicked() {
                        actions.push(NotesAction::SelectNode(*id));
                    }
                    ui.label(RichText::new("›").color(t.text_tertiary).size(12.5));
                }
                ui.label(
                    RichText::new(short_date(e.started_at))
                        .color(t.text_secondary)
                        .size(12.5),
                );
            });
            ui.add_space(8.0);
            episode_card(ui, &t, state, e, nodes, &mut actions, "", None);
            ui.add_space(12.0);
        });
    actions
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

#[cfg(test)]
mod pending_tests {
    use super::*;

    fn up() -> BrainStatus {
        BrainStatus {
            availability: Some(Ok("2.1".into())),
            ..Default::default()
        }
    }

    #[test]
    fn queue_settle_and_the_queued_line() {
        let mut s = up();
        assert!(!s.busy("interpret", None));
        s.queue("interpret", None);
        s.queue("interpret", None);
        assert_eq!(s.pending.len(), 1, "a second click adds nothing");
        assert!(s.busy("interpret", None));
        assert!(!s.busy("review", None), "other actions stay enabled");
        assert_eq!(s.queued_line().as_deref(), Some("brain: interpret queued…"));
        s.running = Some("interpret".into());
        assert_eq!(s.queued_line(), None, "running, not queued");
        assert!(s.busy("interpret", None));
        s.running = None;
        s.settle("interpret", None);
        assert!(!s.busy("interpret", None));
    }

    #[test]
    fn cards_are_per_episode_and_an_interpret_settles_reinterprets() {
        let mut s = up();
        s.queue("reinterpret", Some(1));
        s.queue("summarise", Some(2));
        assert!(s.busy("reinterpret", Some(1)));
        assert!(!s.busy("reinterpret", Some(2)));
        s.settle("summarise", Some(2));
        assert!(!s.busy("summarise", Some(2)));
        s.settle("interpret", None);
        assert!(!s.busy("reinterpret", Some(1)));
    }

    #[test]
    fn a_wait_that_runs_out_re_enables_with_a_tooltip() {
        let mut s = up();
        s.now = 10.0;
        s.queue("review", None);
        s.now = 10.0 + PENDING_TIMEOUT_S - 1.0;
        s.expire();
        assert!(s.busy("review", None));
        s.now = 10.0 + PENDING_TIMEOUT_S;
        s.expire();
        assert!(!s.busy("review", None));
        assert_eq!(s.disabled_reason("review").as_deref(), Some(TIMED_OUT_TIP));
        assert_eq!(s.disabled_reason("interpret"), None);
        s.queue("review", None);
        assert_eq!(s.disabled_reason("review"), None);
    }

    #[test]
    fn unavailable_says_why() {
        let s = BrainStatus {
            availability: Some(Err("offline".into())),
            ..Default::default()
        };
        assert!(s.disabled_reason("interpret").unwrap().contains("offline"));
    }

    fn run(kind: &str, ok: bool, summary: &str, error: Option<&str>) -> BrainRun {
        BrainRun {
            id: 1,
            tier: 1,
            kind: kind.into(),
            started_at: 0,
            finished_at: Some(1),
            model: "sonnet".into(),
            cost_usd: None,
            duration_ms: None,
            ok,
            error: error.map(str::to_owned),
            summary: summary.into(),
        }
    }

    #[test]
    fn progress_shows_in_the_button_label_and_the_strip_line() {
        let mut s = up();
        s.running = Some("interpret".into());
        assert_eq!(s.busy_text("interpret"), "Interpreting…");
        s.progress = Some(Progress {
            kind: "interpret".into(),
            step: 2,
            total: 3,
            label: "chunk".into(),
        });
        assert_eq!(s.busy_text("interpret"), "Interpreting 2/3…");
        assert_eq!(s.progress_text().as_deref(), Some("interpret · chunk 2/3"));
        assert_eq!(s.busy_text("review"), "Reviewing…");
    }

    #[test]
    fn a_finished_run_says_what_it_did() {
        assert_eq!(
            run_toast(&run("interpret", true, "2 of 3 chunks", None)),
            ("Interpreted: 2 of 3 chunks".to_owned(), false)
        );
        assert_eq!(
            run_toast(&run("interpret", true, "0 of 0 chunks", None)).0,
            "Nothing new to interpret"
        );
        assert_eq!(
            run_toast(&run("review", true, "1 changes over 4 episodes", None)).0,
            "Review: 1 changes over 4 episodes"
        );
        assert_eq!(
            run_toast(&run("review", true, "0 changes over 4 episodes", None)).0,
            "Review: nothing to change"
        );
        assert_eq!(
            run_toast(&run("reorganise", true, "Reorganised: a, b", None)).0,
            "Reorganised: a, b (see the change log)"
        );
        assert_eq!(
            run_toast(&run("reorganise", true, "nothing to reorganise", None)).0,
            "Nothing to reorganise"
        );
        assert_eq!(
            run_toast(&run("interpret", false, "", Some("offline"))),
            ("Interpret failed: offline".to_owned(), true)
        );
        assert_eq!(
            nothing_toast("review").as_deref(),
            Some("Nothing to review")
        );
        assert_eq!(nothing_toast("summarise"), None);
    }
}
