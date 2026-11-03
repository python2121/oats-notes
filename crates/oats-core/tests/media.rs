//! The media policy end to end: a fake media context in the pipeline's feed, system audio
//! on the lane, and what lands in the store under Ignore / LogOnly / Transcribe.

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oats_core::pipeline::ListenOptions;
use oats_core::{
    AudioChunk, AudioSourceKind, EngineError, MediaClass, MediaPolicy, MediaStamp, Pipeline,
    PipelineEvent, PipelineHandle, PipelineStatus, SAMPLE_RATE, SegmenterConfig, SpeechEngine,
    Store, Transcription, VadFrame,
};

const TIMEOUT: Duration = Duration::from_secs(10);
const URL: &str = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";

/// VAD on the peak (per lane, recorded); transcription "speech <n> samples".
struct Engine(Arc<Mutex<Vec<AudioSourceKind>>>);

impl SpeechEngine for Engine {
    fn name(&self) -> String {
        "media-test".into()
    }
    fn load(&mut self, _status: &mut dyn FnMut(String)) -> Result<(), EngineError> {
        Ok(())
    }
    fn vad_frame_len(&self) -> Option<usize> {
        Some(1_600)
    }
    fn vad(&mut self, _samples: &[f32]) -> Result<Vec<VadFrame>, EngineError> {
        Err(EngineError::Vad("only vad_lane is used".into()))
    }
    fn vad_lane(
        &mut self,
        lane: AudioSourceKind,
        samples: &[f32],
    ) -> Result<Vec<VadFrame>, EngineError> {
        self.0.lock().unwrap().push(lane);
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        Ok(vec![VadFrame {
            probability: peak.min(1.0),
            is_speech: peak > 0.1,
        }])
    }
    fn transcribe(&mut self, samples: &[f32]) -> Result<Transcription, EngineError> {
        Ok(Transcription {
            text: format!("speech {} samples", samples.len()),
            confidence: Some(0.9),
            processing_ms: 0.0,
        })
    }
}

struct Rig {
    h: PipelineHandle,
    db: PathBuf,
    dir: PathBuf,
    vad_calls: Arc<Mutex<Vec<AudioSourceKind>>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(tag: &str, policy: MediaPolicy) -> Rig {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "oats-media-it-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("oats.sqlite");
    // the policy lives in the database's meta; the session reads it when it starts
    Store::open(&db)
        .unwrap()
        .set_meta(MediaPolicy::META_KEY, policy.as_str())
        .unwrap();
    let vad_calls = Arc::new(Mutex::new(Vec::new()));
    let calls = vad_calls.clone();
    let h = Pipeline {
        engine: Box::new(move || Ok(Box::new(Engine(calls)) as Box<dyn SpeechEngine>)),
        store_path: db.clone(),
        segmenter: SegmenterConfig::default(),
        wake: Box::new(|| {}),
    }
    .spawn();
    Rig {
        h,
        db,
        dir,
        vad_calls,
    }
}

fn wait_for(
    h: &PipelineHandle,
    what: &str,
    pred: impl Fn(&PipelineEvent) -> bool,
) -> Vec<PipelineEvent> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match h.recv_timeout(left) {
            Some(ev) => {
                let hit = pred(&ev);
                seen.push(ev);
                if hit {
                    return seen;
                }
            }
            None => break,
        }
    }
    panic!("timed out waiting for {what}; saw {seen:?}");
}

/// `ms` of `amp` on one lane, in 100 ms chunks, the microphone kept alive (silent) alongside.
fn send_system(tx: &Sender<AudioChunk>, amp: f32, ms: usize) {
    for _ in 0..ms / 100 {
        for (source, a) in [(AudioSourceKind::Mic, 0.0), (AudioSourceKind::System, amp)] {
            tx.send(AudioChunk {
                source,
                samples: vec![a; SAMPLE_RATE as usize / 10],
            })
            .unwrap();
        }
    }
}

fn stamp(class: MediaClass, url: &str, title: &str) -> MediaStamp {
    MediaStamp::new(class, Some(url.into()), title, "Firefox")
}

