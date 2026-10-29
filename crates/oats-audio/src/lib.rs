//! Audio capture into 16 kHz mono f32 chunks of [`CHUNK_LEN`] samples (100 ms), tagged with
//! their lane ([`AudioChunk`]) and sent over one `mpsc` channel shared by both captures:
//!
//! - [`Capture`]: the microphone, chosen by [`InputPolicy`] — never a Bluetooth headset unless
//!   asked for by name (a headset's microphone switches it to its call profile and playback
//!   degrades): `OATS_INPUT_DEVICE` if set, else the built-in microphone, else the system
//!   default with a warning.
//! - [`SystemCapture`] (`system` module): what the computer plays — cpal's loopback on macOS
//!   (a Core Audio process tap on the default output device; macOS 14.6+, the "System Audio
//!   Recording Only" permission) and the sink monitor on Linux (PipeWire host) — through the
//!   same mix-down and resampler.
//!
//! The microphone has two [`MicMode`]s: `Plain` (the HAL input unit, the default) and
//! `VoiceProcessing` (macOS: Apple's voice-processing I/O unit — echo cancellation, noise
//! suppression — a spike, see `examples/aec_spike.rs`; `OATS_MIC_MODE=vpio`).
//!
//! The cpal callback does the minimum: convert to f32 and mix down to mono, run the
//! [`resample::Resampler`], and send full chunks. All scratch buffers are reused; the only
//! allocation in steady state is the `Vec` handed to the channel, ten times a second.
//!
//! A lost device (unplugged USB mic, the input going away) arrives on cpal's error callback;
//! a terminal error drops that capture's sender there. For the microphone that ends the
//! session once the other lane's sender is gone too ("audio input closed" in the UI); the
//! system-audio lane rebuilds itself (see [`SystemCapture`]). Following a change of the
//! default input device is not done: listening stops and the user starts it again (cpal's
//! `DeviceChanged` — an automatic reroute of a default-device stream — is not terminal and is
//! only logged).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod file_source;
pub mod measure;
pub mod resample;
pub mod system;

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use oats_core::SAMPLE_RATE;
pub use oats_core::{AudioChunk, AudioSourceKind};

pub use crate::file_source::{ChunkSink, FileSource, FileSourceOptions, WavError};
use crate::resample::Resampler;
pub use crate::system::{OutputInfo, SystemCapture, output_is_speakers};

/// Samples per chunk sent to the sink: 100 ms at 16 kHz.
pub const CHUNK_LEN: usize = SAMPLE_RATE as usize / 10;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no audio input device is available")]
    NoInputDevice,
    #[error("no audio output device is available (nothing to record system audio from)")]
    NoOutputDevice,
    #[error("unsupported input sample format: {0}")]
    UnsupportedFormat(String),
    #[error("audio input error: {0}")]
    Cpal(String),
    #[error("system audio capture is not supported on this platform's audio host")]
    LoopbackUnsupported,
    #[error("could not start the system audio thread: {0}")]
    Spawn(std::io::Error),
}

/// The device capture is running on, with its native (pre-resampling) config — for
/// [`MicMode::VoiceProcessing`], the voice-processing unit's client format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// How the microphone is recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MicMode {
    /// The device's plain input (cpal's HAL input unit): what the microphone hears.
    #[default]
    Plain,
    /// macOS: Apple's voice-processing I/O unit (`kAudioUnitSubType_VoiceProcessingIO`):
    /// echo cancellation, noise suppression, optional AGC. A SPIKE, not the default: whether
    /// it removes what OTHER apps play through the speakers is measured by
    /// `examples/aec_spike.rs`. Elsewhere it falls back to [`MicMode::Plain`] with a warning.
    VoiceProcessing,
}

