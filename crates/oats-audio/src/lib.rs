//! Microphone capture: the default input device, at its default config, delivered as 16 kHz
//! mono f32 chunks of [`CHUNK_LEN`] samples (100 ms) over an `mpsc` channel.
//!
//! The cpal callback does the minimum: convert to f32 and mix down to mono, run the
//! [`resample::Resampler`], and send full chunks. All scratch buffers are reused; the only
//! allocation in steady state is the `Vec` handed to the channel, ten times a second.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod resample;

use std::sync::mpsc::Sender;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use oats_core::SAMPLE_RATE;

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
    let mut conv = Converter::new(config.sample_rate, config.channels, sink);
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| conv.push(data),
            |err| log::warn!("audio input stream error: {err}"),
            None,
        )
        .map_err(|e| CaptureError::Cpal(e.to_string()))
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
    sink: Sender<Vec<f32>>,
    closed: bool,
}

impl Converter {
    fn new(in_rate: u32, channels: u16, sink: Sender<Vec<f32>>) -> Self {
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
            if self.sink.send(out).is_err() {
                log::info!("audio receiver gone; capture callback idle");
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
        let mut conv = Converter::new(48_000, 2, tx);
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
        let mut conv = Converter::new(16_000, 1, tx);
        conv.push(&vec![0.0f32; CHUNK_LEN]);
        assert!(conv.closed);
    }
}
