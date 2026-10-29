//! Echo-cancellation spike: does Apple's voice-processing microphone (VoiceProcessingIO)
//! remove what OTHER apps play through the speakers from our microphone capture?
//!
//! Each run opens the microphone in one mode, records 16 kHz mono for `--secs` (default 12)
//! into memory, and after a `--lead` (default 2 s) of room noise plays a fixed sentence through
//! the default output with `say` — another process, i.e. "another app". It then prints the RMS
//! level of the room (lead-in), of the playback window, their difference, and (with
//! `--reference`) how strongly the sentence itself is in the capture: the best normalised
//! cross-correlation against a WAV of the same sentence, as a waveform peak and as a 10 ms
//! envelope peak, over lags up to `--max-lag-ms` (default 500) after `say` was started.
//!
//! Run, back to back, with the built-in speakers as the output (no headphones) at a normal
//! listening volume (~50 %), in a quiet room, without talking:
//!
//! ```text
//! say -o /tmp/aec-ref.wav --data-format=LEI16@16000 "$(cargo run -q --release -p oats-audio --example aec_spike -- --print-sentence)"
//! cargo run --release -p oats-audio --example aec_spike -- --mode plain --reference /tmp/aec-ref.wav
//! cargo run --release -p oats-audio --example aec_spike -- --mode vpio  --reference /tmp/aec-ref.wav
//! ```
//!
//! (`scripts/aec-spike.sh` does exactly that and prints both side by side.)
//!
//! How to read it: the PLAIN run is the control — the speakers must show up clearly
//! (playback − room ≥ ~15 dB, waveform/envelope peaks well above the vpio run's). If the
//! VPIO run's playback − room difference is ≥ 20 dB SMALLER than plain's and its correlation
//! peaks collapse (envelope peak near the noise level, waveform peak a few hundredths), then
//! VoiceProcessingIO cancels other apps' audio and is worth wiring in (`OATS_MIC_MODE=vpio`).
//! A drop of only a few dB means it does not (it only cancels what it plays itself) — the
//! text-level echo rule stays the answer. If plain's peak is reported at the edge of the lag
//! window, `say` started late: rerun with `--max-lag-ms 1500`.
//!
//! Other flags: `--output <device>` plays through that output (`say -a <device>`, e.g.
//! "MacBook Air Speakers"); `--agc on|off` sets the voice unit's AGC (default: Apple's,
//! on); `--vpio-output off` disables the unit's output bus instead of feeding it silence;
//! `OATS_INPUT_DEVICE` picks the microphone (default: the built-in one).
//!
//! It needs microphone permission for the terminal and plays sound: do not run it during a
//! call.

use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use oats_audio::measure::{centred, envelope, ncc_peak, rms_dbfs, span};
use oats_audio::{Capture, InputPolicy, MicMode, VoiceProcessingOptions};

/// The sentence `say` plays; the reference WAV must be rendered from exactly this text.
const SENTENCE: &str = "The quarterly budget review moves to Thursday at three, and Maria \
                        will send the revised numbers to the whole team before lunch.";

const RATE: usize = 16_000;
/// 10 ms envelope hop.
const HOP: usize = RATE / 100;

struct Args {
    mode: MicMode,
    secs: f64,
    lead: f64,
    output: Option<String>,
    reference: Option<String>,
    max_lag_ms: usize,
    agc: Option<bool>,
    vpio_output: bool,
}

fn value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn on_off(v: &str) -> Result<bool, String> {
    match v {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        other => Err(format!("expected on|off, got \"{other}\"")),
    }
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mode = match value(args, "--mode") {
        Some(m) => MicMode::parse(m).ok_or(format!("--mode plain|vpio, got \"{m}\""))?,
        None => MicMode::Plain,
    };
    let num = |flag: &str, default: f64| -> Result<f64, String> {
        value(args, flag).map_or(Ok(default), |v| {
            v.parse::<f64>()
                .ok()
                .filter(|x| x.is_finite() && *x > 0.0)
                .ok_or(format!("{flag}: a positive number, got \"{v}\""))
        })
    };
    let secs = num("--secs", 12.0)?;
    let lead = num("--lead", 2.0)?;
    if lead + 1.0 >= secs {
        return Err("--secs must exceed --lead by more than a second".to_owned());
    }
    Ok(Args {
        mode,
        secs,
        lead,
        output: value(args, "--output").map(str::to_owned),
        reference: value(args, "--reference").map(str::to_owned),
        max_lag_ms: num("--max-lag-ms", 500.0)? as usize,
        agc: value(args, "--agc").map(on_off).transpose()?,
        vpio_output: value(args, "--vpio-output").map_or(Ok(true), on_off)?,
    })
}

