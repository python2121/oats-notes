//! End-to-end check of the macOS engine without a microphone: load the models (first run
//! downloads them), run VAD over a 16 kHz mono 16-bit WAV, transcribe the whole file.
//!
//!   say -o speech.wav --data-format=LEI16@16000 "some sentence"
//!   cargo run -p oats-engine-fluidaudio --example transcribe_wav -- speech.wav

use std::time::Instant;

#[path = "common/wav.rs"]
mod wav;

fn main() -> Result<(), String> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: transcribe_wav <file.wav>")?;
    let samples = wav::read_wav(&std::fs::read(&path).map_err(|e| e.to_string())?)?;
    println!("{path}: {:.2} s of audio", samples.len() as f64 / 16000.0);

    let factory = oats_engine_fluidaudio::factory();
    let t = Instant::now();
    let mut engine = factory().map_err(|e| e.to_string())?;
    engine
        .load(&mut |line| println!("  [{:>6.1} s] {line}", t.elapsed().as_secs_f64()))
        .map_err(|e| e.to_string())?;
    println!(
        "engine ready in {:.1} s: {}",
        t.elapsed().as_secs_f64(),
        engine.name()
    );

    if let Some(frame) = engine.vad_frame_len() {
        let t = Instant::now();
        let mut speech = 0usize;
        let mut total = 0usize;
        for chunk in samples.chunks(frame) {
            let frames = engine.vad(chunk).map_err(|e| e.to_string())?;
            total += frames.len();
            speech += frames.iter().filter(|f| f.is_speech).count();
        }
        println!(
            "vad: {speech}/{total} frames speech ({} ms per frame) in {:.0} ms",
            frame * 1000 / 16000,
            t.elapsed().as_secs_f64() * 1000.0
        );
    }

    let result = engine.transcribe(&samples).map_err(|e| e.to_string())?;
    println!(
        "transcribed in {:.0} ms (confidence {:?}):\n  {}",
        result.processing_ms, result.confidence, result.text
    );
    Ok(())
}
