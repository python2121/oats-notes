//! System audio ("them"): what the computer plays, captured from the default OUTPUT device and
//! delivered like the microphone — mixed down, resampled to 16 kHz mono, 100 ms chunks tagged
//! [`AudioSourceKind::System`] on the shared channel.
//!
//! - **macOS** (14.6+): a Core Audio process tap on the default output device inside a private
//!   aggregate device, built by cpal (`vendor/cpal`'s `build_loopback_stream_raw` — stock cpal
//!   taps only output devices WITHOUT an input, and on a headset such as AirPods would open
//!   the headset's microphone instead, switching it to its call profile). Needs the "System
//!   Audio Recording Only" permission (Privacy & Security → Screen & System Audio Recording);
//!   from a terminal the grant goes to the terminal app, a bundled `.app` needs
//!   `NSAudioCaptureUsageDescription`. Denial is silent: the tap delivers zeros (the pipeline
//!   reports "no signal").
//! - **Linux**: cpal's PipeWire host exposes sinks as duplex devices, and an input stream on a
//!   sink records its monitor — the same `default_output_device()` call (needs cpal's
//!   `pipewire` feature; under plain ALSA the lane fails to open and listening carries on with
//!   the microphone).
//!
//! The tap is bound to the device it was created on, and cpal has no change notification for
//! an input stream, so a monitor thread polls the default output device every
//! [`OUTPUT_POLL`] (one property read) and rebuilds the stream when it changes (AirPods
//! connect) or after a terminal stream error (at most every [`RETRY_AFTER`]). Every (re)open
//! reports the output through the `on_output` callback, which the app uses to switch the echo
//! rule (speakers → on, headphones → off). While the stream is being rebuilt (< 2 s) the lane
//! delivers nothing; its sample clock is not corrected for that gap.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use oats_core::{AudioChunk, AudioSourceKind, SAMPLE_RATE};

use crate::{CaptureError, Converter, DeviceInfo, Sink, device_name, on_stream_error};

/// How often the monitor thread looks at the default output device.
pub const OUTPUT_POLL: Duration = Duration::from_secs(2);
/// Minimum spacing of reopen attempts after a failure on the same device.
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// The output device system audio is being recorded from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputInfo {
    pub name: String,
    /// [`output_is_speakers`]: the microphone can hear it (the echo rule matters).
    pub speakers: bool,
}

/// Whether an output device (by name) is a loudspeaker the microphone hears: the computer's
/// built-in speakers ("MacBook Air Speakers", "Built-in Output") or another device named a
/// speaker or display audio. Headphones, AirPods and other headsets — and unknown names —
/// are not (on those the echo rule is skipped).
pub fn output_is_speakers(name: &str) -> bool {
    let n = name.to_lowercase();
    let headphones = [
        "headphone",
        "airpods",
        "buds",
        "headset",
        "beats",
        "earphone",
    ]
    .iter()
    .any(|h| n.contains(h));
    !headphones
        && ["macbook", "built-in", "builtin", "speaker", "display"]
            .iter()
            .any(|s| n.contains(s))
}

/// A running system-audio capture (and its device monitor). Dropping it stops both.
///
/// The stream is opened on the capture's own thread, never the caller's: on macOS the first
/// open BLOCKS inside Core Audio while the "would like access to record your system audio"
/// permission prompt is up, and that must not freeze the UI.
pub struct SystemCapture {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
    output: Arc<Mutex<Option<OutputInfo>>>,
}

impl std::fmt::Debug for SystemCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemCapture")
            .field("output", &self.output())
            .finish_non_exhaustive()
    }
}

/// How long dropping a [`SystemCapture`] waits for its thread (which may be stuck behind a
/// permission prompt) before detaching it.
const JOIN_TIMEOUT: Duration = Duration::from_millis(500);

/// One open loopback stream.
struct Running {
    _stream: Stream,
    sink: Sink,
    id: Option<cpal::DeviceId>,
}

