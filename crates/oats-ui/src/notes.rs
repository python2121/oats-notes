//! The All notes view (`View::Notes`, name kept for the saved pref; once "Conversations"):
//! every conversation note, newest first under day headings as collapsed rows (date, time
//! range, title, a chip naming its project or "Unfiled", topic chips; low-signal notes behind
//! "Show N low-signal"), a click opening the full card — title, time range, the
//! node chip (category › project › topic), the summary (a small markdown subset: headings,
//! bullets, **bold**), decisions, action items as a checklist-looking list, topics, a
//! "Re-interpret" button — with the brain's control strip (Interpret now / Review now /
//! Reorganise…, last run, today's spend) and the recent-changes log (what the review and
//! reorganisation passes did, with Undo where possible) at the top.

use chrono::Local;
use egui::{Align2, Rect, RichText, Sense, vec2};
use oats_core::{
    ActionItem, BrainRun, Change, Decision, Episode, EpisodeContent, Node, NoteItem, TranscriptRow,
};

use crate::theme::Tokens;
use crate::transcript::{TranscriptAction, clock, day_heading, day_of, transcript_rows};
use crate::widgets::inset_wrapped;

/// What the user did in a notes view (All notes, a node page, a note card) this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum NotesAction {
    InterpretNow,
    ReviewNow,
    Reorganise,
    Reinterpret(i64),
    /// Delete a note for good (confirmed in the edit view).
    DeleteEpisode(i64),
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
    /// "Mark as media" on a call in an app classed Media: a media note (the Media view).
    MarkAsMedia(i64),
    /// "Summarise" a media note (one model call with the media prompt).
    Summarise(i64),
    /// A note's decision / action item was clicked: the transcript at the segment it was said.
    ShowSegment(i64),
    /// An action item's checkbox: mark it complete (`done`) or open again.
    SetActionItemDone {
        episode_id: i64,
        text: String,
        done: bool,
    },
    /// "Move to…" on a note: the text typed in its field (resolved by the app: the tree
    /// first, the model when that is ambiguous or new).
    MoveText {
        episode: i64,
        text: String,
    },
    /// A note was dragged onto another one and dropped at `at`: the app asks before merging
    /// `dragged` INTO `into`.
    MergeDrop {
        dragged: i64,
        into: i64,
        at: egui::Pos2,
    },
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
        "rewrite" => "Re-interpret",
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
        "reinterpret" | "rewrite" => "Re-interpreting…",
        "summarise" => "Summarising…",
        "move" | "move_segment" => "Moving…",
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

/// What a note row marks, mirroring the sidebar: notes that started at or after
/// `today_start` (local midnight) are today's, `latest` (`Store::latest_note`) is the most
/// recent note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowMarks {
    pub today_start: i64,
    pub latest: Option<i64>,
}

impl Default for RowMarks {
    fn default() -> Self {
        RowMarks {
            today_start: i64::MAX,
            latest: None,
        }
    }
}

/// How a note row is drawn: plain, today's (date and time in white) or the most recent
/// (title, date and time in the accent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowMark {
    Plain,
    Today,
    Latest,
}

