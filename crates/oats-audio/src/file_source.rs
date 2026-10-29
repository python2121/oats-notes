//! A deterministic audio source: a PCM 16-bit mono 16 kHz WAV file played into the same
//! `Sender<Vec<f32>>` the microphone capture feeds, in [`CHUNK_LEN`] (100 ms) chunks.
//!
//! Paced in real time by default (one chunk per 100 ms) on a monotonic schedule — chunk `n` is
//! due at `start + n × interval`, so a late wake-up never pushes later chunks back and drift
//! does not accumulate. `speed` divides the interval (4.0 = four times real time); `looping`
//! starts the file again at its end. When the file ends (not looping) the thread drops its
//! sender, so the pipeline sees `Disconnected` and stops listening — exactly like a lost
//! microphone. Dropping the [`FileSource`] stops playback at once (the thread waits on a
//! channel the guard owns, not in a plain sleep) and joins the thread.
//!
//! Used by `oats-notes --audio-file` for benchmarks and reproducible end-to-end runs.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use oats_core::SAMPLE_RATE;

use crate::CHUNK_LEN;

/// Real-time spacing of [`CHUNK_LEN`]-sample chunks.
pub const CHUNK_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("not a RIFF/WAVE file")]
    NotWav,
    #[error("malformed fmt chunk")]
    BadFmt,
    #[error("data chunk before fmt chunk")]
    DataBeforeFmt,
    #[error(
        "need PCM 16-bit mono {SAMPLE_RATE} Hz, got format tag {tag}, {channels} ch, {rate} Hz, {bits} bit"
    )]
    Unsupported {
        tag: u16,
        channels: u16,
        rate: u32,
        bits: u16,
    },
    #[error("no data chunk")]
    NoData,
    #[error("invalid playback speed {0} (must be a finite number > 0)")]
    BadSpeed(f64),
    #[error("could not spawn the file source thread: {0}")]
    Spawn(std::io::Error),
}

/// Minimal RIFF/WAVE reader: PCM 16-bit mono at [`SAMPLE_RATE`] only (what
/// `say -o x.wav --data-format=LEI16@16000` writes). Unknown chunks are skipped; a truncated
/// data chunk yields the samples present.
pub fn read_wav(bytes: &[u8]) -> Result<Vec<f32>, WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWav);
    }
    let mut pos = 12usize;
    let mut format: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let end = pos.saturating_add(8).saturating_add(size).min(bytes.len());
        let body = &bytes[pos + 8..end];
        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err(WavError::BadFmt);
                }
                let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
                let u32_at =
                    |i: usize| u32::from_le_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
                format = Some((u16_at(0), u16_at(2), u32_at(4), u16_at(14)));
            }
            b"data" => {
                let (tag, channels, rate, bits) = format.ok_or(WavError::DataBeforeFmt)?;
                if tag != 1 || channels != 1 || rate != SAMPLE_RATE || bits != 16 {
                    return Err(WavError::Unsupported {
                        tag,
                        channels,
                        rate,
                        bits,
                    });
                }
                return Ok(body
                    .chunks_exact(2)
                    .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
                    .collect());
            }
            _ => {}
        }
        pos = pos
            .saturating_add(8)
            .saturating_add(size)
            .saturating_add(size & 1);
    }
    Err(WavError::NoData)
}

/// Read and decode a WAV file (see [`read_wav`]).
pub fn load_wav(path: &Path) -> Result<Vec<f32>, WavError> {
    let bytes = std::fs::read(path).map_err(|source| WavError::Io {
        path: path.display().to_string(),
        source,
    })?;
    read_wav(&bytes)
}

