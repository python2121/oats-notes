//! Integration tests for the listening pipeline: a scripted engine (deterministic VAD on the
//! sample values, transcription keyed on them) and the `NullEngine`, driven over a real audio
//! channel into a real store file.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use oats_core::{
    EngineError, NullEngine, Pipeline, PipelineEvent, PipelineHandle, PipelineStatus, SAMPLE_RATE,
    SegmenterConfig, SpeechEngine, Store, Transcription, VadFrame, View,
};

const TIMEOUT: Duration = Duration::from_secs(10);
const FRAME: usize = 512;
/// Amplitude the scripted engine hears as speech it can transcribe.
const SPEECH: f32 = 0.3;
/// Amplitude it hears as speech but transcribes to nothing (a mumble).
const MUMBLE: f32 = 0.5;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "oats-pipeline-it-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("oats.sqlite")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the scripted engine does wrong, if anything.
#[derive(Clone, Copy, Default)]
struct Faults {
    vad_fails: bool,
    transcribe_fails: bool,
}

/// VAD: a frame is speech when its peak exceeds 0.1. Transcription: empty for a segment with
/// the [`MUMBLE`] amplitude in it, else "speech <n> samples".
struct Scripted(Faults);

impl SpeechEngine for Scripted {
    fn name(&self) -> String {
        "scripted".into()
    }
    fn load(&mut self, status: &mut dyn FnMut(String)) -> Result<(), EngineError> {
        status("loading scripted".into());
        Ok(())
    }
    fn vad_frame_len(&self) -> Option<usize> {
        Some(FRAME)
    }
    fn vad(&mut self, samples: &[f32]) -> Result<Vec<VadFrame>, EngineError> {
        if self.0.vad_fails {
            return Err(EngineError::Vad("scripted VAD failure".into()));
        }
        Ok(samples
            .chunks(FRAME)
            .map(|f| {
                let peak = f.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                VadFrame {
                    probability: peak.min(1.0),
                    is_speech: peak > 0.1,
                }
            })
            .collect())
    }
    fn transcribe(&mut self, samples: &[f32]) -> Result<Transcription, EngineError> {
        if self.0.transcribe_fails {
            return Err(EngineError::Transcribe("scripted failure".into()));
        }
        let mumble = samples.iter().any(|s| (*s - MUMBLE).abs() < 1e-6);
        Ok(Transcription {
            text: if mumble {
                "   ".into()
            } else {
                format!("  speech {} samples  ", samples.len())
            },
            confidence: Some(0.8),
            processing_ms: 1.0,
        })
    }
}

struct Rig {
    h: PipelineHandle,
    wakes: Arc<AtomicUsize>,
    _dir: TempDir,
    db: PathBuf,
}

fn spawn_with(tag: &str, engine: oats_core::EngineFactory) -> Rig {
    let dir = TempDir::new(tag);
    let db = dir.db();
    let wakes = Arc::new(AtomicUsize::new(0));
    let w = wakes.clone();
    let h = Pipeline {
        engine,
        store_path: db.clone(),
        segmenter: SegmenterConfig::default(),
        wake: Box::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        }),
    }
    .spawn();
    Rig {
        h,
        wakes,
        _dir: dir,
        db,
    }
}

fn scripted(tag: &str, faults: Faults) -> Rig {
    spawn_with(
        tag,
        Box::new(move || Ok(Box::new(Scripted(faults)) as Box<dyn SpeechEngine>)),
    )
}

fn null(tag: &str) -> Rig {
    spawn_with(
        tag,
        Box::new(|| Ok(Box::new(NullEngine) as Box<dyn SpeechEngine>)),
    )
}

