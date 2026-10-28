//! Speech segmentation: VAD-labelled frames in, speech segments out. Pure and synchronous so it
//! is unit-testable without audio hardware or a model.
//!
//! Rules (all in milliseconds of audio, converted to frames by the caller's frame length):
//! - A segment opens on the first speech frame and includes `pre_roll_ms` of audio from before
//!   it (so a word's onset is not clipped).
//! - It closes after `hangover_ms` of continuous non-speech; the trailing silence is kept
//!   (the model likes a little context at the end).
//! - A closed segment with less than `min_speech_ms` of speech frames is dropped (a cough, a
//!   door).
//! - A segment that reaches `max_segment_ms` is emitted immediately and a new one continues
//!   without pre-roll, so a long monologue arrives in ≤ 30 s pieces the model handles well.

use std::collections::VecDeque;

use crate::engine::SAMPLE_RATE;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SegmenterConfig {
    pub pre_roll_ms: u32,
    pub hangover_ms: u32,
    pub min_speech_ms: u32,
    pub max_segment_ms: u32,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            pre_roll_ms: 300,
            hangover_ms: 800,
            min_speech_ms: 400,
            max_segment_ms: 30_000,
        }
    }
}

/// A closed speech segment.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    /// 16 kHz mono samples, pre-roll and trailing hangover included.
    pub samples: Vec<f32>,
    /// Index (in samples since the segmenter was created) of `samples[0]`.
    pub start_sample: u64,
    /// Milliseconds of frames the VAD called speech.
    pub speech_ms: u32,
}

impl Segment {
    pub fn duration_ms(&self) -> u32 {
        (self.samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE)) as u32
    }
}

/// The segmenter state machine. Feed frames of a constant length with [`Segmenter::push`].
#[derive(Debug)]
pub struct Segmenter {
    config: SegmenterConfig,
    /// Rule lengths in frames; fixed by the first frame's length.
    frames: Option<FrameRules>,
    /// The last few frames while idle (pre-roll), oldest first.
    ring: VecDeque<Vec<f32>>,
    /// Total samples held in `ring`.
    ring_samples: usize,
    /// The segment being built, if speech is in progress.
    open: Option<OpenSegment>,
    /// Samples consumed so far.
    position: u64,
}

#[derive(Clone, Copy, Debug)]
struct FrameRules {
    frame_len: usize,
    pre_roll: usize,
    hangover: usize,
    min_speech: usize,
    max_segment: usize,
}

impl FrameRules {
    fn new(config: &SegmenterConfig, frame_len: usize) -> Self {
        let frame_len = frame_len.max(1);
        let to_frames = |ms: u32| {
            let samples = u64::from(ms) * u64::from(SAMPLE_RATE) / 1000;
            ((samples / frame_len as u64) as usize).max(1)
        };
        Self {
            frame_len,
            // no pre-roll when none is asked for
            pre_roll: if config.pre_roll_ms == 0 {
                0
            } else {
                to_frames(config.pre_roll_ms)
            },
            hangover: to_frames(config.hangover_ms),
            min_speech: to_frames(config.min_speech_ms),
            max_segment: to_frames(config.max_segment_ms),
        }
    }
}

#[derive(Debug)]
struct OpenSegment {
    samples: Vec<f32>,
    start_sample: u64,
    /// Frames in this segment (pre-roll excluded).
    frames: usize,
    speech_frames: usize,
    /// Consecutive non-speech frames at the end.
    silence_run: usize,
}

impl Segmenter {
    pub fn new(config: SegmenterConfig) -> Self {
        Self {
            config,
            frames: None,
            ring: VecDeque::new(),
            ring_samples: 0,
            open: None,
            position: 0,
        }
    }

