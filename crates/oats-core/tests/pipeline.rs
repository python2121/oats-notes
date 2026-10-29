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
        if let PipelineEvent::Level { rms_db, .. } = e {
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
    // verdicts arrive in batches, so the first Level (sent before any verdict) reports no
    // speech; a later chunk past the Level interval reports the open segment
    std::thread::sleep(Duration::from_millis(300));
    send(&tx1, SPEECH, 100);
    wait_for(&rig.h, &mut seen, "in speech", |e| {
        matches!(
            e,
            PipelineEvent::Level {
                in_speech: true,
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
    // see listen_while_listening_...: the first Level precedes any batched verdict
    std::thread::sleep(Duration::from_millis(300));
    send(&tx, SPEECH, 100);
    wait_for(&rig.h, &mut seen, "in speech", |e| {
        matches!(
            e,
            PipelineEvent::Level {
                in_speech: true,
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

/// Like [`Scripted`] but with 100 ms frames, recording the sample count of every `vad` call.
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
        Scripted(Faults::default()).vad(samples).map(|v| {
            // Scripted chunks by 512; re-derive per 1600-sample frame instead
            let _ = v;
            samples
                .chunks(1_600)
                .map(|f| {
                    let peak = f.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                    VadFrame {
                        probability: peak.min(1.0),
                        is_speech: peak > 0.1,
                    }
                })
                .collect()
        })
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
fn engine_vad_is_called_with_batches_of_four_frames() {
    let (rig, calls) = recording("batch");
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, SPEECH, 4_000);
    stop_and_wait(&rig, &mut seen, sid);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls, vec![6_400; 10], "4 s = 40 frames = 10 batches of 4");
    let segs = segments(&seen);
    assert_eq!(segs.len(), 1, "{seen:?}");
    let (session_start, _) = session_row(&rig.db, sid);
    let offset = segs[0].started_at - session_start;
    assert!(offset <= 400, "segment starts with the audio: {offset} ms");
    let duration = segs[0].ended_at - segs[0].started_at;
    assert!((3_600..=4_400).contains(&duration), "duration {duration}");
}

#[test]
fn a_quiet_room_stops_calling_the_engine_after_the_hangover() {
    let (rig, calls) = recording("quiet");
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, 0.0, 10_000);
    stop_and_wait(&rig, &mut seen, sid);
    let n = calls.lock().unwrap().len();
    assert_eq!(n, 3, "only the hangover batches reach the engine");
    assert!(segments(&seen).is_empty(), "{seen:?}");
}

#[test]
fn speech_after_a_gated_quiet_period_reaches_the_engine_again() {
    let (rig, calls) = recording("wake");
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, 0.0, 8_000);
    send(&tx, SPEECH, 2_000);
    send(&tx, 0.0, 2_000);
    stop_and_wait(&rig, &mut seen, sid);
    let n = calls.lock().unwrap().len();
    assert!(n > 3, "engine consulted again for the speech: {n}");
    assert_eq!(segments(&seen).len(), 1, "{seen:?}");
}

#[test]
fn a_partial_batch_is_classified_at_stop() {
    let (rig, calls) = recording("partial");
    let mut seen = Vec::new();
    wait_idle(&rig.h, &mut seen);
    let (tx, sid) = start(&rig.h, &mut seen);
    send(&tx, SPEECH, 2_500);
    stop_and_wait(&rig, &mut seen, sid);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![6_400, 6_400, 6_400, 6_400, 6_400, 6_400, 1_600],
        "25 frames: 4+4+... partial"
    );
    assert_eq!(segments(&seen).len(), 1, "{seen:?}");
}