/// Wait for the first event matching `pred`; every event seen on the way is appended to
/// `seen`.
fn wait_for(
    h: &PipelineHandle,
    seen: &mut Vec<PipelineEvent>,
    what: &str,
    pred: impl Fn(&PipelineEvent) -> bool,
) -> PipelineEvent {
    let deadline = Instant::now() + TIMEOUT;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match h.recv_timeout(left) {
            Some(ev) => {
                seen.push(ev.clone());
                if pred(&ev) {
                    return ev;
                }
            }
            None => break,
        }
    }
    panic!("timed out waiting for {what}; saw {seen:?}");
}

fn wait_idle(h: &PipelineHandle, seen: &mut Vec<PipelineEvent>) {
    wait_for(h, seen, "idle", |e| {
        matches!(e, PipelineEvent::Status(PipelineStatus::Idle))
    });
}

fn session_started(e: &PipelineEvent) -> Option<i64> {
    match e {
        PipelineEvent::SessionStarted(id) => Some(*id),
        _ => None,
    }
}

fn start(h: &PipelineHandle, seen: &mut Vec<PipelineEvent>) -> (Sender<Vec<f32>>, i64) {
    let (tx, rx) = channel();
    h.start(rx);
    let ev = wait_for(h, seen, "session start", |e| session_started(e).is_some());
    (tx, session_started(&ev).unwrap())
}

/// `ms` of constant `amp`, in 100 ms chunks.
fn send(tx: &Sender<Vec<f32>>, amp: f32, ms: usize) {
    let mut left = ms * SAMPLE_RATE as usize / 1000;
    while left > 0 {
        let n = left.min(1_600);
        tx.send(vec![amp; n]).unwrap();
        left -= n;
    }
}

/// `(started_at, ended_at)` of a session, straight from the file.
fn session_row(db: &PathBuf, id: i64) -> (i64, Option<i64>) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT started_at, ended_at FROM sessions WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

fn segments(e: &[PipelineEvent]) -> Vec<&oats_core::TranscriptRow> {
    e.iter()
        .filter_map(|e| match e {
            PipelineEvent::Segment(r) => Some(r),
            _ => None,
        })
        .collect()
}

#[test]
fn startup_reports_loading_engine_and_idle_in_order() {
    let rig = scripted("startup", Faults::default());
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    assert_eq!(
        seen,
        vec![
            PipelineEvent::Status(PipelineStatus::Loading("Loading speech engine…".into())),
            PipelineEvent::Status(PipelineStatus::Loading("loading scripted".into())),
            PipelineEvent::Engine("scripted".into()),
            PipelineEvent::Status(PipelineStatus::Idle),
        ]
    );
    // `wake` runs right after each event is queued: one per event
    let deadline = Instant::now() + TIMEOUT;
    while rig.wakes.load(Ordering::SeqCst) < seen.len() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        rig.wakes.load(Ordering::SeqCst),
        seen.len(),
        "one wake per event"
    );
    // stop while idle is a no-op
    rig.h.stop();
    assert!(rig.h.recv_timeout(Duration::from_millis(200)).is_none());
}

#[test]
fn level_events_are_rate_limited_to_about_4_hz() {
    let rig = null("levels");
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, _) = start(&rig.h, &mut seen);
    seen.clear();
    // 1 s of audio in 10 ms chunks, delivered at about real time
    let t0 = Instant::now();
    for i in 0..100 {
        tx.send(vec![if i % 2 == 0 { 0.2 } else { 0.0 }; 160])
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
    // and another 2 s delivered all at once (the worker catches up as fast as it can)
    for _ in 0..200 {
        tx.send(vec![0.0; 160]).unwrap();
    }
    rig.h.stop();
    wait_for(&rig.h, &mut seen, "session end", |e| {
        matches!(e, PipelineEvent::SessionEnded(_))
    });
    let elapsed = t0.elapsed().as_millis() as usize;
    let levels = seen
        .iter()
        .filter(|e| matches!(e, PipelineEvent::Level { .. }))
        .count();
    assert!(levels >= 2, "{levels} level events in {elapsed} ms");
    assert!(
        levels <= elapsed / 250 + 2,
        "{levels} level events in {elapsed} ms (≤ ~4 Hz expected)"
    );
    for e in &seen {
        if let PipelineEvent::Level {
            rms_db_mic: rms_db,
            rms_db_system,
            ..
        } = e
        {
            assert!((-100.0..=0.0).contains(rms_db_system), "{rms_db_system}");
            assert!((-100.0..=0.0).contains(rms_db), "{rms_db}");
        }
    }
}

