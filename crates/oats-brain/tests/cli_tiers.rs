//! Every tier's request, end to end through the real [`ClaudeCli`] invocation against a fake
//! `claude` shell script that checks the isolation flags, the model, the system prompt and the
//! schema it was given, and that `CLAUDECODE` did not leak into its environment; then the
//! canned structured output is parsed and applied to a store.
//!
//! `CLAUDECODE` removal can only be observed when the parent has it set, and setting an
//! environment variable in-process is `unsafe` (forbidden here), so the test re-runs itself as
//! a child process with `CLAUDECODE=1` in its environment.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Local, NaiveDate, NaiveTime};
use oats_brain::episodes::Chunk;
use oats_brain::{
    ClaudeCli, CliError, Executed, HotwordConfig, Request, Response, dates, hotword, interpret,
};
use oats_core::{EpisodeContent, NodeKind, Store, View};
use serde_json::{Value, json};

const CHILD_ENV: &str = "OATS_BRAIN_IT_CHILD";

/// Re-run the named test in a child with `envs` set. Returns `true` when this IS the child.
fn run_in_child(test: &str, envs: &[(&str, &str)]) -> bool {
    if std::env::var_os(CHILD_ENV).is_some() {
        return true;
    }
    let exe = std::env::current_exe().unwrap();
    let mut cmd = std::process::Command::new(exe);
    cmd.args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "child run of {test} failed:\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    false
}

struct Fake {
    dir: PathBuf,
    cli: ClaudeCli,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The fake: records argv pieces, cwd and stdin; fails with exit 97 and an `ASSERT:` line on
/// stderr when an isolation flag or the expected model is missing or `CLAUDECODE` is set;
/// answers with the contents of `response`.
fn fake(tag: &str) -> Fake {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "oats-brain-it-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("claude");
    let body = format!(
        r#"#!/bin/sh
D='{dir}'
pwd -P > "$D/cwd"
case " $* " in *" --version "*) echo "2.1.312 (Claude Code)"; exit 0;; esac
cat > "$D/stdin"
fail() {{ echo "ASSERT: $1" >&2; exit 97; }}
[ -z "${{CLAUDECODE+x}}" ] || fail "CLAUDECODE is set in the child environment"
[ "$1" = "-p" ] || fail "first argument is $1, not -p"
prev=""; safe=0; tools=0; strict=0; nosess=0; noslash=0; settings=0; json=0; model=""; schema=0
for a in "$@"; do
  case "$prev" in
    --tools) [ -z "$a" ] && tools=1;;
    --setting-sources) [ -z "$a" ] && settings=1;;
    --output-format) [ "$a" = "json" ] && json=1;;
    --model) model="$a";;
    --json-schema) schema=1; printf '%s' "$a" > "$D/schema";;
    --system-prompt) printf '%s' "$a" > "$D/system";;
  esac
  case "$a" in
    --safe-mode) safe=1;;
    --strict-mcp-config) strict=1;;
    --no-session-persistence) nosess=1;;
    --disable-slash-commands) noslash=1;;
  esac
  prev="$a"
done
[ $safe = 1 ] || fail "--safe-mode missing"
[ $tools = 1 ] || fail "--tools \"\" missing"
[ $strict = 1 ] || fail "--strict-mcp-config missing"
[ $nosess = 1 ] || fail "--no-session-persistence missing"
[ $noslash = 1 ] || fail "--disable-slash-commands missing"
[ $settings = 1 ] || fail "--setting-sources \"\" missing"
[ $json = 1 ] || fail "--output-format json missing"
[ $schema = 1 ] || fail "--json-schema missing"
want=$(cat "$D/expect_model")
[ "$model" = "$want" ] || fail "--model is '$model', expected '$want'"
cat "$D/response"
"#,
        dir = dir.display()
    );
    std::fs::write(&script, body).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cli = ClaudeCli {
        binary: script,
        model: Some("haiku".into()),
        timeout: Duration::from_secs(10),
        max_budget_usd: Some(0.5),
        work_dir: Some(dir.join("work")),
        isolate: true,
    };
    Fake { dir, cli }
}

