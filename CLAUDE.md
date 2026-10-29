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
crates/oats-audio              cpal capture → 16 kHz mono f32 chunks tagged mic/system + own FIR resampler: microphone (InputPolicy, MicMode plain | voice processing) and system audio (SystemCapture)
crates/oats-engine-fluidaudio  macOS engine: Parakeet TDT + Silero VAD (stateful stream, 1 inference / 256 ms) on CoreML via the vendored fluidaudio-rs (THE swappable crate)
crates/oats-status             status-item contract: StatusState in, StatusAction out, NoStatusItem  (no platform code)
crates/oats-status-macos       macOS status item: tray-icon (muda menu), template waveform icon (THE other swappable crate)
crates/oats-ui                 eframe shell: theme (SF Pro from the system), native menu bar (muda), sidebar tree, transcript / Notes / Calendar views
apps/oats-notes                the binary: wires engine + audio + store + status item + brain + UI
vendor/fluidaudio-rs           vendored, pinned fluidaudio-rs 0.14.8 (MIT) + our stateful VAD FFI; NOT a workspace member
vendor/cpal                    vendored cpal 0.18.2 (Apache-2.0) via [patch.crates-io] + an explicit loopback stream and a voice-processing input stream (macOS); NOT a workspace member
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

Data flow: `oats-audio` (cpal callbacks, resample; microphone + system audio) → one mpsc of
`AudioChunk { source: Mic | System, samples }` → `oats-core::pipeline` worker thread (owns the
engine; two lanes, VAD every frame per lane, the model only on closed speech segments) →
`Store` (SQLite, WAL) + `PipelineEvent`s → `oats-ui` (repaints only on events) → every stored
segment is forwarded to the `oats-brain` worker (`BrainCommand::SegmentArrived`), which runs
the `claude` CLI on its own thread and reports `BrainEvent`s back (the UI reloads on `Changed`).

Failure edges: a lost microphone arrives on cpal's error callback; a terminal error
(device / host gone, permission revoked, stream invalidated) drops that capture's sender. A
lane error closes only that lane: the session runs on while the other capture's sender lives
(the system capture rebuilds itself, see below; a microphone that stops delivering while
system audio flows — 30 system chunks in a row — is reported once as a "Microphone: …"
warning); when every sender is gone the pipeline sees
`Disconnected` and ends the session, and the UI stops listening with "audio input closed" in
the status strip (following a newly chosen default input device is not done — the user
starts listening again). A session row the store refuses becomes
`PipelineEvent::ListenFailed` and the UI closes the microphone. Dropping the `PipelineHandle`
joins for at most 2 s (the worker also checks a cancel flag between engine-load steps), so
quitting during a first-run model download doesn't hang. The UI schedules one
`request_repaint_after` until local midnight so "Today" rolls over without other events.

## Audio sources and lanes (`crates/oats-audio`, `oats-core::pipeline`, `oats-core::echo`)

Design and sources: `docs/research-system-audio.md` (phases 1–4 built; phase 5 — per-app taps,
meeting detection from the Core Audio process list, VPIO/AEC — not). Two lanes, never mixed:
**mic** ("me") and **system** ("them": what the computer plays — the other side of a call, a
video). `oats_core::{AudioSourceKind, AudioChunk}` are the shared types.

**Verified on hardware 2026-10-29 (commit bad1dd5+, MacBook Air, output = built-in speakers, "System Audio Recording Only" granted to ClaudeShip):** `levels --system` delivered 99 system chunks (94 with signal); a live `--listen` run stored system segments from a video playing on the Mac and marked the microphone's copies as `echo_of` those rows (log: `mic segment 1 marked as the echo of system segment 3`); the input was "MacBook Air Microphone" and the tap "MacBook Air Speakers" (echo rule on, Auto). Not yet checked: AirPods as the output (none connected). Details: `docs/verification-2026-10-28.md`.

