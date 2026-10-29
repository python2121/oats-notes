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
    /// Show the Notes view (a seeded interpreted episode with a project, action items and a
    /// change-log line) instead of the transcript.
    pub notes: bool,
    /// Show the Work category's view (its projects, their notes, the transcript toggle).
    pub work: bool,
    /// Show the Calendar view (seeded events and a reminder).
    pub calendar: bool,
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
                    start_listening: false,
                },
                pipeline,
                audio,
                store,
                None,
                None,
            )
        });
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
    if options.notes {
        harness.state_mut().run(Command::ShowNotes);
    } else if options.work {
        let app = harness.state_mut();
        if !app.selftest_show_node("Work") {
            return Err("no Work category in the seeded store".to_owned());
        }
    } else if options.calendar {
        harness.state_mut().run(Command::ShowCalendar);
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

/// A session and eight segments: four yesterday afternoon, four this morning (two of them
/// system audio from a call, plus a hidden microphone echo); two filed under Work.
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
    // this morning's stand-up was a call: the other side (lines 4 and 6) came in as system
    // audio ("them"); the microphone's echo of line 6 is stored but never shown
    let them = [4usize, 6];
    for (i, (start, text, confidence)) in lines.into_iter().enumerate() {
        let source = if them.contains(&i) {
            oats_core::AudioSourceKind::System
        } else {
            oats_core::AudioSourceKind::Mic
        };
        let row = store.insert_segment_from(
            session,
            start,
            start + 8_000,
            text,
            confidence,
            source,
            None,
        )?;
        if i == 6 {
            store.insert_segment_from(
                session,
                start + 300,
                start + 8_400,
                text,
                Some(0.7),
                oats_core::AudioSourceKind::Mic,
                Some(row.id),
            )?;
        }
        ids.push(row.id);
    }
    store.end_session(session, today + 10 * H + 6 * M)?;
    if let Some(work) = store.categories()?.into_iter().find(|c| c.name == "Work") {
        for &i in [0usize, 4].iter() {
            if let Some(&id) = ids.get(i) {
                store.set_segment_node(id, Some(work.id))?;
            }
        }
        // what the brain would have made of yesterday's meeting: a project under Work, the
        // episode filed under it, an event it found, a reminder from a voice command
        use oats_core::{ActionItem, EpisodeContent, NewEvent, NewReminder, NodeKind, Origin};
        let project = store.add_node(
            Some(work.id),
            NodeKind::Project,
            "Export rewrite",
            "the Q4 export rewrite (CSV/Excel first, PDF later)",
            Origin::Auto,
        )?;
        let now = today + 10 * H + 7 * M;
        let episode = store.insert_episode(
            yesterday + 14 * H + 2 * M,
            yesterday + 14 * H + 12 * M,
            &EpisodeContent {
                title: "Q4 roadmap and the export rewrite".into(),
                summary: "- The export rewrite still fits before the holidays if the **PDF templates** are dropped for now.\n- Priya sends the updated estimate by Thursday; Marco books the design review for early next week.\n- Scope is locked after the review.".into(),
                node_id: Some(project.id),
                decisions: vec!["Drop the PDF templates from the Q4 scope".into()],
                action_items: vec![
                    ActionItem { text: "Send the updated estimate".into(), owner: Some("Priya".into()), due: Some("Thursday".into()) },
                    ActionItem { text: "Book the design review".into(), owner: Some("Marco".into()), due: Some("early next week".into()) },
                ],
                topics: vec!["export rewrite".into(), "Q4 roadmap".into()],
                low_signal: false,
                model: "claude-sonnet-4-5".into(),
                cost_usd: Some(0.031),
            },
            now,
        )?;
        store.mark_segments_interpreted(&ids[..3], episode.id, now)?;
        store.insert_episode(
            yesterday + 15 * H + 40 * M,
            yesterday + 15 * H + 41 * M,
            &EpisodeContent {
                title: "Television in the background".into(),
                summary: "A few lines of a news programme.".into(),
                low_signal: true,
                model: "claude-sonnet-4-5".into(),
                cost_usd: Some(0.004),
                ..Default::default()
            },
            now,
        )?;
        let vendor = store.add_node(
            Some(work.id),
            NodeKind::Project,
            "Vendor contract",
            "legal review of the data retention clause",
            Origin::Auto,
        )?;
        store.insert_episode(
            today + 9 * H + 15 * M,
            today + 9 * H + 22 * M,
            &EpisodeContent {
                title: "Stand-up: sync fix shipped, vendor contract slips".into(),
                summary: "- The Friday sync bug is fixed and deployed; crash reports are near zero.\n- Legal wants another pass on the **data retention clause**, so signing slips to next week.".into(),
                node_id: Some(vendor.id),
                decisions: vec!["Hold the signature until legal clears the retention clause".into()],
                topics: vec!["data retention".into()],
                model: "claude-sonnet-4-5".into(),
                cost_usd: Some(0.012),
                ..Default::default()
            },
            now,
        )?;
        store.upsert_event(
            &NewEvent {
                when_start: today + 2 * 24 * H + 10 * H,
                all_day: false,
                title: "Design review (export rewrite)".into(),
                details: "Marco to book; scope locks afterwards".into(),
                node_id: Some(project.id),
                source: Some(Origin::Interpretation),
                source_episode_id: Some(episode.id),
                confidence: Some(0.8),
                dedupe_key: Some("selftest:review".into()),
                ..Default::default()
            },
            now,
        )?;
        let taxes = store.upsert_event(
            &NewEvent {
                when_start: today + 45 * 24 * H,
                all_day: true,
                title: "Property taxes due".into(),
                source: Some(Origin::Hotword),
                source_segment_id: ids.get(7).copied(),
                confidence: Some(1.0),
                dedupe_key: Some("selftest:taxes".into()),
                ..Default::default()
            },
            now,
        )?;
        store.upsert_reminder(
            &NewReminder {
                event_id: Some(taxes.id),
                due: today + 38 * 24 * H,
                text: "Property taxes due in one week".into(),
                source: Some(Origin::Hotword),
                source_segment_id: ids.get(7).copied(),
                dedupe_key: Some("selftest:taxes-reminder".into()),
            },
            now,
        )?;
        let run = store.begin_run(1, "interpret", "sonnet", now - 4_000)?;
        store.finish_run(
            run,
            &oats_core::RunOutcome {
                finished_at: now,
                model: Some("claude-sonnet-4-5".into()),
                cost_usd: Some(0.035),
                duration_ms: Some(4_100),
                ok: true,
                error: None,
                summary: "2 of 2 chunks".into(),
            },
        )?;
        store.record_change(
            Some(run),
            "node_create",
            "Created project 'Export rewrite'",
            &serde_json::Value::Null,
            &serde_json::json!({"id": project.id}),
            now,
        )?;
    }
    Ok(())
}