impl SystemCapture {
    /// Record the default output device into `sink` (chunks tagged
    /// [`AudioSourceKind::System`]), following default-output changes.
    pub fn start(sink: Sender<AudioChunk>) -> Result<SystemCapture, CaptureError> {
        Self::start_with(sink, |_| {})
    }

    /// [`Self::start`], calling `on_output` (from the capture thread) with the output device
    /// each time a stream opens: the first time and after every reopen. Returns at once; an
    /// open that fails is logged and retried (see the module docs).
    pub fn start_with(
        sink: Sender<AudioChunk>,
        on_output: impl Fn(&OutputInfo) + Send + 'static,
    ) -> Result<SystemCapture, CaptureError> {
        let output = Arc::new(Mutex::new(None));
        let shown = output.clone();
        let report = move |o: &OutputInfo| {
            if let Ok(mut g) = shown.lock() {
                *g = Some(o.clone());
            }
            on_output(o);
        };
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("oats-system-audio".to_owned())
            .spawn(move || monitor(sink, &stop_rx, &report))
            .map_err(CaptureError::Spawn)?;
        Ok(SystemCapture {
            stop: Some(stop_tx),
            thread: Some(thread),
            output,
        })
    }

    /// The output device being recorded (`None` until the first stream is open).
    pub fn output(&self) -> Option<OutputInfo> {
        self.output.lock().ok().and_then(|g| g.clone())
    }
}

impl Drop for SystemCapture {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let deadline = Instant::now() + JOIN_TIMEOUT;
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                log::warn!(
                    "system audio: the capture thread is still opening the stream (a \
                     permission prompt?); not waiting for it"
                );
            }
        }
    }
}

fn output_info(device: &DeviceInfo) -> OutputInfo {
    OutputInfo {
        name: device.name.clone(),
        speakers: output_is_speakers(&device.name),
    }
}

/// The monitor thread: owns the stream; reopens it when the default output changes or the
/// stream died; ends (dropping the stream and its sender) when the guard is dropped.
fn monitor(
    master: Sender<AudioChunk>,
    stop_rx: &mpsc::Receiver<()>,
    on_output: &dyn Fn(&OutputInfo),
) {
    let host = cpal::default_host();
    // the device the last failed open was for, and when
    let mut failed: Option<(Option<cpal::DeviceId>, Instant)> = None;
    let mut running = match open(&master) {
        Ok((r, device)) => {
            on_output(&output_info(&device));
            Some(r)
        }
        Err(e) => {
            log::warn!("system audio: could not open: {e}");
            failed = Some((
                host.default_output_device().and_then(|d| d.id().ok()),
                Instant::now(),
            ));
            None
        }
    };
    if stop_rx.try_recv() != Err(mpsc::TryRecvError::Empty) {
        return; // stopped while opening
    }
    // a message or a dropped guard ends the loop
    while let Err(RecvTimeoutError::Timeout) = stop_rx.recv_timeout(OUTPUT_POLL) {
        let current = host.default_output_device().and_then(|d| d.id().ok());
        let reopen = match (&running, &failed) {
            (Some(r), _) => r.id != current || r.sink.is_closed(),
            (None, Some((id, at))) => *id != current || at.elapsed() >= RETRY_AFTER,
            (None, None) => true,
        };
        if !reopen {
            continue;
        }
        let why = match &running {
            Some(r) if r.sink.is_closed() => "the stream stopped",
            Some(_) => "the default output device changed",
            None => "retrying",
        };
        log::info!("system audio: reopening ({why})");
        running = None; // the old tap goes first
        match open(&master) {
            Ok((r, device)) => {
                failed = None;
                running = Some(r);
                on_output(&output_info(&device));
            }
            Err(e) => {
                log::warn!("system audio: could not reopen: {e}");
                failed = Some((current, Instant::now()));
            }
        }
    }
    log::info!("system audio: stopped");
}