- **Microphone device policy** (`oats_audio::InputPolicy`, default `BuiltIn`; a user decision):
  never capture from a Bluetooth headset unless named — its microphone switches AirPods to the
  call profile and playback degrades. Order: `OATS_INPUT_DEVICE` (a device name: exact,
  case-insensitive, else substring) → the built-in microphone (macOS: name contains
  "MacBook" / "iMac" / "Built-in"; Linux: the default input) → the system default with a `warn!`
  ("may be a Bluetooth headset or another unknown device"). The choice is logged at info
  (`audio input (BuiltIn): "MacBook Air Microphone" …`). `InputPolicy::SystemDefault` /
  `Named` exist for a later UI. Pure picker: `choose_input` (tested).
- **System audio** (`oats_audio::SystemCapture::start(sink)` / `start_with(sink, on_output)`):
  the default OUTPUT device recorded through the same mix-down + resampler. macOS: a Core
  Audio process tap + private aggregate device built by cpal; we call the vendored cpal's
  `build_loopback_stream_raw` (stock cpal opens a headset's microphone when the output device
  also has an input — see `vendor/cpal/VENDORED.md`). Linux: an input stream on the PipeWire
  host's default sink records its monitor (needs cpal's `pipewire` feature, not enabled yet;
  under ALSA / JACK the lane is refused — there the default output device is the default
  input, so it would record the microphone — and the mic carries on). The stream is opened on the
  capture's OWN thread: on macOS the first open blocks inside Core Audio while the
  permission prompt is up (observed), and that must not freeze the UI; dropping the guard
  waits ≤ 0.5 s, then detaches. The thread polls the default output every 2 s (one property
  read; cpal has no change notification for an input stream) and rebuilds the tap when it
  changes (AirPods connect) or after a terminal error (retry ≤ every 30 s); each open reports
  `OutputInfo { name, speakers }` (`output_is_speakers`: MacBook/Built-in/"speaker"/"display",
  not headphones/AirPods/buds/headsets). Known gap: the lane's sample clock is not corrected
  for the < 2 s rebuild gap.
- **TCC permission**: system audio needs "System Audio Recording Only" (System Settings →
  Privacy & Security → Screen & System Audio Recording). Run from a terminal the grant (and
  the prompt) goes to the *responsible* app — the terminal (or whatever spawned it; under
  ClaudeShip the prompt read "claudeship would like access to record your system audio").
  iTerm / VS Code may not prompt: add them by hand. Denial is silent: the tap delivers
  zeros. Permission changes need a restart of that app. A bundled `.app` needs
  `NSAudioCaptureUsageDescription` (in the Info.plist itself) next to
  `NSMicrophoneUsageDescription`. `cargo run -p oats-audio --example levels -- --system`
  is the check (prints both lanes, and "all zeros" when the grant is missing).