#[test]
fn no_level_events_while_levels_are_unwanted_but_segments_still_arrive() {
    let rig = scripted("nolevels", Faults::default());
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    rig.h.set_levels_wanted(false);
    let (tx, _) = start(&rig.h, &mut seen);
    seen.clear();
    send(&tx, SPEECH, 1_000);
    send(&tx, 0.0, 1_500);
    wait_for(&rig.h, &mut seen, "segment", |e| {
        matches!(e, PipelineEvent::Segment(_))
    });
    rig.h.stop();
    wait_for(&rig.h, &mut seen, "session end", |e| {
        matches!(e, PipelineEvent::SessionEnded(_))
    });
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, PipelineEvent::Level { .. })),
        "{seen:?}"
    );
}

#[test]
fn listen_while_listening_flushes_and_closes_the_previous_session() {
    let rig = scripted("relisten", Faults::default());
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx1, s1) = start(&rig.h, &mut seen);
    // a second of speech with no trailing silence: the segment is still open
    send(&tx1, SPEECH, 1_000);
    wait_for(&rig.h, &mut seen, "in speech", |e| {
        matches!(
            e,
            PipelineEvent::Level {
                in_speech_mic: true,
                ..
            }
        )
    });
    // let the worker drain the channel (it runs far faster than real time)
    std::thread::sleep(Duration::from_millis(250));
    seen.clear();
    let (tx2, s2) = start(&rig.h, &mut seen);
    assert_ne!(s1, s2);
    let order: Vec<&PipelineEvent> = seen
        .iter()
        .filter(|e| !matches!(e, PipelineEvent::Level { .. }))
        .collect();
    assert!(
        matches!(
            order.as_slice(),
            [
                PipelineEvent::Segment(row),
                PipelineEvent::SessionEnded(ended),
                PipelineEvent::Status(PipelineStatus::Idle),
                PipelineEvent::SessionStarted(started),
            ] if row.session_id == s1 && *ended == s1 && *started == s2
        ),
        "{order:?}"
    );
    let (_, ended_at) = session_row(&rig.db, s1);
    assert!(ended_at.is_some(), "the first session is closed");
    assert_eq!(session_row(&rig.db, s2).1, None, "the second is open");
    // the new session works and the old channel is no longer read
    tx1.send(vec![SPEECH; 16_000]).ok();
    send(&tx2, SPEECH, 1_000);
    send(&tx2, 0.0, 1_000);
    wait_for(
        &rig.h,
        &mut seen,
        "second segment",
        |e| matches!(e, PipelineEvent::Segment(r) if r.session_id == s2),
    );
    drop(tx2);
    wait_for(&rig.h, &mut seen, "session end", |e| {
        *e == PipelineEvent::SessionEnded(s2)
    });
    let rows = Store::open(&rig.db)
        .unwrap()
        .segments(View::All, 0)
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.session_id).collect::<Vec<_>>(),
        vec![s1, s2]
    );
}

