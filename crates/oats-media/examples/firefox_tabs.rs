//! Prints Firefox's selected tabs from the session store, then (with `--watch N`) polls the
//! session file's mtime for N seconds and prints every rewrite. Read-only.
//!
//! `cargo run -p oats-media --example firefox_tabs -- --watch 60`

use std::time::{Duration, Instant, SystemTime};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let watch = args
        .iter()
        .position(|a| a == "--watch")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<u64>().ok());
    let roots = oats_media::firefox::default_roots();
    let Some((path, modified)) = oats_media::firefox::newest_recovery_file(&roots) else {
        println!("no recovery.jsonlz4 under {roots:?}");
        return;
    };
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    println!(
        "{} (written {:.0} s ago)",
        path.display(),
        age.as_secs_f64()
    );
    match oats_media::firefox::read_session_file(&path) {
        Ok(s) => {
            for w in s.tabs.iter().filter(|t| t.selected) {
                let kind = oats_media::classify_url(&w.tab.url);
                println!(
                    "  {} {:?} {} | {}",
                    if w.front_window { "*" } else { " " },
                    kind,
                    w.tab.url,
                    w.tab.title
                );
            }
        }
        Err(e) => println!("  error: {e}"),
    }
    let Some(secs) = watch else { return };
    let start = Instant::now();
    let mut last = modified;
    let mut last_change = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(500));
        let Some((p, m)) = oats_media::firefox::newest_recovery_file(&roots) else {
            continue;
        };
        if m != last {
            let tabs = oats_media::firefox::read_session_file(&p)
                .map(|s| {
                    s.tabs
                        .iter()
                        .find(|t| t.selected)
                        .map(|w| w.tab.url.clone())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            println!(
                "  t={:5.1}s rewritten ({:.1} s after the previous) front tab: {tabs}",
                start.elapsed().as_secs_f64(),
                last_change.elapsed().as_secs_f64()
            );
            last = m;
            last_change = Instant::now();
        }
    }
}