/// Open and start a loopback stream on the default output device.
fn open(master: &Sender<AudioChunk>) -> Result<(Running, DeviceInfo), CaptureError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(CaptureError::NoOutputDevice)?;
    let name = device_name(&device, "(unknown output device)");
    let id = device.id().ok();
    let supported = device
        .default_output_config()
        .map_err(|e| CaptureError::Cpal(e.to_string()))?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let info = DeviceInfo {
        name,
        sample_rate: config.sample_rate,
        channels: config.channels,
    };
    let sink = Sink::new(master.clone(), AudioSourceKind::System);
    let stream = build_loopback(&device, &config, format, sink.clone())?;
    stream
        .play()
        .map_err(|e| CaptureError::Cpal(e.to_string()))?;
    log::info!(
        "system audio: \"{}\" at {} Hz, {} channel(s), {:?} → {} Hz mono ({})",
        info.name,
        info.sample_rate,
        info.channels,
        format,
        SAMPLE_RATE,
        if output_is_speakers(&info.name) {
            "speakers"
        } else {
            "not speakers: no echo rule in auto mode"
        }
    );
    Ok((
        Running {
            _stream: stream,
            sink,
            id,
        },
        info,
    ))
}

/// macOS: an explicit loopback (process tap) stream, never the device's microphone.
#[cfg(target_os = "macos")]
fn build_loopback(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    sink: Sink,
) -> Result<Stream, CaptureError> {
    let on_error = sink.clone();
    let mut conv = Converter::new(config.sample_rate, config.channels, sink);
    let data = move |data: &cpal::Data, _: &cpal::InputCallbackInfo| match format {
        SampleFormat::F32 => {
            if let Some(s) = data.as_slice::<f32>() {
                conv.push(s);
            }
        }
        SampleFormat::I16 => {
            if let Some(s) = data.as_slice::<i16>() {
                conv.push(s);
            }
        }
        SampleFormat::I32 => {
            if let Some(s) = data.as_slice::<i32>() {
                conv.push(s);
            }
        }
        _ => {}
    };
    if !matches!(
        format,
        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32
    ) {
        return Err(CaptureError::UnsupportedFormat(format!("{format:?}")));
    }
    #[allow(unreachable_patterns)]
    match device.as_inner() {
        cpal::platform::DeviceInner::CoreAudio(d) => d
            .build_loopback_stream_raw(
                *config,
                format,
                data,
                move |err| on_stream_error(&on_error, &err),
                None,
            )
            .map(Stream::from)
            .map_err(|e| CaptureError::Cpal(e.to_string())),
        _ => Err(CaptureError::LoopbackUnsupported),
    }
}

/// Elsewhere: an input stream on the output device — the sink's monitor on cpal's PipeWire
/// host; other hosts refuse it.
#[cfg(not(target_os = "macos"))]
fn build_loopback(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    sink: Sink,
) -> Result<Stream, CaptureError> {
    match format {
        SampleFormat::F32 => crate::build::<f32>(device, config, sink),
        SampleFormat::I16 => crate::build::<i16>(device, config, sink),
        SampleFormat::I32 => crate::build::<i32>(device, config, sink),
        other => Err(CaptureError::UnsupportedFormat(format!("{other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speakers_versus_headphones() {
        assert!(output_is_speakers("MacBook Air Speakers"));
        assert!(output_is_speakers("Built-in Output"));
        assert!(output_is_speakers("LG UltraFine Display Audio"));
        assert!(output_is_speakers("Sonos Speaker"));
        assert!(!output_is_speakers("Alex’s AirPods"));
        assert!(!output_is_speakers("External Headphones"));
        assert!(!output_is_speakers("Galaxy Buds2 Pro"));
        assert!(!output_is_speakers("Jabra Headset"));
        assert!(!output_is_speakers("Scarlett 2i2 USB"));
    }
}
