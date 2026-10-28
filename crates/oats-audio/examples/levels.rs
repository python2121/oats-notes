//! Prints the level of each 100 ms chunk from the default microphone for 5 seconds.
//!
//! `cargo run -p oats-audio --example levels` (the terminal needs microphone permission).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use oats_audio::Capture;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::channel();
    let capture = Capture::start(tx)?;
    let d = capture.device();
    println!(
        "capturing from \"{}\" ({} Hz, {} ch) → 16 kHz mono",
        d.name, d.sample_rate, d.channels
    );
    let end = Instant::now() + Duration::from_secs(5);
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        let Ok(chunk) = rx.recv_timeout(left) else {
            break;
        };
        let mean_sq = chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len().max(1) as f32;
        let db = (20.0 * mean_sq.sqrt().max(1e-5).log10()).max(-100.0);
        let bar = "#".repeat(((db + 80.0).max(0.0) / 2.0) as usize);
        println!("{db:7.1} dBFS  {bar}");
    }
    Ok(())
}
