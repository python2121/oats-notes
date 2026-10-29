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
#[derive(Clone, Debug, Default)]
pub struct SelftestOptions {
    /// Show the app in the Listening state with a fake input level (no audio is opened) and the
    /// Transcripts view, so the pill, the meter and both day headings are in the capture.
    pub listening: bool,
    /// Show the Conversations view (unfiled seeded episodes and a
    /// change-log line) instead of the transcript.
    pub notes: bool,
    /// Show the Work category's view (its projects, their notes, the transcript toggle).
    pub work: bool,
    /// Show the project landing page of "Export rewrite" (next steps, collapsed notes) with its
    /// sidebar layer open.
    pub project: bool,
    /// With `project`: every note expanded.
    pub expanded: bool,
    /// Show the Calendar view (the month grid with seeded events and todos, today selected).
    pub calendar: bool,
    /// Show the Todos view (open, snoozed, undated and completed todos).
    pub todos: bool,
    /// Seed a todo that is due now and a voice command's confirmation, so the pop-ups show
    /// (over the Todos view unless another view is asked for).
    pub toast: bool,
    /// Start with the sidebar hidden (the control bar then shows the restore button).
    pub no_sidebar: bool,
    /// Open Preferences over seeded apps (Discord, Slack, Zoom, Spotify, Firefox, a game; the
    /// policy at Log only so "transcribe anyway" shows).
    pub prefs: bool,
    /// Open the search bar with this query over the seeded data.
    pub search: Option<String>,
    /// Open the Topic view for this topic over the seeded data.
    pub topic: Option<String>,
}

/// Apps seen playing sound, a couple of classes set, the policy at Log only.
fn seed_apps(store: &Store) -> Result<(), oats_core::StoreError> {
    use oats_core::{MediaPolicy, ProducerClass};
    let now = Local::now().timestamp_millis();
    let apps = [
        ("com.hnc.Discord", "Discord"),
        ("com.tinyspeck.slackmacgap", "Slack"),
        ("us.zoom.xos", "Zoom"),
        ("com.spotify.client", "Spotify"),
        ("org.mozilla.firefox", "Firefox"),
        ("com.example.game", "Moonlight Racer"),
    ];
    for (i, (b, n)) in apps.iter().enumerate() {
        store.note_producers_seen(
            &[((*b).to_owned(), (*n).to_owned())],
            now - i as i64 * 60_000,
        )?;
    }
    store.set_producer_override("com.hnc.discord", ProducerClass::Auto, true)?;
    store.set_producer_override("com.example.game", ProducerClass::Ignore, false)?;
    store.set_meta(MediaPolicy::META_KEY, MediaPolicy::LogOnly.as_str())?;
    Ok(())
}

