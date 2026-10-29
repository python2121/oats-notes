//! Hotword commands: "hey computer, property taxes are due on 12/12/2026, also set a reminder
//! one week ahead". Detection runs on the transcript TEXT of each new segment (not on audio):
//! configurable phrases, case- and punctuation-insensitive so ASR variants ("Hey, computer.")
//! match. A detected command goes straight to a small haiku request with its own prompt and
//! schema; the actions that come back (an event AND a reminder for the example above) are
//! executed against the store with `source = hotword` and the source segment id, so the user
//! can audit what a spoken command did.
//!
//! Todos and reminders are one thing (a reminder is a todo with a due time): `add_todo` and
//! its alias `add_reminder` both make a todo; every spoken `add_event` also gets a todo due
//! when the event starts (an all-day event: 09:00 that day), so a dictated due date pops up
//! that morning. Interpretation-extracted events get no todo (too noisy).

use chrono::{DateTime, TimeZone};
use oats_core::store::name_key;
use oats_core::{
    Event, NewEvent, NewTodo, Node, NodeKind, Note, Origin, Store, StoreError, Todo, TranscriptRow,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::claude_cli::Request;
use crate::dates;
use crate::interpret::{DEFAULT_MAX_NEW_NODES, render, render_tree, strip_delimiters};

pub const COMMAND_SYSTEM: &str = include_str!("../prompts/command.system.md");
pub const COMMAND_USER: &str = include_str!("../prompts/command.user.md");

/// Environment variable with comma-separated hotword phrases.
pub const HOTWORDS_ENV: &str = "OATS_HOTWORDS";

/// At most this many actions of one command are executed (more is never a real command).
pub const MAX_ACTIONS: usize = 8;
/// Longest title / name / date / time string kept from a command action (characters).
pub const MAX_TITLE_CHARS: usize = 200;
/// Longest note text / details kept from a command action (characters).
pub const MAX_TEXT_CHARS: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotwordConfig {
    /// Lower-case phrases, words separated by single spaces.
    pub phrases: Vec<String>,
}

impl Default for HotwordConfig {
    fn default() -> Self {
        HotwordConfig {
            phrases: vec!["hey computer".into(), "hey oats".into()],
        }
    }
}

impl HotwordConfig {
    /// `OATS_HOTWORDS="hey computer,ok notes"`, else the defaults.
    pub fn from_env() -> Self {
        match std::env::var(HOTWORDS_ENV) {
            Ok(v) if !v.trim().is_empty() => HotwordConfig {
                phrases: v
                    .split(',')
                    .map(|p| words(p).join(" "))
                    .filter(|p| !p.is_empty())
                    .collect(),
            },
            _ => HotwordConfig::default(),
        }
    }
}

/// Lower-case words with edge punctuation stripped ("Hey," → "hey"; "12/12/2026" is kept).
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Whether a segment may carry a voice command: only what the user said into the microphone.
/// System audio ("them": a call, a video) saying "hey computer" never commands this computer,
/// and neither does a microphone echo of it.
pub fn can_command(row: &oats_core::TranscriptRow) -> bool {
    row.source == oats_core::AudioSourceKind::Mic && row.echo_of.is_none()
}

/// The command text after the hotword, if the segment contains one (the original words after
/// the phrase, so dates and names survive).
pub fn detect(text: &str, cfg: &HotwordConfig) -> Option<String> {
    let norm = words(text);
    let original: Vec<&str> = text.split_whitespace().collect();
    if norm.len() != original.len() {
        // a token was pure punctuation: fall back to the normalised words
        return detect_in(&norm, &norm, cfg);
    }
    detect_in(&norm, &original, cfg)
}

/// The command after the EARLIEST hotword spoken (whichever phrase it is; at one position the
/// longest phrase wins). A second hotword later in the utterance stays part of the command
/// text, so the model sees both requests and returns actions for each.
fn detect_in<S: AsRef<str>>(
    norm: &[String],
    original: &[S],
    cfg: &HotwordConfig,
) -> Option<String> {
    let phrases: Vec<Vec<&str>> = cfg
        .phrases
        .iter()
        .map(|p| p.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>())
        .filter(|p| !p.is_empty())
        .collect();
    for i in 0..norm.len() {
        let matched = phrases
            .iter()
            .filter(|p| {
                i + p.len() <= norm.len()
                    && norm[i..i + p.len()]
                        .iter()
                        .map(String::as_str)
                        .eq(p.iter().copied())
            })
            .map(Vec::len)
            .max();
        if let Some(len) = matched {
            let tail: Vec<&str> = original[i + len..].iter().map(AsRef::as_ref).collect();
            let cmd = tail
                .join(" ")
                .trim_start_matches([',', '.', ':', '!', '?', ' '])
                .trim()
                .to_owned();
            return Some(cmd);
        }
    }
    None
}

/// One action the model derived from the command.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct CommandAction {
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub all_day: Option<bool>,
    #[serde(default)]
    pub relative_to_event: Option<bool>,
    #[serde(default)]
    pub offset_days: Option<i64>,
    /// "in 2 hours / 2 days / 3 weeks": minutes from the moment the command was spoken.
    #[serde(default)]
    pub in_minutes: Option<i64>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub details: Option<String>,
    #[serde(default)]
    pub node_name: Option<String>,
    #[serde(default)]
    pub category_name: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct CommandResult {
    #[serde(default)]
    pub actions: Vec<CommandAction>,
}

pub fn command_schema() -> Value {
    let short = json!({ "type": ["string", "null"], "maxLength": MAX_TITLE_CHARS });
    let long = json!({ "type": ["string", "null"], "maxLength": MAX_TEXT_CHARS });
    json!({
        "type": "object",
        "properties": {
            "actions": {
                "type": "array",
                "maxItems": MAX_ACTIONS,
                "items": {
                    "type": "object",
                    "properties": {
                        "intent": { "type": "string", "enum": ["add_event", "add_todo", "add_reminder", "add_note", "create_project", "unknown"] },
                        "title": short,
                        "date": { "type": ["string", "null"], "description": "YYYY-MM-DD", "maxLength": 32 },
                        "time": { "type": ["string", "null"], "description": "HH:MM", "maxLength": 32 },
                        "all_day": { "type": "boolean" },
                        "relative_to_event": { "type": "boolean" },
                        "offset_days": {
                            "type": ["integer", "null"],
                            "minimum": -dates::MAX_OFFSET_DAYS,
                            "maximum": dates::MAX_OFFSET_DAYS
                        },
                        "in_minutes": {
                            "type": ["integer", "null"],
                            "minimum": 0,
                            "maximum": dates::MAX_IN_MINUTES
                        },
                        "text": long,
                        "details": long,
                        "node_name": short,
                        "category_name": short
                    },
                    "required": ["intent", "title", "date", "time", "all_day", "relative_to_event", "offset_days", "in_minutes", "text", "details", "node_name", "category_name"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["actions"],
        "additionalProperties": false
    })
}

/// The command request. `now` is the moment the command was SPOKEN (the segment's own
/// timestamp, not the time the request runs — a command worked off a backlog after an
/// outage still resolves "tomorrow" against the day it was said).
pub fn command_request(
    tree: &[Node],
    command: &str,
    now: &DateTime<chrono::Local>,
    model: Option<String>,
) -> Request {
    Request {
        system_prompt: COMMAND_SYSTEM.to_owned(),
        user_prompt: render(
            COMMAND_USER,
            &[
                ("now", &dates::describe_now(now)),
                ("tree", &render_tree(tree, None)),
                ("command", &strip_delimiters(command)),
            ],
        ),
        json_schema: Some(command_schema()),
        model,
        max_turns: None,
    }
}

/// Parse the structured output, bounded whatever the model returned: at most
/// [`MAX_ACTIONS`] actions, strings cut to [`MAX_TITLE_CHARS`] / [`MAX_TEXT_CHARS`],
/// `offset_days` clamped to ±[`dates::MAX_OFFSET_DAYS`].
pub fn parse_command(v: &Value) -> Result<CommandResult, serde_json::Error> {
    let mut r: CommandResult = serde_json::from_value(v.clone())?;
    r.actions.truncate(MAX_ACTIONS);
    let cut = |s: &mut Option<String>, n: usize| {
        if let Some(t) = s
            && t.chars().count() > n
        {
            *t = t.chars().take(n).collect();
        }
    };
    for a in &mut r.actions {
        a.intent = a.intent.chars().take(32).collect();
        for s in [
            &mut a.title,
            &mut a.date,
            &mut a.time,
            &mut a.node_name,
            &mut a.category_name,
        ] {
            cut(s, MAX_TITLE_CHARS);
        }
        cut(&mut a.text, MAX_TEXT_CHARS);
        cut(&mut a.details, MAX_TEXT_CHARS);
        a.offset_days = a
            .offset_days
            .map(|d| d.clamp(-dates::MAX_OFFSET_DAYS, dates::MAX_OFFSET_DAYS));
        a.in_minutes = a.in_minutes.map(|m| m.clamp(0, dates::MAX_IN_MINUTES));
    }
    Ok(r)
}

/// What executing a command did.
#[derive(Clone, Debug, PartialEq)]
pub enum Executed {
    Event(Event),
    /// A todo (`add_todo`, or a reminder: a todo with a due time).
    Todo(Todo),
    Note(Note),
    Project(Node),
    /// Not understood, or missing what it needed (the text says what).
    Unknown(String),
}

impl Executed {
    /// A one-line confirmation for the status strip.
    pub fn describe<Tz: TimeZone>(&self, tz: &Tz) -> String
    where
        Tz::Offset: std::fmt::Display,
    {
        let day = |ms: i64| {
            tz.timestamp_millis_opt(ms)
                .single()
                .map(|d| d.format("%a %-d %b %Y").to_string())
                .unwrap_or_default()
        };
        let at = |ms: i64| {
            tz.timestamp_millis_opt(ms)
                .single()
                .map(|d| d.format("%a %-d %b %Y %H:%M").to_string())
                .unwrap_or_default()
        };
        match self {
            Executed::Event(e) if e.all_day => {
                format!("Event: {} on {}", e.title, day(e.when_start))
            }
            Executed::Event(e) => format!("Event: {} at {}", e.title, at(e.when_start)),
            Executed::Todo(r) => match r.due {
                Some(due) => format!("Todo: {} · due {}", r.text, at(due)),
                None => format!("Todo: {}", r.text),
            },
            Executed::Note(n) => format!("Note: {}", n.text),
            Executed::Project(p) => format!("Project: {}", p.name),
            Executed::Unknown(t) => format!("Not understood: {t}"),
        }
    }

    /// The confirmation pop-up's text for something the command created ("Todo added: …",
    /// "Reminder set: … — Thu 12 Dec, 09:00", "Event added: …", "Note filed: …",
    /// "Project created: …"); `None` for what wasn't understood.
    pub fn confirmation<Tz: TimeZone>(&self, tz: &Tz) -> Option<String>
    where
        Tz::Offset: std::fmt::Display,
    {
        let fmt = |ms: i64, f: &str| {
            tz.timestamp_millis_opt(ms)
                .single()
                .map(|d| d.format(f).to_string())
                .unwrap_or_default()
        };
        Some(match self {
            Executed::Todo(t) => match t.due {
                Some(due) => format!(
                    "Reminder set: {} — {}",
                    t.text,
                    fmt(due, "%a %-d %b, %H:%M")
                ),
                None => format!("Todo added: {}", t.text),
            },
            Executed::Event(e) if e.all_day => {
                format!(
                    "Event added: {} — {}",
                    e.title,
                    fmt(e.when_start, "%a %-d %b %Y")
                )
            }
            Executed::Event(e) => format!(
                "Event added: {} — {}",
                e.title,
                fmt(e.when_start, "%a %-d %b %Y, %H:%M")
            ),
            Executed::Note(n) => {
                let mut text: String = n.text.chars().take(60).collect();
                if n.text.chars().count() > 60 {
                    text.push('…');
                }
                format!("Note filed: {text}")
            }
            Executed::Project(p) => format!("Project created: {}", p.name),
            Executed::Unknown(_) => return None,
        })
    }
}

/// When an event's todo is due: its start; an all-day event's at
/// [`dates::default_due_time`] that day.
fn event_due<Tz: TimeZone>(ev: &Event, tz: &Tz) -> Option<i64> {
    if !ev.all_day {
        return Some(ev.when_start);
    }
    let day = tz
        .timestamp_millis_opt(ev.when_start)
        .single()?
        .date_naive();
    dates::due_ms(day, None, tz)
}

/// Store a voice command's todo, deduplicated per segment (a retried command doesn't add it
/// twice): `hotword:<segment>:reminder:<due day>:<text>`, or `…:todo:<text>` undated. A newly
/// created one is recorded in the change log.
fn add_todo<Tz: TimeZone>(
    store: &Store,
    t: &NewTodo,
    now_ms: i64,
    tz: &Tz,
) -> Result<Todo, StoreError>
where
    Tz::Offset: std::fmt::Display,
{
    let seg = t.source_segment_id.unwrap_or(0);
    let key = match t.due.and_then(|d| tz.timestamp_millis_opt(d).single()) {
        Some(d) => format!(
            "hotword:{seg}:reminder:{}:{}",
            d.format("%Y-%m-%d"),
            name_key(&t.text)
        ),
        None => format!("hotword:{seg}:todo:{}", name_key(&t.text)),
    };
    let todo = store.add_todo(
        &NewTodo {
            dedupe_key: Some(key),
            ..t.clone()
        },
        now_ms,
    )?;
    if todo.created_at == now_ms {
        let due = todo
            .due
            .and_then(|d| tz.timestamp_millis_opt(d).single())
            .map(|d| format!(" (due {})", d.format("%a %-d %b %Y %H:%M")))
            .unwrap_or_default();
        store.record_change(
            None,
            "todo_create",
            &format!("Todo by voice command: {}{due}", todo.text),
            &json!(null),
            &json!(todo),
            now_ms,
        )?;
    }
    Ok(todo)
}

fn find_by_name(tree: &[Node], name: &str) -> Option<Node> {
    let key = name_key(name);
    tree.iter()
        .find(|n| !n.archived && name_key(&n.name) == key)
        .cloned()
}

/// Execute the actions for a command spoken in `segment` (at most
/// [`DEFAULT_MAX_NEW_NODES`] new projects; see [`execute_capped`]).
pub fn execute<Tz: TimeZone>(
    store: &Store,
    tree: &[Node],
    segment: &TranscriptRow,
    result: &CommandResult,
    now_ms: i64,
    tz: &Tz,
) -> Result<Vec<Executed>, StoreError>
where
    Tz::Offset: std::fmt::Display,
{
    execute_capped(
        store,
        tree,
        segment,
        result,
        now_ms,
        tz,
        DEFAULT_MAX_NEW_NODES,
    )
}

/// [`execute`] with the per-run cap on new projects (the scheduler passes
/// `max_new_nodes_per_run`); a `create_project` past the cap is reported as not done. The
/// action list is bounded again here ([`MAX_ACTIONS`], `offset_days` clamped) whatever the
/// caller parsed.
pub fn execute_capped<Tz: TimeZone>(
    store: &Store,
    tree: &[Node],
    segment: &TranscriptRow,
    result: &CommandResult,
    now_ms: i64,
    tz: &Tz,
    max_new_nodes: usize,
) -> Result<Vec<Executed>, StoreError>
where
    Tz::Offset: std::fmt::Display,
{
    let mut new_nodes = 0usize;
    let mut out = Vec::new();
    let mut last_event: Option<Event> = None;
    let seg = segment.id;
    let text_of = |a: &CommandAction| {
        a.text
            .clone()
            .or_else(|| a.title.clone())
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    for a in result.actions.iter().take(MAX_ACTIONS) {
        let node = a.node_name.as_deref().and_then(|n| find_by_name(tree, n));
        let offset_days = a
            .offset_days
            .map(|d| d.clamp(-dates::MAX_OFFSET_DAYS, dates::MAX_OFFSET_DAYS));
        match a.intent.as_str() {
            "add_event" => {
                let title = a
                    .title
                    .clone()
                    .or_else(|| a.text.clone())
                    .unwrap_or_default();
                let title = title.trim();
                let date = a.date.as_deref().and_then(dates::parse_date);
                let (Some(d), false) = (date, title.is_empty()) else {
                    out.push(Executed::Unknown(format!(
                        "event without a date or title: {}",
                        text_of(a)
                    )));
                    continue;
                };
                let time = a.time.as_deref().and_then(dates::parse_time);
                let Some(when_start) = dates::to_ms(d, time, tz) else {
                    continue;
                };
                let ev = store.upsert_event(
                    &NewEvent {
                        when_start,
                        when_end: None,
                        all_day: a.all_day.unwrap_or(time.is_none()),
                        title: title.to_owned(),
                        details: a.details.clone().unwrap_or_default().trim().to_owned(),
                        node_id: node.as_ref().map(|n| n.id),
                        source: Some(Origin::Hotword),
                        source_segment_id: Some(seg),
                        source_episode_id: None,
                        confidence: Some(1.0),
                        dedupe_key: Some(format!(
                            "hotword:{seg}:{}:{}",
                            d.format("%Y-%m-%d"),
                            name_key(title)
                        )),
                    },
                    now_ms,
                )?;
                // the event's own todo, due when it starts (all day: that morning), so a
                // dictated due date pops up on the day
                if let Some(due) = event_due(&ev, tz) {
                    add_todo(
                        store,
                        &NewTodo {
                            text: ev.title.clone(),
                            due: Some(due),
                            event_id: Some(ev.id),
                            source: Some(Origin::Hotword),
                            source_segment_id: Some(seg),
                            node_id: ev.node_id,
                            dedupe_key: None,
                        },
                        now_ms,
                        tz,
                    )?;
                }
                last_event = Some(ev.clone());
                out.push(Executed::Event(ev));
            }
            // a reminder is a todo with a due time
            "add_todo" | "add_reminder" => {
                let reminder = a.intent == "add_reminder";
                let relative = a.relative_to_event.unwrap_or(false)
                    || (a.date.is_none() && a.in_minutes.is_none() && offset_days.is_some());
                let spoken = tz.timestamp_millis_opt(segment.started_at).single();
                let in_minutes = a
                    .in_minutes
                    .or_else(|| a.date.as_deref().and_then(dates::parse_in_phrase));
                let (due, default_text) = if relative {
                    match &last_event {
                        Some(ev) => {
                            let due = event_due(ev, tz)
                                .and_then(|ms| tz.timestamp_millis_opt(ms).single())
                                .and_then(|s| dates::shift_days(&s, offset_days.unwrap_or(0)))
                                .map(|d| d.timestamp_millis());
                            let days = -offset_days.unwrap_or(0);
                            let when = match days {
                                1 => "tomorrow".to_owned(),
                                7 => "in one week".to_owned(),
                                n if n > 0 => format!("in {n} days"),
                                _ => "".to_owned(),
                            };
                            (due, format!("{} {when}", ev.title).trim().to_owned())
                        }
                        None => (None, String::new()),
                    }
                } else if let Some(m) = in_minutes {
                    (
                        spoken.and_then(|s| dates::after_minutes(&s, m)),
                        String::new(),
                    )
                } else {
                    let due = a.date.as_deref().and_then(dates::parse_date).and_then(|d| {
                        dates::due_ms(d, a.time.as_deref().and_then(dates::parse_time), tz)
                    });
                    (due, String::new())
                };
                // a reminder needs a time; a todo may be undated — unless it asked for a date
                // that couldn't be read
                let unreadable = a.date.as_deref().is_some_and(|d| !d.trim().is_empty());
                if due.is_none() && (reminder || relative || unreadable) {
                    out.push(Executed::Unknown(format!(
                        "reminder without a date: {}",
                        text_of(a)
                    )));
                    continue;
                }
                let text = {
                    let t = text_of(a);
                    if t.is_empty() { default_text } else { t }
                };
                if text.is_empty() {
                    out.push(Executed::Unknown("todo without text".into()));
                    continue;
                }
                let t = add_todo(
                    store,
                    &NewTodo {
                        text,
                        due,
                        event_id: if relative {
                            last_event.as_ref().map(|e| e.id)
                        } else {
                            None
                        },
                        source: Some(Origin::Hotword),
                        source_segment_id: Some(seg),
                        node_id: node.as_ref().map(|n| n.id),
                        dedupe_key: None,
                    },
                    now_ms,
                    tz,
                )?;
                out.push(Executed::Todo(t));
            }
            "add_note" => {
                let text = text_of(a);
                if text.is_empty() {
                    out.push(Executed::Unknown("empty note".into()));
                    continue;
                }
                let n = store.insert_note(
                    node.as_ref().map(|n| n.id),
                    &text,
                    Origin::Hotword,
                    Some(seg),
                    now_ms,
                )?;
                out.push(Executed::Note(n));
            }
            "create_project" => {
                let name = a
                    .title
                    .clone()
                    .or_else(|| a.text.clone())
                    .unwrap_or_default();
                let name = name.trim();
                if name.is_empty() {
                    out.push(Executed::Unknown("project without a name".into()));
                    continue;
                }
                let category = a
                    .category_name
                    .as_deref()
                    .and_then(|c| find_by_name(tree, c))
                    .filter(|n| n.kind == NodeKind::Category)
                    .or_else(|| {
                        tree.iter()
                            .find(|n| n.kind == NodeKind::Category && !n.archived)
                            .cloned()
                    });
                let Some(category) = category else {
                    out.push(Executed::Unknown(
                        "no category to put the project under".into(),
                    ));
                    continue;
                };
                let existed = store.find_node(Some(category.id), name)?.is_some();
                if !existed && new_nodes >= max_new_nodes {
                    out.push(Executed::Unknown(format!(
                        "project '{name}' not created: the limit of {max_new_nodes} new projects per run was reached"
                    )));
                    continue;
                }
                let p = store.add_node(
                    Some(category.id),
                    NodeKind::Project,
                    name,
                    a.details.as_deref().unwrap_or(""),
                    Origin::Hotword,
                )?;
                if !existed {
                    new_nodes += 1;
                    store.record_change(
                        None,
                        "node_create",
                        &format!("Created project '{}' by voice command", p.name),
                        &json!(null),
                        &json!(p),
                        now_ms,
                    )?;
                }
                out.push(Executed::Project(p));
            }
            _ => out.push(Executed::Unknown(text_of(a))),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_users_own_microphone_commands() {
        let mic = oats_core::TranscriptRow::default();
        assert!(can_command(&mic));
        let them = oats_core::TranscriptRow {
            source: oats_core::AudioSourceKind::System,
            ..mic.clone()
        };
        assert!(!can_command(&them));
        let echo = oats_core::TranscriptRow {
            echo_of: Some(4),
            ..mic
        };
        assert!(!can_command(&echo));
    }
    use chrono::{Datelike, FixedOffset, NaiveDate, Timelike};

    const SENTENCE: &str =
        "Hey, computer, property taxes are due on 12/12/2026, also set a reminder one week ahead.";

    #[test]
    fn detects_hotwords_loosely() {
        let cfg = HotwordConfig::default();
        assert_eq!(
            detect(SENTENCE, &cfg).as_deref(),
            Some("property taxes are due on 12/12/2026, also set a reminder one week ahead.")
        );
        assert_eq!(
            detect("HEY COMPUTER remind me", &cfg).as_deref(),
            Some("remind me")
        );
        assert_eq!(
            detect("so I said hey oats: note that milk is out", &cfg).as_deref(),
            Some("note that milk is out")
        );
        assert_eq!(
            detect("hey , computer , do it", &cfg).as_deref(),
            Some("do it")
        );
        assert_eq!(detect("the computer said hey", &cfg), None);
        assert_eq!(detect("heycomputer", &cfg), None);
        assert_eq!(detect("", &cfg), None);
        let custom = HotwordConfig {
            phrases: vec!["ok notes".into()],
        };
        assert_eq!(
            detect("Ok, Notes! lunch at noon", &custom).as_deref(),
            Some("lunch at noon")
        );
        assert_eq!(detect(SENTENCE, &custom), None);
    }

    #[test]
    fn request_carries_command_and_tree() {
        let s = Store::open_in_memory().unwrap();
        let tree = s.nodes().unwrap();
        let req = command_request(
            &tree,
            "do the thing",
            &chrono::Local::now(),
            Some("haiku".into()),
        );
        assert!(req.user_prompt.contains("do the thing"));
        assert!(req.user_prompt.contains("Admin (category)"));
        assert_eq!(req.system_prompt, COMMAND_SYSTEM);
        assert_eq!(req.model.as_deref(), Some("haiku"));
    }

    /// The user's own example, end to end against the store with the model's answer canned
    /// (what the command prompt asks for).
    #[test]
    fn property_taxes_example_creates_event_and_reminder() {
        let s = Store::open_in_memory().unwrap();
        let sid = s.begin_session(0, "e").unwrap();
        let seg = s.insert_segment(sid, 1_000, 5_000, SENTENCE, None).unwrap();
        let tree = s.nodes().unwrap();
        let tz = FixedOffset::east_opt(3600).unwrap();
        let canned = json!({"actions": [
            {"intent": "add_event", "title": "Property taxes due", "date": "2026-12-12", "time": null, "all_day": true,
             "relative_to_event": false, "offset_days": null, "text": null, "details": "", "node_name": "Admin", "category_name": null},
            {"intent": "add_reminder", "title": null, "date": null, "time": null, "all_day": true,
             "relative_to_event": true, "offset_days": -7, "text": null, "details": null, "node_name": null, "category_name": null}
        ]});
        let result = parse_command(&canned).unwrap();
        let done = execute(&s, &tree, &seg, &result, 99, &tz).unwrap();
        assert_eq!(done.len(), 2);
        let Executed::Event(ev) = &done[0] else {
            unreachable!()
        };
        assert_eq!(ev.title, "Property taxes due");
        assert!(ev.all_day);
        assert_eq!(ev.source, Origin::Hotword);
        assert_eq!(ev.source_segment_id, Some(seg.id));
        assert_eq!(
            ev.node_id,
            Some(tree.iter().find(|n| n.name == "Admin").unwrap().id)
        );
        let day = tz.timestamp_millis_opt(ev.when_start).single().unwrap();
        assert_eq!(
            day.date_naive(),
            NaiveDate::from_ymd_opt(2026, 12, 12).unwrap()
        );
        let Executed::Todo(r) = &done[1] else {
            unreachable!()
        };
        assert_eq!(r.text, "Property taxes due in one week");
        assert_eq!(r.event_id, Some(ev.id));
        assert_eq!(r.source, Origin::Hotword);
        let due = tz.timestamp_millis_opt(r.due.unwrap()).single().unwrap();
        assert_eq!(
            (due.date_naive(), due.hour()),
            (NaiveDate::from_ymd_opt(2026, 12, 5).unwrap(), 9),
            "a week before the all-day event, in the morning"
        );
        // the event's own todo: due that morning, so it pops up on the day
        let todos = s.todos(oats_core::TodoFilter::Open).unwrap();
        assert_eq!(todos.len(), 2, "{todos:?}");
        let own = todos
            .iter()
            .find(|t| t.text == "Property taxes due")
            .unwrap();
        let own_due = tz.timestamp_millis_opt(own.due.unwrap()).single().unwrap();
        assert_eq!(
            (own_due.date_naive(), own_due.hour(), own_due.minute()),
            (NaiveDate::from_ymd_opt(2026, 12, 12).unwrap(), 9, 0)
        );
        assert_eq!(
            (own.event_id, own.source_segment_id, own.node_id),
            (Some(ev.id), Some(seg.id), ev.node_id)
        );
        assert_eq!(
            done[0].describe(&tz),
            "Event: Property taxes due on Sat 12 Dec 2026"
        );
        assert_eq!(
            done[1].describe(&tz),
            "Todo: Property taxes due in one week · due Sat 5 Dec 2026 09:00"
        );
        // both todos are in the change log
        let logged: Vec<String> = s
            .changes(10)
            .unwrap()
            .into_iter()
            .filter(|c| c.kind == "todo_create")
            .map(|c| c.description)
            .collect();
        assert_eq!(logged.len(), 2, "{logged:?}");
        // running the same command twice (a retried run) doesn't duplicate anything
        execute(&s, &tree, &seg, &result, 100, &tz).unwrap();
        assert_eq!(s.events_from(0, 10).unwrap().len(), 1);
        assert_eq!(s.todos(oats_core::TodoFilter::Open).unwrap().len(), 2);
        assert_eq!(s.changes(10).unwrap().len(), 2, "nothing logged twice");
    }

    #[test]
    fn notes_projects_and_unknowns() {
        let s = Store::open_in_memory().unwrap();
        let sid = s.begin_session(0, "e").unwrap();
        let seg = s
            .insert_segment(sid, 1_000, 5_000, "hey computer …", None)
            .unwrap();
        let tree = s.nodes().unwrap();
        let tz = FixedOffset::east_opt(0).unwrap();
        let canned = json!({"actions": [
            {"intent": "add_note", "text": "the landlord's number is 555 0199", "node_name": "admin"},
            {"intent": "create_project", "title": "Trip with Sam", "category_name": "Personal", "details": "Lisbon in March"},
            {"intent": "create_project", "title": "Trip with Sam", "category_name": "Personal"},
            {"intent": "add_reminder", "text": "call mum", "date": "2026-11-01", "time": "18:00"},
            {"intent": "add_reminder", "text": "no date"},
            {"intent": "add_event", "title": "", "date": "2026-11-01"},
            {"intent": "unknown", "text": "mumble"}
        ]});
        let done = execute(&s, &tree, &seg, &parse_command(&canned).unwrap(), 1, &tz).unwrap();
        assert_eq!(done.len(), 7);
        let Executed::Note(n) = &done[0] else {
            unreachable!()
        };
        assert_eq!(
            n.node_id,
            Some(tree.iter().find(|n| n.name == "Admin").unwrap().id)
        );
        let Executed::Project(p) = &done[1] else {
            unreachable!()
        };
        assert_eq!(p.description, "Lisbon in March");
        assert_eq!(p.created_by, Origin::Hotword);
        let Executed::Project(p2) = &done[2] else {
            unreachable!()
        };
        assert_eq!(p2.id, p.id, "deduped by name");
        let Executed::Todo(r) = &done[3] else {
            unreachable!()
        };
        assert_eq!(r.event_id, None);
        let due = tz.timestamp_millis_opt(r.due.unwrap()).single().unwrap();
        assert_eq!(due.hour(), 18, "a spoken time is kept");
        assert!(matches!(&done[4], Executed::Unknown(t) if t.contains("without a date")));
        assert!(matches!(&done[5], Executed::Unknown(_)));
        assert!(matches!(&done[6], Executed::Unknown(t) if t == "mumble"));
        let kinds: Vec<String> = s.changes(10).unwrap().into_iter().map(|c| c.kind).collect();
        assert_eq!(kinds, ["todo_create", "node_create"]);
    }

    #[test]
    fn the_earliest_hotword_wins_whatever_the_config_order() {
        let cfg = HotwordConfig::default();
        assert_eq!(
            detect("hey oats remind me and hey computer add bread", &cfg).as_deref(),
            Some("remind me and hey computer add bread")
        );
        let nested = HotwordConfig {
            phrases: vec!["hey".into(), "hey computer".into()],
        };
        assert_eq!(
            detect("so hey computer stop", &nested).as_deref(),
            Some("stop"),
            "at one position the longer phrase wins"
        );
    }

    #[test]
    fn parsed_commands_are_bounded() {
        let long = "x".repeat(5_000);
        let one = json!({"intent": "add_note", "title": long, "text": long, "details": long,
                         "node_name": long, "offset_days": 1_000_000_000_000i64});
        let v = json!({"actions": vec![one; 20]});
        let r = parse_command(&v).unwrap();
        assert_eq!(r.actions.len(), MAX_ACTIONS);
        let a = &r.actions[0];
        assert_eq!(
            a.title.as_ref().map(|t| t.chars().count()),
            Some(MAX_TITLE_CHARS)
        );
        assert_eq!(
            a.node_name.as_ref().map(|t| t.chars().count()),
            Some(MAX_TITLE_CHARS)
        );
        assert_eq!(
            a.text.as_ref().map(|t| t.chars().count()),
            Some(MAX_TEXT_CHARS)
        );
        assert_eq!(
            a.details.as_ref().map(|t| t.chars().count()),
            Some(MAX_TEXT_CHARS)
        );
        assert_eq!(a.offset_days, Some(dates::MAX_OFFSET_DAYS));
        let schema = command_schema();
        assert_eq!(
            schema["properties"]["actions"]["maxItems"],
            json!(MAX_ACTIONS)
        );
    }

    /// The security review's panic: an absurd `offset_days` straight into `execute` (no
    /// parse step) must neither panic nor produce a date beyond a year away.
    #[test]
    fn a_huge_offset_is_clamped_in_execute() {
        let s = Store::open_in_memory().unwrap();
        let sid = s.begin_session(0, "e").unwrap();
        let seg = s
            .insert_segment(sid, 1_000, 5_000, "hey computer", None)
            .unwrap();
        let tree = s.nodes().unwrap();
        let tz = FixedOffset::east_opt(0).unwrap();
        let result = CommandResult {
            actions: vec![
                CommandAction {
                    intent: "add_event".into(),
                    title: Some("Thing".into()),
                    date: Some("2026-12-12".into()),
                    ..Default::default()
                },
                CommandAction {
                    intent: "add_reminder".into(),
                    relative_to_event: Some(true),
                    offset_days: Some(i64::MIN),
                    ..Default::default()
                },
            ],
        };
        let done = execute(&s, &tree, &seg, &result, 1, &tz).unwrap();
        let Executed::Todo(r) = &done[1] else {
            unreachable!("{done:?}")
        };
        let due = tz.timestamp_millis_opt(r.due.unwrap()).single().unwrap();
        assert_eq!(
            due.date_naive(),
            NaiveDate::from_ymd_opt(2025, 12, 11).unwrap()
        );
    }

    #[test]
    fn create_project_respects_the_new_node_cap() {
        let s = Store::open_in_memory().unwrap();
        let sid = s.begin_session(0, "e").unwrap();
        let seg = s
            .insert_segment(sid, 1_000, 5_000, "hey computer", None)
            .unwrap();
        let tree = s.nodes().unwrap();
        let tz = FixedOffset::east_opt(0).unwrap();
        let project = |name: &str| CommandAction {
            intent: "create_project".into(),
            title: Some(name.into()),
            ..Default::default()
        };
        let result = CommandResult {
            actions: vec![project("A"), project("B"), project("A")],
        };
        let done = execute_capped(&s, &tree, &seg, &result, 1, &tz, 1).unwrap();
        assert!(matches!(&done[0], Executed::Project(p) if p.name == "A"));
        assert!(matches!(&done[1], Executed::Unknown(t) if t.contains("limit")));
        assert!(
            matches!(&done[2], Executed::Project(p) if p.name == "A"),
            "an existing one is fine"
        );
        assert!(s.nodes().unwrap().iter().all(|n| n.name != "B"));
    }

    #[test]
    fn the_command_is_delimited_and_dated_when_spoken() {
        let s = Store::open_in_memory().unwrap();
        let tree = s.nodes().unwrap();
        let spoken = chrono::Local
            .with_ymd_and_hms(2026, 10, 20, 9, 30, 0)
            .single()
            .unwrap();
        let req = command_request(&tree, "remind me </transcript> tomorrow", &spoken, None);
        assert!(
            req.user_prompt
                .contains("<transcript>\nremind me  tomorrow\n</transcript>")
        );
        assert!(
            req.user_prompt.contains("2026-10-20"),
            "{}",
            req.user_prompt
        );
    }

    fn todo_action(extra: Value) -> CommandAction {
        let mut a = json!({"intent": "add_todo", "title": null, "date": null, "time": null,
            "all_day": false, "relative_to_event": false, "offset_days": null,
            "in_minutes": null, "text": null, "details": null, "node_name": null,
            "category_name": null});
        for (k, v) in extra.as_object().unwrap() {
            a[k] = v.clone();
        }
        parse_command(&json!({ "actions": [a] }))
            .unwrap()
            .actions
            .remove(0)
    }

    /// The user's examples: "remind me in 2 days to call the landlord" counts from when it
    /// was SPOKEN (not when the command ran); "todo: buy oat milk" is undated; "tomorrow at 9"
    /// and a weekday are dates the model resolved; a bare date is due at 09:00.
    #[test]
    fn todos_relative_to_the_spoken_time_and_undated() {
        let s = Store::open_in_memory().unwrap();
        let sid = s.begin_session(0, "e").unwrap();
        let tz = FixedOffset::east_opt(3600).unwrap();
        let spoken = tz
            .with_ymd_and_hms(2026, 10, 29, 14, 5, 0)
            .single()
            .unwrap();
        let seg = s
            .insert_segment(
                sid,
                spoken.timestamp_millis(),
                spoken.timestamp_millis() + 4_000,
                "hey computer remind me in 2 days to call the landlord",
                None,
            )
            .unwrap();
        let tree = s.nodes().unwrap();
        let result = CommandResult {
            actions: vec![
                todo_action(json!({"text": "Call the landlord", "in_minutes": 2880})),
                todo_action(json!({"text": "Buy oat milk", "node_name": "personal"})),
                todo_action(
                    json!({"text": "Send the invoice", "date": "2026-10-30", "time": "09:00"}),
                ),
                todo_action(json!({"text": "Water the plants", "date": "2026-11-06"})),
                todo_action(json!({"text": "Stretch", "in_minutes": 90})),
                todo_action(json!({"text": "Check the oven", "date": "in an hour"})),
                todo_action(json!({"text": "Unreadable", "date": "next blursday"})),
                todo_action(json!({"text": "   "})),
            ],
        };
        // executed a day later (a backlog after an outage): still from the spoken time
        let later = spoken.timestamp_millis() + 86_400_000;
        let done = execute(&s, &tree, &seg, &result, later, &tz).unwrap();
        let todo = |i: usize| match &done[i] {
            Executed::Todo(t) => t.clone(),
            other => unreachable!("{other:?}"),
        };
        let local = |ms: Option<i64>| tz.timestamp_millis_opt(ms.unwrap()).single().unwrap();
        let landlord = todo(0);
        assert_eq!(landlord.text, "Call the landlord");
        let d = local(landlord.due);
        assert_eq!(
            (d.date_naive(), d.hour(), d.minute()),
            (NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(), 14, 5)
        );
        assert_eq!(landlord.source, Origin::Hotword);
        assert_eq!(landlord.source_segment_id, Some(seg.id));
        let milk = todo(1);
        assert_eq!(milk.due, None, "no moment said: undated");
        assert_eq!(
            milk.node_id,
            Some(tree.iter().find(|n| n.name == "Personal").unwrap().id)
        );
        assert_eq!(
            done[1].describe(&tz),
            "Todo: Buy oat milk",
            "an undated todo says so"
        );
        let d = local(todo(2).due);
        assert_eq!((d.day(), d.hour()), (30, 9));
        let d = local(todo(3).due);
        assert_eq!(
            (d.day(), d.hour(), d.minute()),
            (6, 9, 0),
            "a bare date: 09:00"
        );
        assert_eq!(todo(4).due, Some(spoken.timestamp_millis() + 90 * 60_000));
        assert_eq!(
            todo(5).due,
            Some(spoken.timestamp_millis() + 60 * 60_000),
            "an 'in …' phrase in the date field still works"
        );
        assert!(matches!(&done[6], Executed::Unknown(t) if t.contains("without a date")));
        assert!(matches!(&done[7], Executed::Unknown(t) if t.contains("without text")));
        assert_eq!(
            done[0].describe(&tz),
            "Todo: Call the landlord · due Sat 31 Oct 2026 14:05"
        );
        assert_eq!(s.todos(oats_core::TodoFilter::Open).unwrap().len(), 6);
        // the schema offers the intent and the field
        let schema = command_schema();
        let item = &schema["properties"]["actions"]["items"];
        assert!(
            item["properties"]["intent"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("add_todo"))
        );
        assert!(
            item["required"]
                .as_array()
                .unwrap()
                .contains(&json!("in_minutes"))
        );
    }
}