impl RowMarks {
    pub fn of(&self, e: &Episode) -> RowMark {
        if self.latest == Some(e.id) {
            RowMark::Latest
        } else if e.started_at >= self.today_start {
            RowMark::Today
        } else {
            RowMark::Plain
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
    /// All notes lists the low-signal notes too (off by default, reset on every view change).
    pub show_low_signal: bool,
    /// Today's start and the most recent note, for the note rows' marks (set by the app
    /// every frame).
    pub marks: RowMarks,
    /// The card flashing right now and the egui time the flash started.
    pub flash: Option<(i64, f64)>,
    /// Conversation notes heard (partly) from an app classed Media — "Mark as media" is
    /// offered on them.
    pub media_convertible: std::collections::HashSet<i64>,
    /// "Move to…": the open inline field, for a note or a transcript segment.
    pub move_draft: Option<crate::move_to::MoveDraft>,
    /// A note dropped on another, waiting for the user's Yes / No.
    pub confirm_merge: Option<crate::merge::MergeConfirm>,
    /// A note whose Delete was clicked in the edit view: (episode id, title, popup anchor).
    pub confirm_delete: Option<(i64, String, egui::Pos2)>,
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
            decisions: e
                .decisions
                .iter()
                .map(|d| d.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
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
            decisions: lines(&self.decisions)
                .into_iter()
                .map(Decision::plain)
                .collect(),
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

    /// [`Self::to_content`], keeping the filing and provenance (`node_id`, `at_ms`,
    /// `segment_id`) of every decision / action item, and an action item's completion
    /// (`done_at`), whose text is unchanged from `old`.
    pub fn to_content_keeping(&self, old: &Episode) -> EpisodeContent {
        let mut c = self.to_content();
        for d in &mut c.decisions {
            if let Some(o) = old.decisions.iter().find(|o| o.text.trim() == d.text) {
                d.node_id = o.node_id;
                d.at_ms = o.at_ms;
                d.segment_id = o.segment_id;
            }
        }
        for a in &mut c.action_items {
            if let Some(o) = old.action_items.iter().find(|o| o.text.trim() == a.text) {
                a.node_id = o.node_id;
                a.at_ms = o.at_ms;
                a.segment_id = o.segment_id;
                a.done_at = o.done_at;
            }
        }
        c
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
        ..Default::default()
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

/// The first markdown link in a line, `[text](https://…)`: `(before, text, url, after)`.
/// Only http(s) targets count; anything else stays literal text.
pub fn md_link(line: &str) -> Option<(String, String, String, String)> {
    let open = line.find('[')?;
    let rest = &line[open + 1..];
    let close = rest.find("](")?;
    let text = &rest[..close];
    let after_text = &rest[close + 2..];
    let end = after_text.find(')')?;
    let url = &after_text[..end];
    if text.is_empty() || !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    Some((
        line[..open].to_owned(),
        text.to_owned(),
        url.to_owned(),
        after_text[end + 1..].to_owned(),
    ))
}

/// A summary line that may carry one markdown link: the link is a clickable hyperlink
/// (accent), the text around it the usual rich line.
fn linked_line(ui: &mut egui::Ui, t: &Tokens, text: &str, size: f32, hl: &str) {
    match md_link(text) {
        Some((before, label, url, after)) => {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if !before.is_empty() {
                    rich_line(ui, t, &before, size, t.text, hl);
                }
                ui.add(egui::Hyperlink::from_label_and_url(
                    RichText::new(label).size(size).color(t.accent),
                    url.clone(),
                ))
                .on_hover_text(url);
                if !after.is_empty() {
                    rich_line(ui, t, &after, size, t.text, hl);
                }
            });
        }
        None => rich_line(ui, t, text, size, t.text, hl),
    }
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

/// A note's decision / action item: with a source (`segment_id`) the text is a link to the
/// transcript at that moment ("Show in transcript at 2:31 pm"); filed under another project
/// than the note (`filed`), a small chip after it names that project (a click opens it).
#[allow(clippy::too_many_arguments)]
fn item_line(
    ui: &mut egui::Ui,
    t: &Tokens,
    text: &str,
    hl: &str,
    (segment, at): (Option<i64>, Option<i64>),
    filed: Option<i64>,
    done: bool,
    nodes: &[Node],
    actions: &mut Vec<NotesAction>,
) {
    let mut job = egui::text::LayoutJob::default();
    let (color, strikethrough) = if done {
        (t.text_tertiary, egui::Stroke::new(1.0, t.text_tertiary))
    } else {
        (t.text, egui::Stroke::NONE)
    };
    for (span, bold) in bold_spans(text) {
        let font = if bold {
            crate::theme::font_medium(ui.ctx(), 12.5)
        } else {
            t.font(12.5)
        };
        crate::search::append_marked(
            &mut job,
            t,
            &span,
            hl,
            egui::TextFormat {
                font_id: font,
                color,
                strikethrough,
                ..Default::default()
            },
        );
    }
    let chip = |ui: &mut egui::Ui, actions: &mut Vec<NotesAction>| {
        if let Some(n) = filed.and_then(|id| nodes.iter().find(|n| n.id == id)) {
            let r = ui
                .add(
                    // one piece: after the text's last line, or on the next one
                    egui::Label::new(
                        RichText::new(format!("→ {}", n.name))
                            .color(t.accent)
                            .size(11.0),
                    )
                    .truncate()
                    .sense(Sense::click())
                    .selectable(false),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!("Filed under {}", node_path(nodes, n.id)));
            if r.clicked() {
                actions.push(NotesAction::SelectNode(n.id));
            }
        }
    };
    // the text wraps in the width left of the row, the chip flows after it
    let w = ui.available_width().max(40.0);
    ui.allocate_ui_with_layout(
        vec2(w, 0.0),
        egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
        |ui| {
            ui.set_max_width(w);
            let Some(seg) = segment else {
                ui.add(egui::Label::new(job).wrap().selectable(false));
                chip(ui, actions);
                return;
            };
            let bg = ui.painter().add(egui::Shape::Noop);
            let when = at
                .map(|ms| crate::timefmt::clock(ms, &Local).trim().to_owned())
                .map(|c| format!("Show in transcript at {c}"))
                .unwrap_or_else(|| "Show in transcript".to_owned());
            let resp = ui
                .add(
                    egui::Label::new(job)
                        .wrap()
                        .sense(Sense::click())
                        .selectable(false),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(when);
            if resp.hovered() {
                ui.painter().set(
                    bg,
                    egui::Shape::rect_filled(resp.rect.expand2(vec2(4.0, 1.0)), 4.0, t.hover),
                );
            }
            if resp.clicked() {
                actions.push(NotesAction::ShowSegment(seg));
            }
            chip(ui, actions);
        },
    );
}

/// "Move to…" on a note's context menu: opens the inline field.
fn move_menu(ui: &mut egui::Ui, e: &Episode, state: &mut NotesState) {
    if ui.button("Move to…").clicked() {
        state.move_draft = Some(crate::move_to::MoveDraft::new(
            oats_brain::MoveTarget::Episode(e.id),
            e.node_id,
        ));
        ui.close();
    }
}

/// The inline "Move to" field under a note's title.
fn move_field(
    ui: &mut egui::Ui,
    t: &Tokens,
    e: &Episode,
    state: &mut NotesState,
    actions: &mut Vec<NotesAction>,
) {
    if let Some(text) = crate::move_to::move_field_in(
        ui,
        t,
        &mut state.move_draft,
        oats_brain::MoveTarget::Episode(e.id),
        false,
    ) {
        actions.push(NotesAction::MoveText {
            episode: e.id,
            text,
        });
    }
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
                    linked_line(ui, t, &b, 12.5, hl);
                });
            }
            MdLine::Text(x) => linked_line(ui, t, &x, 12.5, hl),
        }
    }
}

/// A small rounded chip.
pub fn chip(ui: &mut egui::Ui, t: &Tokens, text: &str, accent: bool) -> egui::Response {
    chip_max(ui, t, text, accent, f32::INFINITY)
}

