//! The user's two spoken examples end to end through the brain's worker and a fake `claude`
//! (answering by the command text it is given): hotword segment → command run → actions →
//! todo / event rows in the store, and the `Command` event's lines and confirmations.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oats_brain::{Brain, BrainConfig, BrainEvent, BrainHandle, ClaudeCli};
use oats_core::{Origin, Store, TodoFilter};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "oats-brain-todos-it-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Answers a command request by what was said (the prompt arrives on stdin): the landlord
/// sentence → one `add_todo` two days out; the property taxes sentence → one `add_event`.
fn fake_cli(dir: &Path) -> ClaudeCli {
    let script = dir.join("claude");
    let action = |fields: &str| {
        format!(
            r#"{{"intent":"x","title":null,"date":null,"time":null,"all_day":false,"relative_to_event":false,"offset_days":null,"in_minutes":null,"text":null,"details":null,"node_name":null,"category_name":null,{fields}}}"#
        )
        .replace(r#""intent":"x","#, "")
    };
    let answer = |actions: String| {
        format!(
            r#"{{"type":"result","is_error":false,"result":"","total_cost_usd":0.002,"duration_ms":10,"structured_output":{{"actions":[{actions}]}}}}"#
        )
    };
    let landlord = answer(action(
        r#""intent":"add_todo","text":"Call the landlord","in_minutes":2880"#,
    ));
    let taxes = answer(action(
        r#""intent":"add_event","title":"Property taxes due","date":"2026-12-12","all_day":true,"details":"""#,
    ));
    let body = format!(
        r#"#!/bin/sh
case " $* " in *" --version "*) echo "2.1.312 (Claude Code)"; exit 0;; esac
IN=$(cat)
case "$IN" in
  *landlord*) echo '{landlord}';;
  *"property taxes"*) echo '{taxes}';;
  *) echo '{{"type":"result","is_error":true,"result":"unexpected request"}}';;
esac
"#
    );
    std::fs::write(&script, body).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    ClaudeCli {
        binary: script,
        model: None,
        timeout: Duration::from_secs(10),
        max_budget_usd: None,
        work_dir: Some(dir.join("work")),
        isolate: true,
    }
}

fn next_command(h: &BrainHandle) -> (i64, Vec<String>, Vec<String>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match h.recv_timeout(left) {
            Some(BrainEvent::Command {
                segment_id,
                lines,
                confirmations,
            }) => return (segment_id, lines, confirmations),
            Some(other) => seen.push(other),
            None => break,
        }
    }
    panic!("no command event; saw {seen:?}");
}

#[test]
fn the_two_spoken_examples_become_todos() {
    let dir = TempDir::new("examples");
    let db = dir.0.join("oats.sqlite");
    let spoken = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
        - 60_000;
    let (landlord, taxes) = {
        let s = Store::open(&db).unwrap();
        let sid = s.begin_session(spoken - 1_000, "e").unwrap();
        let a = s
            .insert_segment(
                sid,
                spoken,
                spoken + 3_000,
                "hey computer remind me in 2 days to call the landlord",
                None,
            )
            .unwrap();
        let b = s
            .insert_segment(
                sid,
                spoken + 10_000,
                spoken + 14_000,
                "hey computer property taxes are due on 12/12/2026",
                None,
            )
            .unwrap();
        (a, b)
    };
    let h = Brain {
        config: BrainConfig {
            cli: fake_cli(&dir.0),
            ..BrainConfig::default()
        },
        store_path: db.clone(),
        wake: Box::new(|| {}),
    }
    .spawn();

    h.segment_arrived(landlord.clone());
    let (seg, lines, confirmations) = next_command(&h);
    assert_eq!(seg, landlord.id);
    assert!(
        lines[0].starts_with("Todo: Call the landlord · due"),
        "{lines:?}"
    );
    assert_eq!(confirmations.len(), 1);
    assert!(
        confirmations[0].starts_with("Reminder set: Call the landlord — "),
        "{confirmations:?}"
    );

    h.segment_arrived(taxes.clone());
    let (seg, _, confirmations) = next_command(&h);
    assert_eq!(seg, taxes.id);
    assert!(
        confirmations[0].starts_with("Event added: Property taxes due — Sat 12 Dec 2026"),
        "{confirmations:?}"
    );
    drop(h);

    let s = Store::open(&db).unwrap();
    let todos = s.todos(TodoFilter::Open).unwrap();
    assert_eq!(todos.len(), 2, "{todos:?}");
    // soonest first: the landlord (two days after it was said), then the taxes morning
    let call = &todos[0];
    assert_eq!(call.text, "Call the landlord");
    // two days after it was SAID, same clock time
    let said = chrono::TimeZone::timestamp_millis_opt(&chrono::Local, spoken)
        .single()
        .unwrap();
    assert_eq!(
        call.due,
        oats_brain::dates::after_minutes(&said, 2 * 24 * 60)
    );
    assert_eq!(
        (call.source, call.source_segment_id),
        (Origin::Hotword, Some(landlord.id))
    );
    let tax = &todos[1];
    assert_eq!(tax.text, "Property taxes due");
    assert!(tax.event_id.is_some());
    assert_eq!(tax.source_segment_id, Some(taxes.id));
    let ev = s.events_from(0, 10).unwrap();
    assert_eq!(ev.len(), 1);
    assert_eq!(tax.event_id, Some(ev[0].id));
    // the 12th at 09:00 local
    let local = chrono::TimeZone::timestamp_millis_opt(&chrono::Local, tax.due.unwrap())
        .single()
        .unwrap();
    assert_eq!(
        local.format("%Y-%m-%d %H:%M").to_string(),
        "2026-12-12 09:00"
    );
    // each segment was consumed by its command (never part of an episode)
    assert!(
        s.segment(landlord.id)
            .unwrap()
            .is_some_and(|r| r.interpreted_at.is_some())
    );
}
