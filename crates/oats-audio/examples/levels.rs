//! Prints the level of each 100 ms chunk from the microphone (and, with `--system`, from system
//! audio — what the computer plays) for a few seconds.
//!
//! `cargo run -p oats-audio --example levels [-- --system] [--secs N]`
//!
//! The terminal needs microphone permission; `--system` needs "System Audio Recording Only"
//! (System Settings → Privacy & Security → Screen & System Audio Recording) for the terminal
//! app. macOS may prompt at the first run; without the grant the system lane is silent
//! (exact zeros), which this prints as "silent (all zeros)". `OATS_INPUT_DEVICE` picks a
//! microphone by name (default: the built-in one).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use oats_audio::{AudioSourceKind, Capture, InputPolicy, SystemCapture};

fn db(samples: &[f32]) -> f32 {
    let mean_sq = samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32;
    (20.0 * mean_sq.sqrt().max(1e-5).log10()).max(-100.0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let system = args.iter().any(|a| a == "--system");
    let secs: u64 = args
        .iter()
        .position(|a| a == "--secs")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(if system { 10 } else { 5 });

    let (tx, rx) = mpsc::channel();
    let capture = Capture::start_mic(tx.clone(), &InputPolicy::default().with_env())?;
    let d = capture.device();
    println!(
        "mic:    \"{}\" ({} Hz, {} ch) → 16 kHz mono",
        d.name, d.sample_rate, d.channels
    );
    let _system = if system {
        let s = SystemCapture::start_with(tx.clone(), |out| {
            println!(
                "system: recording output \"{}\" ({})",
                out.name,
                if out.speakers {
                    "speakers"
                } else {
                    "not speakers"
                }
            );
        })?;
        println!(
            "system: opening the tap (macOS may ask for permission to record system audio; \
             the lane delivers nothing until that is answered)"
        );
        Some(s)
    } else {
        None
    };
    drop(tx);

    let mut nonzero_system = 0usize;
    let mut system_chunks = 0usize;
    let mut last = [-100.0f32; 2];
    let end = Instant::now() + Duration::from_secs(secs);
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        let Ok(chunk) = rx.recv_timeout(left) else {
            break;
        };
        let level = db(&chunk.samples);
        match chunk.source {
            AudioSourceKind::Mic => {
                last[0] = level;
                let bar = "#".repeat(((level + 80.0).max(0.0) / 2.0) as usize);
                if system {
                    let sys = if system_chunks == 0 {
                        "    (no chunks yet)".to_owned()
                    } else if last[1] <= -100.0 {
                        "    silent (all zeros)".to_owned()
                    } else {
                        format!("{:7.1} dBFS", last[1])
                    };
                    println!("mic {level:7.1} dBFS  system {sys}  {bar}");
                } else {
                    println!("{level:7.1} dBFS  {bar}");
                }
            }
            AudioSourceKind::System => {
                system_chunks += 1;
                if chunk.samples.iter().any(|s| *s != 0.0) {
                    nonzero_system += 1;
                    last[1] = level;
                } else {
                    last[1] = -100.0;
                }
            }
        }
    }
    if system {
        println!(
            "system lane: {system_chunks} chunks, {nonzero_system} with signal{}",
            if system_chunks > 0 && nonzero_system == 0 {
                " — all zeros: nothing played, or the terminal lacks the \"System Audio \
                 Recording Only\" permission (System Settings → Privacy & Security → Screen & \
                 System Audio Recording)"
            } else {
                ""
            }
        );
    }
    Ok(())
}
