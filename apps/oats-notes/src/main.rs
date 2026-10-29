#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]
//! OatsNotes desktop binary: the eframe shell from `oats-ui` (wgpu renderer), with the speech
//! engine (`oats-engine-fluidaudio`, or the null engine with `OATS_ENGINE=null`) and the
//! microphone (`oats-audio`) and the status item (`oats-status-macos`) wired in here — the UI
//! crate depends on the platform-neutral contracts (`oats-core`, `oats-status`) only.
//!
//! `--listen` starts listening as soon as the engine is ready (as if the last session had
//! quit while listening). `--audio-file <wav> [--audio-speed N] [--audio-loop]` plays a
//! 16 kHz mono 16-bit WAV instead of opening the microphone (benchmarks, reproducible runs;
//! `scripts/perf.sh`); `--system-audio-file <wav>` plays a second one as the system-audio lane.
//!
//! Audio: the microphone by `oats_audio::InputPolicy` (`OATS_INPUT_DEVICE`, else the built-in
//! microphone, never silently a Bluetooth headset) and, while the "Capture System Audio" pref is
//! on, system audio from the default output device (`oats_audio::SystemCapture`), both into one
//! tagged channel. The echo rule (`OATS_SYSTEM_DEDUP=auto|always|never`) is switched from the
//! output device the system capture reports: speakers → on, headphones → off (auto).
//!
//! Environment: `OATS_ENGINE=null` (no model), `OATS_NO_NATIVE_MENU=1` (menus in the window),
//! `OATS_NO_STATUS_ITEM=1` (no menu bar extra; closing the window then quits),
//! `OATS_FONT=bundled` (Inter instead of the system font), `OATS_FONT_GRADE` (SF Pro grade),
//! `OATS_DATA_DIR` (where the database lives), `OATS_INPUT_DEVICE` (a microphone by name),
//! `OATS_SYSTEM_DEDUP` (the echo rule), `OATS_MIC_MODE=plain|vpio` (the voice-processing
//! microphone spike, default plain), `RUST_LOG`; the brain's `OATS_BRAIN=off`,
//! `OATS_CLAUDE_BIN`, `OATS_CLAUDE_MODEL`, `OATS_INTERPRET_EVERY_MIN`, `OATS_BRAIN_BUDGET_USD`,
//! `OATS_BRAIN_TIER0_PER_HOUR`, `OATS_CLAUDE_PASS_PROXY`, `OATS_HOTWORDS` (see
//! `oats_brain::BrainConfig::from_env`).
//!
//! `--selftest <out.png> [--listening|--notes|--work|--calendar] [--no-sidebar] [--search <text>] [--topic <text>]` renders the whole window offscreen to a PNG (seeded
//! demo content, the null engine, a throwaway database) — the shell verified without a window.

use std::any::Any;
use std::process::ExitCode;

const APP_NAME: &str = "OatsNotes";

/// The app icon (`scripts/generate_icon.swift`), with macOS's standard transparent margin, so
/// the Dock and ⌘Tab show it at the size every other app has.
const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/icon_1024.png");

/// The window/app icon. macOS gets the full 1024 px artwork (eframe hands it to
/// `NSApplication.applicationIconImage`); X11/Windows get a 256 px copy. `None` (logged) if the
/// PNG can't be decoded; eframe then uses its default.
fn app_icon() -> Option<std::sync::Arc<egui::IconData>> {
    let img = match image::load_from_memory_with_format(APP_ICON_PNG, image::ImageFormat::Png) {
        Ok(img) => img,
        Err(e) => {
            log::warn!("app icon: {e}");
            return None;
        }
    };
    let img = if cfg!(target_os = "macos") {
        img
    } else {
        img.resize_exact(256, 256, image::imageops::FilterType::Lanczos3)
    };
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    Some(std::sync::Arc::new(egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    }))
}

/// The speech engine: FluidAudio (Parakeet + Silero on CoreML) unless `OATS_ENGINE=null`.
fn engine_factory() -> oats_core::EngineFactory {
    if std::env::var("OATS_ENGINE").is_ok_and(|v| v.eq_ignore_ascii_case("null")) {
        log::info!("OATS_ENGINE=null: running without a speech model");
        Box::new(|| Ok(Box::new(oats_core::NullEngine) as Box<dyn oats_core::SpeechEngine>))
    } else {
        oats_engine_fluidaudio::factory()
    }
}