impl MicMode {
    /// `plain` / `hal` → Plain; `vpio` / `voice` / `voice-processing` → VoiceProcessing
    /// (case-insensitive, surrounding whitespace ignored). `None` for anything else.
    pub fn parse(s: &str) -> Option<MicMode> {
        match s.trim().to_lowercase().as_str() {
            "plain" | "hal" => Some(MicMode::Plain),
            "vpio" | "voice" | "voice-processing" | "voiceprocessing" => {
                Some(MicMode::VoiceProcessing)
            }
            _ => None,
        }
    }

    /// `OATS_MIC_MODE` (see [`MicMode::parse`]); unset or empty → Plain, unknown → Plain
    /// with a warning.
    pub fn from_env() -> MicMode {
        match std::env::var("OATS_MIC_MODE") {
            Ok(v) if !v.trim().is_empty() => MicMode::parse(&v).unwrap_or_else(|| {
                log::warn!("OATS_MIC_MODE=\"{v}\" not understood (plain | vpio); using plain");
                MicMode::Plain
            }),
            _ => MicMode::Plain,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MicMode::Plain => "plain",
            MicMode::VoiceProcessing => "vpio",
        }
    }
}

/// Knobs for [`MicMode::VoiceProcessing`] (ignored for `Plain`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceProcessingOptions {
    /// `Some(_)` sets the unit's AGC (`kAUVoiceIOProperty_VoiceProcessingEnableAGC`); `None`
    /// keeps Apple's default (on).
    pub agc: Option<bool>,
    /// Keep the unit's output bus running, fed silence (the unit is full duplex; its echo
    /// reference may need a running output). `false` disables the output bus.
    pub render_silence: bool,
}

impl Default for VoiceProcessingOptions {
    fn default() -> Self {
        VoiceProcessingOptions {
            agc: None,
            render_silence: true,
        }
    }
}

/// Which microphone to open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum InputPolicy {
    /// The computer's own microphone (macOS: the device named "MacBook … Microphone" /
    /// "Built-in …"; elsewhere the default input), falling back to the default input with a
    /// warning. Never silently a Bluetooth headset: its microphone switches it to the call
    /// profile and playback degrades.
    #[default]
    BuiltIn,
    /// Whatever the system's default input is.
    SystemDefault,
    /// The input device with this name (exact, case-insensitive; else the first whose name
    /// contains it), falling back to [`InputPolicy::BuiltIn`] with a warning.
    Named(String),
}

impl InputPolicy {
    /// `OATS_INPUT_DEVICE` (a device name) overrides `self` when set and not empty.
    pub fn with_env(self) -> InputPolicy {
        match std::env::var("OATS_INPUT_DEVICE") {
            Ok(name) if !name.trim().is_empty() => InputPolicy::Named(name.trim().to_owned()),
            _ => self,
        }
    }
}

/// How [`choose_input`] picked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputChoice {
    /// `names[i]`, the device asked for by name.
    Named(usize),
    /// `names[i]`, the built-in microphone.
    BuiltIn(usize),
    /// The system default input; `warn` = why that is not what the policy wanted.
    Default { warn: Option<String> },
}

/// Whether an input device name is the computer's own microphone (macOS naming).
pub fn is_builtin_input(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("macbook") || n.contains("imac") || n.contains("built-in") || n.contains("builtin")
}

