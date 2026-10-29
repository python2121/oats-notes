//! Microphone capture: the default input device, at its default config, delivered as 16 kHz
//! mono f32 chunks of [`CHUNK_LEN`] samples (100 ms) over an `mpsc` channel.
//!
//! The cpal callback does the minimum: convert to f32 and mix down to mono, run the
//! [`resample::Resampler`], and send full chunks. All scratch buffers are reused; the only
//! allocation in steady state is the `Vec` handed to the channel, ten times a second.
//!
//! A lost device (unplugged USB mic, the input going away) arrives on cpal's error callback;
//! a terminal error drops the channel's sender there, so the pipeline sees the audio channel
//! disconnect and ends the session ("audio input closed" in the UI). Following a change of
//! the default input device to a new one is not done: listening stops and the user starts it
//! again (cpal's `DeviceChanged` — an automatic reroute of a default-device stream — is not
//! terminal and is only logged).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod file_source;
pub mod resample;

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use oats_core::SAMPLE_RATE;

pub use crate::file_source::{FileSource, FileSourceOptions, WavError};
use crate::resample::Resampler;

/// Samples per chunk sent to the sink: 100 ms at 16 kHz.
pub const CHUNK_LEN: usize = SAMPLE_RATE as usize / 10;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no audio input device is available")]
    NoInputDevice,
    #[error("unsupported input sample format: {0}")]
    UnsupportedFormat(String),
    #[error("audio input error: {0}")]
    Cpal(String),
}

/// The device capture is running on, with its native (pre-resampling) config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// A running microphone capture. Dropping it stops the stream.
pub struct Capture {
    _stream: Stream,
    device: DeviceInfo,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

impl Capture {
    /// Open the default input device with its default input config (the device's nominal rate
    /// is left alone), start the stream, and send ~100 ms chunks of 16 kHz mono f32 to `sink`.
    /// Capture stops when the returned [`Capture`] is dropped; once the receiver is gone the
    /// callback stops doing any work.
    pub fn start(sink: Sender<Vec<f32>>) -> Result<Capture, CaptureError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(CaptureError::NoInputDevice)?;
        let name = device
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_else(|_| "(unknown input device)".to_owned());
        let supported = device
            .default_input_config()
            .map_err(|e| CaptureError::Cpal(e.to_string()))?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.config();
        let info = DeviceInfo {
            name,
            sample_rate: config.sample_rate,
            channels: config.channels,
        };
        log::info!(
            "audio input: \"{}\" at {} Hz, {} channel(s), {:?} → {} Hz mono",
            info.name,
            info.sample_rate,
            info.channels,
            format,
            SAMPLE_RATE
        );

        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, sink),
            SampleFormat::I16 => build::<i16>(&device, &config, sink),
            SampleFormat::U16 => build::<u16>(&device, &config, sink),
            SampleFormat::I32 => build::<i32>(&device, &config, sink),
            other => return Err(CaptureError::UnsupportedFormat(format!("{other:?}"))),
        }?;
        stream
            .play()
            .map_err(|e| CaptureError::Cpal(e.to_string()))?;
        Ok(Capture {
            _stream: stream,
            device: info,
        })
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    sink: Sender<Vec<f32>>,
) -> Result<Stream, CaptureError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let sink = Sink::new(sink);
    let on_error = sink.clone();
    let mut conv = Converter::new(config.sample_rate, config.channels, sink);
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| conv.push(data),
            move |err| {
                if is_terminal(err.kind()) {
                    log::warn!("audio input lost: {err}; closing the audio channel");
                    on_error.close();
                } else {
                    log::warn!("audio input stream error: {err}");
                }
            },
            None,
        )
        .map_err(|e| CaptureError::Cpal(e.to_string()))
}

/// Stream errors after which no more audio will come: the device is gone, the host went
/// away, access was revoked, or the stream must be rebuilt.
pub fn is_terminal(kind: cpal::ErrorKind) -> bool {
    matches!(
        kind,
        cpal::ErrorKind::DeviceNotAvailable
            | cpal::ErrorKind::HostUnavailable
            | cpal::ErrorKind::PermissionDenied
            | cpal::ErrorKind::StreamInvalidated
    )
}

/// The channel's sender, shared by the data callback and the error callback so the latter
/// can drop it (the receiver then sees `Disconnected`). The lock is uncontended except at
/// the moment of a device loss.
#[derive(Clone)]
struct Sink(Arc<Mutex<Option<Sender<Vec<f32>>>>>);

impl Sink {
    fn new(tx: Sender<Vec<f32>>) -> Self {
        Sink(Arc::new(Mutex::new(Some(tx))))
    }

    /// `false` when the sender is closed or the receiver is gone.
    fn send(&self, chunk: Vec<f32>) -> bool {
        let Ok(mut guard) = self.0.lock() else {
            return false;
        };
        match guard.as_ref() {
            Some(tx) if tx.send(chunk).is_ok() => true,
            Some(_) => {
                *guard = None;
                false
            }
            None => false,
        }
    }

