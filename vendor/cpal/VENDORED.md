# Vendored: cpal

- **Origin:** the `cpal` crate **0.18.2** from crates.io (upstream
  <https://github.com/RustAudio/cpal>, packaged from commit
  `e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7`, see `.cargo_vcs_info.json`).
- **Vendored:** 2026-10-28, for OatsNotes' system-audio lane. We own this copy; nothing is
  upstreamed.
- **License:** Apache-2.0 (`LICENSE`, as shipped).
- **How it is used:** the workspace still depends on `cpal = "0.18"` from crates.io; the root
  `Cargo.toml` replaces it with this copy through `[patch.crates-io] cpal = { path =
  "vendor/cpal" }`. Not a workspace member (`exclude = ["vendor"]`), so the workspace lints
  (`unsafe_code = "forbid"`, …) don't apply; `rustfmt.toml` (formatting disabled) and a
  `[lints.clippy]` table in `Cargo.toml` keep `cargo fmt --all` and `./ci.sh`'s clippy off it.
  Removed from the package: `.github/`, `Dockerfile`, `Cross.toml`, `.cargo-ok`.

## Why

cpal's macOS host records system audio ("loopback": a Core Audio process tap on an output
device inside a private aggregate device) only when `build_input_stream` is called on a device
that has NO input. A headset such as AirPods is one Core Audio device with both an output and a
microphone, so `default_output_device().build_input_stream(..)` would open the headset's
MICROPHONE instead — on Bluetooth that switches it to the call profile and degrades playback,
exactly what OatsNotes must never do. There is no public way to ask for the tap explicitly.

### Symptom without the patch, and the evidence

- Upstream `src/host/coreaudio/macos/device.rs` (0.18.2, in `build_input_stream_raw`):
  `let mut audio_unit = if self.supports_input() { audio_unit_from_device(self, Input) } else
  { LoopbackDevice::from_device(self) … }` — the tap is used only when the device has no
  input channels.
- With the built-in speakers as the default output the stock crate works (the speakers have no
  input). With AirPods (or any headset/USB device with a microphone) as the default output,
  the stock call opens that device's microphone: the "system audio" lane would carry the
  headset mic, and a Bluetooth headset drops to its call profile (mono, low-quality playback)
  for as long as OatsNotes listens. This follows from the code above; it was not reproduced on
  hardware while vendoring (no headset was connected).
- Nothing else is changed: `diff -r` against the registry copy shows only
  `src/host/coreaudio/macos/device.rs` (modifications 1 and 2), `Cargo.toml` (lints table),
  `rustfmt.toml`, this file, and the removed CI files.

## Our modifications

1. `src/host/coreaudio/macos/device.rs`:
   - new `pub fn Device::build_loopback_stream_raw(config, sample_format, data_callback,
     error_callback, timeout)` — always the tap path, never the device's input; and it leaves
     the output device's own physical format / nominal rate untouched (pass the device's
     `default_output_config()`);
   - the private `Device::build_input_stream_raw` takes a `force_loopback: bool` (the trait's
     `build_input_stream_raw` passes `false`: unchanged behaviour).
   `oats-audio/src/system.rs` reaches it through `cpal::Device::as_inner()` →
   `cpal::platform::DeviceInner::CoreAudio(d)` and wraps the result with `cpal::Stream::from`.
2. `src/host/coreaudio/macos/device.rs` (2026-10-29, the echo-cancellation spike): new
   `pub fn Device::build_voice_processing_input_stream_raw(enable_agc: Option<bool>,
   render_silence: bool, data_callback, error_callback) -> Result<(Stream, StreamConfig,
   Vec<String>), Error>` — the device's input through `kAudioUnitSubType_VoiceProcessingIO`
   (coreaudio-rs `IOType::VoiceProcessingIO`) instead of the HAL unit: input enabled on bus 1,
   the device set with `CurrentDevice` (global scope, input element), the output bus kept
   running on silence (`render_silence`) or disabled; the unit's own client rate and channel
   count are read back and only the layout is set (interleaved f32), and the format in effect
   after `initialize` is returned (callers must not assume a rate). Optional properties —
   `kAUVoiceIOProperty_VoiceProcessingEnableAGC` (when `Some`) and
   `kAUVoiceIOProperty_OtherAudioDuckingConfiguration` (advanced ducking off, level Min;
   macOS 14+) — are best effort; what each did is returned as notes.
   `kAUVoiceIOProperty_BypassVoiceProcessing` is never set. Every constant and struct comes
   from `objc2-audio-toolbox` 0.3.2 (features already enabled); none is defined by hand. Its
   input callback duplicates the plain path's (simpler timestamps: capture = callback time).
   `oats-audio`'s `MicMode::VoiceProcessing` reaches it through `cpal::Device::as_inner()`.
   NOT yet verified on hardware: `crates/oats-audio/examples/aec_spike.rs` /
   `scripts/aec-spike.sh` is the check.
3. `Cargo.toml`: the `[lints.clippy]` table at the end; `rustfmt.toml` replaced.

## Updating

Copy the new release from the cargo registry over this directory (keep `VENDORED.md`,
`rustfmt.toml`, the lints table), re-apply modifications 1 and 2, run `./ci.sh`,
`cargo run -p oats-audio --example levels -- --system` with speech playing through the
speakers and through a headset, and `scripts/aec-spike.sh`.