/// [`run_with`] with the default options (Transcripts view, not listening).
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
    seed(&store, options.toast).map_err(|e| format!("seeding the store: {e}"))?;
    if options.prefs {
        seed_apps(&store).map_err(|e| format!("seeding the apps: {e}"))?;
    }
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
    if options.no_sidebar {
        harness.state_mut().run(Command::ToggleSidebar);
    }
    if options.prefs {
        harness.state_mut().run(Command::Preferences);
    }
    if options.listening {
        let app = harness.state_mut();
        app.run(Command::ShowTranscripts);
        app.selftest_fake_listening(-22.0, true);
    }
    if options.notes {
        harness.state_mut().run(Command::ShowNotes);
    } else if options.work {
        let app = harness.state_mut();
        if !app.selftest_show_node("Work") {
            return Err("no Work category in the seeded store".to_owned());
        }
    } else if options.project {
        let app = harness.state_mut();
        app.selftest_open_project_layer("Export rewrite");
        if !app.selftest_show_node("Export rewrite") {
            return Err("no Export rewrite project in the seeded store".to_owned());
        }
        if options.expanded {
            app.selftest_expand_notes();
        }
    } else if options.calendar {
        harness.state_mut().run(Command::ShowCalendar);
    } else if options.todos || options.toast {
        harness.state_mut().run(Command::ShowTodos);
    }
    if options.toast {
        let ctx = harness.ctx.clone();
        harness.state_mut().selftest_command(
            &ctx,
            vec!["Reminder set: Call the landlord — Sat 31 Oct, 14:05".to_owned()],
        );
    }
    if let Some(q) = &options.search {
        harness.state_mut().selftest_search(q);
    }
    if let Some(q) = &options.topic {
        harness.state_mut().selftest_topic(q);
    }
    // settle: fonts, layout, the scroll area's stick-to-bottom, and the Transcripts view paging
    // in the earlier day (one page per frame while the list is shorter than the window)
    harness.run_steps(10);
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
fn seed(store: &Store, due_now: bool) -> Result<(), oats_core::StoreError> {
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
        use oats_core::{ActionItem, EpisodeContent, NewEvent, NewTodo, NodeKind, Origin};
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
        // an unfiled conversation (the Conversations view lists only these)
        store.insert_episode(
            yesterday + 11 * H,
            yesterday + 11 * H + 9 * M,
            &EpisodeContent {
                title: "Call with the landlord about the lease".into(),
                summary: "- Rent stays flat for another year if the renewal is signed by the 15th.\n- Parking permit moves to the **second-floor garage**.".into(),
                decisions: vec!["Sign the renewal before the 15th".into()],
                topics: vec!["lease".into()],
                model: "claude-sonnet-4-5".into(),
                cost_usd: Some(0.009),
                ..Default::default()
            },
            now,
        )?;
        // a later note of the same project, with more action items (the project page's next steps)
        store.insert_episode(
            yesterday + 17 * H,
            yesterday + 17 * H + 25 * M,
            &EpisodeContent {
                title: "Export rewrite: format check-in".into(),
                summary: "- CSV and Excel exports are ready for review.\n- Dates need an ISO 8601 option before the finance team can use the files.".into(),
                node_id: Some(project.id),
                decisions: vec!["Ship CSV first, Excel a week later".into()],
                action_items: vec![
                    ActionItem { text: "Add an ISO 8601 date option to the exporter".into(), owner: Some("Sam".into()), due: Some("Friday".into()) },
                    ActionItem { text: "Share sample files with finance".into(), owner: None, due: None },
                ],
                topics: vec!["export rewrite".into(), "finance".into()],
                low_signal: false,
                model: "claude-sonnet-4-5".into(),
                cost_usd: Some(0.02),
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
        // todos: the taxes day itself and a week before (a voice command), a reminder in two
        // days, undated ones, a snoozed one, completed ones
        let todo = |text: &str, due: Option<i64>, source: Origin, event: Option<i64>| NewTodo {
            text: text.into(),
            due,
            event_id: event,
            source: Some(source),
            source_segment_id: (source == Origin::Hotword)
                .then(|| ids.get(7).copied())
                .flatten(),
            node_id: None,
            dedupe_key: None,
        };
        store.add_todo(
            &todo(
                "Property taxes due",
                Some(today + 45 * 24 * H + 9 * H),
                Origin::Hotword,
                Some(taxes.id),
            ),
            now,
        )?;
        store.add_todo(
            &todo(
                "Property taxes due in one week",
                Some(today + 38 * 24 * H + 9 * H),
                Origin::Hotword,
                Some(taxes.id),
            ),
            now,
        )?;
        store.add_todo(
            &todo(
                "Call the landlord",
                Some(today + 2 * 24 * H + 14 * H + 5 * M),
                Origin::Hotword,
                None,
            ),
            now,
        )?;
        store.add_todo(
            &NewTodo {
                node_id: Some(project.id),
                ..todo(
                    "Send Priya the export test files",
                    Some(today + 24 * H + 9 * H),
                    Origin::User,
                    None,
                )
            },
            now,
        )?;
        store.add_todo(
            &todo("Buy oat milk and coffee beans", None, Origin::Hotword, None),
            now,
        )?;
        store.add_todo(
            &todo("Renew the parking permit", None, Origin::User, None),
            now,
        )?;
        let snoozed = store.add_todo(
            &todo(
                "Book the design review room",
                Some(today + 8 * H),
                Origin::User,
                None,
            ),
            now,
        )?;
        store.snooze_todo(snoozed.id, Local::now().timestamp_millis() + 3 * H)?;
        for (text, days_ago) in [("Send the Q4 estimate", 1), ("Water the plants", 3)] {
            let done = store.add_todo(
                &todo(
                    text,
                    Some(today - days_ago * 24 * H + 10 * H),
                    Origin::User,
                    None,
                ),
                now,
            )?;
            store.complete_todo(done.id, today - days_ago * 24 * H + 11 * H)?;
        }
        // earlier this month, so the grid has something in it
        for (title, days_ago, hour) in [
            ("Dentist", 20i64, Some(15)),
            ("Quarterly planning", 13, Some(10)),
            ("Mum's birthday", 6, None),
        ] {
            let start = today - days_ago * 24 * H + hour.unwrap_or(0) * H;
            store.upsert_event(
                &NewEvent {
                    when_start: start,
                    all_day: hour.is_none(),
                    title: title.into(),
                    source: Some(Origin::Hotword),
                    source_segment_id: ids.get(3).copied(),
                    confidence: Some(1.0),
                    dedupe_key: Some(format!("selftest:{title}")),
                    ..Default::default()
                },
                now,
            )?;
        }
        if due_now {
            // came due a few minutes ago: the reminder pop-up shows
            let at = Local::now().timestamp_millis() - 5 * M;
            store.add_todo(
                &todo(
                    "Pay the property taxes",
                    Some(at),
                    Origin::Hotword,
                    Some(taxes.id),
                ),
                now,
            )?;
        }
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
