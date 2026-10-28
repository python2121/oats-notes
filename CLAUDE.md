# CLAUDE.md

Guidance for Claude Code when working in this repository. Keep it current: when a change alters
the crate layout, the engine contract, the database schema or the build commands, update the
relevant section in the same change.

OatsNotes is an always-on ambient / meeting transcription app (the user's own take on Granola):
it listens all day, transcribes on device, stores the transcript in SQLite, and later files what
it heard into user-defined categories (Work / Admin / Personal by default) with help from a local
`claude` CLI pass. Rust + egui (eframe/wgpu). Primary target: an M2 MacBook Air, running in the
background all day — efficiency is a design goal, not a nicety.

## Design rule: the whole app is portable, the model is not

The on-device speech stack is the ONE platform-specific component. Everything else (audio
capture, segmentation, store, pipeline, UI) is shared and must stay free of macOS-only code.
Moving to Linux means writing one new engine crate against `oats_core::SpeechEngine` and
nothing else.

```
crates/oats-core               engine contract, segmenter, SQLite store, pipeline worker, paths  (no platform code)
crates/oats-audio              cpal microphone capture → 16 kHz mono f32 chunks + own FIR resampler
crates/oats-engine-fluidaudio  macOS engine: Parakeet TDT + Silero VAD on CoreML/ANE via fluidaudio-rs (THE swappable crate)
crates/oats-ui                 eframe shell: theme (SF Pro from the system), native menu bar (muda), sidebar, transcript view
apps/oats-notes                the binary: wires engine + audio + store + UI
```

Data flow: `oats-audio` (cpal callback, resample) → mpsc `Vec<f32>` → `oats-core::pipeline`
worker thread (owns the engine; VAD every frame, the model only on closed speech segments) →
`Store` (SQLite, WAL) + `PipelineEvent`s → `oats-ui` (repaints only on events).

## The engine contract (`crates/oats-core/src/engine.rs`)

`SpeechEngine`: `load(status)` (download/compile models, first run can take a while),
`vad_frame_len()` / `vad(samples)` (cheap, runs all day; `None` = the core's energy VAD is used),
`transcribe(samples)` (one closed segment, ≤ ~30 s, stateless between calls). Engines are built
on the pipeline's own thread via an `EngineFactory`, so they need not be `Send`
(`fluidaudio_rs::FluidAudio` isn't). `NullEngine` runs the whole app without a model
(`OATS_ENGINE=null`): energy VAD, placeholder "transcripts".

## Commands

```bash
./ci.sh                                   # fmt --check + clippy -D warnings + cargo test --workspace
cargo run -p oats-notes                   # the app (first run downloads the Parakeet CoreML model, ~600 MB, into ~/Library/Application Support/FluidAudio)
OATS_ENGINE=null cargo run -p oats-notes  # the app without a model (UI work; speech detected, not transcribed)
cargo run -p oats-audio --example levels  # mic sanity check: prints input level for 5 s
OATS_NO_NATIVE_MENU=1 …                   # in-window egui menu bar instead of the macOS menu bar
OATS_FONT=bundled …                       # Inter instead of the system font
OATS_DATA_DIR=/path …                     # database location override (default ~/Library/Application Support/OatsNotes/oats.sqlite)
swift scripts/generate_icon.swift         # regenerate assets/icon_1024.png
```

Build notes: `oats-engine-fluidaudio` → `fluidaudio-rs` → its build.rs runs `swift build` of
the FluidAudio Swift package (minutes the first time; needs Xcode). The binary needs the Swift
runtime at run time: `apps/oats-notes/build.rs` adds `-rpath /usr/lib/swift` (without it dyld
fails with "no LC_RPATH's found"). Microphone access: run from a terminal, macOS prompts for
the terminal's mic permission; a bundled .app will need `NSMicrophoneUsageDescription`.

## Database (`crates/oats-core/src/store.rs`)

SQLite, WAL, one connection per thread (pipeline writes, UI reads). Unix-ms timestamps (UTC;
the UI formats local time). Tables: `sessions`, `segments` (indexed on `started_at`,
`category_id` nullable), `categories` (seeded Work/Admin/Personal), `meta.schema_version`.

## Conventions

- Workspace lints: `unsafe_code = "forbid"`; clippy `-D warnings`; `oats-core` denies
  `unwrap/expect/panic`. No `todo!` left in committed code.
- The UI shell's look is ported from the SwiftInvert Rust app (same egui/eframe 0.36 stack,
  `craft-ui` theme helpers, SF Pro read from the system with `opsz`/`GRAD` tweaks, `muda` native
  menu generated from the same `menu_model()` as the in-window fallback). Keep that parity.
- Never request continuous repaints; the app must idle at ~0 % CPU. The pipeline's `wake`
  callback is the only thing that wakes the UI.
- Commit independently as features land; don't add AI co-author lines.

## Roadmap (not yet built)

Categorisation pass through the local `claude` CLI (rules on how to file transcripts into
categories), summaries, diarization (FluidAudio has it), system-audio capture for calls,
a menu-bar (status item) toggle, .app packaging with Info.plist, the Linux engine crate.