/// How listening opens audio: the WAV file(s) of `--audio-file` / `--system-audio-file`, or
/// the microphone (by `InputPolicy`, `OATS_INPUT_DEVICE`) plus, when asked, system audio — all
/// into the one tagged channel. The system capture reports its output device; the echo rule
/// follows it (`dedup_mode`). A system capture that fails leaves the microphone running.
fn audio_starter(
    mic_file: Option<std::sync::Arc<[f32]>>,
    system_file: Option<std::sync::Arc<[f32]>>,
    file_options: oats_audio::FileSourceOptions,
    echo_dedup: std::sync::Arc<std::sync::atomic::AtomicBool>,
    dedup_mode: oats_core::DedupMode,
) -> oats_ui::AudioStarter {
    use std::sync::atomic::Ordering;
    match mic_file {
        // --audio-file: the WAV instead of the microphone, from the start each time; a
        // --system-audio-file plays alongside as the system lane (no device: auto dedups)
        Some(samples) => Box::new(move |tx, request| {
            let mic = oats_audio::FileSource::start(samples.clone(), file_options, tx.clone())
                .map_err(|e| e.to_string())?;
            let system = match (&system_file, request.system) {
                (Some(sys), true) => {
                    echo_dedup.store(dedup_mode.active(true), Ordering::Relaxed);
                    let options = oats_audio::FileSourceOptions {
                        source: oats_core::AudioSourceKind::System,
                        ..file_options
                    };
                    Some(
                        oats_audio::FileSource::start(sys.clone(), options, tx)
                            .map_err(|e| e.to_string())?,
                    )
                }
                _ => None,
            };
            Ok(oats_ui::StartedAudio {
                system: system.is_some(),
                guard: Box::new((mic, system)) as Box<dyn Any>,
                system_error: None,
            })
        }),
        None => {
            let policy = oats_audio::InputPolicy::default().with_env();
            let mic_mode = oats_audio::MicMode::from_env();
            Box::new(move |tx, request| {
                let mic = oats_audio::Capture::start_mic_with(
                    tx.clone(),
                    &policy,
                    mic_mode,
                    oats_audio::VoiceProcessingOptions::default(),
                )
                .map_err(|e| e.to_string())?;
                let mut system_error = None;
                let system = if request.system {
                    // until the output is known: on unless the mode says never
                    echo_dedup.store(dedup_mode.active(true), Ordering::Relaxed);
                    let flag = echo_dedup.clone();
                    let started = oats_audio::SystemCapture::start_with(tx, move |out| {
                        let on = dedup_mode.active(out.speakers);
                        flag.store(on, Ordering::Relaxed);
                        log::info!(
                            "echo rule {} (output \"{}\", {dedup_mode:?})",
                            if on { "on" } else { "off" },
                            out.name
                        );
                    });
                    match started {
                        Ok(s) => Some(s),
                        Err(e) => {
                            system_error = Some(e.to_string());
                            None
                        }
                    }
                } else {
                    None
                };
                Ok(oats_ui::StartedAudio {
                    system: system.is_some(),
                    guard: Box::new((mic, system)) as Box<dyn Any>,
                    system_error,
                })
            })
        }
    }
}