/// Discord as the resolver stamps it by default (a media app, no URL).
fn discord(transcribe_anyway: bool) -> MediaStamp {
    let mut s = MediaStamp::new(MediaClass::App, None, "Discord", "Discord");
    s.bundle_id = "com.hnc.discord".into();
    s.policy = transcribe_anyway.then_some(MediaPolicy::Transcribe);
    s
}

fn plays(db: &std::path::Path) -> Vec<oats_core::MediaPlay> {
    Store::open(db)
        .unwrap()
        .media_plays_between(0, i64::MAX)
        .unwrap()
}

#[test]
fn discord_under_log_only_is_logged_not_transcribed() {
    let rig = rig("discord-log", MediaPolicy::LogOnly);
    play(&rig, Some(discord(false)));
    assert!(system_rows(&rig.db).is_empty(), "nothing transcribed");
    let p = plays(&rig.db);
    assert_eq!(p.len(), 1, "{p:?}");
    assert_eq!(p[0].kind, MediaClass::App);
    assert_eq!(p[0].producer, "Discord");
    assert_eq!(p[0].url, None);
}

#[test]
fn discord_with_transcribe_anyway_is_transcribed_as_media_under_log_only() {
    let rig = rig("discord-anyway", MediaPolicy::LogOnly);
    play(&rig, Some(discord(true)));
    let rows = system_rows(&rig.db);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].media_kind, Some(MediaClass::App), "filed as media");
    let p = plays(&rig.db);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].segments_count, 1);
    // the app recorded on the segment (what "Mark as media" looks at)
    let s = Store::open(&rig.db).unwrap();
    let ep = s
        .insert_episode(0, i64::MAX / 2, &oats_core::EpisodeContent::default(), 0)
        .unwrap()
        .id;
    s.mark_segments_interpreted(&[rows[0].id], ep, 0).unwrap();
    assert_eq!(
        s.episode_media_apps(&[ep]).unwrap().get(&ep).cloned(),
        Some(vec!["com.hnc.discord".to_string()])
    );
}

#[test]
fn an_app_classed_ignore_is_skipped_even_under_transcribe() {
    let rig = rig("discord-ignore", MediaPolicy::Transcribe);
    let mut s = discord(false);
    s.policy = Some(MediaPolicy::Ignore);
    play(&rig, Some(s));
    assert!(system_rows(&rig.db).is_empty());
    assert!(plays(&rig.db).is_empty());
}

#[test]
fn on_a_call_discord_is_transcribed_as_them_whatever_the_policy() {
    let rig = rig("discord-call", MediaPolicy::Ignore);
    rig.h.set_on_call(true);
    play(&rig, Some(discord(false)));
    let rows = system_rows(&rig.db);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].media_kind, Some(MediaClass::Meeting));
    assert!(plays(&rig.db).is_empty(), "a call is no media play");
    // the session ended: the switch is off again
    assert!(!rig.h.on_call());
}

#[test]
fn on_a_call_ends_after_silence_on_the_system_lane() {
    let rig = rig("call-quiet", MediaPolicy::LogOnly);
    wait_for(&rig.h, "idle", |e| {
        matches!(e, PipelineEvent::Status(PipelineStatus::Idle))
    });
    rig.h.set_on_call_timeout(Duration::from_secs(5));
    rig.h.set_on_call(true);
    let (tx, rx) = channel();
    rig.h.start_lanes(rx, ListenOptions::default());
    send_system(&tx, 0.3, 2_000);
    send_system(&tx, 0.0, 7_000);
    wait_for(&rig.h, "on a call to end", |e| {
        matches!(
            e,
            PipelineEvent::OnCallEnded {
                after_silence: true
            }
        )
    });
    assert!(!rig.h.on_call());
    drop(tx);
}