/// Pick an input device by `policy` from the input devices' `names` (pure; tested).
/// `platform_names_builtin`: the platform names its built-in microphone recognisably (macOS);
/// elsewhere [`InputPolicy::BuiltIn`] means the default input.
pub fn choose_input(
    policy: &InputPolicy,
    names: &[String],
    platform_names_builtin: bool,
) -> InputChoice {
    let mut note: Option<String> = None;
    if let InputPolicy::Named(want) = policy {
        let w = want.to_lowercase();
        if let Some(i) = names
            .iter()
            .position(|n| n.to_lowercase() == w)
            .or_else(|| names.iter().position(|n| n.to_lowercase().contains(&w)))
        {
            return InputChoice::Named(i);
        }
        note = Some(format!(
            "input device \"{want}\" (OATS_INPUT_DEVICE) not found among {names:?}"
        ));
    }
    if matches!(policy, InputPolicy::SystemDefault) {
        return InputChoice::Default { warn: None };
    }
    if !platform_names_builtin {
        return InputChoice::Default { warn: note };
    }
    if let Some(i) = names.iter().position(|n| is_builtin_input(n)) {
        if let Some(n) = note {
            log::warn!("{n}; using the built-in microphone");
        }
        return InputChoice::BuiltIn(i);
    }
    let why = "no built-in microphone found; using the system default input, which may be a \
               Bluetooth headset (its call profile degrades playback) or another unknown device";
    InputChoice::Default {
        warn: Some(match note {
            Some(n) => format!("{n}; {why}"),
            None => why.to_owned(),
        }),
    }
}

/// A device's display name.
pub(crate) fn device_name(device: &cpal::Device, fallback: &str) -> String {
    device
        .description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| fallback.to_owned())
}

/// The input device [`InputPolicy`] picks on the default host, with its name.
fn pick_input(policy: &InputPolicy) -> Result<(cpal::Device, String), CaptureError> {
    let host = cpal::default_host();
    let devices: Vec<cpal::Device> = host
        .input_devices()
        .map(|d| d.collect())
        .unwrap_or_default();
    let names: Vec<String> = devices
        .iter()
        .map(|d| device_name(d, "(unknown input device)"))
        .collect();
    let choice = choose_input(policy, &names, cfg!(target_os = "macos"));
    let pick = match choice {
        InputChoice::Named(i) | InputChoice::BuiltIn(i) => devices.into_iter().nth(i),
        InputChoice::Default { warn } => {
            let d = host.default_input_device();
            if let (Some(w), Some(d)) = (warn, &d) {
                log::warn!(
                    "audio input: {w} (\"{}\")",
                    device_name(d, "(unknown input device)")
                );
            }
            d
        }
    };
    let device = pick.ok_or(CaptureError::NoInputDevice)?;
    let name = device_name(&device, "(unknown input device)");
    Ok((device, name))
}

/// A running microphone capture. Dropping it stops the stream.
pub struct Capture {
    _stream: Stream,
    device: DeviceInfo,
    mode: MicMode,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture")
            .field("device", &self.device)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl Capture {
    /// Open the microphone [`InputPolicy`] picks (`OATS_INPUT_DEVICE` applied by the caller
    /// through [`InputPolicy::with_env`]) with its default input config (the device's nominal
    /// rate is left alone), start the stream, and send ~100 ms chunks of 16 kHz mono f32,
    /// tagged [`AudioSourceKind::Mic`], to `sink`. Capture stops when the returned [`Capture`]
    /// is dropped; once the receiver is gone the callback stops doing any work.
    pub fn start_mic(
        sink: Sender<AudioChunk>,
        policy: &InputPolicy,
    ) -> Result<Capture, CaptureError> {
        Capture::start_mic_with(
            sink,
            policy,
            MicMode::Plain,
            VoiceProcessingOptions::default(),
        )
    }

    /// [`Capture::start_mic`] in the given [`MicMode`] (the caller applies `OATS_MIC_MODE`
    /// through [`MicMode::from_env`]). Both modes go through the same mix-down and resampler.
    pub fn start_mic_with(
        sink: Sender<AudioChunk>,
        policy: &InputPolicy,
        mode: MicMode,
        vp: VoiceProcessingOptions,
    ) -> Result<Capture, CaptureError> {
        let (device, name) = pick_input(policy)?;
        if mode == MicMode::VoiceProcessing {
            if cfg!(target_os = "macos") {
                return start_voice_processing(sink, policy, device, name, vp);
            }
            log::warn!(
                "audio input: voice processing (OATS_MIC_MODE=vpio) is macOS only; using the \
                 plain input"
            );
        }
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
            "audio input ({policy:?}, mode plain): \"{}\" at {} Hz, {} channel(s), {:?} → {} Hz mono",
            info.name,
            info.sample_rate,
            info.channels,
            format,
            SAMPLE_RATE
        );
        let sink = Sink::new(sink, AudioSourceKind::Mic);
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
            mode: MicMode::Plain,
        })
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }

    /// The mode in effect (a `VoiceProcessing` request falls back to `Plain` off macOS).
    pub fn mode(&self) -> MicMode {
        self.mode
    }
}

