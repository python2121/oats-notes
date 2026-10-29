//! Stateful VAD parity: run FluidAudio's stateless batch VAD (`vad_process_samples`, one
//! call over the whole file, state carried internally) and the vendored stateful stream
//! (`VadStream`, one call per 4096-sample chunk, as the engine runs it) over the same 16 kHz
//! mono 16-bit WAV, and compare chunk by chunk.
//!
//!   cargo run --release -p oats-engine-fluidaudio --example vad_parity -- long.wav [threshold]

#[cfg(target_os = "macos")]
#[path = "common/wav.rs"]
mod wav;

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("vad_parity needs macOS (FluidAudio)");
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), String> {
    use fluidaudio_rs::{FluidAudio, VadStream};
    use std::time::Instant;

    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: vad_parity <file.wav> [threshold]")?;
    let threshold: f32 = args
        .next()
        .map(|t| t.parse().map_err(|e| format!("threshold: {e}")))
        .transpose()?
        .unwrap_or(0.6);
    let samples = wav::read_wav(&std::fs::read(&path).map_err(|e| e.to_string())?)?;
    println!(
        "{path}: {:.1} s, threshold {threshold}",
        samples.len() as f64 / 16000.0
    );

    let fluid = FluidAudio::new().map_err(|e| e.to_string())?;
    fluid.init_vad(threshold).map_err(|e| e.to_string())?;

    let t = Instant::now();
    let batch = fluid
        .vad_process_samples(&samples)
        .map_err(|e| e.to_string())?;
    let batch_ms = t.elapsed().as_secs_f64() * 1000.0;

    let mut stream = fluid.vad_stream_start().map_err(|e| e.to_string())?;
    let t = Instant::now();
    let mut streamed = Vec::with_capacity(batch.len());
    for chunk in samples.chunks(VadStream::CHUNK) {
        let mut padded;
        let chunk = if chunk.len() == VadStream::CHUNK {
            chunk
        } else {
            padded = chunk.to_vec();
            padded.resize(VadStream::CHUNK, 0.0);
            &padded
        };
        streamed.push(stream.process_chunk(chunk).map_err(|e| e.to_string())?);
    }
    let stream_ms = t.elapsed().as_secs_f64() * 1000.0;

    if batch.len() != streamed.len() {
        return Err(format!(
            "chunk count differs: batch {} vs stream {}",
            batch.len(),
            streamed.len()
        ));
    }
    let n = batch.len();
    // the batch path pads a final partial chunk by repeating its last sample, the stream
    // path (like the engine) with zeros; compare whole chunks separately
    let whole = samples.len() / VadStream::CHUNK;
    let mut agree = 0usize;
    let mut max_delta = 0.0f32;
    let mut max_delta_whole = 0.0f32;
    let mut speech_batch = 0usize;
    let mut speech_stream = 0usize;
    for (i, (b, s)) in batch.iter().zip(&streamed).enumerate() {
        agree += usize::from(b.is_voice_active == s.is_voice_active);
        speech_batch += usize::from(b.is_voice_active);
        speech_stream += usize::from(s.is_voice_active);
        let d = (b.probability - s.probability).abs();
        max_delta = max_delta.max(d);
        if i < whole {
            max_delta_whole = max_delta_whole.max(d);
        }
    }
    println!("chunks: {n} ({whole} whole)");
    println!(
        "is_speech agreement: {agree}/{n} = {:.3} %",
        agree as f64 * 100.0 / n as f64
    );
    println!("speech chunks: batch {speech_batch}, stream {speech_stream}");
    println!("max |probability delta|: {max_delta:.2e} (whole chunks: {max_delta_whole:.2e})");
    println!(
        "batch: {batch_ms:.0} ms ({:.2} ms/chunk); stream: {stream_ms:.0} ms ({:.2} ms/chunk incl. FFI)",
        batch_ms / n as f64,
        stream_ms / n as f64
    );
    Ok(())
}