/// [`chip`], never wider than `max_w` (the text cut with …, then the full text on hover).
pub fn chip_max(
    ui: &mut egui::Ui,
    t: &Tokens,
    text: &str,
    accent: bool,
    max_w: f32,
) -> egui::Response {
    let font = t.font(10.5);
    let galley = crate::widgets::truncated_galley(
        ui,
        text,
        font,
        if accent { t.accent } else { t.text_secondary },
        max_w - 12.0,
    );
    let elided = galley.elided;
    let size = vec2(galley.size().x + 12.0, 16.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::hover());
    if elided {
        resp = resp.on_hover_text(text);
    }
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
    // the buttons, then the status line after them or (narrow) on the line below at the same
    // inset; a line longer than the whole row is cut with …
    inset_wrapped(ui, 8.0, 8.0, |ui| {
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
    inset_wrapped(ui, 8.0, 8.0, |ui| {
        ui.label(
            RichText::new(format!("usage ${:.2} today", status.spend_today_usd))
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
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                // Undo at the right edge, the description cut with … before it
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    if undoable && small_button(ui, "Undo", true) {
                        actions.push(NotesAction::Undo(c.id));
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
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
                    });
                });
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

/// `rect` (a card or a note row) as a drop zone for a dragged note: a drop asks to merge it
/// into `into`.
fn merge_drop(
    ui: &mut egui::Ui,
    t: &Tokens,
    rect: Rect,
    into: i64,
    actions: &mut Vec<NotesAction>,
) {
    if let Some(dragged) = crate::merge::drop_zone(ui, t, rect, into) {
        let at = ui.ctx().pointer_latest_pos().unwrap_or(rect.center());
        actions.push(NotesAction::MergeDrop { dragged, into, at });
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
            // The title and the buttons share the first row while the title keeps at least
            // `CARD_TITLE_MIN` (it wraps in what the buttons leave); in a narrower card the
            // buttons take their own row under it, right-aligned.
            let buttons_w = card_buttons_width(ui, state, e);
            let room = ui.available_width() - 12.0 - 8.0 - buttons_w - 12.0;
            let stacked = room < CARD_TITLE_MIN;
            let button_h = ui.spacing().interact_size.y;
            ui.horizontal_top(|ui| {
                ui.add_space(12.0);
                let title_w = if stacked {
                    ui.available_width() - 12.0
                } else {
                    room
                };
                let title_font = crate::theme::font_medium(ui.ctx(), 13.5);
                let line_h = ui.fonts_mut(|f| f.row_height(&title_font));
                ui.allocate_ui_with_layout(
                    vec2(title_w.max(40.0), 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_max_width(title_w.max(40.0));
                        if !stacked {
                            // the first line centred on the buttons
                            ui.add_space(((button_h - line_h) / 2.0).max(0.0));
                        }
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
                                font_id: title_font.clone(),
                                color: title_col,
                                ..Default::default()
                            },
                        );
                        // clickable for its context menu (Mark as a call / Mark as media / Summarise),
                        // draggable onto another note to merge into it
                        let mut resp = ui.add(
                            egui::Label::new(title_job)
                                .sense(Sense::click_and_drag())
                                .wrap(),
                        );
                        crate::merge::drag_source(ui, t, &resp, e.id, &e.title);
                        let convertible = state.media_convertible.contains(&e.id);
                        if e.is_media() || convertible {
                            resp = resp.on_hover_text(if e.is_media() {
                                "Right-click: Mark as a call, Summarise"
                            } else {
                                "Right-click: Mark as media"
                            });
                        }
                        resp.context_menu(|ui| {
                            move_menu(ui, e, state);
                            if e.is_media() || convertible {
                                ui.separator();
                            }
                            {
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
                                            "Not a conversation of yours: make it a media note (no \
                                     todos, no events)",
                                        )
                                        .clicked()
                                {
                                    actions.push(NotesAction::MarkAsMedia(e.id));
                                    ui.close();
                                }
                            }
                        });
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
                    },
                );
                if !stacked {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        ui.add_space(12.0);
                        card_buttons(ui, t, state, e, actions);
                    });
                }
            });
            if stacked {
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(12.0);
                        card_buttons(ui, t, state, e, actions);
                    });
                });
            }
            move_field(ui, t, e, state, actions);
            inset_wrapped(ui, 12.0, 12.0, |ui| {
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
                                    item_line(
                                        ui,
                                        t,
                                        &d.text,
                                        hl,
                                        (d.segment_id, d.at_ms),
                                        d.node_id.filter(|n| Some(*n) != e.node_id),
                                        false,
                                        nodes,
                                        actions,
                                    );
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
                                    ui.set_min_height(ui.spacing().interact_size.y);
                                    ui.add_space(6.0);
                                    let mut done = a.done_at.is_some();
                                    if action_checkbox(ui, &mut done) {
                                        actions.push(NotesAction::SetActionItemDone {
                                            episode_id: e.id,
                                            text: a.text.clone(),
                                            done,
                                        });
                                    }
                                    let mut text = a.text.clone();
                                    if let Some(o) = &a.owner {
                                        text.push_str(&format!(" — **{o}**"));
                                    }
                                    if let Some(d) = &a.due {
                                        text.push_str(&format!(" (due {d})"));
                                    }
                                    item_line(
                                        ui,
                                        t,
                                        &text,
                                        hl,
                                        (a.segment_id, a.at_ms),
                                        a.node_id.filter(|n| Some(*n) != e.node_id),
                                        a.done_at.is_some(),
                                        nodes,
                                        actions,
                                    );
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
    merge_drop(ui, t, rect, e.id, actions);
    focus_card(ui, t, state, e.id, rect);
    ui.add_space(6.0);
    rect
}

/// The narrowest a card's title gets beside its buttons before they move to their own row.
const CARD_TITLE_MIN: f32 = 150.0;

/// A card's buttons (Edit, Re-interpret, Summarise on a media note; or the "Replace your
/// edits?" question), added right to left.
fn card_buttons(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut NotesState,
    e: &Episode,
    actions: &mut Vec<NotesAction>,
) {
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
        if e.is_media() && brain_button(ui, t, &state.brain, "summarise", Some(e.id), "Summarise") {
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
}

/// The width [`card_buttons`] takes, measured before it is drawn.
fn card_buttons_width(ui: &egui::Ui, state: &NotesState, e: &Episode) -> f32 {
    let gap = ui.spacing().item_spacing.x;
    let brain = |kind: &str, id: Option<i64>, label: &str| {
        if state.brain.busy(kind, id) {
            ui.painter()
                .layout_no_wrap(
                    state.brain.busy_text(kind),
                    egui::FontId::proportional(11.5),
                    egui::Color32::WHITE,
                )
                .size()
                .x
                + 30.0
        } else {
            crate::widgets::button_width(ui, label, 11.5)
        }
    };
    let widths: Vec<f32> = if state.confirm_reinterpret == Some(e.id) {
        vec![
            crate::widgets::button_width(ui, "No", 11.5),
            crate::widgets::button_width(ui, "Yes", 11.5),
            ui.painter()
                .layout_no_wrap(
                    "Replace your edits?".to_owned(),
                    egui::FontId::proportional(11.5),
                    egui::Color32::WHITE,
                )
                .size()
                .x,
        ]
    } else {
        let mut w = vec![
            brain("reinterpret", Some(e.id), "Re-interpret"),
            crate::widgets::button_width(ui, "Edit", 11.5),
        ];
        if e.is_media() {
            w.push(brain("summarise", Some(e.id), "Summarise"));
        }
        w
    };
    widths.iter().sum::<f32>() + gap * (widths.len().saturating_sub(1)) as f32
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
    // Escape in the open "Move to" field closes that field, not the edit
    let moving = state
        .move_draft
        .as_ref()
        .is_some_and(|d| d.target == oats_brain::MoveTarget::Episode(e.id) && !d.sidebar);
    let mut cancel =
        !moving && state.confirm_delete.is_none() && ui.input(|i| i.key_pressed(egui::Key::Escape));
    let mut open_move = false;
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
                        // moving is independent of the draft: the edit stays open, Save
                        // still writes it
                        if small_button(ui, "Move to…", true) {
                            open_move = true;
                        }
                        let del = ui.add(egui::Button::new(
                            RichText::new("Delete").size(11.5).color(t.danger),
                        ));
                        if del.clicked() {
                            state.confirm_delete =
                                Some((e.id, e.title.clone(), del.rect.left_bottom()));
                        }
                        ui.label(
                            RichText::new("⌘S saves · Esc cancels")
                                .color(t.text_tertiary)
                                .size(11.0),
                        );
                    });
                });
            });
            if state.editing.is_some() {
                move_field(ui, t, e, state, actions);
            }
            ui.add_space(6.0);
        })
        .response;
    if open_move {
        state.move_draft = Some(crate::move_to::MoveDraft::new(
            oats_brain::MoveTarget::Episode(e.id),
            e.node_id,
        ));
    }
    if save && let Some(d) = state.editing.take() {
        actions.push(NotesAction::SaveEdit(e.id, d.to_content_keeping(e)));
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

/// All notes (`View::Notes`): `episodes` = every conversation note, newest first
/// (`Store::conversations`), under day headings as collapsed rows with a filing chip; the
/// low-signal ones only behind "Show N low-signal".
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
            "No notes yet. Notes appear here as the brain interprets what it hears.",
            t.font(12.5),
            t.text_tertiary,
        );
        return actions;
    }
    let opts = ListOpts {
        days: true,
        filing: true,
        hide_low_signal: true,
    };
    prepare_focus(state, episodes, opts);
    let low_signal = episodes.iter().filter(|e| e.low_signal).count();
    if low_signal == episodes.len() && !state.show_low_signal {
        let rect = ui.available_rect_before_wrap();
        ui.add_space((rect.height() / 2.0 - 20.0).max(0.0));
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(format!(
                    "{low_signal} low-signal note{} hidden",
                    if low_signal == 1 { "" } else { "s" }
                ))
                .color(t.text_tertiary)
                .size(12.5),
            );
            low_signal_toggle(ui, &t, state, low_signal);
        });
        return actions;
    }
    egui::ScrollArea::vertical()
        .id_salt("notes-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(8.0);
            notes_header(ui, &t, state, episodes, opts, low_signal);
            ui.add_space(6.0);
            note_list(ui, &t, state, episodes, nodes, &mut actions, opts);
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
/// projects (or topics) as clickable rows (a project: "Next steps" first, with the items filed
/// here from notes homed elsewhere — `filed` — inline), the notes (episodes) of the subtree
/// newest first,
/// and the raw transcript segments filed there behind a toggle (collapsed by default).
pub fn node_view(
    ui: &mut egui::Ui,
    state: &mut NotesState,
    node: &Node,
    episodes: &[Episode],
    filed: &[(Episode, NoteItem)],
    rows: &[TranscriptRow],
    nodes: &[Node],
) -> NodeViewOut {
    let t = Tokens::get(ui.ctx());
    let mut out = NodeViewOut::default();
    let children = child_summaries(nodes, node.id, episodes);
    egui::ScrollArea::vertical()
        .id_salt("node-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(10.0);
            // a long path or description wraps (at the list's 14 pt insets)
            inset_wrapped(ui, 14.0, 14.0, |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(node_path(nodes, node.id))
                            .font(crate::theme::font_medium(ui.ctx(), 16.0))
                            .color(t.text),
                    )
                    .wrap(),
                );
            });
            if !node.description.is_empty() {
                inset_wrapped(ui, 14.0, 14.0, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&node.description)
                                .color(t.text_secondary)
                                .size(11.5),
                        )
                        .wrap(),
                    );
                });
            }
            ui.add_space(8.0);
            let is_project = node.kind == oats_core::NodeKind::Project;
            let is_category = node.kind == oats_core::NodeKind::Category;
            if is_project {
                next_steps(ui, &t, state, episodes, filed, nodes, false, &mut out.notes);
                project_notes(ui, &t, state, episodes, nodes, &mut out.notes);
                children_section(ui, &t, node, &children, &mut out);
            } else {
                next_steps(
                    ui,
                    &t,
                    state,
                    episodes,
                    filed,
                    nodes,
                    is_category,
                    &mut out.notes,
                );
                children_section(ui, &t, node, &children, &mut out);
                project_notes(ui, &t, state, episodes, nodes, &mut out.notes);
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
                out.transcript = transcript_rows(ui, rows, &mut state.move_draft);
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
            inset_wrapped(ui, 14.0, 14.0, |ui| {
                ui.label(RichText::new(empty).color(t.text_tertiary).size(12.0));
            });
        }
        for c in children {
            // the name and its "N notes · last …" flow on; a long name wraps
            inset_wrapped(ui, 14.0, 14.0, |ui| {
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
    /// When the user ticked it complete (`None` = open).
    pub done_at: Option<i64>,
    /// The project the step belongs to (its own `node_id` or the note's home, a topic mapped
    /// to its project); `None` when it is only in a category or unfiled.
    pub project_id: Option<i64>,
}

/// The project a node belongs to: itself for a project, the nearest project ancestor for a
/// topic, `None` for a category or an unknown id.
pub fn project_of(nodes: &[Node], node_id: i64) -> Option<&Node> {
    let mut cur = nodes.iter().find(|n| n.id == node_id)?;
    for _ in 0..=nodes.len() {
        if cur.kind == oats_core::NodeKind::Project {
            return Some(cur);
        }
        cur = nodes.iter().find(|n| Some(n.id) == cur.parent_id)?;
    }
    None
}

/// A project page's next steps: the action items of its own `episodes` (low-signal notes
/// left out) and, inline among them, the action items filed under the project from notes
/// homed elsewhere (`filed`, [`oats_core::Store::items_filed_under`]); one list, each item
/// once (by note and text), each tagged with its source note. The open items come first,
/// newest note first (a note's items in their order), then the completed ones, most recently
/// completed first.
pub fn collect_next_steps(
    episodes: &[Episode],
    filed: &[(Episode, NoteItem)],
    nodes: &[Node],
) -> Vec<NextStep> {
    let step = |e: &Episode, a: &ActionItem| NextStep {
        project_id: a
            .node_id
            .or(e.node_id)
            .and_then(|id| project_of(nodes, id))
            .map(|p| p.id),
        text: a.text.clone(),
        owner: a.owner.clone(),
        due: a.due.clone(),
        episode_id: e.id,
        episode_title: e.title.clone(),
        started_at: e.started_at,
        done_at: a.done_at,
    };
    let mut steps: Vec<NextStep> = episodes
        .iter()
        .filter(|e| !e.low_signal)
        .flat_map(|e| e.action_items.iter().map(move |a| step(e, a)))
        .collect();
    for (e, item) in filed {
        if let NoteItem::Action(a) = item
            && !e.low_signal
            && !steps
                .iter()
                .any(|s| s.episode_id == e.id && s.text == a.text)
        {
            steps.push(step(e, a));
        }
    }
    // stable: a note's items keep their order
    steps.sort_by_key(|s| {
        (
            s.done_at.is_some(),
            std::cmp::Reverse(s.done_at),
            std::cmp::Reverse(s.started_at),
            std::cmp::Reverse(s.episode_id),
        )
    });
    steps
}

/// How many of `steps` ([`collect_next_steps`] order: open, then completed) the page shows,
/// and whether the "Show all N" / "Show fewer" link is drawn. Collapsed: the open items up to
/// [`NEXT_STEPS_CAP`]; completed items only behind the link, which shows whenever something
/// is hidden that way (more open items than the cap, or anything completed).
pub fn next_steps_shown(steps: &[NextStep], show_all: bool) -> (usize, bool) {
    let open = steps.iter().filter(|s| s.done_at.is_none()).count();
    let link = open > NEXT_STEPS_CAP || open < steps.len();
    let shown = if show_all {
        steps.len()
    } else {
        open.min(NEXT_STEPS_CAP)
    };
    (shown, link)
}

/// An action item's checkbox (the todos' look), centred on the first line of its 12.5-pt
/// text. True when the user toggled it.
fn action_checkbox(ui: &mut egui::Ui, done: &mut bool) -> bool {
    let was = *done;
    crate::widgets::check_box(ui, done, &egui::FontId::proportional(12.5))
        .on_hover_text(if was { "Mark not done" } else { "Mark done" })
        .changed()
}

/// A text link (accent colour, pointing hand).
fn link(ui: &mut egui::Ui, t: &Tokens, text: &str, size: f32) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(text).color(t.accent).size(size)).sense(Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// "Next steps": action items across the page's notes, each linking to its source. On a
/// category page (`chips`) each carries a chip naming its project; an empty category skips
/// the section.
#[allow(clippy::too_many_arguments)]
fn next_steps(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut NotesState,
    episodes: &[Episode],
    filed: &[(Episode, NoteItem)],
    nodes: &[Node],
    chips: bool,
    actions: &mut Vec<NotesAction>,
) {
    let steps = collect_next_steps(episodes, filed, nodes);
    if chips && steps.is_empty() {
        return;
    }
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
    let (shown, show_link) = next_steps_shown(&steps, state.show_all_steps);
    // where "Show all" sat: expanded, a "Show fewer" is drawn there too, so the list can be
    // toggled back without moving the pointer (and again at the bottom after a long scroll)
    let collapsed = next_steps_shown(&steps, false).0;
    for (i, s) in steps[..shown].iter().enumerate() {
        if state.show_all_steps && show_link && i == collapsed && i > 0 {
            next_steps_link(ui, t, state, steps.len());
            ui.add_space(2.0);
        }
        ui.horizontal_top(|ui| {
            // The row pitch egui's own checkbox gave; the box is centred on the text anyway.
            ui.set_min_height(ui.spacing().interact_size.y);
            ui.add_space(10.0);
            let mut done = s.done_at.is_some();
            if action_checkbox(ui, &mut done) {
                actions.push(NotesAction::SetActionItemDone {
                    episode_id: s.episode_id,
                    text: s.text.clone(),
                    done,
                });
            }
            let (color, strikethrough) = if s.done_at.is_some() {
                (t.text_tertiary, egui::Stroke::new(1.0, t.text_tertiary))
            } else {
                (t.text, egui::Stroke::NONE)
            };
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
                        color,
                        strikethrough,
                        ..Default::default()
                    },
                );
            }
            // the text wraps in what is left of the row (14 pt short of its right edge); the
            // project chip follows its last line, or takes the next one when it doesn't fit
            let w = (ui.available_width() - 14.0).max(40.0);
            ui.allocate_ui_with_layout(
                vec2(w, 0.0),
                egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true),
                |ui| {
                    ui.set_max_width(w);
                    let bg = ui.painter().add(egui::Shape::Noop);
                    let resp = ui
                        .add(
                            egui::Label::new(job)
                                .wrap()
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
                            egui::Shape::rect_filled(
                                resp.rect.expand2(vec2(4.0, 1.0)),
                                4.0,
                                t.hover,
                            ),
                        );
                    }
                    if resp.clicked() {
                        actions.push(NotesAction::GoToNote(s.episode_id));
                    }
                    if chips
                        && let Some(p) = s
                            .project_id
                            .and_then(|id| nodes.iter().find(|n| n.id == id))
                    {
                        let r = ui
                            .add(
                                // one piece: on the next line when it doesn't fit after
                                // the text (cut with … only past the whole width)
                                egui::Label::new(
                                    RichText::new(format!("→ {}", p.name))
                                        .color(t.accent)
                                        .size(11.0),
                                )
                                .truncate()
                                .sense(Sense::click())
                                .selectable(false),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(format!("Filed under {}", node_path(nodes, p.id)));
                        if r.clicked() {
                            actions.push(NotesAction::SelectNode(p.id));
                        }
                    }
                },
            );
        });
        ui.add_space(2.0);
    }
    if show_link {
        next_steps_link(ui, t, state, steps.len());
    }
    ui.add_space(8.0);
}