    /// Drop the sender (idempotent).
    fn close(&self) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = None;
        }
    }
}

/// Mix interleaved `channels`-channel audio down to mono by averaging, converting to f32,
/// appending to `out`.
pub fn mix_to_mono<T>(interleaved: &[T], channels: u16, out: &mut Vec<f32>)
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = usize::from(channels.max(1));
    if ch == 1 {
        out.extend(interleaved.iter().map(|&s| s.to_sample::<f32>()));
        return;
    }
    let inv = 1.0 / ch as f32;
    out.extend(
        interleaved
            .chunks_exact(ch)
            .map(|frame| frame.iter().map(|&s| s.to_sample::<f32>()).sum::<f32>() * inv),
    );
}

/// The per-stream state the callback owns: reusable buffers, the resampler, the sink.
struct Converter {
    channels: u16,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    chunk: Vec<f32>,
    resampler: Resampler,
    sink: Sink,
    closed: bool,
}

impl Converter {
    fn new(in_rate: u32, channels: u16, sink: Sink) -> Self {
        Self {
            channels,
            mono: Vec::with_capacity(8192),
            resampled: Vec::with_capacity(4096),
            chunk: Vec::with_capacity(CHUNK_LEN * 2),
            resampler: Resampler::new(in_rate, SAMPLE_RATE),
            sink,
            closed: false,
        }
    }

    fn push<T>(&mut self, data: &[T])
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        if self.closed {
            return;
        }
        self.mono.clear();
        mix_to_mono(data, self.channels, &mut self.mono);
        self.resampled.clear();
        self.resampler.process(&self.mono, &mut self.resampled);
        self.chunk.extend_from_slice(&self.resampled);
        while self.chunk.len() >= CHUNK_LEN {
            let out: Vec<f32> = self.chunk.drain(..CHUNK_LEN).collect();
            if !self.sink.send(out) {
                log::info!("audio channel closed; capture callback idle");
                self.closed = true;
                self.chunk.clear();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn mono_mixdown_averages_channels() {
        let mut out = Vec::new();
        mix_to_mono(&[1.0f32, 0.0, 0.5, -0.5, -1.0, -1.0], 2, &mut out);
        assert_eq!(out, vec![0.5, 0.0, -1.0]);
    }

    #[test]
    fn mono_mixdown_converts_integer_formats() {
        let mut out = Vec::new();
        mix_to_mono(&[i16::MAX, i16::MAX, 0, 0], 2, &mut out);
        assert!((out[0] - 1.0).abs() < 1e-3 && out[1].abs() < 1e-6);

        out.clear();
        mix_to_mono(&[u16::MAX, 32768u16], 1, &mut out);
        assert!((out[0] - 1.0).abs() < 1e-3 && out[1].abs() < 1e-3);
    }

    #[test]
    fn converter_sends_full_chunks() {
        let (tx, rx) = mpsc::channel();
        let mut conv = Converter::new(48_000, 2, Sink::new(tx));
        // 1 s of stereo 48 kHz, in 10 ms callbacks.
        let block = vec![0.25f32; 480 * 2];
        for _ in 0..100 {
            conv.push(&block);
        }
        let chunks: Vec<Vec<f32>> = rx.try_iter().collect();
        // ~16 000 samples minus the filter look-ahead → 9 full chunks.
        assert_eq!(chunks.len(), 9);
        assert!(chunks.iter().all(|c| c.len() == CHUNK_LEN));
        assert!((chunks[5][800] - 0.25).abs() < 1e-4);
    }

    #[test]
    fn converter_stops_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let mut conv = Converter::new(16_000, 1, Sink::new(tx));
        conv.push(&vec![0.0f32; CHUNK_LEN]);
        assert!(conv.closed);
    }

    /// A lost device: the error callback closes the sink while the data callback still holds
    /// its clone; the pipeline's receiver must see the channel disconnect.
    #[test]
    fn closing_the_sink_disconnects_the_receiver() {
        let (tx, rx) = mpsc::channel();
        let sink = Sink::new(tx);
        let mut conv = Converter::new(16_000, 1, sink.clone());
        conv.push(&vec![0.0f32; CHUNK_LEN * 2]);
        assert!(rx.try_recv().is_ok());
        sink.close();
        while rx.try_recv().is_ok() {}
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        conv.push(&vec![0.0f32; CHUNK_LEN]);
        assert!(conv.closed, "the data callback goes idle after a close");
        assert!(is_terminal(cpal::ErrorKind::DeviceNotAvailable));
        assert!(!is_terminal(cpal::ErrorKind::DeviceChanged));
        assert!(!is_terminal(cpal::ErrorKind::Xrun));
    }
}
