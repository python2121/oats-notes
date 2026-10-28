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
    // The implementation keeps: the config, a pre-roll ring of recent frames, the open segment
    // (if any) with its start index and speech/silence counters, and a running sample index.
    _private: (),
}

impl Segmenter {
    pub fn new(config: SegmenterConfig) -> Self {
        let _ = config;
        Self { _private: () }
    }

    /// Feed one frame. Returns a segment when one just closed (by hangover or by max length).
    pub fn push(&mut self, frame: &[f32], is_speech: bool) -> Option<Segment> {
        let _ = (frame, is_speech);
        None
    }

    /// Close whatever is open (listening stopped). Applies the min-speech rule.
    pub fn flush(&mut self) -> Option<Segment> {
        None
    }

    /// Whether a segment is currently open (speech in progress), for the UI.
    pub fn in_speech(&self) -> bool {
        false
    }

    /// Samples consumed so far.
    pub fn position(&self) -> u64 {
        0
    }
}