/// The "Show all N" / "Show fewer" link of the next steps.
fn next_steps_link(ui: &mut egui::Ui, t: &Tokens, state: &mut NotesState, total: usize) {
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        let label = if state.show_all_steps {
            "Show fewer".to_owned()
        } else {
            format!("Show all {total}")
        };
        if link(ui, t, &label, 11.5).clicked() {
            state.show_all_steps = !state.show_all_steps;
        }
    });
}

/// Mark a note open on the project page (a low-signal note also leaves its own collapse).
fn open_note(state: &mut NotesState, e: &Episode) {
    state.open_notes.insert(e.id);
    if e.low_signal && !state.expanded.contains(&e.id) {
        state.expanded.push(e.id);
    }
}

/// How a list of collapsed note rows is drawn ([`note_list`]).
#[derive(Clone, Copy, Debug, Default)]
struct ListOpts {
    /// Day headings where the local day changes (All notes).
    days: bool,
    /// Each row carries the chip naming where it is filed (All notes).
    filing: bool,
    /// Low-signal notes are listed only while [`NotesState::show_low_signal`] is on.
    hide_low_signal: bool,
}

/// Run before a note list is laid out: a jump to a collapsed note opens it (and reveals it
/// when it is a hidden low-signal note), so `focus_card` finds its card.
fn prepare_focus(state: &mut NotesState, episodes: &[Episode], opts: ListOpts) {
    if let Some(f) = state.pending_focus
        && let Some(e) = episodes.iter().find(|e| e.id == f)
    {
        open_note(state, e);
        if opts.hide_low_signal && e.low_signal {
            state.show_low_signal = true;
        }
    }
}

