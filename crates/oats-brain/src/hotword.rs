//! Hotword commands: "hey computer, property taxes are due on 12/12/2026, also set a reminder
//! one week ahead". Detection runs on the transcript TEXT of each new segment (not on audio):
//! configurable phrases, case- and punctuation-insensitive so ASR variants ("Hey, computer.")
//! match. A detected command goes straight to a small haiku request with its own prompt and
//! schema; the actions that come back (an event AND a reminder for the example above) are
//! executed against the store with `source = hotword` and the source segment id, so the user
//! can audit what a spoken command did.

use chrono::{DateTime, TimeZone};
use oats_core::store::name_key;
use oats_core::{
    Event, NewEvent, NewReminder, Node, NodeKind, Note, Origin, Reminder, Store, StoreError,
    TranscriptRow,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::claude_cli::Request;
use crate::dates;
use crate::interpret::{render, render_tree};

pub const COMMAND_SYSTEM: &str = include_str!("../prompts/command.system.md");
pub const COMMAND_USER: &str = include_str!("../prompts/command.user.md");

/// Environment variable with comma-separated hotword phrases.
pub const HOTWORDS_ENV: &str = "OATS_HOTWORDS";

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

fn detect_in<S: AsRef<str>>(
    norm: &[String],
    original: &[S],
    cfg: &HotwordConfig,
) -> Option<String> {
    for phrase in &cfg.phrases {
        let p: Vec<&str> = phrase.split(' ').collect();
        if p.is_empty() {
            continue;
        }
        for i in 0..norm.len() {
            if i + p.len() <= norm.len()
                && norm[i..i + p.len()]
                    .iter()
                    .map(String::as_str)
                    .eq(p.iter().copied())
            {
                let tail: Vec<&str> = original[i + p.len()..].iter().map(AsRef::as_ref).collect();
                let cmd = tail
                    .join(" ")
                    .trim_start_matches([',', '.', ':', '!', '?', ' '])
                    .trim()
                    .to_owned();
                return Some(cmd);
            }
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
    let ns = json!({ "type": ["string", "null"] });
    json!({
        "type": "object",
        "properties": {
            "actions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "intent": { "type": "string", "enum": ["add_event", "add_reminder", "add_note", "create_project", "unknown"] },
                        "title": ns,
                        "date": { "type": ["string", "null"], "description": "YYYY-MM-DD" },
                        "time": { "type": ["string", "null"], "description": "HH:MM" },
                        "all_day": { "type": "boolean" },
                        "relative_to_event": { "type": "boolean" },
                        "offset_days": { "type": ["integer", "null"] },
                        "text": ns,
                        "details": ns,
                        "node_name": ns,
                        "category_name": ns
                    },
                    "required": ["intent", "title", "date", "time", "all_day", "relative_to_event", "offset_days", "text", "details", "node_name", "category_name"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["actions"],
        "additionalProperties": false
    })
}

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
                ("command", command),
            ],
        ),
        json_schema: Some(command_schema()),
        model,
        max_turns: None,
    }
}

pub fn parse_command(v: &Value) -> Result<CommandResult, serde_json::Error> {
    serde_json::from_value(v.clone())
}

/// What executing a command did.
#[derive(Clone, Debug, PartialEq)]
pub enum Executed {
    Event(Event),
    Reminder(Reminder),
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
            Executed::Reminder(r) => format!("Reminder: {} on {}", r.text, day(r.due)),
            Executed::Note(n) => format!("Note: {}", n.text),
            Executed::Project(p) => format!("Project: {}", p.name),
            Executed::Unknown(t) => format!("Not understood: {t}"),
        }
    }
}

fn find_by_name(tree: &[Node], name: &str) -> Option<Node> {
    let key = name_key(name);
    tree.iter()
        .find(|n| !n.archived && name_key(&n.name) == key)
        .cloned()
}

/// Execute the actions for a command spoken in `segment`.
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
    for a in &result.actions {
        let node = a.node_name.as_deref().and_then(|n| find_by_name(tree, n));
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
                last_event = Some(ev.clone());
                out.push(Executed::Event(ev));
            }
            "add_reminder" => {
                let relative = a.relative_to_event.unwrap_or(false)
                    || (a.date.is_none() && a.offset_days.is_some());
                let (due, default_text) = if relative {
                    match &last_event {
                        Some(ev) => {
                            let start = tz.timestamp_millis_opt(ev.when_start).single();
                            let due = start
                                .and_then(|s| dates::shift_days(&s, a.offset_days.unwrap_or(0)))
                                .map(|d| d.timestamp_millis());
                            let days = -a.offset_days.unwrap_or(0);
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
                } else {
                    let due = a.date.as_deref().and_then(dates::parse_date).and_then(|d| {
                        dates::to_ms(d, a.time.as_deref().and_then(dates::parse_time), tz)
                    });
                    (due, String::new())
                };
                let Some(due) = due else {
                    out.push(Executed::Unknown(format!(
                        "reminder without a date: {}",
                        text_of(a)
                    )));
                    continue;
                };
                let text = {
                    let t = text_of(a);
                    if t.is_empty() { default_text } else { t }
                };
                if text.is_empty() {
                    out.push(Executed::Unknown("reminder without text".into()));
                    continue;
                }
                let due_day = tz
                    .timestamp_millis_opt(due)
                    .single()
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                let r = store.upsert_reminder(
                    &NewReminder {
                        event_id: if relative {
                            last_event.as_ref().map(|e| e.id)
                        } else {
                            None
                        },
                        due,
                        text: text.clone(),
                        source: Some(Origin::Hotword),
                        source_segment_id: Some(seg),
                        dedupe_key: Some(format!(
                            "hotword:{seg}:reminder:{due_day}:{}",
                            name_key(&text)
                        )),
                    },
                    now_ms,
                )?;
                out.push(Executed::Reminder(r));
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
                let p = store.add_node(
                    Some(category.id),
                    NodeKind::Project,
                    name,
                    a.details.as_deref().unwrap_or(""),
                    Origin::Hotword,
                )?;
                if !existed {
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
    use chrono::{FixedOffset, NaiveDate};

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
        let Executed::Reminder(r) = &done[1] else {
            unreachable!()
        };
        assert_eq!(r.text, "Property taxes due in one week");
        assert_eq!(r.event_id, Some(ev.id));
        assert_eq!(r.source, Origin::Hotword);
        let due = tz.timestamp_millis_opt(r.due).single().unwrap();
        assert_eq!(
            due.date_naive(),
            NaiveDate::from_ymd_opt(2026, 12, 5).unwrap()
        );
        assert_eq!(
            done[0].describe(&tz),
            "Event: Property taxes due on Sat 12 Dec 2026"
        );
        assert_eq!(
            done[1].describe(&tz),
            "Reminder: Property taxes due in one week on Sat 5 Dec 2026"
        );
        // running the same command twice (a retried run) doesn't duplicate anything
        execute(&s, &tree, &seg, &result, 100, &tz).unwrap();
        assert_eq!(s.events_from(0, 10).unwrap().len(), 1);
        assert_eq!(s.reminders(true, 10).unwrap().len(), 1);
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
        let Executed::Reminder(r) = &done[3] else {
            unreachable!()
        };
        assert_eq!(r.event_id, None);
        assert!(matches!(&done[4], Executed::Unknown(t) if t.contains("without a date")));
        assert!(matches!(&done[5], Executed::Unknown(_)));
        assert!(matches!(&done[6], Executed::Unknown(t) if t == "mumble"));
        assert_eq!(s.changes(10).unwrap().len(), 1);
    }
}
