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
//! `scripts/perf.sh`).
//!
//! Environment: `OATS_ENGINE=null` (no model), `OATS_NO_NATIVE_MENU=1` (menus in the window),
//! `OATS_NO_STATUS_ITEM=1` (no menu bar extra; closing the window then quits),
//! `OATS_FONT=bundled` (Inter instead of the system font), `OATS_FONT_GRADE` (SF Pro grade),
//! `OATS_DATA_DIR` (where the database lives), `RUST_LOG`; the brain's `OATS_BRAIN=off`,
//! `OATS_CLAUDE_BIN`, `OATS_CLAUDE_MODEL`, `OATS_INTERPRET_EVERY_MIN`, `OATS_BRAIN_BUDGET_USD`,
//! `OATS_BRAIN_TIER0_PER_HOUR`, `OATS_CLAUDE_PASS_PROXY`, `OATS_HOTWORDS` (see
//! `oats_brain::BrainConfig::from_env`).
//!
//! `--selftest <out.png> [--listening|--notes|--calendar]` renders the whole window offscreen to a PNG (seeded
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
        "usage: oats-notes [--listen] [--audio-file <wav> [--audio-speed N] [--audio-loop]]\n       oats-notes --selftest <out.png> [--listening|--notes|--calendar]\n\n\
         --listen       start listening as soon as the speech engine is ready\n\
         --audio-file   listen to a WAV file (PCM 16-bit mono 16 kHz) instead of the microphone,\n\
         \x20              paced in real time; listening stops when the file ends\n\
         --audio-speed  with --audio-file: playback rate relative to real time (default 1)\n\
         --audio-loop   with --audio-file: start the file again at its end\n\
         --selftest     render the window offscreen to a PNG (demo content, no microphone)\n\
         --listening    with --selftest: show the Listening state with a fake input level\n\
         --notes        with --selftest: the Notes view (a seeded interpreted episode)\n\
         --calendar     with --selftest: the Calendar view (seeded events and a reminder)"
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
    let mut file_options = oats_audio::FileSourceOptions::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--listen" => start_listening = true,
            "--audio-file" => match it.next() {
                Some(p) => audio_file = Some(p.into()),
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
    // decoded once up front (a bad file fails here, not at the first Listen)
    let audio_samples: Option<std::sync::Arc<[f32]>> = match &audio_file {
        Some(path) => match oats_audio::file_source::load_wav(path) {
            Ok(s) => Some(s.into()),
            Err(e) => {
                eprintln!("oatsnotes: --audio-file: {e}");
                return ExitCode::from(2);
            }
        },
        None => None,
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
            let audio: oats_ui::AudioStarter = match audio_samples {
                // --audio-file: the WAV instead of the microphone, from the start each time
                Some(samples) => Box::new(move |tx| {
                    oats_audio::FileSource::start(samples.clone(), file_options, tx)
                        .map(|s| Box::new(s) as Box<dyn Any>)
                        .map_err(|e| e.to_string())
                }),
                None => Box::new(|tx| {
                    oats_audio::Capture::start(tx)
                        .map(|c| Box::new(c) as Box<dyn Any>)
                        .map_err(|e| e.to_string())
                }),
            };
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
    for a in args {
        match a.as_str() {
            "--listening" => options.listening = true,
            "--notes" => options.notes = true,
            "--calendar" => options.calendar = true,
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
        Box::new(|_tx| Err("the selftest never opens the microphone".to_owned()));
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