/// macOS: the microphone through the voice-processing I/O unit (vendored cpal's
/// `build_voice_processing_input_stream_raw`); the unit's own client format (interleaved f32
/// at its rate and channel count) is read back and fed to the usual converter.
#[cfg(target_os = "macos")]
fn start_voice_processing(
    sink: Sender<AudioChunk>,
    policy: &InputPolicy,
    device: cpal::Device,
    name: String,
    vp: VoiceProcessingOptions,
) -> Result<Capture, CaptureError> {
    let sink = Sink::new(sink, AudioSourceKind::Mic);
    let on_error = sink.clone();
    // The converter needs the unit's format, known only once the unit is built: the callback
    // creates it on first use from the shared cell.
    let format: Arc<Mutex<Option<(u32, u16)>>> = Arc::new(Mutex::new(None));
    let cell = format.clone();
    let mut conv: Option<Converter> = None;
    let mut pending: Option<Sink> = Some(sink);
    let data = move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
        if conv.is_none() {
            let fmt = cell.lock().ok().and_then(|g| *g);
            if let (Some((rate, ch)), Some(s)) = (fmt, pending.take()) {
                conv = Some(Converter::new(rate, ch, s));
            }
        }
        if let (Some(c), Some(samples)) = (conv.as_mut(), data.as_slice::<f32>()) {
            c.push(samples);
        }
    };
    #[allow(unreachable_patterns)]
    let (stream, config, notes) = match device.as_inner() {
        cpal::platform::DeviceInner::CoreAudio(d) => d
            .build_voice_processing_input_stream_raw(vp.agc, vp.render_silence, data, move |err| {
                on_stream_error(&on_error, &err)
            })
            .map_err(|e| CaptureError::Cpal(format!("voice-processing input: {e}")))?,
        _ => {
            return Err(CaptureError::Cpal(
                "voice processing needs the CoreAudio host".to_owned(),
            ));
        }
    };
    if let Ok(mut g) = format.lock() {
        *g = Some((config.sample_rate, config.channels));
    }
    let info = DeviceInfo {
        name,
        sample_rate: config.sample_rate,
        channels: config.channels,
    };
    log::info!(
        "audio input ({policy:?}, mode vpio): \"{}\" through the voice-processing unit at {} Hz, \
         {} channel(s), f32 → {} Hz mono; {}",
        info.name,
        info.sample_rate,
        info.channels,
        SAMPLE_RATE,
        notes.join("; ")
    );
    let stream = Stream::from(stream);
    stream
        .play()
        .map_err(|e| CaptureError::Cpal(e.to_string()))?;
    Ok(Capture {
        _stream: stream,
        device: info,
        mode: MicMode::VoiceProcessing,
    })
}

#[cfg(not(target_os = "macos"))]
fn start_voice_processing(
    _sink: Sender<AudioChunk>,
    _policy: &InputPolicy,
    _device: cpal::Device,
    _name: String,
    _vp: VoiceProcessingOptions,
) -> Result<Capture, CaptureError> {
    Err(CaptureError::Cpal(
        "voice processing is macOS only".to_owned(),
    ))
}

pub(crate) fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    sink: Sink,
) -> Result<Stream, CaptureError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let on_error = sink.clone();
    let mut conv = Converter::new(config.sample_rate, config.channels, sink);
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| conv.push(data),
            move |err| on_stream_error(&on_error, &err),
            None,
        )
        .map_err(|e| CaptureError::Cpal(e.to_string()))
}