/// Play 4 s of "speech" on the system lane while `stamp` is in the feed, then stop.
fn play(rig: &Rig, stamp: Option<MediaStamp>) -> Vec<PipelineEvent> {
    wait_for(&rig.h, "idle", |e| {
        matches!(e, PipelineEvent::Status(PipelineStatus::Idle))
    });
    rig.h.media_feed().set(stamp);
    let (tx, rx) = channel();
    rig.h.start_lanes(
        rx,
        ListenOptions {
            system: true,
            ..ListenOptions::default()
        },
    );
    wait_for(&rig.h, "session", |e| {
        matches!(e, PipelineEvent::SessionStarted(_))
    });
    send_system(&tx, 0.3, 4_000);
    send_system(&tx, 0.0, 1_000);
    // closing the channel (not `stop`) ends the session after every queued chunk
    drop(tx);
    wait_for(&rig.h, "idle", |e| {
        matches!(e, PipelineEvent::Status(PipelineStatus::Idle))
    })
}

fn system_rows(db: &std::path::Path) -> Vec<oats_core::TranscriptRow> {
    Store::open(db)
        .unwrap()
        .segments_between(0, i64::MAX)
        .unwrap()
        .into_iter()
        .filter(|r| r.source == AudioSourceKind::System)
        .collect()
}

#[test]
fn ignore_skips_vad_and_transcription_and_logs_nothing() {
    let rig = rig("ignore", MediaPolicy::Ignore);
    play(
        &rig,
        Some(stamp(MediaClass::YouTube, URL, "Never Gonna Give You Up")),
    );
    assert_eq!(rig.h.media_policy(), MediaPolicy::Ignore);
    assert!(system_rows(&rig.db).is_empty(), "nothing transcribed");
    assert!(
        !rig.vad_calls
            .lock()
            .unwrap()
            .contains(&AudioSourceKind::System),
        "no VAD on the system lane while media plays"
    );
    let plays = Store::open(&rig.db)
        .unwrap()
        .media_plays_between(0, i64::MAX)
        .unwrap();
    assert!(plays.is_empty());
}

#[test]
fn log_only_writes_a_play_row_and_no_segments() {
    let rig = rig("log", MediaPolicy::LogOnly);
    play(
        &rig,
        Some(stamp(MediaClass::YouTube, URL, "Never Gonna Give You Up")),
    );
    assert!(system_rows(&rig.db).is_empty());
    let plays = Store::open(&rig.db)
        .unwrap()
        .media_plays_between(0, i64::MAX)
        .unwrap();
    assert_eq!(plays.len(), 1, "{plays:?}");
    let p = &plays[0];
    assert_eq!(p.kind, MediaClass::YouTube);
    assert_eq!(p.url.as_deref(), Some(URL));
    assert_eq!(p.title, "Never Gonna Give You Up");
    assert_eq!(p.producer, "Firefox");
    assert_eq!(p.segments_count, 0);
    let len = p.ended_at - p.started_at;
    assert!((3_500..=4_500).contains(&len), "play length {len} ms");
}

#[test]
fn transcribe_stamps_segments_and_counts_them_in_the_play() {
    let rig = rig("transcribe", MediaPolicy::Transcribe);
    let seen = play(
        &rig,
        Some(stamp(MediaClass::YouTube, URL, "Never Gonna Give You Up")),
    );
    let rows = system_rows(&rig.db);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].media_kind, Some(MediaClass::YouTube));
    assert_eq!(rows[0].media_ref.as_deref(), Some(URL));
    // the reported row carries the stamp too
    assert!(seen.iter().any(
        |e| matches!(e, PipelineEvent::Segment(r) if r.media_kind == Some(MediaClass::YouTube))
    ));
    let plays = Store::open(&rig.db)
        .unwrap()
        .media_plays_between(0, i64::MAX)
        .unwrap();
    assert_eq!(plays.len(), 1);
    assert_eq!(plays[0].segments_count, 1);
}

#[test]
fn a_meeting_is_never_media_even_under_ignore() {
    let rig = rig("meeting", MediaPolicy::Ignore);
    play(
        &rig,
        Some(stamp(
            MediaClass::Meeting,
            "https://meet.google.com/abc-defg-hij",
            "Meet",
        )),
    );
    let rows = system_rows(&rig.db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].media_kind, Some(MediaClass::Meeting));
    assert!(
        Store::open(&rig.db)
            .unwrap()
            .media_plays_between(0, i64::MAX)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn nothing_known_leaves_system_segments_unstamped() {
    let rig = rig("none", MediaPolicy::LogOnly);
    play(&rig, None);
    let rows = system_rows(&rig.db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].media_kind, None);
    assert_eq!(rows[0].media_ref, None);
}