#[test]
fn stop_flushes_the_open_segment_with_consistent_times() {
    let rig = scripted("stop", Faults::default());
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, 0.0, 500);
    send(&tx, SPEECH, 2_000);
    // let the worker drain the channel before asking it to stop (one Level event only: the
    // audio arrives faster than the 10 Hz limit)
    std::thread::sleep(Duration::from_millis(250));
    rig.h.stop();
    wait_for(&rig.h, &mut seen, "session end", |e| {
        *e == PipelineEvent::SessionEnded(sid)
    });
    let segs = segments(&seen);
    assert_eq!(segs.len(), 1, "{seen:?}");
    let row = segs[0].clone();
    assert_eq!(row.session_id, sid);
    assert!(row.started_at < row.ended_at);
    let (session_start, session_end) = session_row(&rig.db, sid);
    assert!(session_end.is_some());
    // sample-accurate: the segment begins at the pre-roll before the speech (500 ms in, minus
    // up to 300 ms) and lasts the speech plus that pre-roll
    let offset = row.started_at - session_start;
    assert!((150..=500).contains(&offset), "offset {offset} ms");
    let duration = row.ended_at - row.started_at;
    assert!(
        (1_000..=3_000).contains(&duration),
        "duration {duration} ms for 2 s of speech"
    );
    assert!(row.text.starts_with("speech "), "{}", row.text);
    assert_eq!(row.text, row.text.trim(), "stored trimmed");
    assert_eq!(row.confidence, Some(0.8));
    // what was reported is what was stored
    let stored = Store::open(&rig.db)
        .unwrap()
        .segments(View::All, 0)
        .unwrap();
    assert_eq!(stored, vec![row]);
}

#[test]
fn a_segment_with_empty_text_is_not_stored() {
    let rig = scripted("empty", Faults::default());
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, MUMBLE, 1_000);
    send(&tx, 0.0, 1_500);
    send(&tx, SPEECH, 1_000);
    send(&tx, 0.0, 1_500);
    let ev = wait_for(&rig.h, &mut seen, "segment", |e| {
        matches!(e, PipelineEvent::Segment(_))
    });
    let PipelineEvent::Segment(row) = ev else {
        unreachable!()
    };
    assert!(row.text.starts_with("speech"), "{}", row.text);
    rig.h.stop();
    wait_for(&rig.h, &mut seen, "session end", |e| {
        *e == PipelineEvent::SessionEnded(sid)
    });
    assert_eq!(segments(&seen).len(), 1, "{seen:?}");
    assert!(
        !seen.iter().any(|e| matches!(e, PipelineEvent::Warning(_))),
        "an empty transcription is not a problem: {seen:?}"
    );
    let stored = Store::open(&rig.db)
        .unwrap()
        .segments(View::All, 0)
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, row.id);
}

#[test]
fn engine_failures_are_warnings_not_rows() {
    let rig = scripted(
        "faults",
        Faults {
            vad_fails: false,
            transcribe_fails: true,
        },
    );
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, SPEECH, 1_000);
    send(&tx, 0.0, 1_500);
    wait_for(
        &rig.h,
        &mut seen,
        "warning",
        |e| matches!(e, PipelineEvent::Warning(w) if w.contains("scripted failure")),
    );
    rig.h.stop();
    wait_for(&rig.h, &mut seen, "session end", |e| {
        *e == PipelineEvent::SessionEnded(sid)
    });
    assert!(segments(&seen).is_empty());
    drop(rig);

    // a failing VAD warns once, however many frames fail, and finds no speech
    let rig = scripted(
        "vad-fault",
        Faults {
            vad_fails: true,
            transcribe_fails: false,
        },
    );
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, SPEECH, 3_000);
    std::thread::sleep(Duration::from_millis(200));
    rig.h.stop();
    wait_for(&rig.h, &mut seen, "session end", |e| {
        *e == PipelineEvent::SessionEnded(sid)
    });
    let warnings = seen
        .iter()
        .filter(|e| matches!(e, PipelineEvent::Warning(_)))
        .count();
    assert_eq!(warnings, 1, "{seen:?}");
    assert!(segments(&seen).is_empty());
}