/// Encode 16 kHz mono samples as a PCM 16-bit WAV (test material, e.g. a silence file).
pub fn encode_wav(samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Playback options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FileSourceOptions {
    /// Playback rate relative to real time (1.0 = one chunk per 100 ms).
    pub speed: f64,
    /// Start again at the end of the file instead of closing the channel.
    pub looping: bool,
}

impl Default for FileSourceOptions {
    fn default() -> Self {
        Self {
            speed: 1.0,
            looping: false,
        }
    }
}

/// The monotonic schedule: chunk `n` is due at `start + n × interval`.
#[derive(Clone, Copy, Debug)]
pub struct Pacer {
    start: Instant,
    interval: Duration,
}

impl Pacer {
    pub fn new(start: Instant, speed: f64) -> Result<Self, WavError> {
        if !speed.is_finite() || speed <= 0.0 {
            return Err(WavError::BadSpeed(speed));
        }
        Ok(Self {
            start,
            interval: CHUNK_INTERVAL.div_f64(speed),
        })
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// When chunk `n` (0-based) is due. Depends only on `n`, never on when earlier chunks
    /// actually went out, so lateness does not accumulate.
    pub fn due(&self, n: u64) -> Instant {
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        self.start + self.interval.saturating_mul(n)
    }
}

/// Splits the file into the chunk sequence, wrapping around when looping.
#[derive(Debug)]
struct Chunker {
    samples: Arc<[f32]>,
    pos: usize,
    looping: bool,
}

impl Chunker {
    fn next_chunk(&mut self) -> Option<Vec<f32>> {
        if self.samples.is_empty() {
            return None;
        }
        if self.pos >= self.samples.len() {
            if !self.looping {
                return None;
            }
            self.pos = 0;
        }
        let end = (self.pos + CHUNK_LEN).min(self.samples.len());
        let chunk = self.samples[self.pos..end].to_vec();
        self.pos = end;
        Some(chunk)
    }
}

/// A running file playback. Dropping it stops playback and joins the thread.
pub struct FileSource {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for FileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSource").finish_non_exhaustive()
    }
}

impl FileSource {
    /// Load `path` and start playing it into `sink`.
    pub fn open(
        path: &Path,
        options: FileSourceOptions,
        sink: Sender<Vec<f32>>,
    ) -> Result<FileSource, WavError> {
        let samples: Arc<[f32]> = load_wav(path)?.into();
        Self::start(samples, options, sink)
    }

    /// Start playing already-decoded 16 kHz mono `samples` into `sink`.
    pub fn start(
        samples: Arc<[f32]>,
        options: FileSourceOptions,
        sink: Sender<Vec<f32>>,
    ) -> Result<FileSource, WavError> {
        let pacer = Pacer::new(Instant::now(), options.speed)?;
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        log::info!(
            "audio input: file, {:.1} s at {}× real time{}",
            samples.len() as f64 / f64::from(SAMPLE_RATE),
            options.speed,
            if options.looping { ", looping" } else { "" }
        );
        let mut chunker = Chunker {
            samples,
            pos: 0,
            looping: options.looping,
        };
        let thread = std::thread::Builder::new()
            .name("oats-file-source".to_owned())
            .spawn(move || {
                let mut n: u64 = 0;
                while let Some(chunk) = chunker.next_chunk() {
                    // wait until this chunk is due; a message or a dropped guard stops us
                    let wait = pacer.due(n).saturating_duration_since(Instant::now());
                    match stop_rx.recv_timeout(wait) {
                        Err(RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    }
                    if sink.send(chunk).is_err() {
                        return;
                    }
                    n += 1;
                }
                log::info!("audio file ended after {n} chunks; closing the audio channel");
                // `sink` drops here → the pipeline sees Disconnected
            })
            .map_err(WavError::Spawn)?;
        Ok(FileSource {
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }
}

impl Drop for FileSource {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| (i % 100) as f32 / 200.0).collect()
    }

    #[test]
    fn wav_roundtrip() {
        let samples = ramp(5000);
        let back = read_wav(&encode_wav(&samples)).unwrap_or_default();
        assert_eq!(back.len(), 5000);
        assert!(back.iter().zip(&samples).all(|(a, b)| (a - b).abs() < 1e-4));
    }

    #[test]
    fn wav_reader_skips_unknown_chunks_and_odd_padding() {
        let base = encode_wav(&[0.5, -0.5]);
        let mut bytes = base[..12].to_vec();
        bytes.extend_from_slice(b"LIST");
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 0]); // 3 bytes + pad byte
        bytes.extend_from_slice(&base[12..]);
        let s = read_wav(&bytes).unwrap_or_default();
        assert_eq!(s.len(), 2);
        assert!((s[0] - 0.5).abs() < 1e-3 && (s[1] + 0.5).abs() < 1e-3);
    }

    #[test]
    fn wav_reader_rejects_other_formats() {
        assert!(matches!(read_wav(b"nope"), Err(WavError::NotWav)));
        let mut stereo = encode_wav(&[0.0; 4]);
        stereo[22] = 2; // channels
        assert!(matches!(
            read_wav(&stereo),
            Err(WavError::Unsupported { channels: 2, .. })
        ));
        let mut rate = encode_wav(&[0.0; 4]);
        rate[24..28].copy_from_slice(&44_100u32.to_le_bytes());
        assert!(matches!(
            read_wav(&rate),
            Err(WavError::Unsupported { rate: 44_100, .. })
        ));
        let no_data = encode_wav(&[])[..36].to_vec();
        assert!(matches!(read_wav(&no_data), Err(WavError::NoData)));
        let mut short_fmt = encode_wav(&[0.0]);
        short_fmt[16..20].copy_from_slice(&4u32.to_le_bytes());
        assert!(read_wav(&short_fmt).is_err());
    }

    #[test]
    fn pacer_schedule_is_absolute() {
        let t0 = Instant::now();
        let p = Pacer::new(t0, 1.0).unwrap_or_else(|_| unreachable!());
        assert_eq!(p.due(0), t0);
        assert_eq!(p.due(10), t0 + Duration::from_secs(1));
        assert_eq!(p.due(3000), t0 + Duration::from_secs(300));
        let fast = Pacer::new(t0, 4.0).unwrap_or_else(|_| unreachable!());
        assert_eq!(fast.interval(), Duration::from_millis(25));
        assert_eq!(fast.due(40), t0 + Duration::from_secs(1));
        assert!(Pacer::new(t0, 0.0).is_err());
        assert!(Pacer::new(t0, -1.0).is_err());
        assert!(Pacer::new(t0, f64::NAN).is_err());
    }

    #[test]
    fn chunker_splits_and_loops() {
        let samples: Arc<[f32]> = ramp(CHUNK_LEN * 2 + 10).into();
        let mut c = Chunker {
            samples: samples.clone(),
            pos: 0,
            looping: false,
        };
        let lens: Vec<usize> = std::iter::from_fn(|| c.next_chunk().map(|v| v.len())).collect();
        assert_eq!(lens, vec![CHUNK_LEN, CHUNK_LEN, 10]);

        let mut c = Chunker {
            samples,
            pos: 0,
            looping: true,
        };
        let lens: Vec<usize> = (0..5)
            .filter_map(|_| c.next_chunk().map(|v| v.len()))
            .collect();
        assert_eq!(lens, vec![CHUNK_LEN, CHUNK_LEN, 10, CHUNK_LEN, CHUNK_LEN]);
    }

    /// 20 chunks at 50× real time (2 ms apart): all arrive, in order, paced (not all at once,
    /// not much later than scheduled), then the channel disconnects.
    #[test]
    fn plays_the_file_paced_then_disconnects() {
        let samples: Arc<[f32]> = ramp(CHUNK_LEN * 20).into();
        let (tx, rx) = mpsc::channel();
        let t0 = Instant::now();
        let src = FileSource::start(
            samples.clone(),
            FileSourceOptions {
                speed: 50.0,
                looping: false,
            },
            tx,
        );
        assert!(src.is_ok());
        let mut got = Vec::new();
        while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(300)) {
            got.extend(chunk);
        }
        let elapsed = t0.elapsed();
        assert_eq!(got.as_slice(), &*samples);
        // 19 intervals of 2 ms = 38 ms minimum; a generous ceiling for a loaded CI machine
        assert!(elapsed >= Duration::from_millis(38), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(600), "{elapsed:?}");
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    /// At real time a chunk is due every 100 ms; dropping the guard stops playback promptly
    /// (well before the next chunk) and closes the channel.
    #[test]
    fn dropping_the_guard_stops_playback() {
        let samples: Arc<[f32]> = ramp(CHUNK_LEN * 100).into();
        let (tx, rx) = mpsc::channel();
        let src = FileSource::start(
            samples,
            FileSourceOptions {
                speed: 1.0,
                looping: true,
            },
            tx,
        );
        assert!(rx.recv_timeout(Duration::from_millis(250)).is_ok()); // chunk 0 is due at once
        let t = Instant::now();
        drop(src);
        assert!(t.elapsed() < Duration::from_millis(80), "{:?}", t.elapsed());
        while rx.try_recv().is_ok() {}
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn a_bad_speed_is_refused() {
        let (tx, _rx) = mpsc::channel();
        let r = FileSource::start(
            Arc::from(vec![0.0f32; 10]),
            FileSourceOptions {
                speed: 0.0,
                looping: false,
            },
            tx,
        );
        assert!(matches!(r, Err(WavError::BadSpeed(_))));
    }
}
