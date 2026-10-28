//! End-to-end check of the macOS engine without a microphone: load the models (first run
//! downloads them), run VAD over a 16 kHz mono 16-bit WAV, transcribe the whole file.
//!
//!   say -o speech.wav --data-format=LEI16@16000 "some sentence"
//!   cargo run -p oats-engine-fluidaudio --example transcribe_wav -- speech.wav

use std::time::Instant;

/// Minimal RIFF/WAVE reader for PCM 16-bit mono at 16 kHz (what `say` writes).
fn read_wav(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut pos = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body = &bytes[pos + 8..(pos + 8 + size).min(bytes.len())];
        match id {
            b"fmt " => {
                let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
                let u32_at =
                    |i: usize| u32::from_le_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
                format = Some((u16_at(0), u16_at(2), u32_at(4), u16_at(14)));
            }
            b"data" => {
                let (tag, channels, rate, bits) = format.ok_or("data before fmt")?;
                if tag != 1 || channels != 1 || rate != 16_000 || bits != 16 {
                    return Err(format!(
                        "need PCM 16-bit mono 16 kHz, got tag {tag}, {channels} ch, {rate} Hz, {bits} bit"
                    ));
                }
                return Ok(body
                    .chunks_exact(2)
                    .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
                    .collect());
            }
            _ => {}
        }
        pos += 8 + size + (size & 1);
    }
    Err("no data chunk".into())
}

fn main() -> Result<(), String> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: transcribe_wav <file.wav>")?;
    let samples = read_wav(&std::fs::read(&path).map_err(|e| e.to_string())?)?;
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