#[test]
fn engine_load_failure_reports_failed_and_ignores_listen() {
    let rig = spawn_with(
        "load-fail",
        Box::new(|| -> Result<Box<dyn SpeechEngine>, EngineError> {
            Err(EngineError::Load("no model here".into()))
        }),
    );
    let mut seen = Vec::new();
    let ev = wait_for(&rig.h, &mut seen, "failure", |e| {
        matches!(e, PipelineEvent::Status(PipelineStatus::Failed(_)))
    });
    assert!(
        matches!(&ev, PipelineEvent::Status(PipelineStatus::Failed(m)) if m.contains("no model here")),
        "{ev:?}"
    );
    let (tx, rx) = channel();
    rig.h.start(rx);
    tx.send(vec![0.3; 1_600]).ok();
    assert!(
        rig.h.recv_timeout(Duration::from_millis(300)).is_none(),
        "no session after a failed load"
    );
    let t = Instant::now();
    drop(rig);
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn dropping_the_handle_while_listening_shuts_down_promptly_and_saves() {
    let rig = scripted("drop", Faults::default());
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, SPEECH, 1_500);
    wait_for(&rig.h, &mut seen, "in speech", |e| {
        matches!(
            e,
            PipelineEvent::Level {
                in_speech_mic: true,
                ..
            }
        )
    });
    std::thread::sleep(Duration::from_millis(150));
    let Rig { h, _dir, db, .. } = rig;
    let t = Instant::now();
    drop(h); // the audio sender is still alive: the worker must not wait for it
    let took = t.elapsed();
    assert!(took < Duration::from_secs(2), "drop took {took:?}");
    // the open segment was flushed and the session closed on the way out
    let rows = Store::open(&db).unwrap().segments(View::All, 0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_id, sid);
    assert!(session_row(&db, sid).1.is_some());
    drop(tx);
}

#[test]
fn dropping_an_idle_handle_is_prompt() {
    let rig = null("drop-idle");
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let t = Instant::now();
    drop(rig);
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn null_engine_long_speech_is_split_at_the_max_length() {
    // 65 s of continuous speech: two 30 s pieces while listening, the rest on stop
    let rig = null("long");
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    let tone: Vec<f32> = (0..1_600)
        .map(|i| 0.3 * (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 16_000.0).sin())
        .collect();
    for _ in 0..650 {
        tx.send(tone.clone()).unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
    rig.h.stop();
    wait_for(&rig.h, &mut seen, "session end", |e| {
        *e == PipelineEvent::SessionEnded(sid)
    });
    let segs = segments(&seen);
    assert_eq!(segs.len(), 3, "{segs:?}");
    for w in segs.windows(2) {
        assert!(w[0].ended_at <= w[1].started_at, "pieces don't overlap");
    }
    let total: i64 = segs.iter().map(|r| r.ended_at - r.started_at).sum();
    assert!((64_000..=66_000).contains(&total), "{total} ms of 65 s");
    assert!(segs[0].text.contains("30.0 s"), "{}", segs[0].text);
}

/// An engine VAD with 100 ms frames (speech = peak above 0.1) that records the sample count
/// of every `vad` call.
struct Recording(Arc<std::sync::Mutex<Vec<usize>>>);

impl SpeechEngine for Recording {
    fn name(&self) -> String {
        "recording".into()
    }
    fn load(&mut self, _status: &mut dyn FnMut(String)) -> Result<(), EngineError> {
        Ok(())
    }
    fn vad_frame_len(&self) -> Option<usize> {
        Some(1_600)
    }
    fn vad(&mut self, samples: &[f32]) -> Result<Vec<VadFrame>, EngineError> {
        self.0.lock().unwrap().push(samples.len());
        Ok(samples
            .chunks(1_600)
            .map(|f| {
                let peak = f.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                VadFrame {
                    probability: peak.min(1.0),
                    is_speech: peak > 0.1,
                }
            })
            .collect())
    }
    fn transcribe(&mut self, samples: &[f32]) -> Result<Transcription, EngineError> {
        Scripted(Faults::default()).transcribe(samples)
    }
}

fn recording(tag: &str) -> (Rig, Arc<std::sync::Mutex<Vec<usize>>>) {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let c = calls.clone();
    let rig = spawn_with(
        tag,
        Box::new(move || Ok(Box::new(Recording(c)) as Box<dyn SpeechEngine>)),
    );
    (rig, calls)
}

fn stop_and_wait(rig: &Rig, seen: &mut Vec<PipelineEvent>, sid: i64) {
    std::thread::sleep(Duration::from_millis(300));
    rig.h.stop();
    wait_for(&rig.h, seen, "session end", |e| {
        *e == PipelineEvent::SessionEnded(sid)
    });
}

#[test]
fn engine_vad_is_called_once_per_frame() {
    let (rig, calls) = recording("per-frame");
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, SPEECH, 4_000);
    stop_and_wait(&rig, &mut seen, sid);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls, vec![1_600; 40], "4 s = 40 frames, one call each");
    let segs = segments(&seen);
    assert_eq!(segs.len(), 1, "{seen:?}");
    let (session_start, _) = session_row(&rig.db, sid);
    let offset = segs[0].started_at - session_start;
    assert!(offset <= 400, "segment starts with the audio: {offset} ms");
    let duration = segs[0].ended_at - segs[0].started_at;
    assert!((3_600..=4_400).contains(&duration), "duration {duration}");
}