impl Fake {
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    /// Run `req` expecting `model` on the command line; the script answers `structured`.
    fn run(&self, req: &Request, model: &str, structured: Value) -> Response {
        std::fs::write(self.dir.join("expect_model"), model).unwrap();
        let response = json!({
            "type": "result", "subtype": "success", "is_error": false, "result": "",
            "duration_ms": 1200, "num_turns": 2, "session_id": "fake",
            "total_cost_usd": 0.0125, "structured_output": structured,
            "model_usage": {format!("claude-{model}-test"): {"inputTokens": 1}}
        });
        std::fs::write(self.dir.join("response"), response.to_string()).unwrap();
        let resp = match self.cli.run(req) {
            Ok(r) => r,
            Err(CliError::Exit { code, stderr }) => {
                panic!("the fake rejected the invocation ({code}): {stderr}")
            }
            Err(e) => panic!("{e}"),
        };
        // the request reached the CLI intact: system prompt in argv, schema in argv, the
        // user prompt over stdin, the data dir as cwd
        assert_eq!(self.read("system"), req.system_prompt);
        let schema: Value = serde_json::from_str(&self.read("schema")).unwrap();
        assert_eq!(Some(&schema), req.json_schema.as_ref());
        assert_eq!(self.read("stdin"), req.user_prompt);
        let cwd = PathBuf::from(self.read("cwd").trim());
        assert_eq!(cwd, canon(&self.dir.join("work")));
        assert_eq!(resp.cost_usd, Some(0.0125));
        assert_eq!(
            resp.model.as_deref(),
            Some(format!("claude-{model}-test").as_str())
        );
        resp
    }
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Every tier, in the order the scheduler would meet them on a working day.
#[test]
fn every_tier_runs_through_the_isolated_cli() {
    if !run_in_child(
        "every_tier_runs_through_the_isolated_cli",
        &[("CLAUDECODE", "1")],
    ) {
        return;
    }
    assert_eq!(
        std::env::var("CLAUDECODE").as_deref(),
        Ok("1"),
        "the child must have CLAUDECODE set for the check to mean anything"
    );
    let f = fake("tiers");
    let s = Store::open_in_memory().unwrap();
    let work = s.categories().unwrap()[0].id;
    let sid = s.begin_session(0, "e").unwrap();
    let t0 = 1_793_000_000_000; // 2026-10-26
    let lines = [
        "Okay so the exporter needs to ship on Friday",
        "Sam will write the CSV tests and we have the customer call on Monday at half ten",
        "The PDF part can wait until next quarter",
    ];
    let segs: Vec<_> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let t = t0 + i as i64 * 30_000;
            s.insert_segment(sid, t, t + 20_000, l, Some(0.9)).unwrap()
        })
        .collect();
    let now = Local::now();
    let now_ms = now.timestamp_millis();

    // ---- tier 0: categorise (haiku) ---------------------------------------------------------
    let tree = s.nodes().unwrap();
    let req = interpret::categorise_request(&tree, &segs, &now, Some("haiku".into()));
    let resp = f.run(
        &req,
        "haiku",
        json!({"items": [
            {"segment_id": segs[0].id, "node_id": work},
            {"segment_id": segs[1].id, "node_id": null},
            {"segment_id": segs[2].id, "node_id": 424242}
        ]}),
    );
    let cat: interpret::Categorisation = serde_json::from_value(resp.structured.unwrap()).unwrap();
    assert_eq!(cat.items.len(), 3);
    assert_eq!(
        interpret::apply_categorisation(&s, &tree, &segs, &cat).unwrap(),
        1
    );
    assert!(s.uncategorised_segments(10).unwrap().is_empty());
    assert_eq!(s.segments(View::Node(work), 0).unwrap().len(), 1);

    // ---- tier 1: interpret (sonnet) ---------------------------------------------------------
    let chunk = Chunk {
        started_at: segs[0].started_at,
        ended_at: segs[2].ended_at,
        segments: segs.clone(),
    };
    let req = interpret::interpret_request(&tree, &chunk, None, &now, Some("sonnet".into()));
    assert!(req.user_prompt.contains(lines[1]));
    let resp = f.run(
        &req,
        "sonnet",
        json!({
            "title": "Exporter planning", "summary": "- Ship **Friday**\n- PDF later",
            "low_signal": false, "node_id": work,
            "new_project": {"parent_id": work, "name": "Exporter", "description": "the CSV/PDF exporter"},
            "new_topic": {"parent_id": null, "name": "CSV", "description": "CSV export"},
            "decisions": ["Ship on Friday", "  "],
            "action_items": [{"text": "Write the CSV tests", "owner": "Sam", "due": "Friday"}],
            "topics": ["exporter", "csv"],
            "events": [{"title": "Customer call", "date": "2026-11-02", "time": "10:30",
                        "end_date": null, "all_day": false, "details": "exporter demo",
                        "confidence": 0.8}]
        }),
    );
    let interp = interpret::parse_interpretation(resp.structured.as_ref().unwrap()).unwrap();
    let model = resp.model.clone().unwrap();
    let applied = interpret::apply_interpretation(
        &s,
        &tree,
        &chunk,
        &interp,
        None,
        &model,
        resp.cost_usd,
        None,
        3,
        now_ms,
    )
    .unwrap();
    let project = s.find_node(Some(work), "exporter").unwrap().unwrap();
    let topic = s.find_node(Some(project.id), "csv").unwrap().unwrap();
    assert_eq!(
        (project.kind, topic.kind),
        (NodeKind::Project, NodeKind::Topic)
    );
    assert_eq!(applied.new_nodes.len(), 2);
    let ep = &applied.episode;
    assert_eq!(ep.title, "Exporter planning");
    assert_eq!(
        ep.node_id,
        Some(topic.id),
        "filed at the most specific node"
    );
    assert_eq!(ep.decisions, vec!["Ship on Friday".to_owned()]);
    assert_eq!(ep.action_items[0].owner.as_deref(), Some("Sam"));
    assert_eq!(ep.model, "claude-sonnet-test");
    assert_eq!(ep.cost_usd, Some(0.0125));
    assert_eq!(
        (ep.started_at, ep.ended_at),
        (chunk.started_at, chunk.ended_at)
    );
    assert_eq!(s.uninterpreted_count().unwrap(), 0);
    assert_eq!(s.segments_of_episode(ep.id).unwrap().len(), 3);
    let events = s.events_of_episode(ep.id).unwrap();
    assert_eq!(events.len(), 1);
    let want = dates::to_ms(
        NaiveDate::from_ymd_opt(2026, 11, 2).unwrap(),
        NaiveTime::from_hms_opt(10, 30, 0),
        &Local,
    );
    assert_eq!(Some(events[0].when_start), want);
    assert!(!events[0].all_day);
    assert_eq!(events[0].node_id, Some(topic.id));
    assert_eq!(
        s.changes(10)
            .unwrap()
            .iter()
            .filter(|c| c.kind == "node_create")
            .count(),
        2
    );

    // ---- tier 0: a hotword command (haiku) ---------------------------------------------------
    let spoken = s
        .insert_segment(
            sid,
            t0 + 200_000,
            t0 + 205_000,
            "Hey, computer: note that the wifi password is on the fridge, and start a project called Garden",
            None,
        )
        .unwrap();
    let command = hotword::detect(&spoken.text, &HotwordConfig::default()).unwrap();
    assert!(command.starts_with("note that the wifi"), "{command}");
    let tree = s.nodes().unwrap();
    let req = hotword::command_request(&tree, &command, &now, Some("haiku".into()));
    assert!(req.user_prompt.contains(&command));
    let resp = f.run(
        &req,
        "haiku",
        json!({"actions": [
            {"intent": "add_note", "title": null, "date": null, "time": null, "all_day": false,
             "relative_to_event": false, "offset_days": null,
             "text": "The wifi password is on the fridge", "details": null,
             "node_name": "personal", "category_name": null},
            {"intent": "create_project", "title": "Garden", "date": null, "time": null,
             "all_day": false, "relative_to_event": false, "offset_days": null, "text": null,
             "details": "raised beds", "node_name": null, "category_name": "Personal"}
        ]}),
    );
    let result = hotword::parse_command(resp.structured.as_ref().unwrap()).unwrap();
    let done = hotword::execute(&s, &tree, &spoken, &result, now_ms, &Local).unwrap();
    let personal = s.find_node(None, "Personal").unwrap().unwrap();
    assert!(
        matches!(&done[..], [Executed::Note(n), Executed::Project(p)]
            if n.node_id == Some(personal.id) && p.parent_id == Some(personal.id) && p.name == "Garden"),
        "{done:?}"
    );
    assert_eq!(s.notes(10).unwrap()[0].source_segment_id, Some(spoken.id));

    // ---- tier 2: review (sonnet) -------------------------------------------------------------
    let content = |title: &str| EpisodeContent {
        title: title.into(),
        summary: "s".into(),
        node_id: Some(project.id),
        ..Default::default()
    };
    let cont = s
        .insert_episode(
            t0 + 100_000,
            t0 + 110_000,
            &content("Exporter planning (cont.)"),
            now_ms,
        )
        .unwrap();
    let tv = s
        .insert_episode(t0 + 300_000, t0 + 310_000, &content("News"), now_ms)
        .unwrap();
    let episodes = s.episodes_between(t0 - 1, t0 + 1_000_000).unwrap();
    let all_events = s.events_from(i64::MIN, 100).unwrap();
    let tree = s.nodes().unwrap();
    let req = interpret::review_request(&tree, &episodes, &all_events, &now, Some("sonnet".into()));
    assert!(req.user_prompt.contains("Exporter planning (cont.)"));
    let none = Value::Null;
    let action = |kind: &str, extra: Value| {
        let mut a = json!({"kind": kind, "from": none, "into": none, "episode_id": none,
            "node_id": none, "event_id": none, "new_parent_id": none, "name": none,
            "description": none, "reason": "test"});
        if let (Some(a), Some(extra)) = (a.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                a.insert(k.clone(), v.clone());
            }
        }
        a
    };
    let resp = f.run(
        &req,
        "sonnet",
        json!({"actions": [
            action("merge_episodes", json!({"from": cont.id, "into": ep.id})),
            action("mark_low_signal", json!({"episode_id": tv.id})),
            action("set_node", json!({"episode_id": tv.id, "node_id": personal.id}))
        ]}),
    );
    let actions: interpret::Actions = serde_json::from_value(resp.structured.unwrap()).unwrap();
    let done = interpret::apply_review(&s, &tree, &episodes, &actions, None, now_ms).unwrap();
    assert_eq!(done.len(), 3, "{done:?}");
    assert!(s.episode(cont.id).unwrap().is_none());
    let merged = s.episode(ep.id).unwrap().unwrap();
    assert_eq!(merged.ended_at, ep.ended_at.max(t0 + 110_000));
    let tv_now = s.episode(tv.id).unwrap().unwrap();
    assert!(tv_now.low_signal);
    assert_eq!(tv_now.node_id, Some(personal.id));

    // ---- tier 3: reorganise (opus) -----------------------------------------------------------
    let tree = s.nodes().unwrap();
    let usage = s.node_usage().unwrap();
    let episodes = s.episodes(60).unwrap();
    let req = interpret::reorganise_request(&tree, &usage, &episodes, &now, Some("opus".into()));
    assert!(req.user_prompt.contains("episodes"), "usage is rendered");
    let resp = f.run(
        &req,
        "opus",
        json!({"actions": [
            action("rename_node", json!({"node_id": project.id, "name": "CSV exporter"})),
            action("set_description", json!({"node_id": topic.id, "description": "CSV only"})),
            action("archive_node", json!({"node_id": personal.id}))
        ]}),
    );
    let actions: interpret::Actions = serde_json::from_value(resp.structured.unwrap()).unwrap();
    let done = interpret::apply_reorganisation(&s, &tree, &actions, None, now_ms).unwrap();
    assert_eq!(done.len(), 2, "categories are off limits: {done:?}");
    assert_eq!(s.node(project.id).unwrap().unwrap().name, "CSV exporter");
    assert_eq!(s.node(topic.id).unwrap().unwrap().description, "CSV only");
    assert!(!s.node(personal.id).unwrap().unwrap().archived);

    // ---- a request without a model falls back to the CLI's default ---------------------------
    let req = Request {
        model: None,
        ..interpret::categorise_request(&tree, &segs, &now, None)
    };
    f.run(&req, "haiku", json!({"items": []}));
}

/// Without the isolation flags the fake refuses: proves the script really checks them (the
/// tier test above would otherwise pass vacuously).
#[test]
fn the_fake_rejects_a_non_isolated_invocation() {
    let mut f = fake("not-isolated");
    f.cli.isolate = false;
    std::fs::write(f.dir.join("expect_model"), "haiku").unwrap();
    std::fs::write(f.dir.join("response"), "{}").unwrap();
    let req = interpret::categorise_request(&[], &[], &Local::now(), Some("haiku".into()));
    match f.cli.run(&req) {
        Err(CliError::Exit { code: 97, stderr }) => {
            assert!(stderr.contains("--safe-mode missing"), "{stderr}")
        }
        other => panic!("{other:?}"),
    }
    // and a wrong model is caught
    f.cli.isolate = true;
    std::fs::write(f.dir.join("expect_model"), "opus").unwrap();
    match f.cli.run(&req) {
        Err(CliError::Exit { code: 97, stderr }) => assert!(stderr.contains("--model"), "{stderr}"),
        other => panic!("{other:?}"),
    }
    // `available()` works against the same script
    assert_eq!(f.cli.available().unwrap().major, 2);
}
