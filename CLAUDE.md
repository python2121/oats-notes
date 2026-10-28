# CLAUDE.md

Guidance for Claude Code when working in this repository. Keep it current: when a change alters
the crate layout, the engine contract, the database schema or the build commands, update the
relevant section in the same change.

OatsNotes is an always-on ambient / meeting transcription app (the user's own take on Granola):
it listens all day, transcribes on device, stores the transcript in SQLite, and a "brain" built
on the locally installed `claude` CLI (the user's own login, no API key) turns it into notes:
episodes with title / summary / decisions / action items, filed into a tree of user categories
(Work / Admin / Personal by default) → auto-created projects → topics, a calendar of
significant dates, and spoken commands ("hey computer, …"). Rust + egui (eframe/wgpu). Primary
target: an M2 MacBook Air, running in the background all day — efficiency is a design goal,
not a nicety.

## Design rule: the whole app is portable, two components are swappable

Exactly two components are platform-specific, and each sits behind a platform-neutral contract
with one implementation crate per platform, picked by the binary:

- the on-device speech engine — `oats_core::SpeechEngine` / `EngineFactory`
  (`oats-engine-fluidaudio` on macOS);
- the status-bar item (menu bar extra) — `oats_status::StatusItem` / `StatusItemFactory`
  (`oats-status-macos`; a KDE/Linux crate would speak StatusNotifierItem, e.g. via `ksni`).

Everything else (audio capture, segmentation, store, pipeline, UI) is shared and must stay
free of platform-only code (the native macOS menu bar in `oats-ui` is the one cfg-gated
exception, with the in-window egui bar as its portable twin). Moving to Linux means writing
one engine crate and one status crate against those contracts, and nothing else — the UI
never changes.

```
crates/oats-core               engine contract, segmenter, SQLite store, pipeline worker, paths  (no platform code)
crates/oats-brain              the brain: `claude` CLI wrapper, episode cutting, prompts (prompts/*.md), hotword commands, tiered scheduler  (no platform code)
crates/oats-audio              cpal microphone capture → 16 kHz mono f32 chunks + own FIR resampler
crates/oats-engine-fluidaudio  macOS engine: Parakeet TDT + Silero VAD on CoreML/ANE via fluidaudio-rs (THE swappable crate)
crates/oats-status             status-item contract: StatusState in, StatusAction out, NoStatusItem  (no platform code)
crates/oats-status-macos       macOS status item: tray-icon (muda menu), template waveform icon (THE other swappable crate)
crates/oats-ui                 eframe shell: theme (SF Pro from the system), native menu bar (muda), sidebar tree, transcript / Notes / Calendar views
apps/oats-notes                the binary: wires engine + audio + store + status item + brain + UI
```

## The status item (`crates/oats-status/src/lib.rs`)

`StatusItem`: `update(&StatusState)` (listening, enabled, tooltip; called only on change),
`poll() -> Vec<StatusAction>` (ToggleListening / ShowWindow / Quit), and
`handle_menu_id(id) -> bool` for platforms where menu events come through a stream shared with
the host. Built on the UI thread inside eframe's creator closure (AppKit needs NSApp) from a
`StatusItemFactory`, which gets a `wake` callback for platforms that deliver events on another
thread. On macOS both `muda` menus (menu bar + status item) share muda's ONE global event
handler: `oats-ui::menu_events` installs it once, and each frame routes ids — `status.*` to
the status item, the rest to the native menu bar. Nothing else may call
`muda::MenuEvent::set_event_handler`.

Window behaviour: while a status item exists, closing the window only hides it (listening
carries on; "Show OatsNotes" brings it back); Quit (⌘Q or the status item's Quit) goes through
`OatsApp::request_quit` and really quits. eframe runs `App::logic` (not `ui`) while the window
is hidden, so everything the status item drives lives in `logic`. Without a status item
(`OATS_NO_STATUS_ITEM=1`, or none on the platform) closing the window quits.

Data flow: `oats-audio` (cpal callback, resample) → mpsc `Vec<f32>` → `oats-core::pipeline`
worker thread (owns the engine; VAD every frame, the model only on closed speech segments) →
`Store` (SQLite, WAL) + `PipelineEvent`s → `oats-ui` (repaints only on events) → every stored
segment is forwarded to the `oats-brain` worker (`BrainCommand::SegmentArrived`), which runs
the `claude` CLI on its own thread and reports `BrainEvent`s back (the UI reloads on `Changed`).

## The brain (`crates/oats-brain`)

Design and sources: `docs/research-claude-cli-and-interpretation.md`. Four modules plus the
scheduler, all platform-neutral, all testable without the CLI (a fake `claude` shell script):

- `claude_cli`: `ClaudeCli::run(&Request) -> Response` spawns
  `claude -p --output-format json --model <alias> --safe-mode --setting-sources ""
  --strict-mcp-config --tools "" --disable-slash-commands --no-session-persistence
  --max-turns 2 --max-budget-usd X --system-prompt … --json-schema …`, prompt on stdin,
  `CLAUDECODE` removed from the env (so it works when launched from inside Claude Code), cwd =
  the data dir (so no project CLAUDE.md / `.mcp.json` is loaded; `--bare` is NOT usable: it
  refuses subscription login). Parses the one `result` object: `structured_output`,
  `total_cost_usd`, `duration_ms`, `num_turns`, `session_id`, `model_usage`; `is_error` /
  stderr are classified into `NotLoggedIn`, `RateLimited`, `Refused`, `Timeout`, `Exit`.
- `episodes`: pure cut of segments into episodes — a gap > 10 min starts a new one, capped at
  3 h / 400 segments.
- `interpret`: prompts in `crates/oats-brain/prompts/*.md` (`include_str!`; edit the text,
  rebuild) — `interpret.*` (episode → title, markdown summary, filing into an existing node or
  a proposed project/topic, decisions, action items, topics, significant dates; `low_signal`
  for TV / music / self-talk), `categorise.*` (a batch of new segments → node per segment),
  `review.*` (tier 2: merge split episodes, re-file, mark low signal, drop duplicate events),
  `reorganise.*` (tier 3: merge / rename / move / archive projects and topics). Each job is
  request builder → `serde` parse of the structured output → `apply_*` against the store,
  so tests use canned JSON. Anti-sprawl: existing nodes are preferred, names are matched
  case/whitespace-insensitively (`store::name_key`), at most `max_new_nodes_per_run` new nodes
  per run. Events are upserted by `dedupe_key = ep:<episode>:<date>:<name_key(title)>`, so a
  revision updates in place and drops what it no longer proposes.
- `hotword`: "hey computer" / "hey oats" (`OATS_HOTWORDS`) detected on segment TEXT,
  punctuation-insensitive; the command text goes to `command.*` (haiku) → actions
  `add_event` / `add_reminder` (relative offsets: "one week ahead" = `offset_days: -7` from the
  event in the same command) / `add_note` / `create_project` / `unknown`, executed with
  `source = hotword` and the source segment id. The segment is marked consumed (never part of
  an episode).
- `scheduler`: one worker thread (`Brain::spawn() -> BrainHandle`, same shape as the
  pipeline), one job at a time, blocks on `recv_timeout` until the next due job. Tiers:
  0 reactive (haiku): hotword commands at once; categorisation 3 min after the first new
  segment, batches of 40. 1 rolling (sonnet): interpretation every 15 min
  (`OATS_INTERPRET_EVERY_MIN`) while uninterpreted segments exist, ⌘I on demand; an episode
  that ended less than a gap before the new segments is REVISED with the fuller transcript
  (`episodes.revision`, `last_revised_at`); chunks under 80 chars are filed as low-signal
  "Fragments" without a call. 2 review (sonnet): every 4 h or after 8 touched episodes or on
  demand. 3 reorganise (opus): once a day in the 02:00–05:00 local idle window (next window
  when the machine was asleep) or on demand ("Reorganise…"). Budget: `OATS_BRAIN_BUDGET_USD`
  (default $2/day, summed from `brain_runs`; tiers 1–3 skip when hit, tier 0 never), 20 runs
  per hour, `--max-budget-usd 0.50` per call. Every run → `brain_runs`; every structural change
  → `changes` (before/after JSON; `interpret::undo_change` reverses renames, moves, archives,
  descriptions, episode re-filing; merges are recorded but not undoable).
  Env: `OATS_BRAIN=off`, `OATS_CLAUDE_BIN` (default `~/.local/bin/claude`, then PATH),
  `OATS_CLAUDE_MODEL` (every tier), `OATS_BRAIN_MODEL_0..3` (per tier).

`cargo run -p oats-brain --example interpret_demo` runs a fake meeting and the hotword example
through the REAL CLI (a few cents) — the end-to-end check.

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
cargo run -p oats-notes -- --selftest out.png [--listening|--notes|--calendar]   # whole window rendered offscreen to a PNG (egui_kittest), seeded demo rows / episode / events — how the shell is verified without a window
cargo run -p oats-notes -- --listen      # start listening as soon as the engine is ready (doesn't change the remembered pref)
OATS_NO_NATIVE_MENU=1 …                   # in-window egui menu bar instead of the macOS menu bar
OATS_NO_STATUS_ITEM=1 …                   # no menu bar extra (closing the window then quits)
OATS_FONT=bundled …                       # Inter instead of the system font
OATS_DATA_DIR=/path …                     # database location override (default ~/Library/Application Support/OatsNotes/oats.sqlite)
OATS_BRAIN=off …                          # no claude calls (the brain thread idles)
OATS_CLAUDE_BIN=/path/claude OATS_CLAUDE_MODEL=haiku OATS_INTERPRET_EVERY_MIN=5 OATS_BRAIN_BUDGET_USD=1 …   # brain knobs
cargo run -p oats-brain --example interpret_demo   # real claude call: a fake meeting + "hey computer" command, prints the structured results and cost
swift scripts/generate_icon.swift         # regenerate assets/icon_1024.png
```

Build notes: `oats-engine-fluidaudio` → `fluidaudio-rs` → its build.rs runs `swift build` of
the FluidAudio Swift package (minutes the first time; needs Xcode). The binary needs the Swift
runtime at run time: `apps/oats-notes/build.rs` adds `-rpath /usr/lib/swift` (without it dyld
fails with "no LC_RPATH's found"). Microphone access: run from a terminal, macOS prompts for
the terminal's mic permission; a bundled .app will need `NSMicrophoneUsageDescription`.

## Database (`crates/oats-core/src/store.rs`)

SQLite, WAL, one connection per thread (pipeline writes segments, the brain writes
interpretations, the UI reads and edits). Unix-ms timestamps (UTC; the UI formats local time).
Schema version 2 (`meta.schema_version`; a v1 file is migrated on open — `categories` become
the root `nodes` with the same ids, `segments` is rebuilt): `sessions`; `nodes` (ONE tree:
`kind` category | project | topic, `parent_id`, `description`, `created_by` user | auto |
hotword, `archived`; categories seeded Work/Admin/Personal); `segments` (`node_id` = most
specific node, `episode_id`, `interpreted_at`, `categorised`); `episodes` (title, markdown
summary, node, decisions / action items / topics JSON, `low_signal`, model, cost, `revision`,
`last_revised_at`); `events` (`dedupe_key` UNIQUE, `source`, `source_segment_id`,
`source_episode_id`, `confidence`); `reminders` (`event_id`, `due`, `done`); `notes`;
`brain_runs` (tier, kind, model, cost, duration, ok/error, summary); `changes` (kind,
description, before/after JSON, `undone`). `View::Node(id)` means the subtree (recursive CTE).

## Conventions

- Workspace lints: `unsafe_code = "forbid"`; clippy `-D warnings`; `oats-core` and
  `oats-brain` (non-test code) deny `unwrap/expect/panic`. No `todo!` left in committed code.
- Unit tests never call the real `claude`: `oats-brain` tests write a fake shell script that
  answers by schema (`claude_cli::tests::fake`, `scheduler::tests::fake_cli`).
- The UI shell's look is ported from the SwiftInvert Rust app (same egui/eframe 0.36 stack,
  `craft-ui` theme helpers, SF Pro read from the system with `opsz`/`GRAD` tweaks, `muda` native
  menu generated from the same `menu_model()` as the in-window fallback). Keep that parity.
- Never request continuous repaints; the app must idle at ~0 % CPU. The pipeline's and the
  brain's `wake` callbacks and menu clicks (the shared muda handler) are the only things that
  wake the UI; the brain thread itself blocks until its next due job.
- Commit independently as features land; don't add AI co-author lines.

## Roadmap (not yet built)

### Offline resilience of the brain (required; big one)

Listening and transcription must NEVER depend on `claude`. If the CLI is missing, not logged
in, rate-limited, or the network is down, the app keeps listening and storing segments, and the
brain catches up when the connection is back: periodic availability re-probe with backoff,
"claude unavailable — N segments waiting" in the status strip, then the backlog is worked
through oldest-first within the budget (hotword commands first, then rolling interpretation,
with relative dates resolved against the segment's own timestamp, not the catch-up time).

Brain: a month-grid calendar view, undo for node merges (recorded, not reversible yet),
reminders that notify (they are only listed), prompt editing without a rebuild, per-tier
budgets in the UI. Elsewhere: diarization (FluidAudio has it), system-audio capture for calls,
.app packaging with Info.plist (then `LSUIElement`-style options, Dock reopen), the Linux engine
and status-item crates.