/// Whether a row is listed under `opts`.
fn listed(state: &NotesState, e: &Episode, opts: ListOpts) -> bool {
    !(opts.hide_low_signal && e.low_signal && !state.show_low_signal)
}

/// The "Notes" header line: the label, "Expand all · Collapse all" over the listed notes, and
/// (All notes) the low-signal toggle when `low_signal` > 0.
fn notes_header(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut NotesState,
    episodes: &[Episode],
    opts: ListOpts,
    low_signal: usize,
) {
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
                    if listed(state, e, opts) {
                        open_note(state, e);
                    }
                }
            }
            ui.label(RichText::new("·").color(t.text_tertiary).size(11.0));
            if link(ui, t, "Collapse all", 11.0).clicked() {
                state.open_notes.clear();
            }
        }
        if opts.hide_low_signal && low_signal > 0 {
            ui.label(RichText::new("·").color(t.text_tertiary).size(11.0));
            low_signal_toggle(ui, t, state, low_signal);
        }
    });
}

/// "Show N low-signal" / "Hide low-signal".
fn low_signal_toggle(ui: &mut egui::Ui, t: &Tokens, state: &mut NotesState, n: usize) {
    let label = if state.show_low_signal {
        "Hide low-signal".to_owned()
    } else {
        format!("Show {n} low-signal")
    };
    if link(ui, t, &label, 11.0).clicked() {
        state.show_low_signal = !state.show_low_signal;
    }
}