/// The error callback: a terminal error closes this capture's sender (its lane).
pub(crate) fn on_stream_error(sink: &Sink, err: &cpal::Error) {
    if is_terminal(err.kind()) {
        log::warn!(
            "{} audio lost: {err}; closing its lane",
            sink.source.as_str()
        );
        sink.close();
    } else {
        log::warn!("{} audio stream error: {err}", sink.source.as_str());
    }
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

/// One capture's end of the shared channel, shared by the data callback and the error
/// callback so the latter can drop it (when every capture's sender is gone the receiver sees
/// `Disconnected`). The lock is uncontended except at the moment of a device loss.
#[derive(Clone)]
pub(crate) struct Sink {
    tx: Arc<Mutex<Option<Sender<AudioChunk>>>>,
    source: AudioSourceKind,
}

impl Sink {
    pub(crate) fn new(tx: Sender<AudioChunk>, source: AudioSourceKind) -> Self {
        Sink {
            tx: Arc::new(Mutex::new(Some(tx))),
            source,
        }
    }

    /// `false` when the sender is closed or the receiver is gone.
    fn send(&self, samples: Vec<f32>) -> bool {
        let Ok(mut guard) = self.tx.lock() else {
            return false;
        };
        let chunk = AudioChunk {
            source: self.source,
            samples,
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
    pub(crate) fn close(&self) {
        if let Ok(mut guard) = self.tx.lock() {
            *guard = None;
        }
    }

    /// Whether the sender was closed (a terminal stream error, or the receiver went away).
    pub(crate) fn is_closed(&self) -> bool {
        self.tx.lock().map(|g| g.is_none()).unwrap_or(true)
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
pub(crate) struct Converter {
    channels: u16,
    mono: Vec<f32>,
    resampled: Vec<f32>,
    chunk: Vec<f32>,
    resampler: Resampler,
    sink: Sink,
    closed: bool,
}

impl Converter {
    pub(crate) fn new(in_rate: u32, channels: u16, sink: Sink) -> Self {
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

    pub(crate) fn push<T>(&mut self, data: &[T])
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
        let mut conv = Converter::new(48_000, 2, Sink::new(tx, AudioSourceKind::System));
        // 1 s of stereo 48 kHz, in 10 ms callbacks.
        let block = vec![0.25f32; 480 * 2];
        for _ in 0..100 {
            conv.push(&block);
        }
        let chunks: Vec<AudioChunk> = rx.try_iter().collect();
        assert!(chunks.iter().all(|c| c.source == AudioSourceKind::System));
        let chunks: Vec<Vec<f32>> = chunks.into_iter().map(|c| c.samples).collect();
        // ~16 000 samples minus the filter look-ahead → 9 full chunks.
        assert_eq!(chunks.len(), 9);
        assert!(chunks.iter().all(|c| c.len() == CHUNK_LEN));
        assert!((chunks[5][800] - 0.25).abs() < 1e-4);
    }

    #[test]
    fn converter_stops_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let mut conv = Converter::new(16_000, 1, Sink::new(tx, AudioSourceKind::Mic));
        conv.push(&vec![0.0f32; CHUNK_LEN]);
        assert!(conv.closed);
    }

    /// A lost device: the error callback closes the sink while the data callback still holds
    /// its clone; the pipeline's receiver must see the channel disconnect.
    #[test]
    fn closing_the_sink_disconnects_the_receiver() {
        let (tx, rx) = mpsc::channel();
        let sink = Sink::new(tx, AudioSourceKind::Mic);
        let mut conv = Converter::new(16_000, 1, sink.clone());
        conv.push(&vec![0.0f32; CHUNK_LEN * 2]);
        assert!(rx.try_recv().is_ok());
        assert!(!sink.is_closed());
        sink.close();
        assert!(sink.is_closed());
        while rx.try_recv().is_ok() {}
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        conv.push(&vec![0.0f32; CHUNK_LEN]);
        assert!(conv.closed, "the data callback goes idle after a close");
        assert!(is_terminal(cpal::ErrorKind::DeviceNotAvailable));
        assert!(!is_terminal(cpal::ErrorKind::DeviceChanged));
        assert!(!is_terminal(cpal::ErrorKind::Xrun));
    }

    #[test]
    fn mic_mode_parses_and_defaults_to_plain() {
        assert_eq!(MicMode::default(), MicMode::Plain);
        assert_eq!(MicMode::parse("vpio"), Some(MicMode::VoiceProcessing));
        assert_eq!(MicMode::parse(" VPIO "), Some(MicMode::VoiceProcessing));
        assert_eq!(
            MicMode::parse("voice-processing"),
            Some(MicMode::VoiceProcessing)
        );
        assert_eq!(MicMode::parse("plain"), Some(MicMode::Plain));
        assert_eq!(MicMode::parse("Hal"), Some(MicMode::Plain));
        assert_eq!(MicMode::parse("aec"), None);
        assert_eq!(MicMode::parse(""), None);
        for m in [MicMode::Plain, MicMode::VoiceProcessing] {
            assert_eq!(MicMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(
            VoiceProcessingOptions::default(),
            VoiceProcessingOptions {
                agc: None,
                render_silence: true
            }
        );
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_built_in_microphone_wins_over_a_headset() {
        let devs = names(&[
            "Alex’s AirPods",
            "MacBook Air Microphone",
            "Alex’s iPhone Microphone",
        ]);
        assert_eq!(
            choose_input(&InputPolicy::BuiltIn, &devs, true),
            InputChoice::BuiltIn(1)
        );
        // an older Mac's naming
        assert_eq!(
            choose_input(
                &InputPolicy::BuiltIn,
                &names(&["Built-in Microphone"]),
                true
            ),
            InputChoice::BuiltIn(0)
        );
        // an iMac names its microphone after itself
        assert_eq!(
            choose_input(
                &InputPolicy::BuiltIn,
                &names(&["Alex’s AirPods", "iMac Microphone"]),
                true
            ),
            InputChoice::BuiltIn(1)
        );
        // no built-in microphone (a Mac mini): the default, with a warning
        assert!(matches!(
            choose_input(&InputPolicy::BuiltIn, &names(&["Alex’s AirPods"]), true),
            InputChoice::Default { warn: Some(w) } if w.contains("Bluetooth")
        ));
        // Linux: the default input, nothing to warn about
        assert_eq!(
            choose_input(&InputPolicy::BuiltIn, &devs, false),
            InputChoice::Default { warn: None }
        );
        assert_eq!(
            choose_input(&InputPolicy::SystemDefault, &devs, true),
            InputChoice::Default { warn: None }
        );
    }

    #[test]
    fn a_named_device_is_found_case_insensitively_else_the_built_in() {
        let devs = names(&[
            "MacBook Air Microphone",
            "Scarlett 2i2 USB",
            "Alex’s AirPods",
        ]);
        let named = |n: &str| InputPolicy::Named(n.to_owned());
        assert_eq!(
            choose_input(&named("scarlett 2i2 usb"), &devs, true),
            InputChoice::Named(1)
        );
        assert_eq!(
            choose_input(&named("airpods"), &devs, true),
            InputChoice::Named(2)
        );
        assert_eq!(
            choose_input(&named("Yeti"), &devs, true),
            InputChoice::BuiltIn(0)
        );
        assert!(matches!(
            choose_input(&named("Yeti"), &names(&["Alex’s AirPods"]), true),
            InputChoice::Default { warn: Some(w) } if w.contains("Yeti")
        ));
        assert_eq!(InputPolicy::default(), InputPolicy::BuiltIn);
    }
}
