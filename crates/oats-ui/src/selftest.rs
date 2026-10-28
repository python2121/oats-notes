//! The offscreen screenshot harness (`oats-notes --selftest out.png`): the whole window built
//! headlessly with `egui_kittest` over wgpu — bundled Inter (deterministic), no native menu, no
//! persistence — at 1100×720 pt and 2× pixels per point, a few frames run, the frame written to a
//! PNG. Seeds the store with a short meeting across yesterday and today first, so the capture
//! shows date headings, wrapping rows, the categories and the status line.

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::Local;
use egui_kittest::Harness;
use oats_core::{PipelineHandle, PipelineStatus, Store};

use crate::app::{AppOptions, AudioStarter, Command, OatsApp};
use crate::transcript::midnight_ms;

/// Window size in points.
pub const SIZE: [f32; 2] = [1100.0, 720.0];
pub const PIXELS_PER_POINT: f32 = 2.0;

/// Capture variants.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelftestOptions {
    /// Show the app in the Listening state with a fake input level (no audio is opened) and the
    /// All Transcripts view, so the pill, the meter and both day headings are in the capture.
    pub listening: bool,
}

/// [`run_with`] with the default options (Today view, not listening).
pub fn run(
    out: &Path,
    store: Store,
    pipeline: PipelineHandle,
    audio: AudioStarter,
) -> Result<[u32; 2], String> {
    run_with(out, store, pipeline, audio, SelftestOptions::default())
}

/// Seed `store`, build the app, render, write `out`. Returns the PNG's pixel size.
pub fn run_with(
    out: &Path,
    store: Store,
    pipeline: PipelineHandle,
    audio: AudioStarter,
    options: SelftestOptions,
) -> Result<[u32; 2], String> {
    seed(&store).map_err(|e| format!("seeding the store: {e}"))?;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(SIZE[0], SIZE[1]))
        .with_pixels_per_point(PIXELS_PER_POINT)
        .wgpu()
        .build_eframe(|cc| {
            OatsApp::new(
                cc,
                AppOptions {
                    native_menu: false,
                    system_font: false,
                    persist: false,
                },
                pipeline,
                audio,
                store,
            )
        });
    harness
        .state_mut()
        .selftest_set_data_dir("selftest database (seeded demo content)".to_owned());
    // let the engine report ready (the null engine loads at once)
    let deadline = Instant::now() + Duration::from_secs(20);
    while !matches!(
        harness.state().status(),
        PipelineStatus::Idle | PipelineStatus::Failed(_)
    ) {
        if Instant::now() > deadline {
            return Err(format!(
                "the engine did not get ready: {:?}",
                harness.state().status()
            ));
        }
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    if options.listening {
        let app = harness.state_mut();
        app.run(Command::ShowAll);
        app.selftest_fake_listening(-22.0, true);
    }
    // settle: fonts, layout, the scroll area's stick-to-bottom
    harness.run_steps(4);
    let image = harness.render()?;
    let (w, h) = image.dimensions();
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    image
        .save(out)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    Ok([w, h])
}

/// A session and eight segments: four yesterday afternoon, four this morning; two filed under
/// Work.
fn seed(store: &Store) -> Result<(), oats_core::StoreError> {
    const H: i64 = 3_600_000;
    const M: i64 = 60_000;
    let today = midnight_ms(&Local::now());
    let yesterday = today - 24 * H;
    let lines: [(i64, &str, Option<f32>); 8] = [
        (
            yesterday + 14 * H + 2 * M,
            "Okay, let's get started. The main thing today is the Q4 roadmap and whether the \
             export rewrite still fits before the holidays.",
            Some(0.94),
        ),
        (
            yesterday + 14 * H + 6 * M,
            "I think it fits if we drop the PDF templates for now.",
            Some(0.91),
        ),
        (
            yesterday + 14 * H + 11 * M,
            "Priya, can you send the updated estimate by Thursday? Then Marco can book the \
             design review for early next week and we can lock the scope.",
            Some(0.88),
        ),
        (
            yesterday + 15 * H + 40 * M,
            "Reminder to renew the parking permit before the end of the month.",
            Some(0.97),
        ),
        (
            today + 9 * H + 15 * M,
            "Morning everyone. Quick stand-up: the sync bug from Friday is fixed and deployed, \
             and the crash reports have dropped to almost nothing since the patch went out.",
            Some(0.93),
        ),
        (
            today + 9 * H + 18 * M,
            "Nice. Anything blocking?",
            Some(0.89),
        ),
        (
            today + 9 * H + 21 * M,
            "Just the vendor contract. Legal wants another pass on the data retention clause \
             before we sign, so that probably slips to next week.",
            Some(0.9),
        ),
        (
            today + 10 * H + 5 * M,
            "Pick up oat milk and coffee beans on the way home.",
            None,
        ),
    ];
    let session = store.begin_session(yesterday + 14 * H, "selftest")?;
    let mut ids = Vec::new();
    for (start, text, confidence) in lines {
        let row = store.insert_segment(session, start, start + 8_000, text, confidence)?;
        ids.push(row.id);
    }
    store.end_session(session, today + 10 * H + 6 * M)?;
    if let Some(work) = store.categories()?.into_iter().find(|c| c.name == "Work") {
        for &i in [0usize, 4].iter() {
            if let Some(&id) = ids.get(i) {
                store.set_segment_category(id, Some(work.id))?;
            }
        }
    }
    Ok(())
}