/// The collapsed note rows shared by the project page and All notes: one [`note_row`] each,
/// a click toggling the full card inline (`NotesState::open_notes`).
fn note_list(
    ui: &mut egui::Ui,
    t: &Tokens,
    state: &mut NotesState,
    episodes: &[Episode],
    nodes: &[Node],
    actions: &mut Vec<NotesAction>,
    opts: ListOpts,
) {
    let this_year = chrono::Datelike::year(&Local::now());
    let mut last_day = None;
    for e in episodes {
        if !listed(state, e, opts) {
            continue;
        }
        if opts.days {
            let day = day_of(e.started_at, &Local);
            if last_day != Some(day) {
                if last_day.is_some() {
                    ui.add_space(6.0);
                }
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
                ui.add_space(2.0);
            }
        }
        // "Move to…" from a closed row: its field lives in the open card
        if state
            .move_draft
            .as_ref()
            .is_some_and(|d| d.target == oats_brain::MoveTarget::Episode(e.id) && !d.sidebar)
        {
            state.open_notes.insert(e.id);
        }
        let open = state.open_notes.contains(&e.id);
        let filing = opts.filing.then_some(nodes);
        // under day headings the date column only repeats the heading: a narrow list drops it
        let show_date = !(opts.days && ui.available_width() < NOTE_ROW_NARROW);
        if note_row(ui, t, e, state, open, filing, show_date, actions) {
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
    let opts = ListOpts::default();
    prepare_focus(state, episodes, opts);
    notes_header(ui, t, state, episodes, opts, 0);
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
    note_list(ui, t, state, episodes, nodes, actions, opts);
    ui.add_space(8.0);
}

/// Where a note is filed, for its All notes chip: the project (a topic maps to its project)
/// with the note's full path, or `None` when it has no node or only a bare category.
pub fn filing_of<'a>(nodes: &'a [Node], e: &Episode) -> Option<(&'a Node, String)> {
    let node = e.node_id?;
    let p = project_of(nodes, node)?;
    Some((p, node_path(nodes, node)))
}

/// The filing chip of a note: its label, the project it opens (`None`: "Unfiled", not
/// clickable) and its hover text.
fn filing_chip_label(nodes: &[Node], e: &Episode) -> (String, Option<i64>, String) {
    match filing_of(nodes, e) {
        Some((p, path)) => {
            let mut name: String = p.name.chars().take(28).collect();
            if p.name.chars().count() > 28 {
                name.push('…');
            }
            (name, Some(p.id), path)
        }
        None => {
            let hover = match e.node_id.and_then(|n| nodes.iter().find(|x| x.id == n)) {
                Some(c) => format!("In {}, not under a project", c.name),
                None => "Not filed under a project".to_owned(),
            };
            ("Unfiled".to_owned(), None, hover)
        }
    }
}

/// The chip naming where a note is filed: outlined, the project's name (clickable) or
/// "Unfiled" in the tertiary colour (not clickable).
/// Never wider than `max_w` (the label cut with …).
fn filing_chip(
    ui: &mut egui::Ui,
    t: &Tokens,
    label: &str,
    clickable: bool,
    max_w: f32,
) -> egui::Response {
    let galley =
        crate::widgets::truncated_galley(ui, label, t.font(10.5), t.text_secondary, max_w - 12.0);
    let size = vec2(galley.size().x + 12.0, 16.0);
    let sense = if clickable {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let col = if !clickable {
            t.text_tertiary
        } else if resp.hovered() {
            t.text
        } else {
            t.text_secondary
        };
        if clickable && resp.hovered() {
            ui.painter().rect_filled(rect, 8.0, t.hover);
        }
        ui.painter().rect_stroke(
            rect.shrink(0.5),
            8.0,
            egui::Stroke::new(1.0, t.button_border),
            egui::StrokeKind::Inside,
        );
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, col);
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, clickable, label));
    if clickable {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// A collapsed note row's title keeps at least this much before chips are dropped.
const NOTE_ROW_TITLE_MIN: f32 = 110.0;
/// The narrowest a row's filing chip is cut to.
const FILING_CHIP_MIN: f32 = 56.0;
/// Below this row width a list under day headings drops the rows' (repeated) date column.
const NOTE_ROW_NARROW: f32 = 520.0;

/// One collapsed-note row (24 pt). `true` when the row (not a chip) was clicked. With
/// `filing` (All notes) a chip naming the note's project (or "Unfiled") comes before the topic
/// chips. Today's notes draw their date and time range in pure white, the most recent note
/// ([`NotesState::marks`]) its title, date and time range in the accent colour. Without
/// `show_date` the row starts at the time range. The painted title is clipped before the
/// chips, and the chips never take more than a third of the row.
#[allow(clippy::too_many_arguments)]
fn note_row(
    ui: &mut egui::Ui,
    t: &Tokens,
    e: &Episode,
    state: &mut NotesState,
    open: bool,
    filing: Option<&[Node]>,
    show_date: bool,
    actions: &mut Vec<NotesAction>,
) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click_and_drag());
    resp.context_menu(|ui| {
        move_menu(ui, e, state);
    });
    crate::merge::drag_source(ui, t, &resp, e.id, &e.title);
    merge_drop(ui, t, rect.shrink2(vec2(6.0, 0.0)), e.id, actions);
    let mark = state.marks.of(e);
    let resp = resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(format!(
            "{} · {}{}",
            e.title,
            short_date(e.started_at),
            match mark {
                RowMark::Latest => " · most recent",
                RowMark::Today => " · today",
                RowMark::Plain => "",
            }
        ));
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("Note: {}", e.title))
    });
    if !ui.is_rect_visible(rect) {
        return resp.clicked();
    }
    let (when_col, latest) = match mark {
        RowMark::Latest => (t.accent, true),
        RowMark::Today => (egui::Color32::WHITE, false),
        RowMark::Plain => (t.text_tertiary, false),
    };
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
    if show_date {
        let date = p.layout_no_wrap(short_date(e.started_at), t.mono(10.5), when_col);
        let w = date.size().x;
        p.galley(egui::pos2(x, cy - date.size().y / 2.0), date, when_col);
        x += 46.0_f32.max(w + 6.0);
    }
    let range = crate::timefmt::range(e.started_at, e.ended_at, &Local);
    let range = p.layout_no_wrap(range, t.mono(10.5), when_col);
    // the widest 12-hour range ("11:00–11:09 am"), so the titles line up
    let col = p
        .layout_no_wrap("00:00–00:00 am".to_owned(), t.mono(10.5), when_col)
        .size()
        .x;
    let w = range.size().x.max(col);
    p.galley(egui::pos2(x, cy - range.size().y / 2.0), range, when_col);
    x += w + 10.0;
    // chips from the right edge, as many as fit in a third of the row and leave the title
    // `NOTE_ROW_TITLE_MIN` (the filing chip always: it comes first, cut with … when it is
    // wider than that)
    let chip_w = |text: &str| {
        p.layout_no_wrap(text.to_owned(), t.font(10.5), t.text_secondary)
            .size()
            .x
            + 12.0
            + 4.0
    };
    let filing = filing.map(|nodes| filing_chip_label(nodes, e));
    let topics: Vec<&String> = e.topics.iter().take(3).collect();
    let room = rect.right() - 14.0 - 8.0 - x;
    let budget = (rect.width() / 3.0).min(room - NOTE_ROW_TITLE_MIN).max(0.0);
    let filing_w = filing
        .as_ref()
        .map_or(0.0, |(l, _, _)| chip_w(l).min(budget.max(FILING_CHIP_MIN)));
    let mut chips_w = filing_w;
    let mut fit = 0;
    for tp in &topics {
        let w = chip_w(tp);
        if chips_w + w > budget {
            break;
        }
        chips_w += w;
        fit += 1;
    }
    let chips_left = rect.right() - 14.0 - chips_w;
    let title_col = if latest {
        t.accent
    } else if e.low_signal {
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
    if fit > 0 || filing.is_some() {
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
        if let Some((label, project, hover)) = &filing {
            let r = filing_chip(&mut cui, t, label, project.is_some(), filing_w - 4.0)
                .on_hover_text(hover);
            if let Some(id) = project
                && r.clicked()
            {
                actions.push(NotesAction::SelectNode(*id));
            }
        }
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
            // a long path wraps, every line at the list's inset
            inset_wrapped(ui, 14.0, 14.0, |ui| {
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
    fn an_edit_keeps_the_filing_and_source_of_unchanged_items_and_steps_merge_inline() {
        let old = Episode {
            id: 1,
            started_at: 10,
            ended_at: 20,
            title: "Dev call".into(),
            summary: String::new(),
            node_id: Some(5),
            decisions: vec![Decision {
                text: "Ship it".into(),
                node_id: Some(7),
                at_ms: Some(11),
                segment_id: Some(3),
            }],
            action_items: vec![ActionItem {
                text: "Do x".into(),
                node_id: Some(7),
                segment_id: Some(3),
                ..Default::default()
            }],
            topics: vec![],
            low_signal: false,
            model: String::new(),
            cost_usd: None,
            revision: 1,
            created_at: 0,
            last_revised_at: 0,
            user_edited_at: None,
            kind: "conversation".into(),
        };
        let mut d = EditDraft::from_episode(&old);
        d.action_items.push_str("\nDo y | Ana");
        let mut old_done = old.clone();
        old_done.action_items[0].done_at = Some(99);
        let c = d.to_content_keeping(&old_done);
        assert_eq!(c.action_items[0].done_at, Some(99), "completion kept");
        assert_eq!(c.action_items[1].done_at, None);
        let c = d.to_content_keeping(&old);
        assert_eq!(c.decisions[0].segment_id, Some(3));
        assert_eq!(c.action_items[0].node_id, Some(7));
        assert_eq!(c.action_items[1].node_id, None);

        // the project's own note (older) and the item filed here from the dev call (newer)
        let own = Episode {
            id: 2,
            started_at: 5,
            action_items: vec![ActionItem {
                text: "Own step".into(),
                ..Default::default()
            }],
            ..old.clone()
        };
        let filed = vec![(old.clone(), NoteItem::Action(old.action_items[0].clone()))];
        let steps = collect_next_steps(std::slice::from_ref(&own), &filed, &[]);
        let texts: Vec<&str> = steps.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["Do x", "Own step"]);
        assert_eq!(steps[0].episode_id, 1);
        // a note both homed here and cross-filed lists its item once
        let both = collect_next_steps(std::slice::from_ref(&old), &filed, &[]);
        assert_eq!(both.len(), 1);
    }

    #[test]
    fn project_of_walks_up_and_steps_carry_their_project() {
        let node = |id, parent_id, kind| Node {
            id,
            parent_id,
            kind,
            name: format!("n{id}"),
            description: String::new(),
            created_by: Origin::User,
            position: 0,
            archived: false,
        };
        let nodes = vec![
            node(1, None, NodeKind::Category),
            node(2, Some(1), NodeKind::Project),
            node(3, Some(2), NodeKind::Topic),
            node(4, Some(1), NodeKind::Project),
        ];
        assert_eq!(project_of(&nodes, 3).map(|n| n.id), Some(2));
        assert_eq!(project_of(&nodes, 2).map(|n| n.id), Some(2));
        assert!(project_of(&nodes, 1).is_none());
        assert!(project_of(&nodes, 99).is_none());

        let ep = |id: i64, home: Option<i64>, item_node: Option<i64>, text: &str| Episode {
            id,
            started_at: id,
            node_id: home,
            action_items: vec![ActionItem {
                text: text.into(),
                node_id: item_node,
                ..Default::default()
            }],
            title: format!("n{id}"),
            ended_at: id,
            summary: String::new(),
            decisions: vec![],
            topics: vec![],
            low_signal: false,
            model: String::new(),
            cost_usd: None,
            revision: 1,
            created_at: 0,
            last_revised_at: 0,
            user_edited_at: None,
            kind: "conversation".into(),
        };
        let own = ep(1, Some(3), None, "own, homed on a topic");
        let cross = ep(2, Some(4), None, "cross-filed");
        let unfiled = ep(3, None, None, "unfiled");
        let filed = vec![(
            cross.clone(),
            NoteItem::Action(ActionItem {
                text: "cross-filed".into(),
                node_id: Some(3),
                ..Default::default()
            }),
        )];
        let steps = collect_next_steps(&[own, unfiled], &filed, &nodes);
        let project = |t: &str| {
            steps
                .iter()
                .find(|s| s.text.starts_with(t))
                .map(|s| s.project_id)
        };
        assert_eq!(project("own"), Some(Some(2)));
        assert_eq!(
            project("cross"),
            Some(Some(2)),
            "item node (topic) beats the note's home"
        );
        assert_eq!(project("unfiled"), Some(None));
    }

    #[test]
    fn completed_next_steps_go_last_newest_completion_first_behind_show_all() {
        let ep = |id: i64, started_at: i64, items: &[(&str, Option<i64>)]| Episode {
            id,
            started_at,
            title: format!("n{id}"),
            action_items: items
                .iter()
                .map(|(text, done_at)| ActionItem {
                    text: (*text).into(),
                    done_at: *done_at,
                    ..Default::default()
                })
                .collect(),
            ended_at: started_at,
            summary: String::new(),
            node_id: None,
            decisions: vec![],
            topics: vec![],
            low_signal: false,
            model: String::new(),
            cost_usd: None,
            revision: 1,
            created_at: 0,
            last_revised_at: 0,
            user_edited_at: None,
            kind: "conversation".into(),
        };
        let eps = vec![
            ep(1, 100, &[("old a", Some(5)), ("old b", None)]),
            ep(
                2,
                200,
                &[("new a", None), ("new b", Some(9)), ("new c", None)],
            ),
        ];
        let steps = collect_next_steps(&eps, &[], &[]);
        let texts: Vec<&str> = steps.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["new a", "new c", "old b", "new b", "old a"]);
        // collapsed: the open ones only, and the link because two are completed
        assert_eq!(next_steps_shown(&steps, false), (3, true));
        assert_eq!(next_steps_shown(&steps, true), (5, true));
        // nothing completed, under the cap: no link
        let open = vec![ep(3, 1, &[("x", None), ("y", None)])];
        let steps = collect_next_steps(&open, &[], &[]);
        assert_eq!(next_steps_shown(&steps, false), (2, false));
        // every item completed: nothing shown collapsed, only the link
        let done = vec![ep(4, 1, &[("x", Some(1))])];
        let steps = collect_next_steps(&done, &[], &[]);
        assert_eq!(next_steps_shown(&steps, false), (0, true));
        // more open items than the cap
        let many: Vec<(String, Option<i64>)> = (0..NEXT_STEPS_CAP + 2)
            .map(|i| (format!("s{i}"), None))
            .collect();
        let refs: Vec<(&str, Option<i64>)> = many.iter().map(|(t, d)| (t.as_str(), *d)).collect();
        let steps = collect_next_steps(&[ep(5, 1, &refs)], &[], &[]);
        assert_eq!(next_steps_shown(&steps, false), (NEXT_STEPS_CAP, true));
    }

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
    fn markdown_links_are_split_out() {
        assert_eq!(
            md_link("**Media:** [Rust 2026](https://youtu.be/x) · 35 min"),
            Some((
                "**Media:** ".to_owned(),
                "Rust 2026".to_owned(),
                "https://youtu.be/x".to_owned(),
                " · 35 min".to_owned()
            ))
        );
        assert_eq!(md_link("no link [here]"), None);
        assert_eq!(md_link("[x](javascript:alert(1))"), None);
        assert_eq!(md_link("[](https://a.b)"), None);
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

/// The confirmation pop-up for deleting a note from its edit view. Returns the episode id when
/// confirmed; clears `confirm` on any answer (Escape and Cancel close it).
pub fn confirm_delete_popup(
    ctx: &egui::Context,
    t: &Tokens,
    confirm: &mut Option<(i64, String, egui::Pos2)>,
) -> Option<i64> {
    let (id, title, at) = confirm.as_ref()?.clone();
    let mut out = None;
    let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    egui::Area::new(egui::Id::new("delete-confirm"))
        .order(egui::Order::Foreground)
        .fixed_pos(at)
        .constrain(true)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(320.0);
                ui.label(
                    RichText::new(format!("Delete “{title}”?"))
                        .size(12.0)
                        .color(t.text),
                );
                ui.label(
                    RichText::new(
                        "The transcript stays; the note will not be re-created. \
                         Undo not available.",
                    )
                    .size(11.0)
                    .color(t.text_tertiary),
                );
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Delete").color(t.danger)).clicked() {
                        out = Some(id);
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        });
    if out.is_some() || close {
        *confirm = None;
    }
    out
}