#[test]
fn a_quiet_room_still_reaches_the_engine_every_frame() {
    let (rig, calls) = recording("quiet");
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, 0.0, 10_000);
    stop_and_wait(&rig, &mut seen, sid);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![1_600; 100],
        "no gate: every frame is classified"
    );
    assert!(segments(&seen).is_empty(), "{seen:?}");
}

#[test]
fn speech_after_a_quiet_period_is_one_segment() {
    let (rig, calls) = recording("wake");
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, 0.0, 8_000);
    send(&tx, SPEECH, 2_000);
    send(&tx, 0.0, 2_000);
    stop_and_wait(&rig, &mut seen, sid);
    assert_eq!(calls.lock().unwrap().len(), 120, "12 s = 120 frames");
    let segs = segments(&seen);
    assert_eq!(segs.len(), 1, "{seen:?}");
    let (session_start, _) = session_row(&rig.db, sid);
    let offset = segs[0].started_at - session_start;
    assert!(
        (7_600..=8_400).contains(&offset),
        "segment starts with the speech: {offset} ms"
    );
}

// ---- two lanes: microphone + system audio ------------------------------------------------------

use oats_core::pipeline::ListenOptions;
use oats_core::{AudioChunk, AudioSourceKind};

fn start_lanes(
    h: &PipelineHandle,
    seen: &mut Vec<PipelineEvent>,
    options: ListenOptions,
) -> (Sender<AudioChunk>, i64) {
    let (tx, rx) = channel();
    h.start_lanes(rx, options);
    let ev = wait_for(h, seen, "session start", |e| session_started(e).is_some());
    (tx, session_started(&ev).unwrap())
}

/// `ms` of constant `amp` on one lane, in 100 ms chunks.
fn send_lane(tx: &Sender<AudioChunk>, source: AudioSourceKind, amp: f32, ms: usize) {
    let mut left = ms * SAMPLE_RATE as usize / 1000;
    while left > 0 {
        let n = left.min(1_600);
        tx.send(AudioChunk {
            source,
            samples: vec![amp; n],
        })
        .unwrap();
        left -= n;
    }
}

/// Both lanes interleaved chunk by chunk: `mic` and `system` amplitudes for `ms`.
fn send_both(tx: &Sender<AudioChunk>, mic: f32, system: f32, ms: usize) {
    for _ in 0..ms / 100 {
        send_lane(tx, AudioSourceKind::Mic, mic, 100);
        send_lane(tx, AudioSourceKind::System, system, 100);
    }
}