fn spawn_say(output: Option<&str>) -> std::io::Result<Child> {
    let mut cmd = Command::new("say");
    if let Some(dev) = output {
        cmd.args(["-a", dev]);
    }
    cmd.arg(SENTENCE).spawn()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|a| a == "--print-sentence") {
        println!("{SENTENCE}");
        return Ok(());
    }
    let args = parse_args(&raw)?;
    let reference = match &args.reference {
        Some(p) => Some(oats_audio::file_source::load_wav(std::path::Path::new(p))?),
        None => None,
    };

    let (tx, rx) = mpsc::channel();
    let vp = VoiceProcessingOptions {
        agc: args.agc,
        render_silence: args.vpio_output,
    };
    let capture = Capture::start_mic_with(tx, &InputPolicy::default().with_env(), args.mode, vp)?;
    let d = capture.device();
    println!(
        "[{}] mic \"{}\" ({} Hz, {} ch → 16 kHz mono), {:.0} s, say after {:.1} s{}",
        capture.mode().as_str(),
        d.name,
        d.sample_rate,
        d.channels,
        args.secs,
        args.lead,
        args.output
            .as_deref()
            .map(|o| format!(" through \"{o}\""))
            .unwrap_or_default()
    );
    let mode = capture.mode();
    if mode != args.mode {
        println!("  (asked for {}; not available here)", args.mode.as_str());
    }

    let total = (args.secs * RATE as f64) as usize;
    let lead = (args.lead * RATE as f64) as usize;
    let mut samples: Vec<f32> = Vec::with_capacity(total + RATE);
    let mut say: Option<Child> = None;
    let mut say_start: Option<usize> = None;
    let mut say_end: Option<usize> = None;
    let deadline = Instant::now() + Duration::from_secs_f64(args.secs + 5.0);
    while samples.len() < total {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            println!("  capture stalled: {} of {total} samples", samples.len());
            break;
        };
        let Ok(chunk) = rx.recv_timeout(left.min(Duration::from_millis(200))) else {
            continue;
        };
        samples.extend_from_slice(&chunk.samples);
        if say_start.is_none() && samples.len() >= lead {
            say = Some(spawn_say(args.output.as_deref())?);
            say_start = Some(samples.len());
        }
        if say_end.is_none()
            && let Some(child) = say.as_mut()
            && child.try_wait()?.is_some()
        {
            say_end = Some(samples.len());
        }
    }
    drop(capture);
    if let Some(mut child) = say {
        if say_end.is_none() {
            println!("  say still playing at the end of the recording (use a longer --secs)");
        }
        let status = child.wait()?;
        if !status.success() {
            return Err(format!("say failed: {status}").into());
        }
    }
    let start = say_start.ok_or("the recording ended before playback started")?;
    let end = say_end.unwrap_or(samples.len());

    let room = rms_dbfs(span(&samples, RATE / 2, start));
    // skip say's start-up (~0.3 s) so the window is mostly speech
    let play = rms_dbfs(span(&samples, start + RATE * 3 / 10, end));
    let diff = play - room;
    println!("  room (lead-in)   {room:7.1} dBFS");
    println!("  during playback  {play:7.1} dBFS");
    println!("  difference       {diff:7.1} dB");

    let mut wave = None;
    let mut env = None;
    if let Some(r) = &reference {
        let max_lag = args.max_lag_ms * RATE / 1000;
        wave = ncc_peak(&samples, r, start, max_lag);
        let cap_env = centred(&envelope(
            span(&samples, start, start + max_lag + r.len()),
            HOP,
        ));
        env = ncc_peak(&cap_env, &centred(&envelope(r, HOP)), 0, max_lag / HOP);
        let show = |name: &str, p: Option<(f32, usize)>, unit: usize| match p {
            Some((peak, lag)) => {
                let lag_ms = lag * unit * 1000 / RATE;
                let edge = if lag * unit + unit >= max_lag {
                    "  (at the lag window's edge: try --max-lag-ms 1500)"
                } else {
                    ""
                };
                println!("  {name} peak {peak:6.3} at {lag_ms} ms{edge}");
            }
            None => println!("  {name} peak: nothing to correlate"),
        };
        show("waveform", wave, 1);
        show("envelope", env, HOP);
    }

    let heard = if diff >= 10.0 {
        "the played sentence is clearly in the capture"
    } else if diff >= 3.0 {
        "the played sentence is faintly in the capture"
    } else {
        "the played sentence is not measurably above the room noise (cancelled — or not \
         audible: check the volume and the output)"
    };
    println!(
        "VERDICT [{}]: {heard} ({diff:+.1} dB over the room)",
        mode.as_str()
    );
    if mode == MicMode::Plain && diff < 10.0 {
        println!(
            "  note: the plain run is the control and should hear the speakers; turn the \
             volume up or check the output device"
        );
    }
    // one machine-readable line for scripts/aec-spike.sh
    let p = |x: Option<(f32, usize)>| x.map_or("nan".to_owned(), |(v, _)| format!("{v:.3}"));
    println!(
        "RESULT mode={} room_db={room:.1} play_db={play:.1} diff_db={diff:.1} wave_peak={} env_peak={}",
        mode.as_str(),
        p(wave),
        p(env)
    );
    Ok(())
}
