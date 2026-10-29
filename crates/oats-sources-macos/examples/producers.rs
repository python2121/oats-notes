//! Prints the processes producing audio output, every second for N seconds (default 10).
//! Property reads only: opens no stream, needs no permission.
//!
//! `cargo run --manifest-path crates/oats-sources-macos/Cargo.toml --example producers -- 20`

use oats_sources::ProducerWatch;

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let Some(mut source) = oats_sources_macos::MacProducers::new() else {
        println!("no Core Audio process objects on this system");
        return;
    };
    let mut watch = ProducerWatch::new();
    for t in 0..secs {
        let started = std::time::Instant::now();
        if let Some(now) = watch.poll(&mut source) {
            println!("t={t:3}s ({:?} per poll):", started.elapsed());
            if now.is_empty() {
                println!("    nothing playing");
            }
            for p in now {
                println!("    pid {:6} {:40} {}", p.pid, p.bundle_id, p.name);
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