/// An engine whose VAD records which lane each call was for; transcribes like [`Scripted`].
struct LaneRecording(Arc<std::sync::Mutex<Vec<AudioSourceKind>>>);

impl SpeechEngine for LaneRecording {
    fn name(&self) -> String {
        "lanes".into()
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
        Scripted(Faults::default()).transcribe(samples)
    }
}

fn lane_recording(tag: &str) -> (Rig, Arc<std::sync::Mutex<Vec<AudioSourceKind>>>) {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let c = calls.clone();
    let rig = spawn_with(
        tag,
        Box::new(move || Ok(Box::new(LaneRecording(c)) as Box<dyn SpeechEngine>)),
    );
    (rig, calls)
}

/// Speech on both lanes at different times: one segment per lane, tagged with its source,
/// stored and listed in time order; VAD runs per lane (`vad_lane`), except on all-zero system
/// frames; `Level` events carry both lanes.
#[test]
fn two_lanes_segment_separately_and_interleave_by_time() {
    let (rig, calls) = lane_recording("lanes");
    rig.h.set_echo_dedup(false);
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start_lanes(&rig.h, &mut seen, ListenOptions::default());
    // 0–1 s silence (system: digital zeros), 1–3 s "me", 3–4 s quiet,
    // 4–6 s "them" (a different length: different text), 6–8 s quiet
    send_both(&tx, 0.01, 0.0, 1_000);
    send_both(&tx, SPEECH, 0.0, 2_000);
    send_both(&tx, 0.01, 0.0, 1_000);
    send_both(&tx, 0.01, SPEECH, 2_500);
    send_both(&tx, 0.01, 0.0, 2_000);
    // levels: past the rate limit, a quiet (not speech) system chunk shows up on its own lane
    std::thread::sleep(Duration::from_millis(300));
    send_lane(&tx, AudioSourceKind::System, 0.05, 100);
    wait_for(&rig.h, &mut seen, "system level", |e| {
        matches!(
            e,
            PipelineEvent::Level { rms_db_system, rms_db_mic, .. }
                if (-30.0..-20.0).contains(rms_db_system) && *rms_db_mic < -30.0
        )
    });
    stop_and_wait(&rig, &mut seen, sid);

    let segs = segments(&seen);
    assert_eq!(segs.len(), 2, "{seen:?}");
    assert_eq!(segs[0].source, AudioSourceKind::Mic);
    assert_eq!(segs[1].source, AudioSourceKind::System);
    assert!(segs.iter().all(|r| r.echo_of.is_none()));
    assert!(segs[0].ended_at <= segs[1].started_at + 500);
    let stored = Store::open(&rig.db)
        .unwrap()
        .segments(View::All, 0)
        .unwrap();
    assert_eq!(
        stored.iter().map(|r| r.source).collect::<Vec<_>>(),
        vec![AudioSourceKind::Mic, AudioSourceKind::System]
    );
    // every mic frame (85) is classified; system frames only while not all zeros (25 + 1)
    let calls = calls.lock().unwrap().clone();
    let mic = calls.iter().filter(|l| **l == AudioSourceKind::Mic).count();
    let system = calls
        .iter()
        .filter(|l| **l == AudioSourceKind::System)
        .count();
    assert_eq!(mic, 85, "{calls:?}");
    assert_eq!(system, 26, "{calls:?}");
}