/// The status item (menu bar extra): the macOS implementation, unless `OATS_NO_STATUS_ITEM` is
/// set. A Linux (StatusNotifierItem) crate would be picked here the same way. `None` (logged)
/// when the platform has none or it fails to install.
fn status_item(ctx: &egui::Context) -> Option<Box<dyn oats_status::StatusItem>> {
    if std::env::var_os("OATS_NO_STATUS_ITEM").is_some() {
        log::info!("OATS_NO_STATUS_ITEM: no status item");
        return None;
    }
    let ctx = ctx.clone();
    match oats_status_macos::factory()(Box::new(move || ctx.request_repaint())) {
        Ok(item) => Some(item),
        Err(e) => {
            log::warn!("status item: {e}");
            None
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: oats-notes [--listen] [--audio-file <wav> [--system-audio-file <wav>] [--audio-speed N] [--audio-loop]]\n       oats-notes --selftest <out.png> [--listening|--notes|--work|--calendar] [--no-sidebar] [--search <text>] [--topic <text>]\n\n\
         --listen       start listening as soon as the speech engine is ready\n\
         --audio-file   listen to a WAV file (PCM 16-bit mono 16 kHz) instead of the microphone,\n\
         \x20              paced in real time; listening stops when the file ends\n\
         --system-audio-file  with --audio-file: a second WAV played as the system-audio lane\n\
         --audio-speed  with --audio-file: playback rate relative to real time (default 1)\n\
         --audio-loop   with --audio-file: start the file again at its end\n\
         --selftest     render the window offscreen to a PNG (demo content, no microphone)\n\
         --listening    with --selftest: show the Listening state with a fake input level\n\
         --notes        with --selftest: the Notes view (a seeded interpreted episode)\n\
         --work         with --selftest: the Work category view (projects and their notes)\n\
         --calendar     with --selftest: the Calendar view (seeded events and a reminder)\n\
         --search TEXT  with --selftest: open the search bar (⌘F) with this query\n\
         --topic TEXT   with --selftest: open the Topic view for this topic"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return usage();
    }
    if args.first().map(String::as_str) == Some("--selftest") {
        return selftest_cli(&args[1..]);
    }
    let mut start_listening = false;
    let mut audio_file: Option<std::path::PathBuf> = None;
    let mut system_audio_file: Option<std::path::PathBuf> = None;
    let mut file_options = oats_audio::FileSourceOptions::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--listen" => start_listening = true,
            "--audio-file" => match it.next() {
                Some(p) => audio_file = Some(p.into()),
                None => return usage(),
            },
            "--system-audio-file" => match it.next() {
                Some(p) => system_audio_file = Some(p.into()),
                None => return usage(),
            },
            "--audio-speed" => match it.next().and_then(|v| v.parse::<f64>().ok()) {
                Some(v) if v.is_finite() && v > 0.0 => file_options.speed = v,
                _ => return usage(),
            },
            "--audio-loop" => file_options.looping = true,
            _ => return usage(),
        }
    }
    if system_audio_file.is_some() && audio_file.is_none() {
        return usage();
    }
    // decoded once up front (a bad file fails here, not at the first Listen)
    let load = |path: &Option<std::path::PathBuf>,
                flag: &str|
     -> Result<Option<std::sync::Arc<[f32]>>, ExitCode> {
        match path {
            Some(path) => match oats_audio::file_source::load_wav(path) {
                Ok(s) => Ok(Some(s.into())),
                Err(e) => {
                    eprintln!("oatsnotes: {flag}: {e}");
                    Err(ExitCode::from(2))
                }
            },
            None => Ok(None),
        }
    };
    let audio_samples = match load(&audio_file, "--audio-file") {
        Ok(s) => s,
        Err(code) => return code,
    };
    let system_samples = match load(&system_audio_file, "--system-audio-file") {
        Ok(s) => s,
        Err(code) => return code,
    };

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder {
            icon: app_icon(),
            ..Default::default()
        }
        .with_title(APP_NAME)
        .with_app_id("oatsnotes")
        .with_inner_size([1100.0, 720.0])
        .with_min_inner_size([800.0, 520.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            // owner-only data directory (0700; tightened if an older build made it 0755);
            // the store creates the database file 0600
            if let Err(e) = oats_core::paths::secure_data_dir() {
                log::warn!("data directory: {e}");
            }
            let db = oats_core::paths::database_path();
            let store = match oats_core::Store::open(&db) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("oatsnotes: cannot open {}: {e}", db.display());
                    return Err(Box::new(e));
                }
            };
            log::info!("database: {}", db.display());
            let ctx = cc.egui_ctx.clone();
            let pipeline = oats_core::Pipeline {
                engine: engine_factory(),
                store_path: db.clone(),
                segmenter: Default::default(),
                wake: Box::new(move || ctx.request_repaint()),
            }
            .spawn();
            // the brain: the local `claude` CLI, its own store connection, its own thread;
            // OATS_BRAIN=off keeps the thread but runs nothing
            let mut brain_config = oats_brain::BrainConfig::from_env();
            // an empty directory of its own, never the data dir (which holds the database)
            brain_config.cli.work_dir = Some(oats_core::paths::claude_work_dir());
            let ctx = cc.egui_ctx.clone();
            let brain = oats_brain::Brain {
                config: brain_config,
                store_path: db,
                wake: Box::new(move || ctx.request_repaint()),
            }
            .spawn();
            let echo_dedup = pipeline.echo_dedup_flag();
            let dedup_mode = oats_core::DedupMode::from_env();
            log::info!("echo rule (OATS_SYSTEM_DEDUP): {dedup_mode:?}");
            let audio = audio_starter(
                audio_samples,
                system_samples,
                file_options,
                echo_dedup,
                dedup_mode,
            );
            // macOS: the menus go in the system menu bar (opt out with OATS_NO_NATIVE_MENU=1);
            // every other platform keeps the in-window egui menu bar.
            let native_menu =
                cfg!(target_os = "macos") && std::env::var_os("OATS_NO_NATIVE_MENU").is_none();
            let app = oats_ui::OatsApp::new(
                cc,
                oats_ui::AppOptions {
                    native_menu,
                    // OATS_FONT=bundled keeps the bundled Inter instead of the system font
                    system_font: std::env::var("OATS_FONT").map_or(true, |v| v != "bundled"),
                    persist: true,
                    start_listening,
                },
                pipeline,
                audio,
                store,
                // created here: AppKit wants it on the main thread, after NSApp exists
                status_item(&cc.egui_ctx),
                Some(brain),
            );
            #[cfg(target_os = "macos")]
            if native_menu {
                return Ok(match oats_ui::native_menu::NativeMenuApp::new(app) {
                    Ok(wrapped) => Box::new(wrapped) as Box<dyn eframe::App>,
                    Err(app) => app,
                });
            }
            Ok(Box::new(app))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("oatsnotes: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `--selftest <out.png> [--listening]`: the null engine, a fresh database in `OATS_DATA_DIR`
/// (else a temp dir), the window rendered offscreen.
#[cfg(feature = "selftest")]
fn selftest_cli(args: &[String]) -> ExitCode {
    let mut out: Option<std::path::PathBuf> = None;
    let mut options = oats_ui::selftest::SelftestOptions::default();
    let mut args = args.iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--search" => match args.next() {
                Some(q) => options.search = Some(q.clone()),
                None => return usage(),
            },
            "--topic" => match args.next() {
                Some(q) => options.topic = Some(q.clone()),
                None => return usage(),
            },
            "--listening" => options.listening = true,
            "--notes" => options.notes = true,
            "--calendar" => options.calendar = true,
            "--work" => options.work = true,
            "--no-sidebar" => options.no_sidebar = true,
            f if !f.starts_with("--") && out.is_none() => out = Some(f.into()),
            _ => return usage(),
        }
    }
    let Some(out) = out else { return usage() };
    let dir = std::env::var_os("OATS_DATA_DIR").map_or_else(
        || std::env::temp_dir().join(format!("oatsnotes-selftest-{}", std::process::id())),
        std::path::PathBuf::from,
    );
    let db = dir.join("selftest.sqlite");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    let store = match oats_core::Store::open(&db) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("selftest failed: cannot open {}: {e}", db.display());
            return ExitCode::FAILURE;
        }
    };
    let pipeline = oats_core::Pipeline {
        engine: Box::new(
            || Ok(Box::new(oats_core::NullEngine) as Box<dyn oats_core::SpeechEngine>),
        ),
        store_path: db,
        segmenter: Default::default(),
        wake: Box::new(|| {}),
    }
    .spawn();
    let audio: oats_ui::AudioStarter =
        Box::new(|_tx, _request| Err("the selftest never opens the microphone".to_owned()));
    let result = oats_ui::selftest::run_with(&out, store, pipeline, audio, options);
    if std::env::var_os("OATS_DATA_DIR").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    match result {
        Ok([w, h]) => {
            println!("selftest: wrote {} ({w}×{h})", out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("selftest failed: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "selftest"))]
fn selftest_cli(_args: &[String]) -> ExitCode {
    eprintln!("oats-notes: built without the `selftest` feature");
    ExitCode::FAILURE
}