    /// Feed one frame. Returns a segment when one just closed (by hangover or by max length).
    pub fn push(&mut self, frame: &[f32], is_speech: bool) -> Option<Segment> {
        let rules = *self
            .frames
            .get_or_insert_with(|| FrameRules::new(&self.config, frame.len()));
        let frame_start = self.position;
        self.position += frame.len() as u64;

        let Some(open) = self.open.as_mut() else {
            if is_speech {
                let mut samples = Vec::with_capacity(self.ring_samples + frame.len());
                for f in self.ring.drain(..) {
                    samples.extend_from_slice(&f);
                }
                let start_sample = frame_start - self.ring_samples as u64;
                self.ring_samples = 0;
                samples.extend_from_slice(frame);
                self.open = Some(OpenSegment {
                    samples,
                    start_sample,
                    frames: 1,
                    speech_frames: 1,
                    silence_run: 0,
                });
                return self.split_if_full(rules);
            }
            if rules.pre_roll > 0 {
                self.ring.push_back(frame.to_vec());
                self.ring_samples += frame.len();
                while self.ring.len() > rules.pre_roll {
                    if let Some(old) = self.ring.pop_front() {
                        self.ring_samples -= old.len();
                    }
                }
            }
            return None;
        };

        open.samples.extend_from_slice(frame);
        open.frames += 1;
        if is_speech {
            open.speech_frames += 1;
            open.silence_run = 0;
        } else {
            open.silence_run += 1;
            if open.silence_run >= rules.hangover {
                return self.close(rules);
            }
        }
        self.split_if_full(rules)
    }

    /// Close whatever is open (listening stopped). Applies the min-speech rule.
    pub fn flush(&mut self) -> Option<Segment> {
        let rules = self.frames?;
        self.close(rules)
    }

    /// Whether a segment is currently open (speech in progress), for the UI.
    pub fn in_speech(&self) -> bool {
        self.open.is_some()
    }

    /// Samples consumed so far.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Emit the open segment if it reached the maximum length, continuing without pre-roll.
    fn split_if_full(&mut self, rules: FrameRules) -> Option<Segment> {
        let full = self
            .open
            .as_ref()
            .is_some_and(|o| o.samples.len() >= rules.max_segment * rules.frame_len);
        if !full {
            return None;
        }
        let out = self.close(rules);
        self.open = Some(OpenSegment {
            samples: Vec::new(),
            start_sample: self.position,
            frames: 0,
            speech_frames: 0,
            silence_run: 0,
        });
        out
    }

