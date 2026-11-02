# Vendored: fluidaudio-rs

- **Origin:** the `fluidaudio-rs` crate **0.14.8** from crates.io (upstream
  <https://github.com/FluidInference/fluidaudio-rs>, packaged from commit
  `186d4e780d59d01c4f762a4f1a97a4dbae188dac`). Rust bindings over a Swift bridge to
  FluidAudio.
- **FluidAudio pin:** `Package.swift` → `https://github.com/FluidInference/FluidAudio.git`,
  `exact: "0.14.8"` (`Package.resolved`: revision `56607d90b97de7d95a731200563bf49aa5beef20`).
  `build.rs` fetches and builds it with `swift build` at cargo build time; FluidAudio's source
  is not vendored (Apache-2.0, see `/THIRD_PARTY.md`).
- **Vendored:** 2026-10-28, for OatsNotes. We own this copy; nothing is upstreamed.
- **License:** MIT, as declared by the crate's `Cargo.toml`. Upstream ships no license file,
  so `LICENSE` here is the standard MIT text with the upstream authors as the holders.

It is referenced by path from the workspace root (`fluidaudio-rs = { path =
"vendor/fluidaudio-rs" }`) and is **not** a workspace member (`exclude = ["vendor"]`), so
OatsNotes' workspace lints (`unsafe_code = "forbid"`, …) don't apply to it; `rustfmt.toml`
and a `[lints.clippy]` table keep `cargo fmt --all` and `./ci.sh`'s clippy off it too. It is built with its own `edition = "2021"`.

## Our modifications

1. **Stateful VAD stream** (the reason for vendoring). Upstream's `vad_process_samples`
   resets Silero's LSTM state on every call, so live audio fed 256 ms at a time had to
   re-feed context (3 inferences per chunk). Added:
   - `swift/VadStream_ffi.swift` (new): `BridgeVadStream` holding a FluidAudio
     `VadStreamState`, driven by FluidAudio's **public** `VadManager.processStreamingChunk(_:state:)`
     (exactly one inference per 4096-sample chunk; the stream's start/end hysteresis events
     are ignored, only the probability and the carried model state are used). C exports
     `fluidaudio_vad_stream_create`, `fluidaudio_vad_stream_process_chunk` (exactly 4096
     samples, out-probability and out-is-voice-active; the `init_vad` threshold applies),
     `fluidaudio_vad_stream_reset`, `fluidaudio_vad_stream_destroy`.
   - `swift/FluidAudioBridge.swift`: the bridge remembers the `initializeVad` threshold
     (`vadThreshold`) and gains `makeVadStream()`. Marked `OatsNotes` in comments.
   - `src/ffi/bridge.rs`: the four `extern "C"` declarations, `FluidAudioBridge::vad_stream_create`,
     and the owning `VadStreamHandle` (not `Send`; `Drop` destroys); `src/ffi/mod.rs` re-exports it.
   - `src/lib.rs`: `FluidAudio::vad_stream_start() -> Result<VadStream>` and the public
     `VadStream` (`CHUNK = 4096`, `process_chunk(&mut self, &[f32]) -> Result<VadFrame>`,
     `reset()`; not `Send`; drop frees the Swift object). The stream retains the Swift
     `VadManager`, so it stays valid across `cleanup()` and a later `init_vad`.
   - `README.md`: a short usage section and a note pointing here.
2. **Packaging:** `Cargo.toml` is the crate's original manifest (`Cargo.toml.orig`), not the
   registry-normalised one; dropped `.cargo-ok`, `.cargo_vcs_info.json` and the crate's own
   `Cargo.lock` (unused for a path dependency); added `LICENSE`, this file, and a
   `rustfmt.toml` with `disable_all_formatting = true` (`cargo fmt --all` follows path
   dependencies, and would otherwise reformat upstream code with our settings).
   `Cargo.toml` also gains `[lints.clippy] all = allow` (clippy doesn't cap lints for path
   dependencies, and `./ci.sh` runs it with `-D warnings`).

Everything else (ASR, diarization, TTS, Qwen3, examples, tests, `AGENTS.md`) is as shipped.
3. **Real error text.** Upstream printed the Swift error to stdout and returned -1, so Rust only
   saw "Transcription failed". `FluidAudioBridgeInternal.lastError` (locked) is set in the
   `catch` of `fluidaudio_transcribe_file` / `_transcribe_samples` / `_vad_process_file` /
   `_vad_process_samples` (the `print` stays); new export `fluidaudio_last_error` (strdup'd,
   freed with `fluidaudio_free_string`); `src/ffi/bridge.rs` `last_error()` appends it:
   `transcription failed: <swift error>`. The VAD stream functions have no bridge object and are unchanged.

## Why no FluidAudio version bump

The task was to find the least-risk route to a stateful VAD. At the pinned 0.14.8,
`VadManager.processChunk(_:inputState:)` is `internal`, but FluidAudio 0.14.8 already has a
public streaming API in `Sources/FluidAudio/VAD/VadManager+Streaming.swift`:
`makeStreamState()` / `VadStreamState` (whose `modelState: VadState` is the LSTM hidden/cell
state plus the 64-sample context) and `processStreamingChunk(_:state:config:…)`, which calls
`processChunk(chunk, inputState: state.modelState)` and returns the next state. So the
bridge needs only new FFI; the pin and the ASR API the bridge uses stay exactly as they were
(no Swift package re-resolve, no model re-download). Releases up to v0.18.0 existed at the time.

**Parity** (`cargo run --release -p oats-engine-fluidaudio --example vad_parity -- long.wav`,
327 s of 16 kHz speech, 1 278 chunks): the stream reproduces the stateless batch
`vad_process_samples` (which carries state internally over one call) bit for bit —
100 % `is_speech` agreement, max probability delta 0.0, at threshold 0.6 and 0.85.

## Updating

To move to a newer FluidAudio:

1. Change `exact: "X.Y.Z"` in `Package.swift`, set `version = "X.Y.Z"` in `Cargo.toml` (the
   crate's convention: crate version = FluidAudio version), delete `Package.resolved` (or run
   `swift package resolve` here) and `cargo build -p oats-engine-fluidaudio`.
2. Fix `swift/*.swift` against FluidAudio's API changes. Check that `VadStreamState`,
   `processStreamingChunk` and `VadManager.chunkSize == 4096` still hold.
3. Run `vad_parity` (expect 100 % / 0.0) and `transcribe_wav` on a short WAV, then `./ci.sh`.
4. Update the pin lines above and the engine name string in
   `crates/oats-engine-fluidaudio/src/lib.rs` ("FluidAudio 0.14.8").

To pull a newer upstream fluidaudio-rs instead: copy the new crate over this directory,
re-apply the modifications listed above (they are confined to the files named), and update
this file.
