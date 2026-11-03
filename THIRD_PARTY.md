# Third-party code

Code and assets from other projects that are part of this repository or its build. Bundled
fonts and images are listed in `assets/ATTRIBUTION.md`.

## Vendored source

- `vendor/fluidaudio-rs/` — **fluidaudio-rs** 0.14.8, Rust bindings for FluidAudio, by
  FluidInference and contributors (<https://github.com/FluidInference/fluidaudio-rs>).
  MIT License (`vendor/fluidaudio-rs/LICENSE`). Modified by OatsNotes (a stateful VAD
  stream); origin and changes in `vendor/fluidaudio-rs/VENDORED.md`.

- `vendor/cpal/` — **cpal** 0.18.2, cross-platform audio I/O, by the RustAudio contributors
  (<https://github.com/RustAudio/cpal>). Apache License 2.0 (`vendor/cpal/LICENSE`). Modified
  by OatsNotes (an explicit loopback stream on macOS, so system audio from a headset never
  opens its microphone); origin and changes in `vendor/cpal/VENDORED.md`. Used through
  `[patch.crates-io]` in the root `Cargo.toml`.

## Fetched at build time (macOS)

- **FluidAudio** 0.14.8 (<https://github.com/FluidInference/FluidAudio>), Apache License 2.0.
  The Swift package `vendor/fluidaudio-rs/Package.swift` pins; `swift build` downloads it into
  the cargo build directory and links it statically into the macOS binary.
- Models it downloads on first run into `~/Library/Application Support/FluidAudio`
  (Parakeet TDT v3, Silero VAD as CoreML conversions) carry their own licenses, listed on
  their Hugging Face model cards. Speaker recognition adds the CoreML conversions in
  `FluidInference/speaker-diarization-coreml`: the pyannote segmentation model
  (pyannote.audio, MIT) and the WeSpeaker ResNet speaker-embedding model (WeSpeaker,
  Apache-2.0; the pretrained weights' terms are on their model cards) — see that repository's
  model card for the exact licenses before redistributing.

Other Rust dependencies come from crates.io / git under their own licenses (`Cargo.lock`).