- **Pipeline lanes**: `PipelineHandle::start_lanes(rx, ListenOptions { system, no_signal_after
  })` (the old `start(Receiver<Vec<f32>>)` stays: every chunk is the mic lane). Each lane has
  its own pending buffer, `Segmenter`, `SampleClock` (anchored at the lane's first chunk) and
  VAD state (`SpeechEngine::vad_lane`; `NullEngine` → one `EnergyVad` per lane); `transcribe`
  is shared, on the one worker thread, first come first served. All-zero system frames skip
  VAD (the tap delivers digital silence while nothing plays) and all-zero system chunks send
  no `Level` event. `PipelineEvent::Level { rms_db_mic, rms_db_system, in_speech_mic,
  in_speech_system }` (−100 for a lane that is silent / stale > 1 s); the UI meter shows the
  louder lane. `PipelineEvent::SystemAudio { signal: false }` when system audio was expected
  and produced only zeros for 30 s after the start (`true` when sound arrives later).
  Stop / flush closes both lanes (system first). Segments carry `source`.
- **Echo / duplication** (`oats_core::echo`, pure, tested; the user's chosen baseline): on
  speakers the microphone hears the call. Before inserting a MIC segment, the system
  segments reaching into ±45 s of it (`Store::segments_near`) are compared: word-Jaccard OR
  containment ≥ 0.8 over normalised words (lower case, punctuation/apostrophes dropped),
  exact match only when either side has < 4 words; the system rows overlapping the mic
  segment's own span (± 2 s, not the whole window) joined are tried too. A match is STORED
  with `segments.echo_of = <system id>` (nothing is dropped; the rule is auditable: `Store::echoes_of`). The reverse order (system segment stored second) marks the
  stored mic row and sends `PipelineEvent::EchoMarked { segment, echo_of }` (the UI drops the
  row; a queued voice command that became an echo is not run). Echoes are excluded from every listing: the views, counts, the brain's queues and
  transcript rendering; deleting a system segment deletes its echoes. Switch:
  `OATS_SYSTEM_DEDUP=auto|always|never` (default auto; `oats_core::DedupMode`): auto runs the
  rule only while the default output is speakers (`OutputInfo::speakers`), i.e. skips it on
  headphones/AirPods. The binary sets `PipelineHandle::set_echo_dedup` from the system
  capture's `on_output`; with `--system-audio-file` (no device) auto counts as speakers.
- **Echo cancellation spike** (built 2026-10-29, NOT run on hardware yet): does Apple's
  voice-processing microphone (`kAudioUnitSubType_VoiceProcessingIO`: AEC, noise suppression,
  AGC) remove what OTHER apps play through the speakers? `oats_audio::MicMode { Plain,
  VoiceProcessing }` (`Capture::start_mic_with(sink, policy, mode, VoiceProcessingOptions {
  agc, render_silence })`; `OATS_MIC_MODE=plain|vpio`, default plain; off macOS vpio falls
  back to plain with a warn) goes through the vendored cpal's
  `build_voice_processing_input_stream_raw` (the unit's own rate / channel count read back
  and logged; other-audio ducking off on macOS 14+) and the same mix-down/resampler. The
  measurement: `scripts/aec-spike.sh` (renders the sentence with `say -o`, runs
  `examples/aec_spike.rs --mode plain` then `--mode vpio`, each recording 12 s while `say`
  plays the sentence after a 2 s lead, and prints room / playback dBFS, their difference and
  waveform + envelope cross-correlation peaks against the reference side by side). It plays
  sound and opens the mic: never during a call. Outcome: vpio passes when its playback −
  room difference is ≥ 20 dB below plain's and the correlation peaks collapse; then it can
  replace (or back) the text-level echo rule on speakers. VoiceProcessing is NOT the
  default until the spike passes. Pure helpers: `oats_audio::measure` (tested).
- **UI**: "Capture System Audio" (Listen menu, checked) and a "System audio" pill in the
  control bar toggle the `AppPrefs::capture_system` pref (persisted, default on; toggling while
  listening reopens the captures). The status strip reads "listening · mic + system" /
  "listening · mic", plus "system audio: no signal" after the 30 s check; a system capture that
  fails to open shows "System audio: …" as the warning while the mic runs. Transcript rows from
  system audio carry a small "them" tag under the time; echoes never appear.
  `oats_ui::AudioStarter` = `Fn(Sender<AudioChunk>, AudioRequest { system }) ->
  Result<StartedAudio { guard, system, system_error }, String>`.

## The brain (`crates/oats-brain`)

Design and sources: `docs/research-claude-cli-and-interpretation.md`. Four modules plus the
scheduler, all platform-neutral, all testable without the CLI (a fake `claude` shell script):

- `claude_cli`: `ClaudeCli::run(&Request) -> Response` spawns
  `claude -p --output-format json --model <alias> --safe-mode --setting-sources ""
  --strict-mcp-config --tools "" --disable-slash-commands --no-session-persistence
  --max-turns 2 --max-budget-usd X --system-prompt … --json-schema …`, prompt on stdin
  (argv carries only the static system prompt and schema). The child's environment is an
  ALLOWLIST (`child_env`, `env_clear()` first): `HOME`, `USER`, `LOGNAME`, `TMPDIR`, `LANG`,
  `LC_ALL`/`LC_CTYPE`, `TZ`, `CLAUDE_CONFIG_DIR` (when set), `PATH` fixed to
  `/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin` + the claude binary's directory, and
  `CLAUDE_CODE_ENTRYPOINT=oats-notes`, `DISABLE_TELEMETRY=1`, `DISABLE_ERROR_REPORTING=1`,
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `DISABLE_AUTOUPDATER=1`. Nothing else of the
  app's environment reaches it (no `ANTHROPIC_BASE_URL`/`ANTHROPIC_API_KEY`, no
  `CLAUDECODE`/`CLAUDE_CODE_SESSION_ID…` of a parent Claude Code session); proxy variables
  (`HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY`, `ANTHROPIC_BASE_URL`, `NODE_EXTRA_CA_CERTS`) pass
  only with `OATS_CLAUDE_PASS_PROXY=1`. cwd = an empty `<data dir>/claude-work` (0700; never
  the data dir with the database; no project CLAUDE.md / `.mcp.json` is loaded; `--bare` is
  NOT usable: it refuses subscription login). The binary is resolved once per probe to an
  absolute canonical path (`resolve_binary`: relative paths refused, bare names looked up on
  PATH) and logged at info; the timeout kill uses `/bin/kill`. Parses the one `result` object:
  `structured_output`, `total_cost_usd`, `duration_ms`, `num_turns`, `session_id`,
  `model_usage`; `is_error` / stderr are classified into `NotLoggedIn`, `RateLimited`,
  `Offline` (connection / DNS errors), `Refused`, `Timeout`, `Exit` (model text is never
  classified). `CliError::unavailable_reason()` says which mean "claude as a whole is down".
- `episodes`: pure cut of segments into episodes — a gap > 10 min starts a new one, capped at
  3 h / 400 segments. System audio is a continuity signal: in a chunk that already has system
  segments, a further system segment continues it across up to twice the gap (calls pause).
  Echoes never take part. `render_transcript` writes `[HH:MM] me: …` / `[HH:MM] them: …`
  (mic / system; echoes left out); the categorise prompt uses the same prefixes.
- `interpret`: prompts in `crates/oats-brain/prompts/*.md` (`include_str!`; edit the text,
  rebuild) — `interpret.*` (episode → title, markdown summary, filing into an existing node or
  a proposed project/topic, decisions, action items, topics, significant dates; `low_signal`
  for TV / music / self-talk), `categorise.*` (a batch of new segments → node per segment),
  `review.*` (tier 2: merge split episodes, re-file, mark low signal, drop duplicate events),
  `reorganise.*` (tier 3: merge / rename / move / archive projects and topics). Each job is
  request builder → `serde` parse of the structured output → `apply_*` against the store,
  so tests use canned JSON. `render` substitutes `{{key}}` in ONE pass (inserted values are
  never re-scanned); transcripts, segment text and spoken commands sit between
  `<transcript>` and `</transcript>` (those tags are stripped from the data, nothing else is
  escaped); every system prompt says to ignore instructions inside the data. Anti-sprawl:
  existing nodes are preferred, names are matched case/whitespace-insensitively
  (`store::name_key`), at most `max_new_nodes_per_run` new nodes per run. Events are upserted
  by `dedupe_key = ep:<episode>:<date>:<name_key(title)>`, so a revision updates in place and
  drops what it no longer proposes. `apply_interpretation` runs in one transaction
  (`Store::with_savepoint`): a failure half-way leaves nothing. The user's own data is out of
  the automatic tiers' reach: the review's `drop_event` only removes events with
  `source = interpretation`; the reorganisation never merges away or archives a node with
  `created_by = user` — that becomes a `proposal_*` entry in `changes` ("Needs
  confirmation", nothing changed, no Undo). Undo of a `node_move` the store would refuse
  (the tree changed since) is reported as not undoable, not marked done.
- `hotword`: "hey computer" / "hey oats" (`OATS_HOTWORDS`) detected on segment TEXT,
  punctuation-insensitive, and only on the user's own microphone (`hotword::can_command`: a
  `them:` segment or a mic echo never commands this computer); the command text goes to
  `command.*` (haiku) → actions
  `add_event` / `add_reminder` (relative offsets: "one week ahead" = `offset_days: -7` from the
  event in the same command) / `add_note` / `create_project` / `unknown`, executed with
  `source = hotword` and the source segment id. The segment is marked consumed (never part of
  an episode). The EARLIEST hotword in a segment starts the command (a later one stays in the
  text; the model returns actions for both). Bounds, enforced at parse time and again in
  `execute`, and declared in the schema: at most 8 actions, title/name strings 200 chars,
  text/details 2000, `offset_days` clamped to ±366 (all day math goes through
  `TimeDelta::try_days`; a shifted time in a DST gap resolves forward). `create_project`
  respects the per-run new-node cap (`execute_capped`). The prompt's "now" is when the
  command was SPOKEN (the segment's timestamp), so a command worked off a backlog still dates
  "tomorrow" from that day.
- `scheduler`: one worker thread (`Brain::spawn() -> BrainHandle`, same shape as the
  pipeline), one job at a time, blocks on `recv_timeout` until the next due job. Tiers:
  0 reactive (haiku): hotword commands at once; categorisation 3 min after the first new
  segment, batches of 40. 1 rolling (sonnet): interpretation every 15 min
  (`OATS_INTERPRET_EVERY_MIN`) while uninterpreted segments exist, ⌘I on demand; an episode
  that ended less than a gap before the new segments is REVISED with the fuller transcript
  (`episodes.revision`, `last_revised_at`); an episode with system segments gets a
  `meeting: true` hint in the user prompt (`interpret::meeting_hint`: "meetings usually belong
  under Work — a hint, not a rule"), and the system prompt explains `me:` / `them:`; chunks under 80 chars are filed as low-signal
  "Fragments" without a call. 2 review (sonnet): every 4 h or after 8 touched episodes or on
  demand. 3 reorganise (opus): once a day in the 02:00–05:00 local idle window (next window
  when the machine was asleep) or on demand ("Reorganise…"). Budget: `OATS_BRAIN_BUDGET_USD`
  (default $2/day, summed from `brain_runs`; tiers 1–3 skip when hit), 20 runs per hour for
  tiers 1–3, `--max-budget-usd 0.50` per call. Tier 0 counts toward the daily total but has
  its own limits, so voice commands keep working while interpretation pauses: at most
  `OATS_BRAIN_TIER0_PER_HOUR` (default 30) tier-0 runs per rolling hour, and a hard ceiling of
  twice the daily budget; a command held back waits 5 min. A command that fails (bad output,
  refusal — not unavailability) is retried after 1 min, then 2, and given up after 3 tries:
  consumed, and recorded as a `command_failed` warning in `changes`. Every job runs inside
  `catch_unwind`: a panic becomes a failed run + `Warning` + the usual backoff (and a panic
  outside a job restarts the worker after a minute), so the thread never dies silently.
  Logging: info carries counts, costs and durations only; summaries, dictated text, titles
  and CLI output are at debug. Every run → `brain_runs`; every structural change → `changes`
  (before/after JSON; `interpret::undo_change` reverses renames, moves, archives,
  descriptions, episode re-filing; merges are recorded but not undoable).
  Env: `OATS_BRAIN=off`, `OATS_CLAUDE_BIN` (default `~/.local/bin/claude`, then PATH),
  `OATS_CLAUDE_MODEL` (every tier), `OATS_BRAIN_MODEL_0..3` (per tier),
  `OATS_CLAUDE_PASS_PROXY=1` (let proxy variables through to `claude`).

### Offline resilience (built)

Listening and transcription never depend on `claude`. When the CLI is missing, not logged in,
offline or rate limited (`CliError::unavailable_reason`), the brain marks itself unavailable
(`BrainEvent::Availability(Err("not logged in"))` etc.), runs nothing but a free re-probe
(`claude --version` only, with backoff 1 min → 2 → 4 → 8 → 15 min cap, reset by the next
successful call; the first real call after a probe is the probe for login and rate limits),
keeps every new segment and voice command waiting, and reports
`BrainEvent::Backlog { waiting_segments, waiting_commands }`. The status strip then reads
"claude unavailable (not logged in) — N segments waiting" (also in the Notes strip). When it
is back, the backlog is worked oldest-first: queued hotword commands first (an interpret run
that finds hotword segments in the backlog hands them to tier 0 and comes back after them),
then the rolling interpretation in chronological chunks within the budget, each chunk shown
as it lands with a short pause between (`catchup_pause`), "catching up — N segments waiting"
meanwhile, and `Backlog { 0, 0 }` when done. Tested end to end with the fake CLI
(`scheduler::tests::a_logged_out_cli_queues_everything_and_the_backlog_drains_in_order_when_it_returns`).

`cargo run -p oats-brain --example interpret_demo` runs a fake meeting and the hotword example
through the REAL CLI (a few cents) — the end-to-end check.

## The engine contract (`crates/oats-core/src/engine.rs`)

`SpeechEngine`: `load(status)` (download/compile models, first run can take a while),
`vad_frame_len()` / `vad(samples)` (cheap, runs all day; `None` = the core's energy VAD is used),
`vad_lane(lane, samples)` (per-lane VAD state; the default forwards to `vad`; FluidAudio keeps
one `VadStream` per lane — the mic's made by `load`, the system's on its first chunk),
`transcribe(samples)` (one closed segment, ≤ ~30 s, stateless between calls). Engines are built
on the pipeline's own thread via an `EngineFactory`, so they need not be `Send`
(`fluidaudio_rs::VadStream` isn't). The pipeline calls `vad` once per `vad_frame_len()` frame
as audio arrives. FluidAudio's engine runs each 4096-sample frame through a stateful Silero
stream (`FluidAudio::vad_stream_start`, LSTM state carried across calls): one inference per
256 ms, no context re-feed. The contract has no session boundary, so that state also carries
across a listen stop/start (harmless: Silero re-converges within a few chunks). `NullEngine` runs the whole app without a model
(`OATS_ENGINE=null`): energy VAD, placeholder "transcripts".

## Commands

```bash
./ci.sh                                   # fmt --check + clippy -D warnings + cargo test --workspace
cargo run -p oats-notes                   # the app (first run downloads the Parakeet CoreML model, ~600 MB, into ~/Library/Application Support/FluidAudio)
OATS_ENGINE=null cargo run -p oats-notes  # the app without a model (UI work; speech detected, not transcribed)
cargo run -p oats-audio --example levels  # mic sanity check: prints input level for 5 s
scripts/aec-spike.sh [--output "MacBook Air Speakers"]   # echo-cancellation spike: plain vs voice-processing mic while `say` plays (plays sound; not during a call)
cargo run -p oats-audio --example levels -- --system   # both lanes for 10 s (grants/prompts the System Audio Recording permission; "all zeros" = no grant)
cargo run -p oats-notes -- --listen --audio-file speech.wav [--audio-speed 4] [--audio-loop]   # deterministic input instead of the mic (16 kHz mono i16 WAV)
cargo run -p oats-notes -- --listen --audio-file mic.wav --system-audio-file them.wav           # two lanes from two WAVs (the echo rule end to end)
OATS_INPUT_DEVICE="Scarlett 2i2" …         # microphone by name (default: the built-in microphone, never silently a Bluetooth headset)
OATS_SYSTEM_DEDUP=auto|always|never …      # the mic-echo rule (auto: only while the output is speakers)
scripts/perf.sh [rest|idle|speech|stress]    # CPU/memory/energy profile per scenario (release build); see docs/performance-2026-10-28.md
cargo run -p oats-notes -- --selftest out.png [--listening|--notes|--calendar]   # whole window rendered offscreen to a PNG (egui_kittest), seeded demo rows / episode / events — how the shell is verified without a window
cargo run -p oats-notes -- --listen      # start listening as soon as the engine is ready (doesn't change the remembered pref)
OATS_NO_NATIVE_MENU=1 …                   # in-window egui menu bar instead of the macOS menu bar
OATS_NO_STATUS_ITEM=1 …                   # no menu bar extra (closing the window then quits)
OATS_FONT=bundled …                       # Inter instead of the system font
OATS_DATA_DIR=/path …                     # database location override (default ~/Library/Application Support/OatsNotes/oats.sqlite)
OATS_BRAIN=off …                          # no claude calls (the brain thread idles)
OATS_CLAUDE_BIN=/path/claude OATS_CLAUDE_MODEL=haiku OATS_INTERPRET_EVERY_MIN=5 OATS_BRAIN_BUDGET_USD=1 OATS_BRAIN_TIER0_PER_HOUR=30 …   # brain knobs
OATS_CLAUDE_PASS_PROXY=1 …                # pass HTTPS_PROXY / HTTP_PROXY / NO_PROXY / ANTHROPIC_BASE_URL through to claude (off by default)
cargo run --release -p oats-engine-fluidaudio --example vad_parity -- long.wav   # stateful VAD stream vs FluidAudio's batch VAD, chunk by chunk (expect 100 % / delta 0)
cargo run -p oats-engine-fluidaudio --example transcribe_wav -- speech.wav        # engine end to end on a WAV (load, VAD, transcribe)
cargo run -p oats-brain --example interpret_demo   # real claude call: a fake meeting + "hey computer" command, prints the structured results and cost
swift scripts/generate_icon.swift         # regenerate assets/icon_1024.png
```

Build notes: `oats-engine-fluidaudio` → `fluidaudio-rs` (vendored, `vendor/fluidaudio-rs`) →
its build.rs runs `swift build` of the FluidAudio Swift package (minutes the first time; needs
Xcode; fetches FluidAudio from GitHub at the pinned tag). The binary needs the Swift
runtime at run time: `apps/oats-notes/build.rs` adds `-rpath /usr/lib/swift` (without it dyld
fails with "no LC_RPATH's found"). Microphone access: run from a terminal, macOS prompts for
the terminal's mic permission; a bundled .app will need `NSMicrophoneUsageDescription`.
System audio: "System Audio Recording Only", granted to the terminal app when run from one;
a bundled .app needs `NSAudioCaptureUsageDescription` (see "Audio sources and lanes").

## Database (`crates/oats-core/src/store.rs`)

SQLite, WAL, one connection per thread (pipeline writes segments, the brain writes
interpretations, the UI reads and edits). Unix-ms timestamps (UTC; the UI formats local time).
Owner-only on disk (unix): the data dir is created 0700 (`paths::secure_data_dir` at startup,
which also tightens the default location if an older build left it 0755 — an existing
`OATS_DATA_DIR` is left alone; `Store::open` creates a missing parent 0700) and the database
file is created
0600 before SQLite opens it (an existing file and its `-wal`/`-shm` are tightened);
`PRAGMA secure_delete=ON`, and user deletes (`delete_segment`, the Calendar's event delete via
`Store::checkpoint`) run `PRAGMA wal_checkpoint(TRUNCATE)`. Multi-statement writes use a
nested transaction (`Store::with_savepoint`; outermost `BEGIN IMMEDIATE`, inner levels
`SAVEPOINT`, rolled back on error or panic). `episodes.id` is `AUTOINCREMENT` (an older table
is rebuilt in place on open, same schema version), so a reset or merged episode's id is never
handed to a later episode that would adopt its events.
Schema version 3 (`meta.schema_version`; a v1 file is migrated on open — `categories` become
the root `nodes` with the same ids, `segments` is rebuilt — and a v2 file gets two columns
in place, `segments.source TEXT NOT NULL DEFAULT 'mic'` and `segments.echo_of INTEGER NULL`):
`sessions`; `nodes` (ONE tree:
`kind` category | project | topic, `parent_id`, `description`, `created_by` user | auto |
hotword, `archived`; categories seeded Work/Admin/Personal); `segments` (`node_id` = most
specific node, `episode_id`, `interpreted_at`, `categorised`, `source` mic | system,
`echo_of` = the system segment a mic segment repeats — echoes are left out of every listing:
views, counts, `uninterpreted_*` / `uncategorised_*`, `segments_between`, `segments_of_episode`;
ordering by `started_at, id` interleaves the lanes); `episodes` (title, markdown
summary, node, decisions / action items / topics JSON, `low_signal`, model, cost, `revision`,
`last_revised_at`); `events` (`dedupe_key` UNIQUE, `source`, `source_segment_id`,
`source_episode_id`, `confidence`); `reminders` (`event_id`, `due`, `done`); `notes`;
`brain_runs` (tier, kind, model, cost, duration, ok/error, summary); `changes` (kind,
description, before/after JSON, `undone`). `View::Node(id)` means the subtree (recursive CTE).

## Vendored cpal (`vendor/cpal`)

cpal 0.18.2 from crates.io, swapped in by `[patch.crates-io] cpal = { path = "vendor/cpal" }`
(the workspace still says `cpal = "0.18"`), excluded from the workspace, formatting and clippy
off like fluidaudio-rs. Two additions, in `src/host/coreaudio/macos/device.rs`: a public
`Device::build_loopback_stream_raw` that always builds the process-tap path, because stock
cpal taps an output device only when it has NO input — on AirPods (output + microphone) it
would open the headset microphone and switch it to the call profile; and
`Device::build_voice_processing_input_stream_raw` (the echo-cancellation spike: the input
through the VoiceProcessingIO unit, its format read back). Details, evidence and
the update procedure: `vendor/cpal/VENDORED.md`; attribution: `THIRD_PARTY.md`.

## Vendored fluidaudio-rs (`vendor/fluidaudio-rs`)

Why: upstream's VAD FFI resets Silero's recurrent state on every call, which forced a
two-chunk context re-feed (3 inferences per 256 ms). We vendor the crate (pinned, under our
control, nothing upstreamed) and added a stateful stream (`swift/VadStream_ffi.swift` over
FluidAudio's public `VadManager.processStreamingChunk`, plus `VadStream` in `src/lib.rs`).
It is referenced by path from the root `Cargo.toml` and excluded from the workspace
(`exclude = ["vendor"]`), so our workspace lints (`unsafe_code = "forbid"`, …) don't apply;
its own `rustfmt.toml` (formatting disabled) and `[lints.clippy]` (allow) keep `cargo fmt
--all` and `./ci.sh`'s clippy, which both follow path dependencies, off it. Keep its own style
when editing. FluidAudio is pinned by `Package.swift`
(`exact: "0.14.8"`) and `Package.resolved`; the crate version tracks that pin. To update the
pin: follow "Updating" in `vendor/fluidaudio-rs/VENDORED.md` (bump `Package.swift` + crate
version, fix the Swift bridge, run `vad_parity` and `transcribe_wav`, `./ci.sh`), and keep
VENDORED.md's list of our modifications current. Attribution: `THIRD_PARTY.md`.

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
  wake the UI; the brain thread itself blocks until its next due job. `Level` events are
  rate-limited to 4 Hz and suppressed (`PipelineHandle::set_levels_wanted(false)`) while the
  window is hidden or minimized.
- Commit independently as features land; don't add AI co-author lines.

## Roadmap (not yet built)

Stateful VAD (drop the context re-feed): done 2026-10-28, see `vendor/fluidaudio-rs/VENDORED.md`.
Performance notes: `docs/performance-2026-10-28.md`.

Brain: a month-grid calendar view, undo for node merges (recorded, not reversible yet),
reminders that notify (they are only listed), prompt editing without a rebuild, per-tier
budgets in the UI. Elsewhere: diarization (FluidAudio has it); system audio phase 5
(`docs/research-system-audio.md`: per-app taps and "which app is on the call" from the Core
Audio process list, starting the lane only during calls, VPIO/AEC if text dedup is not
enough), an input-device picker in the UI over `InputPolicy`, a "show echoes" audit view,
cpal's `pipewire` feature for the Linux system lane, the two-lane perf run (two WAVs at 1× and
4×); .app packaging with Info.plist (`NSMicrophoneUsageDescription`,
`NSAudioCaptureUsageDescription`, then `LSUIElement`-style options, Dock reopen), the Linux
engine and status-item crates.
