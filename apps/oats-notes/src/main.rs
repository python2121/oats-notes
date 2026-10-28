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
//! microphone (`oats-audio`) wired in here — the UI crate depends on `oats-core` only.
//!
//! Environment: `OATS_ENGINE=null` (no model), `OATS_NO_NATIVE_MENU=1` (menus in the window),
//! `OATS_FONT=bundled` (Inter instead of the system font), `OATS_FONT_GRADE` (SF Pro grade),
//! `OATS_DATA_DIR` (where the database lives), `RUST_LOG`.

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

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

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
                store_path: db,
                segmenter: Default::default(),
                wake: Box::new(move || ctx.request_repaint()),
            }
            .spawn();
            let audio: oats_ui::AudioStarter = Box::new(|tx| {
                oats_audio::Capture::start(tx)
                    .map(|c| Box::new(c) as Box<dyn Any>)
                    .map_err(|e| e.to_string())
            });
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
                },
                pipeline,
                audio,
                store,
            );
            #[cfg(target_os = "macos")]
            if native_menu {
                return Ok(
                    match oats_ui::native_menu::NativeMenuApp::new(app, &cc.egui_ctx) {
                        Ok(wrapped) => Box::new(wrapped) as Box<dyn eframe::App>,
                        Err(app) => app,
                    },
                );
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