/// The speakers heard by the microphone: the same speech on both lanes. Whichever lane's
/// segment is stored first, the microphone one ends up as the echo of the system one; with
/// the rule off, both stay plain rows.
#[test]
fn a_microphone_echo_of_system_audio_is_linked_and_hidden() {
    for (tag, system_first, dedup) in [
        ("echo-mic-last", true, true),
        ("echo-mic-first", false, true),
        ("echo-off", true, false),
    ] {
        let rig = scripted(tag, Faults::default());
        rig.h.set_echo_dedup(dedup);
        let mut seen = Vec::new();
        wait_idle(&rig.h, &mut seen);
        let (tx, sid) = start_lanes(&rig.h, &mut seen, ListenOptions::default());
        // identical speech on both lanes; the lane that goes quiet first closes first
        send_both(&tx, 0.0, 0.0, 500);
        send_both(&tx, SPEECH, SPEECH, 2_000);
        if system_first {
            send_lane(&tx, AudioSourceKind::System, 0.0, 1_500);
            std::thread::sleep(Duration::from_millis(200));
            send_lane(&tx, AudioSourceKind::Mic, 0.0, 1_500);
        } else {
            send_lane(&tx, AudioSourceKind::Mic, 0.0, 1_500);
            std::thread::sleep(Duration::from_millis(200));
            send_lane(&tx, AudioSourceKind::System, 0.0, 1_500);
        }
        stop_and_wait(&rig, &mut seen, sid);

        let store = Store::open(&rig.db).unwrap();
        let conn = rusqlite::Connection::open(&rig.db).unwrap();
        let all: Vec<(String, Option<i64>)> = conn
            .prepare("SELECT source, echo_of FROM segments ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(all.len(), 2, "{tag}: {all:?}");
        let visible = store.segments(View::All, 0).unwrap();
        if dedup {
            assert_eq!(visible.len(), 1, "{tag}: {visible:?}");
            assert_eq!(visible[0].source, AudioSourceKind::System, "{tag}");
            let echoes = store.echoes_of(visible[0].id).unwrap();
            assert_eq!(echoes.len(), 1, "{tag}");
            assert_eq!(echoes[0].source, AudioSourceKind::Mic, "{tag}");
            if !system_first {
                // the mic row was reported first, then claimed
                assert!(
                    seen.iter().any(|e| matches!(
                        e,
                        PipelineEvent::EchoMarked { segment, echo_of }
                            if *segment == echoes[0].id && *echo_of == visible[0].id
                    )),
                    "{tag}: {seen:?}"
                );
            }
        } else {
            assert_eq!(visible.len(), 2, "{tag}");
            assert!(all.iter().all(|(_, e)| e.is_none()), "{tag}");
        }
    }
}

/// The system lane expected but silent: "no signal" after the configured time, and "signal"
/// once sound arrives. A system capture that goes away (its sender dropped) leaves the
/// microphone lane running.
#[test]
fn a_silent_system_lane_is_reported_and_a_closed_one_leaves_the_mic_running() {
    let rig = scripted("nosignal", Faults::default());
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start_lanes(
        &rig.h,
        &mut seen,
        ListenOptions {
            system: true,
            no_signal_after: Duration::from_millis(300),
        },
    );
    let system_tx = tx.clone();
    send_lane(&system_tx, AudioSourceKind::System, 0.0, 500);
    wait_for(&rig.h, &mut seen, "no signal", |e| {
        *e == PipelineEvent::SystemAudio { signal: false }
    });
    send_lane(&system_tx, AudioSourceKind::System, 0.05, 200);
    wait_for(&rig.h, &mut seen, "signal", |e| {
        *e == PipelineEvent::SystemAudio { signal: true }
    });
    // the system capture stops; the session carries on with the microphone
    drop(system_tx);
    send_lane(&tx, AudioSourceKind::Mic, SPEECH, 1_000);
    send_lane(&tx, AudioSourceKind::Mic, 0.0, 1_500);
    let ev = wait_for(&rig.h, &mut seen, "mic segment", |e| {
        matches!(e, PipelineEvent::Segment(_))
    });
    assert!(matches!(ev, PipelineEvent::Segment(r) if r.source == AudioSourceKind::Mic));
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, PipelineEvent::SessionEnded(_))),
        "{seen:?}"
    );
    drop(tx);
    wait_for(&rig.h, &mut seen, "session end", |e| {
        *e == PipelineEvent::SessionEnded(sid)
    });
}
