//! End-to-end check of the brain against the REAL local `claude` CLI (spends a few cents):
//! a fake episode of a short meeting, interpreted into the structured note, then the user's
//! hotword example turned into actions — both applied to a throwaway in-memory store so the
//! whole path (prompt → CLI → schema → store) is exercised. Prints the results and the cost.
//!
//! ```bash
//! cargo run -p oats-brain --example interpret_demo            # haiku (cheapest)
//! OATS_CLAUDE_MODEL=sonnet cargo run -p oats-brain --example interpret_demo
//! ```

use chrono::Local;
use oats_brain::episodes::Chunk;
use oats_brain::{ClaudeCli, hotword, interpret};
use oats_core::{Store, TranscriptRow};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut cli = ClaudeCli::default();
    if cli.model.is_none() {
        cli.model = Some("haiku".into());
    }
    cli.work_dir = Some(std::env::temp_dir().join("oats-brain-demo"));
    let model = cli.model.clone();
    println!("claude: {}", cli.binary.display());
    match cli.available() {
        Ok(v) => println!("version: {}", v.raw),
        Err(e) => {
            eprintln!("claude is not available: {e}");
            std::process::exit(1);
        }
    }

    let store = match Store::open_in_memory() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("store: {e}");
            std::process::exit(1);
        }
    };
    let now = Local::now();
    let base = now.timestamp_millis() - 40 * 60 * 1000;
    let lines = [
        "Okay, let's get started. The main thing today is the Q4 roadmap and whether the export rewrite still fits before the holidays.",
        "I think it fits if we drop the PDF templates for now and ship CSV and Excel first.",
        "Agreed, let's go with that. Priya, can you send the updated estimate by Thursday?",
        "Sure. Then Marco books the design review for early next week so we can lock the scope.",
        "One more thing: the vendor contract. Legal wants another pass on the data retention clause before we sign, so that probably slips to next week.",
        "Fine. And the customer demo is on the 14th of November at ten, so the exporter has to be stable by then.",
        "Got it. I'll write it up and share the plan tonight.",
    ];
    let session = store.begin_session(base, "demo").unwrap_or(0);
    let mut segments: Vec<TranscriptRow> = Vec::new();
    for (i, text) in lines.iter().enumerate() {
        let at = base + i as i64 * 90_000;
        if let Ok(row) = store.insert_segment(session, at, at + 15_000, text, Some(0.92)) {
            segments.push(row);
        }
    }
    let chunk = Chunk {
        started_at: segments.first().map(|s| s.started_at).unwrap_or(base),
        ended_at: segments.last().map(|s| s.ended_at).unwrap_or(base),
        segments,
    };
    let tree = store.nodes().unwrap_or_default();

    println!(
        "\n== interpretation ({}) ==",
        model.as_deref().unwrap_or("default")
    );
    let req = interpret::interpret_request(&tree, &chunk, None, &now, model.clone());
    let total = match cli.run(&req) {
        Ok(resp) => {
            let v = resp.structured.clone().unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            match interpret::parse_interpretation(&v) {
                Ok(interp) => {
                    let model_name = resp.model.clone().unwrap_or_default();
                    match interpret::apply_interpretation(
                        &store,
                        &tree,
                        &chunk,
                        &interp,
                        None,
                        &model_name,
                        resp.cost_usd,
                        None,
                        3,
                        now.timestamp_millis(),
                    ) {
                        Ok(applied) => {
                            println!(
                                "\nstored episode #{} '{}' (rev {}, node {:?}), {} new node(s), {} event(s)",
                                applied.episode.id,
                                applied.episode.title,
                                applied.episode.revision,
                                applied.episode.node_id,
                                applied.new_nodes.len(),
                                applied.events.len()
                            );
                            for n in &applied.new_nodes {
                                println!(
                                    "  new {}: {} — {}",
                                    n.kind.as_str(),
                                    n.name,
                                    n.description
                                );
                            }
                            for e in &applied.events {
                                println!("  event: {} at {}", e.title, e.when_start);
                            }
                        }
                        Err(e) => eprintln!("apply failed: {e}"),
                    }
                }
                Err(e) => eprintln!("the structured output didn't parse: {e}"),
            }
            println!(
                "\ncost ${:.4}, {} ms, {} turn(s), model {}, session {}",
                resp.cost_usd.unwrap_or(0.0),
                resp.duration_ms.unwrap_or(0),
                resp.num_turns.unwrap_or(0),
                resp.model.as_deref().unwrap_or("?"),
                resp.session_id.as_deref().unwrap_or("?")
            );
            resp.cost_usd.unwrap_or(0.0)
        }
        Err(e) => {
            eprintln!("interpretation failed: {e}");
            std::process::exit(1);
        }
    };

    println!(
        "\n== hotword command ({}) ==",
        model.as_deref().unwrap_or("default")
    );
    let spoken =
        "hey computer, property taxes are due on 12/12/2026, also set a reminder one week ahead";
    let Some(command) = hotword::detect(spoken, &hotword::HotwordConfig::default()) else {
        eprintln!("hotword not detected");
        std::process::exit(1);
    };
    println!("command: {command}");
    let seg = store
        .insert_segment(
            session,
            base + 20 * 60_000,
            base + 20 * 60_000 + 6_000,
            spoken,
            None,
        )
        .ok();
    let req = hotword::command_request(&tree, &command, &now, model.clone());
    match cli.run(&req) {
        Ok(resp) => {
            let v = resp.structured.clone().unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            if let (Ok(result), Some(seg)) = (hotword::parse_command(&v), seg) {
                match hotword::execute(&store, &tree, &seg, &result, now.timestamp_millis(), &Local)
                {
                    Ok(done) => {
                        for d in &done {
                            println!("  {}", d.describe(&Local));
                        }
                    }
                    Err(e) => eprintln!("execute failed: {e}"),
                }
            }
            println!(
                "\ncost ${:.4}, {} ms, model {}",
                resp.cost_usd.unwrap_or(0.0),
                resp.duration_ms.unwrap_or(0),
                resp.model.as_deref().unwrap_or("?")
            );
            println!("\ntotal cost ${:.4}", total + resp.cost_usd.unwrap_or(0.0));
        }
        Err(e) => {
            eprintln!("command failed: {e}");
            std::process::exit(1);
        }
    }
}