#[test]
fn a_long_pause_closes_the_play_and_a_resume_opens_another() {
    let rig = rig("pause", MediaPolicy::LogOnly);
    wait_for(&rig.h, "idle", |e| {
        matches!(e, PipelineEvent::Status(PipelineStatus::Idle))
    });
    rig.h.media_feed().set(Some(stamp(
        MediaClass::Music,
        "https://open.spotify.com/track/1",
        "A song",
    )));
    let (tx, rx) = channel();
    rig.h.start_lanes(rx, ListenOptions::default());
    send_system(&tx, 0.3, 4_000);
    send_system(&tx, 0.0, 60_000); // a short pause: the same play
    send_system(&tx, 0.3, 4_000);
    send_system(&tx, 0.0, 130_000); // a long one: closed
    send_system(&tx, 0.3, 4_000);
    drop(tx);
    wait_for(&rig.h, "idle", |e| {
        matches!(e, PipelineEvent::SessionEnded(_))
    });
    let plays = Store::open(&rig.db)
        .unwrap()
        .media_plays_between(0, i64::MAX)
        .unwrap();
    assert_eq!(plays.len(), 2, "{plays:?}");
    let first = plays[0].ended_at - plays[0].started_at;
    assert!((67_000..=69_000).contains(&first), "first play {first} ms");
    assert!(plays[1].started_at - plays[0].ended_at >= 130_000);
}

#[test]
fn music_is_logged_never_transcribed_even_under_transcribe() {
    let rig = rig("music", MediaPolicy::Transcribe);
    play(
        &rig,
        Some(stamp(
            MediaClass::Music,
            "https://open.spotify.com/playlist/p",
            "A song — An artist",
        )),
    );
    assert!(
        system_rows(&rig.db).is_empty(),
        "music is never transcribed"
    );
    assert!(
        !rig.vad_calls
            .lock()
            .unwrap()
            .contains(&AudioSourceKind::System),
        "no VAD while music plays"
    );
    let p = plays(&rig.db);
    assert_eq!(p.len(), 1, "{p:?}");
    assert_eq!(p[0].kind, MediaClass::Music);
    assert_eq!(p[0].title, "A song — An artist");
}

#[test]
fn a_music_app_transcribed_anyway_is_transcribed() {
    let rig = rig("music-anyway", MediaPolicy::LogOnly);
    let mut s = MediaStamp::new(MediaClass::Music, None, "Spotify", "Spotify");
    s.bundle_id = "com.spotify.client".into();
    s.policy = Some(MediaPolicy::Transcribe);
    play(&rig, Some(s));
    assert_eq!(system_rows(&rig.db).len(), 1);
}

#[test]
fn a_new_track_on_the_same_page_closes_the_play_and_opens_another() {
    let rig = rig("tracks", MediaPolicy::LogOnly);
    wait_for(&rig.h, "idle", |e| {
        matches!(e, PipelineEvent::Status(PipelineStatus::Idle))
    });
    let page = "https://open.spotify.com/playlist/p";
    rig.h
        .media_feed()
        .set(Some(stamp(MediaClass::Music, page, "One — X")));
    let (tx, rx) = channel();
    rig.h.start_lanes(rx, ListenOptions::default());
    send_system(&tx, 0.3, 5_000);
    // the first track's play is open once its chunks were fed
    let deadline = Instant::now() + TIMEOUT;
    while plays(&rig.db).is_empty() {
        assert!(Instant::now() < deadline, "no play opened");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));
    rig.h
        .media_feed()
        .set(Some(stamp(MediaClass::Music, page, "Two — Y")));
    send_system(&tx, 0.3, 5_000);
    drop(tx);
    wait_for(&rig.h, "session end", |e| {
        matches!(e, PipelineEvent::SessionEnded(_))
    });
    let p = plays(&rig.db);
    assert_eq!(
        p.iter().map(|p| p.title.as_str()).collect::<Vec<_>>(),
        ["One — X", "Two — Y"],
        "{p:?}"
    );
    assert!(p.iter().all(|p| p.url.as_deref() == Some(page)));
}