    /// Close the open segment, dropping it when it holds too little speech.
    fn close(&mut self, rules: FrameRules) -> Option<Segment> {
        let open = self.open.take()?;
        if open.frames == 0 || open.speech_frames < rules.min_speech {
            return None;
        }
        let speech_samples = (open.speech_frames * rules.frame_len) as u64;
        Some(Segment {
            samples: open.samples,
            start_sample: open.start_sample,
            speech_ms: (speech_samples * 1000 / u64::from(SAMPLE_RATE)) as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 512; // 32 ms

    fn cfg() -> SegmenterConfig {
        // 512-sample frames: pre-roll 9 frames, hangover 25, min speech 12, max 937
        SegmenterConfig::default()
    }

    /// Frame `n` filled with value `n` so sample provenance is checkable.
    fn frame(n: usize) -> Vec<f32> {
        vec![n as f32; FRAME]
    }

    /// Push frames `range` with the given speech flag, collecting segments.
    fn feed(
        s: &mut Segmenter,
        range: std::ops::Range<usize>,
        speech: bool,
        out: &mut Vec<Segment>,
    ) {
        for n in range {
            out.extend(s.push(&frame(n), speech));
        }
    }

    #[test]
    fn silence_yields_nothing() {
        let mut s = Segmenter::new(cfg());
        let mut out = Vec::new();
        feed(&mut s, 0..1000, false, &mut out);
        assert!(out.is_empty());
        assert!(!s.in_speech());
        assert_eq!(s.position(), 1000 * FRAME as u64);
        assert!(s.flush().is_none());
    }

    #[test]
    fn short_burst_is_dropped() {
        let mut s = Segmenter::new(cfg());
        let mut out = Vec::new();
        feed(&mut s, 0..20, false, &mut out);
        feed(&mut s, 20..25, true, &mut out); // 160 ms < 400 ms
        assert!(s.in_speech());
        feed(&mut s, 25..100, false, &mut out);
        assert!(out.is_empty());
        assert!(!s.in_speech());
    }

    #[test]
    fn speech_with_hangover_yields_one_segment_with_pre_roll() {
        let mut s = Segmenter::new(cfg());
        let mut out = Vec::new();
        feed(&mut s, 0..20, false, &mut out);
        feed(&mut s, 20..50, true, &mut out); // 30 frames = 960 ms
        feed(&mut s, 50..100, false, &mut out);
        assert_eq!(out.len(), 1);
        let seg = &out[0];
        // pre-roll: 300 ms -> 9 frames, so the segment starts at frame 11
        assert_eq!(seg.start_sample, 11 * FRAME as u64);
        assert_eq!(seg.samples[0], 11.0);
        assert_eq!(seg.samples[9 * FRAME], 20.0);
        // 9 pre-roll + 30 speech + 25 hangover frames
        assert_eq!(seg.samples.len(), (9 + 30 + 25) * FRAME);
        assert_eq!(*seg.samples.last().unwrap_or(&-1.0), 74.0);
        assert_eq!(seg.speech_ms, 960);
        assert_eq!(seg.duration_ms(), 2048);
        assert!(!s.in_speech());
    }

    #[test]
    fn pre_roll_is_limited_by_available_audio() {
        let mut s = Segmenter::new(cfg());
        let mut out = Vec::new();
        feed(&mut s, 0..3, false, &mut out);
        feed(&mut s, 3..30, true, &mut out);
        feed(&mut s, 30..60, false, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_sample, 0);
        assert_eq!(out[0].samples[0], 0.0);
    }

    #[test]
    fn brief_pause_does_not_close() {
        let mut s = Segmenter::new(cfg());
        let mut out = Vec::new();
        feed(&mut s, 0..20, true, &mut out);
        feed(&mut s, 20..40, false, &mut out); // 20 < 25 hangover frames
        feed(&mut s, 40..60, true, &mut out);
        feed(&mut s, 60..100, false, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speech_ms, (40 * FRAME * 1000 / 16_000) as u32);
    }

    #[test]
    fn long_speech_splits_at_max_length() {
        let config = SegmenterConfig {
            max_segment_ms: 3_200, // 100 frames
            ..cfg()
        };
        let mut s = Segmenter::new(config);
        let mut out = Vec::new();
        feed(&mut s, 0..20, false, &mut out);
        feed(&mut s, 20..150, true, &mut out);
        assert_eq!(out.len(), 1);
        let first = &out[0];
        assert_eq!(first.samples.len(), 100 * FRAME);
        assert_eq!(first.start_sample, 11 * FRAME as u64);
        // 9 pre-roll frames, then 91 speech frames (20..111)
        assert_eq!(first.samples.last().copied(), Some(110.0));
        assert!(s.in_speech());

        feed(&mut s, 150..250, false, &mut out);
        assert_eq!(out.len(), 2);
        let second = &out[1];
        // continuation: no pre-roll, starts right after the first
        assert_eq!(second.start_sample, 111 * FRAME as u64);
        assert_eq!(second.samples[0], 111.0);
        assert_eq!(second.samples.len(), (39 + 25) * FRAME);
        assert_eq!(second.speech_ms, (39 * FRAME * 1000 / 16_000) as u32);
    }

    #[test]
    fn continuation_with_only_silence_is_dropped() {
        let config = SegmenterConfig {
            max_segment_ms: 3_200,
            pre_roll_ms: 0,
            ..cfg()
        };
        let mut s = Segmenter::new(config);
        let mut out = Vec::new();
        feed(&mut s, 0..100, true, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_sample, 0);
        feed(&mut s, 100..200, false, &mut out);
        assert_eq!(out.len(), 1);
        assert!(!s.in_speech());
    }

    #[test]
    fn flush_closes_open_segment() {
        let mut s = Segmenter::new(cfg());
        let mut out = Vec::new();
        feed(&mut s, 0..30, true, &mut out);
        assert!(out.is_empty());
        let seg = s.flush();
        assert!(seg.as_ref().is_some_and(|g| g.samples.len() == 30 * FRAME));
        assert!(!s.in_speech());
        assert!(s.flush().is_none());
    }

    #[test]
    fn flush_applies_min_speech() {
        let mut s = Segmenter::new(cfg());
        let mut out = Vec::new();
        feed(&mut s, 0..5, true, &mut out);
        assert!(s.flush().is_none());
        assert!(!s.in_speech());
        // fresh segmenter: nothing to flush
        assert!(Segmenter::new(cfg()).flush().is_none());
    }
}
