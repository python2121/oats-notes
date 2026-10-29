//! The voice-command confirmation "ding": two short sine tones (880 Hz then 1320 Hz, 80 ms
//! each, 10 ms fades, about −18 dBFS) played once through the default OUTPUT device. Built on
//! a short-lived thread so the caller (the UI) never waits on Core Audio; any failure is
//! logged and ignored.
//!
//! The system-audio lane records the output device, so it hears the ding too: 160 ms of tone
//! is shorter than the segmenter's minimum speech length, so it never becomes a segment.

use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

/// The two tones, Hz.
pub const TONES: [f32; 2] = [880.0, 1320.0];
/// Length of each tone.
pub const TONE_MS: u32 = 80;
/// Fade in and out of each tone.
pub const FADE_MS: u32 = 10;
/// Peak amplitude (−18 dBFS).
pub const AMPLITUDE: f32 = 0.126;

/// The chime as mono samples at `sample_rate` (pure; the stream callback copies them out).
pub fn samples(sample_rate: u32) -> Vec<f32> {
    let per_tone = (sample_rate as u64 * TONE_MS as u64 / 1000) as usize;
    let fade = ((sample_rate as u64 * FADE_MS as u64 / 1000) as usize).max(1);
    let mut out = Vec::with_capacity(per_tone * TONES.len());
    for hz in TONES {
        for i in 0..per_tone {
            let env = (i.min(per_tone.saturating_sub(1 + i)) as f32 / fade as f32).min(1.0);
            let phase = 2.0 * std::f32::consts::PI * hz * i as f32 / sample_rate as f32;
            out.push(AMPLITUDE * env * phase.sin());
        }
    }
    out
}

/// Play the chime once, in the background. Returns at once.
pub fn play() {
    let spawned = std::thread::Builder::new()
        .name("oats-chime".into())
        .spawn(|| {
            if let Err(e) = play_blocking() {
                log::warn!("chime: {e}");
            }
        });
    if let Err(e) = spawned {
        log::warn!("chime: no thread: {e}");
    }
}

/// Open the default output, play the chime, let it drain, close.
fn play_blocking() -> Result<(), String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let mono = samples(config.sample_rate);
    let length = Duration::from_secs_f64(mono.len() as f64 / f64::from(config.sample_rate));
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, mono),
        SampleFormat::I16 => build::<i16>(&device, &config, mono),
        SampleFormat::U16 => build::<u16>(&device, &config, mono),
        SampleFormat::I32 => build::<i32>(&device, &config, mono),
        other => return Err(format!("unsupported output format {other:?}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    // the chime plus the device's buffer, then the stream is dropped (closed)
    std::thread::sleep(length + Duration::from_millis(150));
    Ok(())
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mono: Vec<f32>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels.max(1));
    let mut pos = 0usize;
    device
        .build_output_stream::<T, _, _>(
            *config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    let v = mono.get(pos).copied().unwrap_or(0.0);
                    pos += 1;
                    for s in frame {
                        *s = T::from_sample(v);
                    }
                }
            },
            |e| log::warn!("chime stream: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_faded_tones_at_minus_18_dbfs() {
        let s = samples(48_000);
        assert_eq!(s.len(), 2 * 48 * TONE_MS as usize);
        let peak = s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak <= AMPLITUDE + 1e-6 && peak > AMPLITUDE * 0.9, "{peak}");
        // faded: silent at both ends of each tone
        assert!(s[0].abs() < 1e-6);
        assert!(s[s.len() - 1].abs() < 0.01);
        let db = 20.0 * AMPLITUDE.log10();
        assert!((db + 18.0).abs() < 0.1, "{db}");
    }
}
