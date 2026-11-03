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
crates/oats-core               engine contract, segmenter, SQLite store, pipeline worker, echo rules (text + audio), speaker voiceprint matching (`speakers`), `when` date parser, paths  (no platform code)
crates/oats-brain              the brain: `claude` CLI wrapper, episode cutting, prompts (prompts/*.md), hotword commands, tiered scheduler  (no platform code)
crates/oats-audio              cpal capture → 16 kHz mono f32 chunks tagged mic/system + own FIR resampler: microphone (InputPolicy, MicMode plain | voice processing) and system audio (SystemCapture); the voice-command ding (chime)
crates/oats-engine-fluidaudio  macOS engine: Parakeet TDT + Silero VAD (stateful stream, 1 inference / 256 ms) + per-segment speaker embeddings (pyannote segmentation + WeSpeaker) on CoreML via the vendored fluidaudio-rs (THE swappable crate)
crates/oats-status             status-item contract: StatusState in, StatusAction out, NoStatusItem  (no platform code)
crates/oats-status-macos       macOS status item: tray-icon (muda menu), template waveform icon (THE other swappable crate)
crates/oats-ui                 eframe shell: theme (SF Pro from the system), native menu bar (muda), sidebar tree, transcript / All notes / Calendar (month grid) / Todos / Media views, node view (notes + projects), reminder + confirmation pop-ups, drag-to-merge (merge.rs)
apps/oats-notes                the binary: wires engine + audio + store + status item + brain + UI
vendor/fluidaudio-rs           vendored, pinned fluidaudio-rs 0.14.8 (MIT) + our stateful VAD FFI and speaker-span FFI; NOT a workspace member
vendor/cpal                    vendored cpal 0.18.2 (Apache-2.0) via [patch.crates-io] + an explicit loopback stream and a voice-processing input stream (macOS); NOT a workspace member
```

## The UI's node view (`crates/oats-ui/src/notes.rs::node_view`)

Categories are folders. Selecting a category / project / topic (`View::Node(id)`) shows its
path as the header, its child projects (topics, under a project) as clickable rows with note
count and last activity, the notes (`Store::episodes_under`, newest first, as collapsed rows — see the project landing page below) of
the subtree, and "Show transcript (N segments)" — the raw segments filed under the subtree,
collapsed by default (`NotesState::show_transcript`, reset on every view change). Transcripts
(`View::Transcripts`, ⌘1; there is no Today view — old prefs saying `Today` / `All` load as it) stays a transcript view, paged by day (below). **All notes** (`View::Notes`, name kept for the saved pref; once "Conversations"; ⌘3, `Command::ShowNotes`; `notes::notes_view`) lists EVERY conversation note, filed or not (`Store::conversations(300)`: `kind != 'media'`, newest first — media notes never, they live in the Media view), under the brain strip: a "Notes · Expand all · Collapse all · Show N low-signal" header line, then day headings (`day_of` / `day_heading`) over the same collapsed 24 pt rows as the project page (`note_row` via the shared `note_list`; a click toggles the card inline, `NotesState::open_notes`). Each row carries a filing chip before its topic chips (`ListOpts::filing`, `notes::filing_of`): the PROJECT's name (a topic maps to its project, `project_of`; hover = the note's full path; click → `NotesAction::SelectNode(project)`), or "Unfiled" in the tertiary colour (no node or a bare category; not clickable); both are AccessKit buttons. Low-signal notes are hidden unless `NotesState::show_low_signal` (reset on every view change in `set_view`; the header's "Show N low-signal" / "Hide low-signal", absent when N = 0); a day whose notes are all hidden draws no heading; when every loaded note is low-signal the view reads "N low-signal notes hidden" + the toggle; with no notes at all "No notes yet. Notes appear here as the brain interprets what it hears." Row marks (`NotesState::marks: RowMarks { today_start, latest }`, set by the app every frame; on the project page too): a note that started today draws its date and time range in pure white, the most recent note (`Store::latest_note`) its title, date and time range in the accent; the row's hover text adds " · today" / " · most recent". The time-range column has a fixed minimum width so titles line up. The app menus are File, Edit, Listen, View, Brain (the one formerly titled "Notes" is now "Brain"). The sidebar shows a muted
"No projects yet" row under a category without projects. `--selftest out.png --work` renders
the Work view offscreen.

**Project landing page** (`node_view` for `kind == project`): header, then "Next steps" — the
action items of the project's episodes (newest note first, low-signal notes skipped, capped at
`NEXT_STEPS_CAP` = 12 with "Show all N"), each with a checkbox, owner/due and a "from <note> · <date>"
link (`NotesAction::GoToNote` → `OatsApp::open_note_page`: the note's OWN single-note view
under its project, exactly as clicking its sidebar row — the project's layer is opened, its
ancestors expanded, the row selected and scrolled into view; a note with no sidebar row — unfiled,
under a bare category, low-signal, or older than the project's five listed notes — is opened
inline and flashed on the page via `pending_focus` instead);
"No next steps yet" when empty. The checkbox
(`NotesAction::SetActionItemDone` → `Store::set_action_item_done`: `done_at` in the item's
JSON, not a hand edit, so `user_edited_at` stays) completes an item like a todo: completed
items go after the open ones (newest completion first), struck through in `t.text_tertiary`,
and only behind "Show all N" (N = open + completed), which shows whenever more than the cap
are open OR anything is completed (`notes::next_steps_shown`); unticking reopens. Expanded,
"Show fewer" is drawn twice: where "Show all" sat (after the items the collapsed list shows,
so it toggles back under the same pointer) and at the bottom of the full list. A note
card's action items carry the same checkbox and strike-through. Then "Notes": every note NEWEST FIRST
(the rest of the app is newest first), one collapsed 24 pt row each (triangle, date, time range,
title, topic chips); a click toggles the full `episode_card` inline (`NotesState::open_notes`,
not persisted; "Expand all · Collapse all"). Topics and the transcript toggle follow.
Categories and topics use the same layout (Next steps → child rows → collapsed Notes →
transcript): Next steps are a subtree roll-up (`episodes_under` / `items_filed_under`); on a
category each step carries a "→ Project" chip (`project_of`: the item's own node else the
note's home, a topic mapped to its project; clickable → `SelectNode`; no chip on project /
topic pages); an empty category skips the section (projects and topics keep "No next steps
yet"); the Notes are the same collapsed rows (`project_notes`) of the whole subtree.

**Cross-filed items and the click chain**: "Next steps" also lists, INLINE in the same list
(sorted newest note first, deduped by note + text), the action items filed under this project
from notes homed elsewhere (`Store::items_filed_under(node)`, loaded into
`OatsApp::filed_items` for a Node view). A row's click (`GoToNote`) on such an item jumps to the
note itself (`open_note_page`: the single-note view under its home project, selected in the
sidebar; All notes with the row opened and the card flashed when unfiled). Inside a
note card each decision / action item with a `segment_id` is a link ("Show in transcript at
2:31 pm", `timefmt`) → `NotesAction::ShowSegment` → `jump_to_segment`; an item filed under
another project than its note carries a "→ Commerce project" chip (opens that project). So:
Commerce page → dev-call note → transcript line. Editing a note keeps the filing / source / completion of
items whose text is unchanged (`EditDraft::to_content_keeping`).

**Move to… on notes**: a note card's title and a project page's note row have a context menu
with "Move to ▸" (every node by full path) and "New project…" (an inline name field in the
card; parent = the note's category, else Work, else the first category; created
`created_by = user` + a `node_create` change), both via `Store::move_episode` (an
`episode_move` change, undoable like `episode_node`).

**Sidebar notes layer**: a project's row has a triangle (closed by default, persisted as
`AppPrefs::sidebar_episodes_open`) listing its last `SIDEBAR_EPISODES` = 5 non-low-signal notes
(newest first, 22 pt rows: truncated title, "29 Oct" at the right), then its topics (the
triangle now folds both). **Where today's notes went**: a note that started today (local day) draws its title and date in pure white (hover text adds " · today"), and so do the project, topic and category rows above it and the All notes row (the only sidebar home of unfiled notes) when an unfiled non-media note started today (`sidebar::today_nodes` once per frame from `sidebar_episodes`; `Store::unfiled_episodes_since`, reloaded with `reload_sidebar_episodes`); it rolls over at local midnight with the existing midnight repaint. **The most recent note** (`Store::latest_note`: the newest non-low-signal conversation note, any day) is drawn in the ACCENT colour instead, and so is the path above it (`sidebar::latest_nodes`; `TodayMarks::latest` / `latest_nodes`, hover text " · most recent"); when it is unfiled the All notes row is the accent one (`latest_conversations`). `sidebar::Mark { Plain, Today, Latest }` is the row colour (`hover_row` / `episode_row`). Clicking one sets `OatsApp::focus_episode` (a UI mode, not a `View`):
`notes::episode_view` shows that note's card under a breadcrumb "Work › Commerce project ›
29 Oct" (node names are links). Escape, the project's sidebar row, any other navigation, or a
breadcrumb link returns to the project page. `--selftest out.png --project [--expanded]
[--project-name "Development team call"]` (the seed has a dev-call note with an item and a
decision filed under Export rewrite).

**Editing notes**: every card has Edit next to Re-interpret. Edit mode (`notes::edit_card`)
shows title, summary (markdown source), decisions (one per line), action items (one per line,
`text | owner | due`, parsed leniently) and topics (comma separated); Save (⌘S) sends
`NotesAction::SaveEdit` → `Store::update_episode_content`, Cancel / Escape discards. A saved
note carries an "edited" chip. Schema v5: `episodes.user_edited_at` (added in place on open).
The brain never rewrites such a note: `interpret_chunk` does not pick an edited episode as the
prior to revise (the new speech becomes its own episode), `apply_interpretation` ignores an
edited prior, and the review never merges an edited episode away; node re-filing, low-signal
marks and events still apply. "Re-interpret" on an edited note first asks "Replace your edits?".
**Delete** (edit view, after "Move to…", danger text) opens a pop-up at the button (`NotesState::confirm_delete`, `notes::confirm_delete_popup`): "Delete “title”? The transcript stays; the note will not be re-created. Undo not available."; confirming sends `NotesAction::DeleteEpisode` → `Store::delete_episode` (one savepoint: segments detached but keep `interpreted_at`, so they are never re-interpreted into a new note; the note's interpretation events and their todos go, hotword / user events stay; an `episode_delete` change with the old note as `before`, `by: user`), then `checkpoint()`, a 4 s toast, and the view falls back to the project page. Not on the undo stack.

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

Failure edges: SIGINT / SIGTERM quit gracefully (the `ctrlc` handler sets a flag that
`logic` turns into `request_quit`: pipeline stopped, session closed, prefs saved; a second signal
exits at once), and sessions a crash left open are closed at startup
(`Store::close_dangling_sessions`, called once before the pipeline spawns). A lost microphone arrives on cpal's error callback; a terminal error
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
`request_repaint_after` until local midnight so the Transcripts view's "today" page rolls over without other events.

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
- **Audio-level echo check** (`oats_core::echo_audio`, pure, tested; runs BEFORE the model —
  echoes were most of the two-lane CPU): the session keeps the last 45 s of system audio
  (`SystemRing`, indexed by the system lane's sample clock; filled unless the knob is never).
  A closing MIC segment is compared with the system audio over its span ± 500 ms: 10 ms RMS
  envelopes (linear), the mic's cut to its active frames (> peak − 20 dB and > its 10th
  percentile + 9 dB, so shared on/off edges don't count), Pearson correlation per lag in
  ± 500 ms. Echo = all of: peak ≥ `ECHO_ENV_THRESHOLD` 0.6 for ≥ 3 s of active audio, rising
  linearly to 0.9 at 1 s (`required_peak`: short envelopes correlate by chance — unrelated
  synthetic voices reach 0.84 at 1 s, 0.53 at 3 s); mic at most +6 dB louder than the
  aligned system audio; at most 250 ms of active mic audio where the system played nothing
  (200 ms reverb allowance — a segment that runs on into the user's reply is not an echo).
  Undecidable (< 1 s active, a flat envelope such as a steady tone, silent system audio)
  = transcribe. A suspected echo is NOT transcribed: linked at once to a system segment
  already stored within ± 2 s (an empty mic row, `echo_of` set, `confidence` NULL, no
  `PipelineEvent`), else pending until one is stored, discarded after 45 s of audio
  (`PipelineHandle::set_echo_pending_timeout`; at session end the rest). The text rule stays
  the backstop for everything transcribed. Knob: `OATS_ECHO_AUDIO=auto|always|never`
  (`EchoAudioMode`, read at `Pipeline::spawn`, `PipelineHandle::set_echo_audio`; auto = while
  the text rule is on, i.e. on speakers; never = old behaviour). Counters
  (`pipeline::EchoStats`: `mic_transcribed`, `echo_skipped_by_audio` = `echo_linked` +
  `echo_discarded` + still pending, `echo_marked_by_text`) are logged at info when the session
  ends and readable via `PipelineHandle::echo_stats()` (no UI event). Calibrated on synthetic
  signals only (`docs/performance-2026-10-28.md` addendum: live check pending). Known gap: a
  system tap rebuild (< 2 s without chunks) shifts the system lane's sample clock behind the
  wall clock by the gap (the clock only resyncs after a sleep), and the shift accumulates per
  rebuild; once past the ± 500 ms lag window the check finds no correlation and segments are
  transcribed as before (lost saving, not lost speech) until listening restarts.
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
- **UI**: the control bar has two independent lane pills, "Mic" and "System audio" (the
  Listen menu's checked "Microphone" / "System Audio" items are the same toggles). Listening is
  on whenever at least one lane is; turning the last one off stops it. A change while listening
  restarts the captures AND the pipeline session (stop, then a new session once the pipeline
  reports Idle via `auto_listen`; lanes are not changed inside a session) with
  `AudioRequest { mic, system }` / `ListenOptions { mic, system }`; system-only listening never
  opens the microphone, and the "microphone stopped" warning is off when `mic` is false.
  `AppPrefs::lanes` (`Lanes { mic, system }`, default both) remembers the last lane set; the
  master switch (⌘L / "Start Listening", the status item's toggle, `--listen`, listening
  resumed at launch) opens that set, "Stop Listening" turns every lane off (the set is kept).
  The old `capture_system` pref is read once into `lanes.system` on load and no longer
  written. A starter that opens nothing (system-only and the tap fails) returns `Err`. The status item's tooltip reads "Listening · mic + system" /
  "… · mic" / "… · system" (`state_label`; the status strip itself no longer repeats the
  state or the lanes), and the System audio pill's tooltip says "nothing has come through for 30 s" after the 30 s check; a system capture that
  fails to open shows "System audio: …" as the warning while the mic runs. Transcript rows from
  system audio carry a small "them" tag under the time; echoes never appear.
  `oats_ui::AudioStarter` = `Fn(Sender<AudioChunk>, AudioRequest { system }) ->
  Result<StartedAudio { guard, system, system_error }, String>`.
  **Result 2026-10-29 (built-in speakers): partial** — playback by other apps attenuated ~15 dB (+24 dB → +9 dB over the room floor), envelope correlation unchanged; the unit ran the mic at 44.1 kHz. The text-level echo rule stays primary; `OATS_MIC_MODE=vpio` stays opt-in. See `docs/verification-2026-10-28.md`.

## UI chrome and preferences

Control bar (34 pt): Listen pill, "System audio" pill, level meter, and the engine loading /
failure line only (the engine name is in the status strip, not repeated; cut with … in what the
search icon leaves, never pushing it out). Status strip (18 pt):
left = engine · segment count · brain line (NOT the listening / idle state nor the open lanes —
the control bar's pills show those; only "loading…" / "failed" and "on a call" are spelled
out; no "system audio: no signal" — that is the System audio pill's tooltip now — and no
"▶ <media>" line — the Media view has it; the engine name carries no chip); right = the warning, else the last voice
command, else EMPTY (the data-directory path is no longer shown). Neither runs over the other
(`app::status_widths`): both in full when they fit, else the right text keeps its width up to
half the strip and the left one is cut with …, then the right one too; a cut text reads in full
on hover.

**Reflow in a narrow window** (the minimum is 800×520 with a 180–360 pt sidebar, so the content
column can be 440 pt wide and ~440 pt tall): nothing may widen a scroll area's content past its
viewport (egui grows a parent's max rect to fit an overflowing child, and every later row is
then laid out too wide). Rows with variable text either wrap (`widgets::inset_wrapped`: a
wrapping row with a hanging left inset and a right inset, or a wrapping child of the remaining
width), give the text an explicit width, or truncate; trailing chips go into the same wrapping
flow (after the last line, else on the next one; a chip is one piece: `Label::truncate`, or
`notes::chip_max` cut with …). Card headers measure their buttons (`card_buttons_width`): the
title wraps in what is left, or the buttons take their own right-aligned row when the title
would get under `CARD_TITLE_MIN` (150 pt). Collapsed note rows keep the title ≥ 110 pt (chips
dropped, the filing chip cut with …) and, under day headings below 520 pt, drop the repeated
date. The calendar grid's rows shrink (78 → 40 pt, fewer chips, "+N") so the
day panel keeps 110 pt. Toast cards are 340 pt, narrower in a narrow column (≥ 240). The search
field takes what its count leaves (at least half the bar; the count is cut with …). The kittest
`every_view_fits_a_constrained_window` builds each view at 660×520 (`oats_ui::selftest::harness`)
and fails on any AccessKit node outside the window or two widgets drawn over each other.

Search (View → "Find…", ⌘F; `Command::Search`, `oats-ui/src/search.rs`): a 36 pt bar under the
control bar (focused at once, match counts, × icon) and a results view that replaces the current
view while open: Notes (episode cards, matches marked), Calendar (events), Transcript (lines),
from `Store::search(query, 200)` (case-insensitive substring, echoes excluded, re-run only when
the text or the data changes). Esc (also from the field) or × closes it and the previous view is
back; ⌘F again re-selects the field. Clicking a note opens its node view (All notes if unfiled), an
event opens Calendar, a line opens Transcripts around that segment, scrolled to the row and flashed
(same jump as the calendar and todos, see below).
`--selftest out.png --search TEXT` renders it.

**Tags in search**: topics (the chips on notes) are the tags. `Store::search` also returns
`tags: Vec<(name, notes)>` (`Store::search_tags`, from `episodes.topics_json`, case- and
spacing-insensitive, one tag named by its most common spelling, at most `TAG_LIMIT` = 40) and
`media` (plays whose title or URL matches, calls left out). One word: tags containing it.
Several words: tags containing ANY word (one-letter words dropped), ranked by how many words
match, then notes, then name — "commerce cod delay" lists "COD delay" before "Commerce"; the
other sections keep matching the whole query as a substring. The results show a **Tags**
section ABOVE the others (chips "name · N"; a click opens the Topic view, "Tagged “name”"),
and a **Media** section (a row opens the Media view at that play, paged back until loaded,
scrolled to and flashed). `--selftest out.png --search "commerce cod delay"` seeds tagged notes.

**Media view** (`View::Media`, ⌘6, View → Media, the last row under Views;
`oats-ui/src/media.rs`): the watch history from `media_plays` (calls left out), newest first,
under day headings, **each media listed ONCE, on its last play** — the Calendar's rule
(`Store::media_history_page_latest(before, limit)`: `media_history_page`'s paging plus the
`NOT EXISTS` scrub of `media_plays_between_latest`; identity = the URL, for music the URL AND
the title, i.e. the track; meetings and URL-less plays are never scrubbed), so a video watched
on three days shows once, under the last day, with that play's time and duration.
`collapse_plays` (consecutive plays of one media ≤ 10 min apart → one `MediaEntry`) still runs
on each page, a no-op on deduplicated data. A row (nothing expands): 96×54 thumbnail slot,
title (medium face), "YouTube · Firefox" / "Spotify" / "Spotify · Firefox", "2:05–2:41 pm ·
36 min", the URL as a link (`ctx.open_url`, tooltip = full URL). Right-click: Open in browser,
Copy link, Summarise and Re-interpret (only on a row with a media note, see below; greyed while
the brain is unavailable or busy with that note), Mark as a call (each play of the row;
`Store::mark_play_as_call`), Show transcript (only when the plays have transcribed segments:
`Store::first_segment_of_plays` → the usual jump), Delete (every play of the media,
`Store::plays_of_same_media`, so no older play resurfaces; `Store::delete_media_plays` + a WAL
checkpoint). Paged 200 rows at a time; the next (older) page loads when the bottom "Loading
older…" marker comes within 1.5 viewports. Empty: "Nothing watched yet." **Media notes live
here** (they have no node, there is no Media category) — without a card: a row whose plays were
transcribed into a media note (`Store::episode_of_play` — the play's media-stamped system
segments' `kind = media` episode; `episodes_of_plays` batch, `plays_of_episode` the inverse;
`OatsApp::media_notes`, reloaded with the pages and on every data change while the view is
open) offers the note's Summarise / Re-interpret in its context menu (`media::note_menu` →
`MediaAction::Summarise` / `Reinterpret`, handed to `apply_notes` as `NotesAction`s: the
cards' own handlers); the note's Edit / Delete are not offered here. A jump to a play (search)
or a media note (search, Topic view: `jump_to_episode` → `OatsApp::open_media_note`, its last
play) goes through `show_media_play`: the play is mapped to its media's last play
(`Store::latest_play_of`), that row paged in, scrolled to and flashed. `--selftest out.png
--media` (seeds a video watched on two days, listed once, and a transcribed video with its
note, whose row is jumped to).

**Thumbnails are opt-in (privacy rule)**: the app makes no network calls of its own except the
model download and the `claude` CLI. Preferences "Fetch video thumbnails (contacts YouTube's
image servers)" (`AppPrefs::fetch_thumbnails`, default OFF) is the single exception: while it
is on, a YouTube play's `https://i.ytimg.com/vi/<video_id>/mqdefault.jpg` is fetched ONCE by
`/usr/bin/curl -q --silent --fail --max-time 5 --proto =https --output <file>` (no HTTP crate;
skipped when curl is missing) on the "oats-thumbs" thread into `<data dir>/thumbs/<id>.jpg`
(dir 0700, file 0600; ids are `[A-Za-z0-9_-]` only, `thumbs::thumb_path`), decoded there (the
`image` crate, png + jpeg) and cached in memory as a texture for the session; failures are not
retried until the next launch. While off nothing is read, fetched or started
(`thumbs::Thumbs::get(.., enabled = false)`), and the slot is a flat `t.field` rectangle with a
▶ (also for non-YouTube plays). Deleting plays deletes a cached file only when no remaining
play shows that video. Fetches log at debug with the video id only.

**Undo / redo** (`oats-ui/src/undo.rs`; Edit ▸ Undo / Redo, ⌘Z / ⇧⌘Z): an in-memory,
in-session history of the user's own quick edits, at most 50 steps, nothing persisted and
nothing written to `changes` (the brain's log keeps its own Undo). Covered: an action item
ticked / unticked (the first `done_at` comes back), a todo's done / snooze / due / text (its
`TodoState` before and after, `Store::restore_todo_state`; the pop-ups' Done / Snooze too), a
hand edit of a note (content + `user_edited_at`, `Store::restore_episode_content`), and a note
moved by "Move to…" when the tree resolved the name (`Store::restore_episode_node`, no change
row). Not recorded (and not clearing it): deletes, merges, brain moves and runs, node
re-parenting, preferences. A new edit clears the redo list; undo / redo apply straight to the
store, reload, and pop a 4 s toast ("Undo: Mark complete", "Nothing to undo", "Couldn't undo:
… (it no longer exists)" — that step is dropped). The menu items read "Undo Mark complete" and
grey out when empty, but carry NO accelerator (`UNDO_SHORTCUT` / `REDO_SHORTCUT` are not in
`SHORTCUTS`): AppKit would hand ⌘Z to the menu before a focused text field, whose own undo must
keep working. Instead `OatsApp::undo_keys` reads ⇧⌘Z then ⌘Z from egui's input every frame,
native menu or not, only while `!egui_wants_keyboard_input()` (the ⌘S-in-edit-mode pattern).

**Sidebar keyboard** (`sidebar::key_nav`, pure steps `navigable_rows` / `nav_decide`): ↓ / ↑ select the next / previous row in draw order (Views, then the visible tree with an open project's notes before its topics; no wrap; with the selection not in the list ↓ takes the first, ↑ the last) and navigate as a click would. → opens a collapsed node (or a project's note layer), else enters its first child; ← closes an open node, else selects the parent. The keys are consumed only while the sidebar is visible, no text field has focus (`egui_wants_keyboard_input`) and neither Preferences nor the merge confirmation is open; the new row is scrolled into view (`SidebarState::reveal_selected`).

Preferences (File → "Preferences…", ⌘,; `Command::Preferences`): an `egui::Window` centred in
the window below the in-window menu bar, 12 pt clear of every edge (`constrain_to`), up to
560 pt wide (narrower when the window is), its whole body in a vertical `ScrollArea` (the Apps
table has none of its own) so it is never taller than the window, with one field, "Your name", saved on Enter / blur / close via `Store::set_user_name`
(a blank name clears it). Stored in the database, `meta` key `user_name`
(`Store::user_name()` / `set_user_name()`). The brain re-reads it at the start of every job
(`BrainConfig::user_name`, `Worker::refresh_user_name`), so no restart or plumbing is needed;
`OATS_USER_NAME` overrides the stored value for scripted runs. It is injected by
`interpret::personalise` (called in `Worker::call`) into the system prompts of interpret,
categorise, review and command through the `{{user_name_note}}` placeholder, which expands
`prompts/user_name.md` (with `{{user_name}}`: "The person using this app is …; `me:` is …;
action items for them are owner \"me\"") or, with no name, a neutral sentence. It is NOT a
hotword; hotwords are unchanged.

## Todos, reminder pop-ups and the calendar (`oats-ui`: `todos`, `toast`, `calendar`)

- **Todos view** (⌘5, View ▸ Todos, sidebar "Todos"; `View::Todos`): `Store::todos(All)`
  order — open dated (soonest first), open undated (manual `position`), completed (most recent
  first) at the BOTTOM, crossed through (`RichText::strikethrough`, text_tertiary, "done
  <date>"). Open rows: checkbox, text, due ("in 20 min" / "today, 15:00" / "tomorrow, 09:00" /
  "in 3 days" / "Thu 12 Dec, 09:00"; overdue in `t.danger`; `todos::due_label`), "snoozed
  until …", node chip, source chip. "+ Add todo" text field plus a "due" field (Enter in
  either adds, source user; `todos::parse_add`): the due text goes through
  `oats_core::when::parse` (lenient and pure, generic over the zone: `2026-11-04 [09:00]`,
  `11/4[/2026]`, `4 nov` / `nov 4`, `today`, `tomorrow`, weekday names / `next friday` (next
  occurrence), `in 2 hours` / `in 3 days` / `in 1 week`, each optionally `at 9`, `at 9:30`,
  `9am`, `14:00`, `noon`; date-only = 09:00 local; a DST gap resolves forward; unreadable =
  `None`, never a guess). The text field also takes a trailing ` @ <date>` ("call landlord @
  tomorrow 9am"; the due field wins if both). An unreadable date is shown in `t.danger` and
  nothing is added. Right-click: Mark Done/Not Done, Edit Text, Set due… (inline field,
  pre-filled `YYYY-MM-DD HH:MM`; Enter saves via `Store::set_todo_due`, empty clears, Escape
  cancels; the calendar's day panel menu has no inline editor, so only Clear due there), Clear
  due, Snooze ▸ 1 hour / 1 day / 1 week, Clear Snooze, Show in Transcript, Delete. The due
  label on a row is clickable and opens the same editor. Dated todos show in the calendar grid
  and day panel (one shared `todos` list, reloaded after every edit). Reloads on `BrainEvent::Changed` / `Command` and after
  local edits.
- **Reminder pop-ups** (`toast::toasts`, an `egui::Area` anchored RIGHT_TOP, 12 pt below the
  control bar, cards 340 pt, corner radius 10, `t.chrome` + 1 px `t.button_border`, the popup
  shadow): every open todo with `COALESCE(snoozed_until, due) <= now` (`Store::due_todos`),
  newest due on top, max 3 + "and N more due" (→ Todos). Card: painted bell, text, "Due …",
  Snooze 1h · 1d · 1w, **Done** (accent). The × in the corner is ALSO Done (tooltip "Done —
  mark complete"): the user's rule is "dismiss = mark complete", so there is no
  dismiss-without-completing. Scheduling: `refresh_due` (after every todo change, on load, on
  "Show OatsNotes" from the status item) reads `due_todos` + `next_due_at`; `logic()`
  schedules ONE `request_repaint_after` for the next due time and refreshes when it passes —
  no polling, no continuous repaint. Pop-ups live in the window only (a hidden window shows
  them when re-shown).
- **Confirmations**: each `BrainEvent::Command` confirmation becomes a lighter card in the
  same stack (check glyph, text, ×), max 3, closed after 4 s by one scheduled repaint
  (hovering holds it open), and the ding plays once per command when the
  `AppPrefs::command_sound` pref is on (Preferences ▸ "Play a sound for voice commands",
  default on). The ding: `oats_audio::chime::play` (880 Hz then 1320 Hz, 80 ms each, 10 ms
  fades, −18 dBFS, default OUTPUT device, own short-lived thread, failures logged), handed to
  the UI as `oats_ui::Chime` via `OatsApp::set_chime` (the UI doesn't depend on oats-audio;
  the selftest and tests have none). The system-audio lane hears it through the tap: 160 ms is
  under the segmenter's 400 ms minimum speech, so it never becomes a segment.
- **Calendar** (⌘4): month grid, ‹ month year › + Today, weeks Monday–Sunday (a pref later),
  outside days dimmed, today outlined in accent, selected day tinted; per cell up to 3
  one-line chips (events `t.accent_alpha`, dated todos `t.button` — `t.field` is invisible on
  the canvas — completed todos struck through) and "+N"; a hotword event's own same-titled
  todo is folded into the event in the grid (`calendar::items_by_day`). The day panel below
  lists the selected day's events and todos in full with their right-click actions.
- **Transcripts paging** (`transcript::TranscriptPages`, `Store::segments_page(before, limit)` /
  `segments_after(after, limit)`, keyed on `(started_at, id)`): ONE contiguous range of rows,
  paged by row count (`PAGE_ROWS` = 1000; a 1000-row page lays out in ~4 ms per frame in the
  headless profile), not by day. Day headings are drawn only where the local day changes
  between consecutive rows, so an empty day never shows and nothing is special about "today"
  (there is no Today view). Initial load = the newest page. The top marker ("Loading earlier…"
  / "Beginning of history" when a page came back short) and, after a jump, the bottom marker
  ("Loading later…") request their page (`TranscriptAction::LoadEarlier/LoadLater`) once within
  `PREFETCH_VIEWPORTS` (1.5) viewport heights of the visible area (`near_viewport`); one
  synchronous page per frame, repaint until satisfied; nothing is requested on the first layout
  of a new range (stick-to-bottom has not applied yet) or while a jump scrolls. Rows inserted
  above the viewport shift content: the row at the viewport top is recorded each frame, and the
  next layout moves the scroll offset by that row's shift (skipped when stuck to the end) and
  discards the pass (`request_discard`), so nothing paints at the old offset. No eviction. Live
  segments append only while the range ends at the newest row (`has_later == false`), and
  stick-to-bottom is off while `has_later`. A jump to a row not loaded replaces the range with
  `PAGE_ROWS / 2` rows either side of it, then pages outward both ways until the tail is
  reached and live appends resume. **Jump to now**: a pill in the Transcripts header next to
  "Show media" (disabled while the newest row is loaded AND the viewport is at the end:
  `TranscriptPages::at_end`); `Command::JumpToNow` reloads the newest page when `has_later`
  (`reset_pages`), drops a pending focus and sets `TranscriptPages::jump_to_end`, which the next
  layout turns into the scroll offset of the end in one frame (stored + `request_discard`, no
  animation, no page load on the way; the stick-to-bottom then holds it). Shortcuts: ⌘1 Transcripts, ⌘3 All notes, ⌘4 Calendar,
  ⌘5 Todos (⌘2 is unused).
- **Jump to the transcript**: clicking a calendar chip / day-panel row / a dictated todo's text
  resolves its source (`source_segment_id`; an interpreted event: the first segment of its
  `source_episode_id`; a todo without one: its event's source), switches to Transcripts, loads a window of
  rows around that segment (replacing the loaded range when it is not in it), scrolls to the row and flashes it 1.5 s
  (`transcript::TranscriptFocus`, consumed when the row is laid out; stick-to-bottom is off
  while it runs). No source / deleted: "No transcript for this item" in the status strip.

## The brain (`crates/oats-brain`)

Design and sources: `docs/research-claude-cli-and-interpretation.md`. Five modules plus the
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
  3 h / 1500 segments; inside a call (the chunk's meeting goes on) only the 3 h cap cuts, never
  the segment count (a lively call's system lane yields ~8–10 short segments a minute: the old
  400 cap split a 50-min call). The scheduler's revise check is the cutter's (`joined_one`), so
  a call's continuation revises it up to 3 h. Calls bound episodes (`episodes::cut`): the first system segment stamped
  `meeting` after anything that is not that call starts a new episode at its start, and mic
  segments up to 2 min before it join it (the user joining); a call ends when 3 min pass with
  no meeting-stamped system segment while other speech continues (that speech starts a new
  episode); two different calls (`media_ref`; the row carries no app) back to back are two
  episodes. The scheduler revises an open episode only when prior + new segments still cut as
  ONE chunk, so a call is revised only by its own call and speech after it never revises it.
  Media is never merged with conversation. System audio is a continuity signal: in a chunk that already has system
  segments, a further system segment continues it across up to twice the gap (calls pause).
  Echoes never take part. `render_transcript` writes `[HH:MM] me: …` / `[HH:MM] them: …`
  (mic / system; echoes left out); `render_transcript_full(…, speakers)` (the scheduler's, via
  `interpret_request_full(…, speakers)` / `categorise_request_with`) labels a line by its voice
  instead (`oats_core::speaker_label`: `Priya:`, `them 2:`, `voice 3:`, `me:`); the categorise
  prompt uses the same labels.
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
  escaped); every system prompt says to ignore instructions inside the data. Filing rule: categories are
  folders; an episode is filed under a PROJECT (an existing one, or `new_project` with
  `parent_id` = the category, named after the concrete subject), the category alone only for
  miscellaneous low-signal content; categorisation prefers the most specific existing project
  and never creates nodes. Anti-sprawl:
  existing nodes are preferred, names are matched case/whitespace-insensitively
  (`store::name_key`), at most `max_new_nodes_per_run` new nodes per run. Events are upserted
  by `dedupe_key = ev:<date>:<name_key(title)>` (no episode id), so a revision updates in place and
  drops what it no longer proposes. `apply_interpretation` runs in one transaction
  (`Store::with_savepoint`): a failure half-way leaves nothing. The user's own data is out of
  the automatic tiers' reach: the review's `drop_event` only removes events with
  `source = interpretation`; the reorganisation never merges away or archives a node with
  `created_by = user` — that becomes a `proposal_*` entry in `changes` ("Needs
  confirmation", nothing changed, no Undo). Undo of a `node_move` the store would refuse
  (the tree changed since) is reported as not undoable, not marked done.
  **Item filing and provenance**: every decision and action item in the output has `project`
  (the name of an EXISTING project/topic it concerns, null = the note's home) and `at` (the
  `HH:MM` of its transcript line). `apply_interpretation` maps `project` →
  `interpret::resolve_item_node` (by `name_key`, or `[id] name`; unknown names, categories and
  the note's own home → `None`; items never create nodes) and `at` → `interpret::resolve_at`
  (the first non-echo segment of the chunk in that local minute, else the nearest within
  10 min → `at_ms` = its start, `segment_id`). Stored in the episode JSON: action items
  `{text, owner?, due?, node_id?, at_ms?, segment_id?, done_at?}` (`done_at` = ticked
  complete, unix ms; a revision — `apply_interpretation` with a prior, so also Rewrite /
  Summarise — carries it to the item with the same trimmed text, as does the edit form); decisions a plain string when bare
  (older rows, still read) else `{text, node_id?, at_ms?, segment_id?}` (`oats_core::Decision`).
  A revision's prior shows each item's `(project: …) (at: HH:MM)`. `merge_nodes` refiles items
  to the survivor, `delete_node` back to their note's home. **Series rule** (prompt): a
  recurring or named meeting series (standing team call, weekly sync, 1:1 with a named
  person, a committee) files the NOTE under a project named for the series ("Development
  team call", "1:1 with Priya"; existing, else `new_project`) and its ITEMS under the
  projects they concern; a one-off conversation about one project files under that project.
  `interpret::series_hint` adds `series: true` + the earlier notes and where they are filed
  when the chunk's meeting link (`meeting_key`, non-empty) was heard on an EARLIER local day
  (`Store::episodes_with_meeting_ref`); the scheduler builds requests with
  `interpret_request_full(…, series)`.
- `hotword`: "hey computer" / "hey oats" (`OATS_HOTWORDS`) detected on segment TEXT,
  punctuation-insensitive, and only on the user's own microphone (`hotword::can_command`: a
  `them:` segment or a mic echo never commands this computer); the command text goes to
  `command.*` (haiku) → actions
  `add_event` / `add_todo` / `add_reminder` (relative offsets: "one week ahead" =
  `offset_days: -7` from the event in the same command) / `add_note` / `create_project` /
  `unknown`, executed with `source = hotword` and the source segment id. TODOS: a reminder is
  a todo with a due time — `add_todo` and its alias `add_reminder` both write a todo
  (`Executed::Todo`, "Todo: … · due …"). Due times: `in_minutes` ("in 2 hours / 2 days / 3
  weeks", counted from when the command was SPOKEN; whole days keep the clock time across
  DST, `dates::after_minutes`; an "in N …" phrase the model put in `date` is parsed as a
  fallback, `dates::parse_in_phrase`), else `date` (+ `time`; a date alone is due at 09:00
  local, `dates::default_due_time`), else undated (`add_reminder` needs a time: "reminder
  without a date"). Every spoken `add_event` also gets its own todo due at its start (all-day:
  09:00 that day) so "property taxes are due 12/12/2026" pops up that morning; a relative
  reminder off an all-day event is due at 09:00 too. Interpretation-extracted events get NO
  todo. Todos are deduped per segment (`hotword:<seg>:reminder:<day>:<text>` /
  `…:todo:<text>`) and a new one is logged in `changes` as `todo_create` (not undoable).
  `BrainEvent::Command` carries `confirmations` (`Executed::confirmation`: "Todo added: …",
  "Reminder set: … — Thu 12 Dec, 09:00", "Event added: …", "Note filed: …" (60 chars),
  "Project created: …"). The segment is marked consumed (never part of
  an episode). The EARLIEST hotword in a segment starts the command (a later one stays in the
  text; the model returns actions for both). Bounds, enforced at parse time and again in
  `execute`, and declared in the schema: at most 8 actions, title/name strings 200 chars,
  text/details 2000, `offset_days` clamped to ±366 (all day math goes through
  `TimeDelta::try_days`; a shifted time in a DST gap resolves forward). `create_project`
  respects the per-run new-node cap (`execute_capped`). The prompt's "now" is when the
  command was SPOKEN (the segment's timestamp), so a command worked off a backlog still dates
  "tomorrow" from that day.
- `moveto` ("Move to…" on a note card / single-note view / project-page note row, and on
  transcript rows): right-click → "Move to…" opens an inline field (placeholder "project or
  topic name, or describe a new one…"; Enter submits, Escape cancels; `oats-ui::move_to`).
  The tree submenu and "New project…" are gone. The app first resolves the text locally with
  no model call (`oats_brain::resolve_local`, pure: a full path "Work › Commerce project" /
  "work/commerce", else an exact name, else a unique prefix, else a unique substring among
  projects and topics; archived nodes ignored) and moves at once (`episode_move` change for a
  note, `set_segment_node` for a segment; toast "Moved to Work › Commerce project").
  Ambiguous or new text sends `BrainCommand::ResolveMove { target: MoveTarget::Episode(id) |
  Segment(id), text, current_node }`: a tier-0 (haiku) job with `prompts/move.{system,user}.md`
  and schema `{action: existing|create, node_id, new_project{name,parent_id,description},
  reason}` (the prompt gets the tree, the item's title/summary or text, its current path). The
  scheduler executes it in one savepoint (`moveto::execute_move`): an unknown node / parent,
  or an empty name is a warning and nothing changes; a new project is
  `created_by = user` with a `node_create` change; the result is a `BrainEvent::Command`
  confirmation ("Moved to …" / "Created Admin › X and moved the note") so the usual toast and
  ding show. Kinds in `Queued` / `Settled`: `move` (episode id) and `move_segment`. Tier-0
  caps apply. While it runs the field is disabled with a spinner; on failure (or when the wait
  ends) it reopens with the text and a warning toast. Needs claude: without it only names in
  the tree work. Also in a note's EDIT view: "Move to…" next to Save / Cancel opens the same
  field under the form; a move does not touch the draft (Escape in the field closes only the
  field), the edit follows the note to its new project (single-note view, draft kept;
  `OatsApp::follow_moved_episode`) and Save still writes it. Sidebar: a project's note rows
  have the card's context menu ("Move to…" — the field opens under the row,
  `MoveDraft::sidebar` — and "Mark as a call" / "Mark as media" when they apply); project /
  topic rows have "Rename…" and "Move to…" (re-parent: an inline field resolved by
  `resolve_local` only, NO model call — ambiguous / unknown names are a warning; a project goes
  under a category or project, a topic anywhere below; never into its own subtree;
  a `node_move` change, undoable). Tree rows carry AccessKit labels "Category: …" / "Project:
  …" / "Topic: …".
- Drag-to-merge (`oats-ui::merge`): a note card's title, a project-page note row and a sidebar
  note row are egui drag sources (payload `DraggedNote(episode id)`, the title follows the
  pointer); cards and note rows are drop zones (accent outline while a note hovers). A drop
  opens a small pop-up at the drop point: "Merge “A” into “B”? Undo not available." Yes / No;
  if either note was hand-edited, then "Replace your edits?" (Yes / Keep my text / Cancel).
  Only then `merge::apply_merge` (one savepoint): `Store::merge_episodes(dragged, into)` —
  segments and events re-pointed, span extended, the dragged row deleted — plus an
  `episode_merge` change (`by: user`; recorded, not undoable). Media and conversation are
  never merged. Then `BrainCommand::Rewrite(into)` (kind `rewrite`, tier 1): the note
  re-interpreted IN PLACE from its own segments as one chunk (`run_in_place`; a plain
  Re-interpret would reset and re-cut it by its gaps, splitting the merge apart), clearing
  `user_edited_at` in the same savepoint; "Keep my text" skips it. Toast "Merged into B — undo
  not available".
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
  `OATS_CLAUDE_PASS_PROXY=1` (let proxy variables through to `claude`), `OATS_USER_NAME` (the user's name for the prompts; overrides the Preferences value).

### Offline resilience (built)

Listening and transcription never depend on `claude`. When the CLI is missing, not logged in,
offline or rate limited (`CliError::unavailable_reason`), the brain marks itself unavailable
(`BrainEvent::Availability(Err("not logged in"))` etc.), runs nothing but a free re-probe
(`claude --version` only, with backoff 1 min → 2 → 4 → 8 → 15 min cap, reset by the next
successful call; the first real call after a probe is the probe for login and rate limits),
keeps every new segment and voice command waiting, and reports
`BrainEvent::Backlog { waiting_segments, waiting_commands }`. The status strip then reads
"claude unavailable (not logged in) — N segments waiting" (also in the All notes strip). When it
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
`transcribe(samples)` (one closed segment, ≤ ~30 s, stateless between calls), `speaker_spans(samples)`
(default `Ok(vec![])`: the local speakers of one closed segment, `SpeakerSpan { start_ms, end_ms,
speech_ms, embedding }`, no clustering across calls — see "Speakers"). Engines are built
on the pipeline's own thread via an `EngineFactory`, so they need not be `Send`
(`fluidaudio_rs::VadStream` isn't). The pipeline calls `vad` once per `vad_frame_len()` frame
as audio arrives. FluidAudio's engine runs each 4096-sample frame through a stateful Silero
stream (`FluidAudio::vad_stream_start`, LSTM state carried across calls): one inference per
256 ms, no context re-feed. `load` loads the speaker models last ("Loading speaker models…");
their failure is a logged warning, never an engine failure (`speaker_spans` then returns nothing). The contract has no session boundary, so that state also carries
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
OATS_ECHO_AUDIO=auto|always|never …        # the audio-level echo pre-check (auto: while the echo rule is on; never = transcribe every mic segment)
scripts/perf.sh [rest|idle|speech|stress]    # CPU/memory/energy profile per scenario (release build); see docs/performance-2026-10-28.md
cargo run -p oats-notes -- --selftest out.png [--listening|--notes|--calendar|--todos|--toast] [--size WxH]   # (--size in points, default 1100x720; 660x520 = the narrowest content column) whole window rendered offscreen to a PNG (egui_kittest), seeded demo rows / episode / events / todos (--notes: All notes with filed, unfiled, today's and a hidden low-signal note) (--toast: a due reminder + a confirmation) — how the shell is verified without a window
cargo run -p oats-notes -- --listen      # start listening as soon as the engine is ready (doesn't change the remembered pref)
OATS_NO_NATIVE_MENU=1 …                   # in-window egui menu bar instead of the macOS menu bar
OATS_NO_STATUS_ITEM=1 …                   # no menu bar extra (closing the window then quits)
OATS_FONT=bundled …                       # Inter instead of the system font
OATS_DATA_DIR=/path …                     # database location override (default ~/Library/Application Support/OatsNotes/oats.sqlite)
OATS_BRAIN=off …                          # no claude calls (the brain thread idles)
OATS_USER_NAME=Alex …                   # your name for the brain's prompts (overrides the Preferences window)
OATS_CLAUDE_BIN=/path/claude OATS_CLAUDE_MODEL=haiku OATS_INTERPRET_EVERY_MIN=5 OATS_BRAIN_BUDGET_USD=1 OATS_BRAIN_TIER0_PER_HOUR=30 …   # brain knobs
OATS_CLAUDE_PASS_PROXY=1 …                # pass HTTPS_PROXY / HTTP_PROXY / NO_PROXY / ANTHROPIC_BASE_URL through to claude (off by default)
cargo run --release -p oats-engine-fluidaudio --example vad_parity -- long.wav   # stateful VAD stream vs FluidAudio's batch VAD, chunk by chunk (expect 100 % / delta 0)
cargo run -p oats-engine-fluidaudio --example transcribe_wav -- speech.wav        # engine end to end on a WAV (load, VAD, transcribe)
cargo run --release -p oats-engine-fluidaudio --example speakers_wav -- two.wav [--piece 5]   # speaker spans per piece, pairwise cosine distances, ms per 10 s vs transcription
OATS_SPEAKERS=on|off …                    # speaker recognition (overrides the Preferences switch; default on)
cargo run -p oats-brain --example interpret_demo   # real claude call: a fake meeting + "hey computer" command, prints the structured results and cost
swift scripts/generate_icon.swift         # regenerate assets/icon_1024.png
```

Build notes: `oats-engine-fluidaudio` → `fluidaudio-rs` (vendored, `vendor/fluidaudio-rs`) →
its build.rs runs `swift build` of the FluidAudio Swift package (minutes the first time; needs
Xcode; fetches FluidAudio from GitHub at the pinned tag). The binary needs the Swift
runtime at run time: `apps/oats-notes/build.rs` adds `-rpath /usr/lib/swift` (without it dyld
fails with "no LC_RPATH's found"). The same build.rs embeds `apps/oats-notes/Info.plist` into
the binary's `__TEXT,__info_plist` section (`-sectcreate`): macOS reads it as the bare
executable's bundle information, so the application menu is titled "OatsNotes"
(`CFBundleName`; AppKit uses the process name otherwise, which is `oats-notes` — setting
`NSProcessInfo.processName` from the menu code does NOT work, AppKit is already up), and the
microphone / system-audio usage descriptions ride along for a future bundle. No bundle
identifier is set on purpose: TCC keeps attributing the grants to the responsible app (the
terminal). Microphone access: run from a terminal, macOS prompts for
the terminal's mic permission; a bundled .app will need `NSMicrophoneUsageDescription`.
System audio: "System Audio Recording Only", granted to the terminal app when run from one;
a bundled .app needs `NSAudioCaptureUsageDescription` (see "Audio sources and lanes").

Note cards render a markdown subset (`notes::md_lines`: `#` headings, bullets, `**bold**`) plus
one `[text](https://…)` link per line (`notes::md_link` → an accent `Hyperlink`; only http(s)
targets, anything else stays literal), so a media note's `**Media:** [title](url)` header is
clickable.

## Topic view and jump-to-note (`crates/oats-ui/src/topic.rs`, `notes.rs`)

Find icon: a 24 pt magnifying glass (`HeaderIcon::Search`, accent while the search bar is open) at the far right of the control bar, tooltip "Find (⌘F)", fires `Command::Search`.
Topic chips on episode cards are clickable (`NotesAction::OpenTopic`): `OatsApp::open_topic` loads
`Store::episodes_with_topic` (case-insensitive whole-topic match via `json_each` on `topics_json`) into
a `TopicState` shown over the current view like the search results (header "Tagged “…”" + count + ×;
Escape closes; never touches the saved `prefs.view`). One concise row per episode (date, time range,
title, node path); clicking a row calls `jump_to_episode`: it sets `NotesState::pending_focus`
(`OatsApp::pending_focus()`) and switches to the episode's node view (All notes without a node). The list first opens a
collapsed target row and, in All notes, turns `show_low_signal` on for a hidden low-signal target
(`notes::prepare_focus`, the same path on the project page); the card
that matches (`notes::focus_card`, run for every laid-out `episode_card`) consumes it with
`ui.scroll_to_rect` and flashes an accent fill for 1.5 s (`NotesState::flash`, repaints only while it
fades). Search-result clicks on a note use the same path.
Selftest: `--selftest out.png --topic "Q4 roadmap"`.

## Speakers (`oats-core::speakers`, `store_speakers.rs`, pipeline, `oats-ui::speakers`, brain)

Who said a line, with a voiceprint per person KEPT across sessions. Design: the engine only
returns embeddings; matching and storage are ours (pure, tested, portable).
- **Engine**: `SpeechEngine::speaker_spans` (see the contract). FluidAudio: the vendored
  bridge's `SpeakerEmbed_ffi.swift` (VENDORED.md modification 4) runs pyannote segmentation over
  10 s windows (≤ 3 local speakers each) and WeSpeaker once per active local speaker; audio
  under 4 s skips segmentation (one embedding, all-ones mask). Models from HF
  `FluidInference/speaker-diarization-coreml` into `~/Library/Application Support/FluidAudio`.
- **Matching** (`oats_core::speakers`, pure): ONE global `SpeakerBook` (a person is the same on
  either lane). Cosine distance to every centroid (plain f32 loop): ≤ `ASSIGN_THRESHOLD` 0.65 →
  that voice; else a new voice when the span has ≥ `MIN_NEW_SPEAKER_SECS` 1 s, else none. The
  centroid learns (weighted mean, re-normalised) only at ≤ `UPDATE_THRESHOLD` 0.45 and ≥ 2 s;
  the history's weight is capped at `MAX_WEIGHT_SECS` 600 (the stored `speech_secs` total is
  not capped). `dominant` = most speech seconds. Auto-me: with no `is_me` voice, the first with
  ≥ `AUTO_ME_MIC_SECS` 60 s of MIC speech in total (`mic_secs`) becomes "me" (logged).
- **Pipeline**: after transcription, only for non-echo segments with text, ≥ 1 s, and not
  media on the system lane (a video's voices are nobody's voiceprint), while
  `PipelineHandle::set_speakers(true)` (default `OATS_SPEAKERS`, else on; the UI sets it from
  `AppPrefs::speakers`). The book loads from the store on the first such segment; a new voice
  is inserted at once (its id), the segment's dominant voice goes into the SAME insert
  (`Store::insert_segment_full`), changed centroids are flushed in one transaction at most every
  60 s and at session end (`flush_speakers`). `PipelineHandle::reload_speakers()` (the UI after
  the user's edits): flush, then reload before the next segment (a merged-away / forgotten
  voice's pending update is a no-op UPDATE). `PipelineEvent::SpeakersChanged` (new voice,
  auto-me) precedes the `Segment`; `Segment` rows carry `speaker_id`. Debug log per segment
  ("speakers: 2 spans in 38 ms (… transcribed in … ms)"); `SpeakerStats` (`speaker_segments`,
  `speaker_ms`, `transcribe_ms`, `assigned`, `speakers_created`, `failed`) logged at info at
  session end, `PipelineHandle::speaker_stats()`.
- **Store v10**: `speakers(id AUTOINCREMENT, name, name_source user|brain, name_confidence,
  is_me, centroid BLOB f32 LE, speech_secs, mic_secs, created_at, last_heard_at)`,
  `segments.speaker_id` (index `segments_speaker`), added in place by `ensure_speakers_schema`
  (a v9 binary refuses v10: `SchemaTooNew`). API: `speakers`, `speaker`, `insert_speaker`,
  `update_speaker_centroid` / `flush_speakers`, `set_speaker_name(id, name, source,
  confidence)` (a brain name never replaces a user name, nor a brain name of higher or equal
  confidence), `restore_speaker_name`, `set_speaker_me` (clears the others), `forget_speaker`
  (row deleted, segments → NULL, WAL checkpoint), `merge_speakers(from, into)` (segments
  repointed, centroids averaged by capped weight, counters summed, `into` keeps its name else
  takes `from`'s).
- **Labels** (`oats_core::speaker_label`, shared by UI and brain): `is_me` → "me" (wins over a
  name); a name; else "them N" (system) / "voice N" (mic), N = the voice id (stable); no voice →
  the lane ("me" / "them").
- **UI**: the app loads the voices once into `OatsApp::speakers` (on `reload`, on
  `SpeakersChanged`, on a segment naming an unknown voice, after its own edits) and publishes the
  map on the egui context every frame (`speakers::publish` / `map`); rows resolve labels by
  hash lookup (`speakers::row_label`). Transcript rows (and search hits) show the tag under the
  time in the voice's stable colour (`Tokens::speaker_colour`, 6 muted hues); the user's own
  mic lines keep no tag; a brain-named voice's hover reads "named from the conversation —
  right-click to correct". Row context menu: "Speaker: <label> ▸" Name… (inline field; empty
  clears), This is me, Merge into… (inline field resolved locally by
  `speakers::resolve_voice`: "them 3" / "voice 3" / "#3" / "3" / exact name / unique prefix /
  "me"), Forget this voice — each a toast, `speakers_edited` (reload + `pipeline.reload_speakers`).
  Preferences: "Recognise speakers (keeps a voiceprint per person in the database)"
  (`AppPrefs::speakers`, default on; greyed when `OATS_SPEAKERS` is set) and the "Speakers"
  table under Apps (name field saved on blur/Enter, "me", last heard, minutes, Forget).
  `--selftest out.png --listening` seeds me / Priya (brain-named) / "them 3" / "voice 4".
- **Brain**: the interpret prompt explains the labels and asks for `speakers: [{label, name,
  confidence, reason}]` only from clear conversational clues (address + reply,
  self-introduction, a series' participant); in the schema (required, may be empty).
  `apply_interpretation` (not for media / low-signal) → `apply_speaker_names`: confidence ≥
  `SPEAKER_NAME_MIN_CONFIDENCE` 0.8, label resolved among THIS chunk's voices
  (`resolve_speaker_label`), never "me", name one line ≤ 60 chars, stored `brain` via
  `set_speaker_name` (higher confidence replaces an earlier brain name; a user name is never
  replaced), plus a `speaker_name` change row — undoable (`undo_change` restores the previous
  name only while the voice still carries that brain name). Hotwords unchanged (mic-only).
- **Cost** (`docs/performance-2026-10-28.md`, 2026-11-03 addendum): ~45 ms per short segment,
  ~65 ms per 10 s window on the M2 — roughly 0.6–1× the Parakeet time per segment (both
  models take a fixed 10 s input), nothing while silent or for echoes / media.

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
Schema version 10 (`meta.schema_version`; v10 added `speakers` + `segments.speaker_id` — see
"Speakers" —, v9 removed the Media category — see "Media on the
system lane" —, v7/v8 added the media tables, v6 re-keyed events; v5 added `episodes.user_edited_at`; a v1 file is migrated on open — `categories` become
the root `nodes` with the same ids, `segments` is rebuilt — a v2 file gets two columns
in place, `segments.source TEXT NOT NULL DEFAULT 'mic'` and `segments.echo_of INTEGER NULL`,
and a v3 file's `reminders` table is rebuilt as the todos, same ids, by
`Store::ensure_todos_table`):
`sessions`; `nodes` (ONE tree:
`kind` category | project | topic, `parent_id`, `description`, `created_by` user | auto |
hotword, `archived`; categories seeded Work/Admin/Personal); `segments` (`speaker_id` = the dominant voice (v10), `node_id` = most
specific node, `episode_id`, `interpreted_at`, `categorised`, `source` mic | system,
`echo_of` = the system segment a mic segment repeats — echoes are left out of every listing:
views, counts, `uninterpreted_*` / `uncategorised_*`, `segments_between`, `segments_of_episode`;
ordering by `started_at, id` interleaves the lanes); `episodes` (title, markdown
summary, node, decisions / action items / topics JSON, `low_signal`, model, cost, `revision`,
`last_revised_at`); `events` (`dedupe_key` UNIQUE: interpretation events are `ev:<local date>:<name_key(title)>` — NO episode id, so a re-interpretation, a revision or a second meeting mentioning the same event that day upserts one row (latest details, `source_episode_id` = latest episode that mentioned it); hotword events are `hotword:<segment>:…`, never colliding; `name_key` = whitespace-collapsed, lower-cased. `reset_episode` leaves the episode's interpretation events pointing at the dead id; `apply_interpretation` then re-points those whose `source_segment_id` is in the new chunk (`Store::adopt_orphan_events`) and drops the ones the new pass didn't propose (hotword/user events never touched). The v6 migration rewrites `ep:<id>:…` keys, collapses twins (lowest id survives with the latest twin's details, todos repointed) and deletes interpretation events whose episode no longer exists, logged at info; `source`, `source_segment_id`,
`source_episode_id`, `confidence`); `reminders` = THE TODOS (name kept; `event_id`, `text`,
`due` NULL = undated, `completed_at` NULL = open — `done` is kept in sync for the record, a
v3 done row got `completed_at = created_at` —, `snoozed_until`, `source` default hotword,
`source_segment_id`, `node_id` (unfiled on node delete, moved on merge), `position` manual
order, `dedupe_key` UNIQUE; API `add_todo(NewTodo)` (upsert on `dedupe_key`; a moved due time
drops the snooze; a completed todo stays completed), `todos(TodoFilter)`, `complete_todo`,
`reopen_todo`, `snooze_todo`, `clear_todo_snooze`, `due_todos(now)`, `next_due_at(now)`,
`set_todo_text`, `delete_todo`); `notes`;
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
  Menu key equivalents live in `SHORTCUTS` (muda accelerators, ⌘-chords only); a chord a focused
  text field must keep (⌘Z / ⇧⌘Z) is never one — read it from egui input instead.
- Brain actions give feedback: Interpret / Review / Reorganise (strip, Brain menu, ⌘I) and the cards' Summarise / Re-interpret go pending the moment they are triggered (`BrainStatus::queue`, confirmed by `BrainEvent::Queued`, ended by `Settled`, which the scheduler sends even when no run came out of it), grey out with a spinner and "Interpreting…" etc., ignore further clicks, and re-enable after 5 min with "Timed out waiting for the brain"; unavailable shows the backlog reason as the tooltip. The spinner repaints only while something is pending. `BrainEvent::Progress` (phase boundaries only: interpret "chunk i/n", review / reorganise gathering → asking claude → applying) feeds the strip line and the button label ("Interpreting 2/3…"); a finished user-triggered run pops one 4 s toast (`notes::run_toast`: "Interpreted: …", "Nothing new to interpret", or "<Kind> failed: …" in the danger colour).
- Alignment: a checkbox beside text goes through `oats-ui::widgets::check_box(ui, &mut on, &font)` (box centred on the first line's cap height; a top-aligned row may `set_min_height(interact_size.y)` to keep egui's pitch); icon glyphs that must sit on a text line use `widgets::paint_glyph_centered` + `cap_center` (the toasts' ×).
- Clock times: every displayed time of day goes through `oats-ui::timefmt` (12-hour "2:05 pm" by default, `AppPrefs::clock_24h` / Preferences "Use 24-hour time" → "14:05"; ranges drop a shared am/pm; monospace columns use `clock_col`). Never write `%H:%M` in UI code (the due editor's input text is the one exception).
- Diagnostics: the binary logs to stderr AND appends to `<data dir>/oats.log` (0600; at start only the last 2 MB are kept; level via `RUST_LOG`, default info; not for `--help` / `--selftest`; named in `--help`). A failed transcription is retried once after 200 ms (`pipeline::TRANSCRIBE_RETRY_DELAY`); only a second failure warns (`EchoStats::transcribe_failed` / `transcribe_retried_ok`, logged at session end; the warn log has lane, length and error, never text). The vendored bridge carries the Swift error text (`fluidaudio_last_error`, VENDORED.md modification 3). Status-strip warnings clear themselves after `WARNING_TTL` (60 s, one scheduled repaint; `OatsApp::expire_warning`), and a stored segment clears a "transcription failed…" warning; the claude-unavailable / backlog and "system audio: no signal" lines are separate and exempt.
- Never request continuous repaints; the app must idle at ~0 % CPU. The pipeline's and the
  brain's `wake` callbacks and menu clicks (the shared muda handler) are the only things that
  wake the UI; the brain thread itself blocks until its next due job. `Level` events are
  rate-limited to 4 Hz and suppressed (`PipelineHandle::set_levels_wanted(false)`) while the
  window is hidden or minimized.
- Commit independently as features land; don't add AI co-author lines.

## Roadmap (not yet built)

Stateful VAD (drop the context re-feed): done 2026-10-28, see `vendor/fluidaudio-rs/VENDORED.md`.
Performance notes: `docs/performance-2026-10-28.md`.

Brain: undo for node merges (recorded, not reversible yet), reminders that notify outside
the window (macOS notifications; today they pop up in the window only), a week-start pref
for the calendar, drag to reorder undated todos (`position` exists), prompt editing without
a rebuild, per-tier
budgets in the UI. Elsewhere: speakers (built 2026-11-03, see "Speakers") — a cheaper per-segment speaker pass (cost is ~0.6–1× transcription), a speakers audit view; system audio phase 5
(`docs/research-system-audio.md`: per-app taps and "which app is on the call" from the Core
Audio process list, starting the lane only during calls, VPIO/AEC if text dedup is not
enough), an input-device picker in the UI over `InputPolicy`, a "show echoes" audit view,
cpal's `pipewire` feature for the Linux system lane, the two-lane perf run (two WAVs at 1× and
4×); .app packaging with Info.plist (`NSMicrophoneUsageDescription`,
`NSAudioCaptureUsageDescription`, then `LSUIElement`-style options, Dock reopen), the Linux
engine and status-item crates.

Layout table — add:
crates/oats-sources            audio-producer contract: Producer { pid, bundle_id, name }, AudioProducers::running_output(), NoProducers, ProducerWatch (changes only)  (no platform code)
crates/oats-sources-macos      macOS producers: Core Audio process objects via objc2-core-audio (already in the tree via cpal); NOT a workspace member (FFI needs unsafe), checked by its own lines in ci.sh
crates/oats-media              media context: Firefox session-store reader (mozlz4 via lz4_flex), URL/bundle classifier, resolve(), MediaWatcher thread  (no platform code)

## Media on the system lane (`oats-core::media`, `oats-media`, `oats-sources*`)

A YouTube video in Firefox must never be taken for the user's conversation ("set a reminder"
from a video creates nothing), and what was watched is recorded.
- Producers: `oats_sources_macos::MacProducers` reads `kAudioHardwarePropertyProcessObjectList`
  and per process `kAudioProcessPropertyIsRunningOutput` / `PID` / `BundleID` (macOS 14.2+;
  property reads only, no TCC prompt, ~1 ms per poll). All `unsafe` sits in its `src/macos.rs`;
  the crate is excluded from the workspace (`exclude` in the root Cargo.toml) and `ci.sh` runs
  its clippy + tests separately (`CARGO_TARGET_DIR=target/sources-macos`). Linux (not built):
  PipeWire playback streams' `application.name` / `application.process.id`.
- Firefox: `oats_media::FirefoxSession` reads the newest
  `<profile>/sessionstore-backups/recovery.jsonlz4` (mozlz4: `mozLz40\0`, u32 LE size, one
  LZ4 block) — every tab's current entry, `selected`, `lastAccessed`, front window first; re-
  parsed only when the mtime changes. Permission-free (no Automation/Accessibility grant, no
  extension), same path layout on Linux (`~/.mozilla/firefox`, snap, flatpak). Firefox rewrites
  it at most every 15 s and only when something changed, so a tab switch shows up with ≤ 15 s
  delay; the store has no "tab is playing" flag.
- `oats_media::resolve(producers, tabs, overrides)` (pure): per producer its bundle kind
  (Spotify/Music → Music, Zoom/Teams/FaceTime/Webex → Meeting, Slack
  `com.tinyspeck.slackmacgap` / Discord `com.hnc.discord` → AppMedia (`MediaClass::App`, media:
  the user doesn't take calls on them), browsers → Browser); for Firefox ONLY the selected tab
  decides (no background-tab guessing: the store has no "playing" flag): the front window's
  (`selectedWindow`), else the first window's. youtube.com/watch|shorts|live, youtu.be →
  YouTube{video_id, title}; meet.google.com, *.zoom.us, teams.microsoft.com, whereby, meet.jit.si,
  … → Meeting (locked); open.spotify.com, music.youtube.com, … → Music (Spotify's web player:
  `oats_media::spotify::parse_title` reads the tab title `"<Track> • <Artist>"` (U+2022; `·`
  accepted too) → stamp title `"<Track> — <Artist>"`; a title without it — "Spotify – Web
  Player" / "Spotify", paused or idle — makes the context `idle`: stamp `policy =
  Some(Ignore)`, ranks last, so no play and no transcript; the exact live title format was
  not checked against a real session store yet); ANY app.slack.com /
  discord.com page → AppMedia, with the per-app class/override of the desktop app
  (`web_app_bundle`: com.tinyspeck.slackmacgap / com.hnc.discord); anything else → Browser
  (transcribed as "them", no meeting hint, not media). Debug log carries the URL host only.
  Several producers: Meeting > YouTube > Music = AppMedia > Browser > Other > an ignored app.
  Meetings are never media.
- Per-app classes (schema v8): `oats_core::ProducerClass { Auto, Meeting, Media, Ignore }` per
  bundle id in `producer_overrides(bundle_id PK, class, transcribe_anyway)` (lower-case ids; Auto
  without transcribe-anyway = no row), applied in `resolve` AFTER the built-in kind but before
  it counts, except for locked calls: the built-in meeting apps / pages (Zoom, Teams, Webex,
  FaceTime, Meet, Whereby, Jitsi — `MeetingApp::locked`, `oats_media::is_locked`) are always
  Meeting. Meeting → `MeetingApp::App` (a call titled by the app); Media → AppMedia (a YouTube /
  music tab stays itself); Ignore → `ignored` (stamp `policy = Some(Ignore)`, ranks last).
  `transcribe_anyway` (media apps only) stamps `policy = Some(Transcribe)`: transcribed and
  filed as media even under Log only / Ignore (so "Mark as a call" can recover it).
  `MediaStamp` carries `bundle_id` (stored in `segments.media_app`) and `policy` (the app's
  own, overriding the global one: `MediaStamp::skips_transcription(global)`). The watcher
  (`MediaWatcher::spawn(…, Some(db))`) re-reads the overrides every 5 s
  (`WatcherConfig::overrides_every`; a change applies without a restart) and records every app
  it sees playing in `producers_seen(bundle_id PK, name, last_seen)`.
- "On a call" (`PipelineHandle::set_on_call`, session-only, nothing persisted): every
  system-audio stamp becomes a call (`MediaStamp::as_call`; a bare "Call" stamp when nothing is
  known) — transcribed as `them`, stamped `meeting` (the brain's meeting hint), no media play —
  whatever the class and policy. Off after `ON_CALL_QUIET_MS` (10 min) of silence on the system
  lane (system-lane audio time; logged; `PipelineEvent::OnCallEnded { after_silence: true }`),
  when the session ends (`after_silence: false`), or when the UI stops listening. UI: "On a
  call" pill in the control bar (enabled while listening with system audio), Listen → "On a
  Call" (⇧⌘C, `Command::ToggleOnCall`), status strip "· on a call".
- Retroactive relabels (`store_media.rs`): `Store::mark_episode_as_call` (media episode →
  `kind = conversation` (node NULL → All notes, "Unfiled"), `**Media:**` header dropped, its system
  segments re-stamped `media_kind = meeting` (unfiled ones `categorised = 0` again), overlapping
  plays → `kind = call`), then the UI sends `BrainCommand::Reinterpret` (the stamps now make
  it a conversation chunk with the meeting hint; no media rules). `mark_episode_as_media` (the
  inverse, offered on a conversation whose `segments.media_app` is classed Media:
  `Store::episode_media_apps` + `ProducerOverrides::effective_class`) → `kind = media`, node
  NULL, segments `media_kind = app`, calls → `app`, Reinterpret. `mark_play_as_call` (a
  Calendar play) → `kind = call` ("Call via Discord 10:12–10:40"), transcribed segments of
  the play re-stamped and their episodes converted; under Log only nothing was kept and the
  confirmation says "No transcript was kept for this (policy: log only)". UI: right-click on a
  media note's title ("Mark as a call", "Summarise"), on a convertible conversation ("Mark as
  media"), on a Calendar play row ("Mark as a call"). All notes (`Store::conversations`)
  never lists media episodes.
- `MediaWatcher` (thread "oats-media", in the binary): active only while system audio is
  captured (its guard is part of the capture guard; blocked, no timer, otherwise); producers
  every 2 s, Firefox every 5 s while Firefox plays; publishes `MediaStamp { class, url, title,
  producer }` into the pipeline's `MediaFeed` (`PipelineHandle::media_feed()`; read per system
  chunk via an atomic version) and wakes the UI on change. `OATS_MEDIA=off` disables it.
  Meeting continuity (`oats_media::MeetingLatch`, per producer bundle id): once a producer's
  audio was classified as a call (its selected tab, or its app), the call STICKS to that
  producer while its audio goes on — selecting Slack / YouTube / a news tab mid-call keeps the
  call's stamp (its first URL, so one episode). It ends after 3 min without the producer's
  audio (`MEETING_HANGOVER_MS`, the cutter's hangover; "audio" = the producer runs output AND
  the system lane had non-zero sound in the last 5 s: `MediaFeed::note_system_signal`, one
  atomic store per non-silent system chunk by the pipeline), when the producer shows a
  DIFFERENT call URL (a new meeting; query / fragment ignored), or when the call's tab is no
  longer open. Only a call the producer shows starts one (idle background call tabs never do).
  While a call is latched the watcher re-resolves on every 2 s look.
- Policy `oats_core::MediaPolicy { Ignore, LogOnly, Transcribe (default since 2026-10-29) }`,
  media only (YouTube / media apps; an app's own `policy` wins). **Music is logged, never
  transcribed**: a `Music` stamp without its own policy is LogOnly whatever the global policy
  (`MediaStamp::policy_under`); only a music APP with "transcribe anyway" (Spotify desktop; the
  Apps table always offers it for music apps) is transcribed — a music web tab has no
  override. Stored in
  `meta.media_policy` (nothing ever seeded it, so an absent key = never chosen = the default; a
  user's explicit "log" stays), read at each session start, `OATS_MEDIA_POLICY` overrides,
  `PipelineHandle::set_media_policy` for a live change. Ignore / LogOnly: no VAD and no
  transcription on the system lane while media plays (the system ring still feeds the mic's
  audio-level echo check, so the room copy is dropped); LogOnly and Transcribe log
  `media_plays` rows (one per contiguous play of one media — `MediaStamp::play_key`: the URL,
  for music URL + title, so each Spotify track is a play; closed on change, stop, session end
  or a pause > 2 min; < 3 s blips dropped; end flushed every 30 s). Transcribe: segments
  stamped and counted in the play. Every stored system segment gets `media_kind` / `media_ref`
  when something is known (calls too: `meeting`).
- Store v8 (`store_media.rs`, idempotent `ensure_media_schema`; v8 = `producer_overrides`,
  `producers_seen`, `segments.media_app`; v7 below): `segments.media_kind`,
  `segments.media_ref`, `episodes.kind` ('conversation' | 'media'; `Episode::kind`,
  `Episode::is_media`), `media_plays(id, started_at, ended_at, kind, url, title, producer,
  segments_count)` (kind also 'app' and 'call'); `stamp_segment_media`, `set_segment_media`,
  `begin_media_play` / `update_media_play` / `media_plays_between`, `episode_kind` /
  `set_episode_kind`, `episode_of_play` / `episodes_of_plays` / `plays_of_episode`.
- **No Media category (schema v9)**: media notes (`kind = media`) and media segments carry
  `node_id = NULL`; the Media VIEW is their home, the sidebar tree never shows them, and
  All notes (`Store::conversations`) keeps excluding them. `Store::migrate_media_category`
  (run once from `init_locked` when a file older than v9 is opened; the version bump records
  it, so a category the user names "Media" later is just a category): for every top-level
  category whose `name_key` is "media" (auto or user-created), media notes and media segments
  in its subtree → node NULL; notes / segments / events / notes / todos filed on the category
  itself → NULL (its conversations become "Unfiled" in All notes); then the category and its subtree
  are deleted when no project / topic under it still holds anything, else it is renamed
  "Media (old)" (`created_by = user`) with those projects kept. Counts are logged at info.
  A v8 binary refuses a v9 file (`SchemaTooNew`).
- Brain: `episodes::cut` never mixes media with conversation (nor two videos); media segments
  render as `[HH:MM] media: …` under `Media: <title> <url>` (titles from `media_plays`); the
  interpret prompt says media is informational (no decisions/actions/todos/reminders/events,
  never the user's words); a media chunk gets a `media: true` hint, a known call a stronger
  `meeting: true`. Media chunks are filed WITHOUT a model call (`file_media_without_call`,
  continuing the same media's episode within the gap; title from the play, else the URL / app);
  `BrainCommand::Summarise(id)` ("Summarise" on a media note) runs one interpretation with the
  media prompt, revising the note in place (`run_in_place`, shared with `Rewrite`). `apply_interpretation` enforces the media rules
  for media chunks: title = the video, node NULL, summary header `**Media:** [title](url)`,
  no decisions/action items/events/new projects, `episodes.kind = media`; the review
  never merges media with conversation nor files a media episode in the tree; categorisation
  marks media segments categorised with node NULL, without a call; hotwords stay mic-only (`can_command`).
- UI: Preferences "Media: Ignore / Log only / Transcribe" and the "Apps" table (`app_rows`:
  fixed rows for `oats_media::KNOWN_APPS` (locked call apps, then Slack / Discord tagged "web + app":
  their web pages take the desktop bundle's class, so a browser-only user can still change it)
  ∪ producers seen ∪ overrides, deduped by bundle id (a seen row replaces the fixed one);
  order locked, Slack/Discord, rest alphabetical; class dropdown "Auto (Media)" / Meeting / Media / Ignore, locked
  rows "Meeting 🔒" greyed, "transcribe anyway" for Media rows while the policy is Ignore /
  Log only, and always for music apps (music is never transcribed otherwise);
  `OatsApp::set_app_class`); (the status strip no longer shows "▶ <title>" while media plays);
  the Calendar day panel lists "Watched: <title>" / "Listened: …" / "Call via …" 10:12–10:40
  (`Store::media_plays_between_latest`: a media — URL, for `music` URL + title — is listed on
  the day of its LAST play only, once; calls and URL-less plays are never scrubbed; index
  `media_plays_url`) (each row is an `egui::Hyperlink` to the play's URL — hover shows it, ⌘-click
  opens a new tab; right-click adds "Open in browser" / "Copy link" before "Mark as a call" /
  "Delete"); consecutive MUSIC plays of one producer and service
  ≤ 10 min apart fold into one row (`calendar::fold_plays`, `PlayFold`): "Listened: Spotify"
  "10:00–10:48 pm · 14 tracks" (distinct titles; one track: "Listened: <Track — Artist>"), a
  click opens the last track's URL, right-click "Mark as a call" / "Delete" act on every play
  of the row (`CalendarAction::MarkPlaysAsCall` / `DeletePlays`); media rows are tagged "media" instead of "them"; the Transcripts view
  hides media rows unless its header's "Show media" pill (`AppPrefs::show_media`, persisted;
  `Store::segments_page_with` / `segments_after_with`) is on — search still lists them.
- Checks: `cargo run -p oats-media --example firefox_tabs -- --watch 60` (Firefox tabs, session
  rewrites), `cargo run --manifest-path crates/oats-sources-macos/Cargo.toml --example producers
  -- 20` (what plays). Roadmap: Linux producers (PipeWire), Chrome/Safari tabs, a "Video" class
  for TV/IINA/VLC, channel names.

Commands block — add:
OATS_MEDIA_POLICY=ignore|log|transcribe …  # media on system audio (default transcribe; Preferences sets meta.media_policy)
cargo run -p oats-notes -- --selftest out.png --prefs   # Preferences over seeded apps (the Apps table)
cargo run -p oats-notes -- --selftest out.png --media   # the Media view (seeded plays over two days, thumbnails off; each media once; a jump to a transcribed video's media note flashes its row)
OATS_MEDIA=off …                            # no media detection (no producer polling, no Firefox reads)
