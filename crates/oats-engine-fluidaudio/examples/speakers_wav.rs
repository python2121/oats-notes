//! Manual check of the engine's speaker spans: load the models (first run downloads the
//! speaker models, ~30 MB), cut a 16 kHz mono 16-bit WAV into pieces (default 30 s, the
//! pipeline's longest segment; `--piece 5` imitates short turns), run
//! `SpeechEngine::speaker_spans` on each piece, print every span and the pairwise cosine
//! distances between their embeddings (the app's `oats_core::speakers` assigns at ≤ 0.65 and
//! updates a voiceprint at ≤ 0.45), and the cost per 10 s of audio (warm, median of 3 runs).
//!
//!   say -v Daniel -o a.wav --data-format=LEI16@16000 --file-format=WAVE "…"
//!   cargo run --release -p oats-engine-fluidaudio --example speakers_wav -- two.wav [--piece 5]

use std::time::Instant;

#[path = "common/wav.rs"]
mod wav;

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    let mut piece_s = 30.0f64;
    while let Some(a) = args.next() {
        if a == "--piece" {
            piece_s = args
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or("--piece needs seconds")?;
        } else {
            path = Some(a);
        }
    }
    let path = path.ok_or("usage: speakers_wav <file.wav> [--piece SECONDS]")?;
    let samples = wav::read_wav(&std::fs::read(&path).map_err(|e| e.to_string())?)?;
    let secs = samples.len() as f64 / 16000.0;
    println!("{path}: {secs:.2} s of audio, pieces of {piece_s} s");

    let factory = oats_engine_fluidaudio::factory();
    let t = Instant::now();
    let mut engine = factory().map_err(|e| e.to_string())?;
    engine
        .load(&mut |line| println!("  [{:>6.1} s] {line}", t.elapsed().as_secs_f64()))
        .map_err(|e| e.to_string())?;
    println!("engine ready in {:.1} s", t.elapsed().as_secs_f64());

    let piece = ((piece_s * 16000.0) as usize).max(16000);
    // warm-up (the first CoreML prediction compiles / loads onto the ANE)
    let t = Instant::now();
    let _ = engine.speaker_spans(&samples[..samples.len().min(piece)]);
    println!(
        "first call (cold): {:.0} ms",
        t.elapsed().as_secs_f64() * 1000.0
    );

    let mut spans = Vec::new();
    let mut runs_ms = Vec::new();
    for run in 0..3 {
        let t = Instant::now();
        let mut all = Vec::new();
        for (i, chunk) in samples.chunks(piece).enumerate() {
            let offset_ms = (i * piece) as i64 * 1000 / 16000;
            for s in engine.speaker_spans(chunk).map_err(|e| e.to_string())? {
                all.push((offset_ms, s));
            }
        }
        runs_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        if run == 0 {
            spans = all;
        }
    }
    runs_ms.sort_by(f64::total_cmp);
    let median = runs_ms[1];
    println!(
        "speaker_spans over the whole file: {median:.0} ms (median of 3) = {:.1} ms per 10 s of audio",
        median / secs * 10.0
    );

    // the cost to compare with: transcribing the same pieces
    let _ = engine.transcribe(&samples[..samples.len().min(piece)]);
    let mut asr = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        for chunk in samples.chunks(piece) {
            engine.transcribe(chunk).map_err(|e| e.to_string())?;
        }
        asr.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    asr.sort_by(f64::total_cmp);
    println!(
        "transcribe over the same pieces: {:.0} ms (median of 3) = {:.1} ms per 10 s; speakers cost {:.0} % of it",
        asr[1],
        asr[1] / secs * 10.0,
        median / asr[1] * 100.0
    );

    for (i, (off, s)) in spans.iter().enumerate() {
        println!(
            "  span {i}: {:>6.2}–{:>6.2} s  speech {:>5.2} s  |e| = {:.3}",
            (off + s.start_ms) as f64 / 1000.0,
            (off + s.end_ms) as f64 / 1000.0,
            s.speech_ms as f64 / 1000.0,
            s.embedding.iter().map(|x| x * x).sum::<f32>().sqrt()
        );
        if std::env::var_os("SPEAKERS_DUMP").is_some() {
            // the raw vector, one line, for cross-run comparisons
            let v: Vec<String> = s.embedding.iter().map(|x| format!("{x:.5}")).collect();
            println!("EMB {i} {}", v.join(" "));
        }
    }
    if spans.len() > 1 {
        println!("pairwise cosine distance:");
        print!("        ");
        for j in 0..spans.len() {
            print!("{j:>6}");
        }
        println!();
        for (i, (_, a)) in spans.iter().enumerate() {
            print!("  {i:>4}  ");
            for (_, b) in &spans {
                print!(
                    "{:>6.3}",
                    oats_core::speakers::cosine_distance(&a.embedding, &b.embedding)
                );
            }
            println!();
        }
    }
    Ok(())
}
