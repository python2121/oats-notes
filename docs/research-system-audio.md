# Research: system audio + microphone at the same time

Date: 2026-10-28. Question: transcribe meetings (Zoom / Meet / Teams, videos) from the
system output **and** the ambient microphone at once — mixed, or both processed side by side?
Evidence from Apple's APIs, cpal, Hyprnote, screenpipe, Granola, Meetily and others, then a
design for this codebase. Cloned sources were read, never run:
`scratchpad/research2/{hyprnote,screenpipe}` (Hyprnote `main` 2026-10, screenpipe `main`
2026-10), and the cpal 0.18.2 sources in the local cargo registry.

Short answer: **everybody processes the two streams separately and interleaves by timestamp;
nobody mixes.** "Me" = mic, "Them" = system audio — that is exactly how Granola gets its
labels. Mixing would cost less than it saves: it loses the attribution, and overlapping speech
(me talking over them) degrades a single-speaker model like Parakeet. The extra cost of a
second lane on this machine is ~1 % of a core idle and well inside the ASR budget.

## 1. Capturing system audio on macOS

### Core Audio process taps (the right API)

`CATapDescription` + `AudioHardwareCreateProcessTap` → private aggregate device with the tap in
`kAudioAggregateDeviceTapListKey` → IOProc → `AudioDeviceStart`. Introduced in macOS 14.2;
the TCC flow is stable from 14.4 (screenpipe floors at 14.4, Apple's reference sample
[insidegui/AudioCap](https://github.com/insidegui/AudioCap) says 14.4+).

- **Permission**: `kTCCServiceAudioCapture`, shown as *System Audio Recording Only* under
  Privacy & Security → *Screen & System Audio Recording*. Needs `NSAudioCaptureUsageDescription`
  in Info.plist (must be in the plist itself — the `INFOPLIST_KEY_` build setting is silently
  ignored). The prompt fires at the first `AudioDeviceStart`, not at tap creation; **denial is
  silent** (tap created, format reads as 48 kHz stereo, buffers are all zeros) and there is no
  public API to query the grant (AudioCap uses private TCC calls). No weekly re-prompt — that
  nag is for screen capture. Sources: [DGR Labs 2026](https://dgrlabs.co/blog/2026-04-25-capturing-system-audio-on-macos-in-2026.html),
  [thunderkitty "2,000 buffers of nothing"](https://www.thunderkitty.app/learn/2000-buffers-of-nothing/),
  [askcanary glossary](https://askcanary.com/glossary/coreaudio-tap/).
- **Unbundled binary from a terminal**: TCC attributes the capture to the *responsible
  process*, i.e. Terminal / iTerm / VS Code, so the grant goes to the terminal app. Terminal
  prompts; iTerm and VS Code often do **not** auto-prompt — you get silence until the terminal
  is added by hand under *System Audio Recording Only*. Permission changes need an app restart.
  Sources: [obsfx/audiograb README](https://github.com/obsfx/audiograb),
  [sbetko/catap](https://github.com/sbetko/catap), [WAMF/audio_io PR 56](https://github.com/WAMF/audio_io/pull/56).
  For us: development works from Terminal.app; the `.app` bundle (roadmap) must carry
  `NSAudioCaptureUsageDescription` next to `NSMicrophoneUsageDescription`.
- **Per-app taps** are possible: `initStereoMixdownOfProcesses:` / `initMonoMixdownOfProcesses:`
  take an *include* list of process AudioObjectIDs (from
  `kAudioHardwarePropertyTranslatePIDToProcessObject`, not raw pids); the
  `…GlobalTapButExcludeProcesses:` family is an *exclude* list. `isExclusive` is the direction
  flag between the two, not a lock — flipping it after init inverts the meaning (a classic bug:
  [waveform PR 86](https://github.com/vipinsight/waveform/pull/86)). macOS 26 adds
  `bundleIDs` + `isProcessRestoreEnabled` (tap by bundle id, re-attach after the app restarts;
  exact bundle match only, so Chrome helpers for Meet are missed —
  [dotnet/macios Xcode 26 diff](https://github.com/dotnet/macios/wiki/CoreAudio-macOS-xcode26.0-b1),
  [SamWongML notes](https://github.com/SamWongML/macos-audio-recording/issues/1)).
  Meeting apps run audio in helpers (`us.zoom.CptHost`, `com.microsoft.teams2.helper`, Chrome's
  GPU/utility process), so a per-app tap needs pid → responsible-app mapping. A global tap is
  the pragmatic default; per-app is a refinement.
- **Format**: the tap's ASBD follows the tapped output device (built-in speakers: 48 kHz
  stereo f32); it can renegotiate when the device changes (Hyprnote re-reads the ASBD every
  128 polls; taps are reported to go all-zero after a renegotiation). Mono mixdown is available
  at the tap (`mono`/`mixdown` flags) — or mix down ourselves, which `oats-audio` already does.
- **Who is on a call** (no TCC needed, 14.2+): `kAudioHardwarePropertyProcessObjectList` →
  per process `kAudioProcessPropertyIsRunningInput` / `IsRunningOutput` /
  `kAudioProcessPropertyBundleID`. This is how screenpipe's default meeting sensor and
  "sigstop" know a call is on, with no prompt
  ([macnotetaker write-up](https://macnotetaker.com/blog/which-app-is-using-mic-coreaudio-process-objects),
  [OpenOats issue 726](https://github.com/yazinsai/OpenOats/issues/726)). Filter
  `historicalaudiod`/empty bundle ids and dictation apps.

### ScreenCaptureKit audio — not for us

`SCStream` with `capturesAudio` always performs a screen recording with frames discarded, so it
needs the full *Screen & System Audio Recording* grant and (since Sequoia) the weekly
re-prompt; the audio-only grant cannot be requested through SCK (OBS maintainers,
[obs-studio #10401](https://github.com/obsproject/obs-studio/issues/10401)). Rust binding:
[`screencapturekit` 11.0.0](https://crates.io/crates/screencapturekit) (MIT/Apache-2.0, active).
Only a fallback for macOS 13 — irrelevant for a 27 target. Virtual devices (BlackHole) are out.

### Rust crates that work today

| crate | what it gives | license / maturity | notes |
|---|---|---|---|
| **cpal 0.18.2** (already ours) | `build_input_stream` on an *output-only* device transparently creates a process tap + private aggregate device: `src/host/coreaudio/macos/loopback.rs` (`LoopbackDevice::from_device`), picked in `device.rs:727-736` when `supports_input()` is false | Apache-2.0/MIT; loopback since 0.17.0 (2025-12), UB / silence / UID-collision fixes in 0.18.0 (2026-06) | README: loopback needs **macOS 14.6+**. Tap = `initWithProcesses_andDeviceUID_withStream(∅, deviceUID, 0)` + `exclusive=true` (= all processes), `Unmuted`, private. Uses `objc2-core-audio` inside; all `unsafe` stays in cpal. No per-app list, no mono flag, no permission probe. Pass the device's `default_output_config()` as the config: `build_input_stream_raw` calls `set_physical_format`/`set_sample_rate` on the real output device for the requested config, and `default_input_config()` errors on an output-only device |
| [cidre](https://github.com/yury/cidre) 0.29/0.30 | `ca::TapDesc::with_mono_global_tap_excluding_processes`, `create_process_tap`, `AggregateDevice::with_desc`, `create_io_proc_id`, `device_start`; `ca::Process::with_pid().is_running_input()` | MIT; 55 releases, used in production by Hyprnote and screenpipe | Big (89K SLoC, feature-gated by OS version); `unsafe` inside; git-pinned by most users |
| [objc2-core-audio](https://docs.rs/objc2-core-audio) 0.3.x | raw `CATapDescription`, `AudioHardwareCreateProcessTap` (`unsafe fn`), aggregate devices | MIT; madsmtm's generated bindings, what cpal uses | thin; you write the aggregate-device dictionary yourself |
| screencapturekit 11 | SCK audio | MIT/Apache-2.0 | wrong permission, see above |

`unsafe_code = "forbid"`: with cpal's loopback nothing changes — `oats-audio` stays safe and
even **cfg-free** (see §5: the same call captures the PipeWire sink monitor on Linux). A later
per-app / meeting-detection crate over cidre or objc2-core-audio would carry `unsafe` and would
sit like the engine: its own crate, either outside the workspace members (`vendor/`-style path
dependency, as fluidaudio-rs) or a member that does not inherit `[lints] workspace = true`.

## 2. Mic + system at the same time — how the apps do it

**Hyprnote** (`crates/audio-actual`, `crates/aec`, `crates/audio-sync`, `crates/listener-core`):
- Mic via cpal (`mic.rs` filters out its own tap device by name to avoid a loop); speaker via a
  cidre **mono global tap** (`speaker/macos.rs:52-79`), format read from `tap.asbd()`.
- `capture/stream.rs::run_dual_loop` pairs mic and speaker chunks with a `Joiner`
  (`capture/joiner.rs`: two queues, pads the missing side with silence after `MAX_LAG = 4`
  chunks) and emits `CaptureFrame { raw_mic, raw_speaker, aec_mic }`.
- AEC = **DTLN-aec** ONNX (`crates/aec`, 128/256/512-unit variants, block 512 / shift 128 at
  16 kHz) fed with the speaker as far-end, preceded by `AecReferenceAligner` (GCC-PHAT lag
  estimate + drift tracker from `crates/audio-sync`) and followed by a linear residual-echo
  gain; user-toggleable (`enable_aec`), resets on alignment change.
- The listener sends **two tracks** (`ChannelMode::MicAndSpeaker`; the recorder writes an
  interleaved stereo WAV, mic L / speaker R) and the transcript's `speaker_index` comes from
  the channel → "You" vs the others. Two streams, never mixed.

**screenpipe** (`crates/screenpipe-audio`):
- Every device is its own stream, tagged `AudioDevice { name, device_type: Input|Output }`
  ("… (input)" / "Display 1 (output)"), each transcribed independently in 30 s chunks with 2 s
  overlap; `transcription/stt.rs:302` passes `is_output_device` into segmentation.
- System audio: cidre process tap (`core/process_tap/macos.rs`, floor 14.4, SCK fallback;
  the private aggregate must be filtered out of the device list or it records itself), and a
  per-process "piggyback" meeting tap on the detected meeting app plus following that app's
  mic (`audio_manager/meeting_piggyback.rs`).
- Echo, three layers: (1) optional **Apple VPIO** for the default mic, run in a *child
  process* because wedged CoreAudio calls can't be cancelled (`core/vpio_process.rs`, needs
  their cpal fork); (2) always-on software **AEC3 + NS + AGC2** via `sonora` (WebRTC APM) on the
  mic with the speaker stream as reference, aligned by capture timestamps
  (`core/aec.rs`, `core/run_record_and_transcribe.rs:198-260`); (3) **text-level dedup in the
  DB**: same-device overlap trim (`transcription/handle_new_transcript.rs:127-151`) and a
  cross-device check (`screenpipe-db/src/db/audio.rs::has_duplicate_from_other_device`): a new
  transcript is dropped when one of the *opposite* device type within **±45 s**
  (`DEDUP_TIME_WINDOW_SECS`) is similar by word-Jaccard **or** containment ≥ 0.85, exact match
  only under 4 words (`screenpipe-db/src/text_similarity.rs`).
- Meeting detection (`screenpipe-engine/src/meeting_watcher`): the `audio_process` sensor
  (Core Audio process objects: which app holds the mic) is the default; UI scan and calendar
  are the other backends; a meeting ends on silence.

**Granola** ([docs: transcription](https://docs.granola.ai/help-center/taking-notes/transcription),
[speaker tags](https://docs.granola.ai/help-center/taking-notes/speaker-attribution)): "uses
your system audio and microphone", cannot isolate apps (music is transcribed too), **"Me" and
"Them" correspond to your microphone input and your system audio**; named speakers come from
a Meet extension / Zoom accessibility reading the active-speaker UI. Nothing published on echo.

**Meetily**: BlackHole on macOS (older builds), WASAPI on Windows; PR
[#823](https://github.com/notoriousbdg/meetily/pull/6) saves mic L / system R as separate
channels; issue [#337](https://github.com/Zackriya-Solutions/meeting-minutes/issues/337) asks
for per-source toggles and "muted mic awareness" (the mic transcribes what the meeting never
heard). Linux went through `parec`/libpulse because cpal was ALSA-only
([PR 806](https://github.com/Zackriya-Solutions/meetily/pull/806)). **Amurex** is a Chrome
extension recording the tab — not comparable.

**VPIO in the wild** ([trailmix #4](https://github.com/stephenturner/trailmix/issues/4),
[moss #3120](https://github.com/motioneso/moss/pull/3120), [sys-voice](https://github.com/tigy32/sys-voice),
[audiounit-rs](https://github.com/doom-fish/audiounit-rs)): `kAudioUnitSubType_VoiceProcessingIO`
with input element 1 enabled and output element 0 enabled but rendering silence gives an
echo-cancelled mic against the whole device output (cpal opens the plain HAL unit, so this needs
coreaudio bindings or a fork). Side effect that matters for an all-day ambient app: **macOS
ducks every other app's audio while VPIO runs**; `kAUVoiceIOProperty_OtherAudioDuckingConfiguration`
level `Min` still ducks ~8 dB and there is no "off" (measurements in
[live-mindmap #110](https://github.com/daiki-beppu/live-mindmap/issues/110)). VPIO also
misbehaves on aggregate/Bluetooth output combos.

So, to the three questions: (a) **separate streams, interleaved by time** — unanimous.
(b) Double transcription: VPIO (optional, side effects), software AEC (DTLN or WebRTC APM —
both heavy dependencies), or **text-level dedup in a time window** (screenpipe's cheap layer),
and headphones make the problem vanish (Granola effectively assumes it). (c) Meeting detection:
audio from the tap + which app holds the mic (Core Audio process objects), calendar as a hint.

## 3. Cost on our architecture

Numbers from `docs/performance-2026-10-28.md` (release, M2 Air): idle listening 2.1 % of a
core with Silero vs 1.2 % with the energy VAD → **one Silero lane ≈ 1 %**; Parakeet 0.56 s mean
/ 0.73 s max per 30 s segment, real-time factor **0.019**; 4× real time still keeps up at
14.7 % CPU.

Two lanes (two VAD states, two segmenters, one engine on the one worker thread — FluidAudio's
objects are not `Send`, so both lanes transcribe on that thread, FIFO):
- Idle: +1 % CPU for the second Silero stream — and only while the tap delivers non-zero
  audio. A tap with nothing playing yields exact digital zeros; a "chunk is all zeros" check
  (one pass, no FFT) skips Silero and the level event, so an ordinary silent desktop costs
  nothing extra.
- Worst case, both lanes in continuous speech (a call where I keep talking over them):
  RTF 0.019 × 2 ≈ **0.04** → about 1.1 s of ASR per 30 s wall time, p95 CPU in the teens. While
  a transcription runs (≤ 0.75 s) VAD pauses for both lanes; the audio channel is an unbounded
  mpsc so nothing is lost, and the catch-up is immediate. Worst added latency: two closed
  segments back to back ≈ 1.5 s before the second row appears. Memory: one more ≤ 30 s
  segmenter buffer (1.9 MB) and pre-roll ring.
- The stress profile (4× real time, one lane) is a strict superset of this load.

Mixing into one 16 kHz stream: saves the ~1 % VAD lane and the schema change; loses me/them
(the user explicitly wants "me, meetings, ambient"), hands Parakeet overlapping speakers it
is not trained for, and makes the mic-hears-the-speakers duplicate *impossible* to remove
(it is baked into the mix). Not worth it.

## 4. Timeline, alignment, attribution

- Each lane gets its own `SampleClock` (`pipeline.rs:228`): wall-clock anchor at session start,
  samples counted per lane, re-synced on sleep. Mic and tap latencies are both tens of ms —
  below one 100 ms chunk — so no cross-lane alignment is needed for *segment* timestamps.
  (Hyprnote's GCC-PHAT aligner exists only for the sample-accurate AEC reference.)
- `segments` gains `source TEXT NOT NULL DEFAULT 'mic'` (`mic` | `system`), schema v3 (ALTER
  TABLE, no rebuild), `TranscriptRow.source`, `Store::insert_segment(.., source)`. Every
  existing query orders by `started_at, id`, so the two lanes interleave on the one timeline
  for free (`store.rs:948-1014`), in the UI, the brain and the episode cutter.
- UI (`oats-ui/src/transcript.rs::transcript_row`): a small marker in the time column —
  "me" rows as today, "them" rows with a glyph/tint and a slight indent; nothing else moves.
- Brain: `episodes.rs` keeps the gap rule, and a chunk containing `system` segments is a
  meeting by construction — pass `meeting: true` (and later the app name) into `interpret`;
  the transcript inside `<transcript>` gets `me:` / `them:` line prefixes so summaries can say
  who decided what. "Meetings go into Work": a prompt hint, not a hard rule (personal calls
  exist); the interpretation still files into the tree.
- The `system` lane's speech presence is also the UI's "in a call" indicator and the status
  item tooltip ("listening · call").

## 5. Linux later

cpal 0.18's native hosts cover it, no new crate:
- **PipeWire host** (`cpal` feature `pipewire`, PipeWire ≥ 0.3.53): `Audio/Sink` nodes are
  exposed as `Duplex` devices, and opening an *input* stream on one sets `STREAM_CAPTURE_SINK`,
  i.e. captures what is playing to that sink — verified in
  `cpal-0.18.2/src/host/pipewire/device.rs:895-925`. So the same
  `default_output_device().build_input_stream(..)` call that creates the tap on macOS captures
  the monitor on KDE. The default Linux host is still ALSA; pick PipeWire explicitly
  (`cpal::host_from_id(HostId::PipeWire)`), falling back to the `pulseaudio` host (pure Rust,
  monitors appear as sources per [whisrs #173](https://github.com/y0sif/whisrs/issues/173))
  and then ALSA (mic only). No TCC equivalent; PipeWire's portal permission is not involved
  for a native client.

## Recommended design for OatsNotes

**Crates to add: none for the first three phases.** cpal 0.18.2 (already pinned) does the tap
on macOS 14.6+ and the sink monitor on PipeWire. Keep `cidre` (MIT) in reserve for a later
`oats-audio-macos` crate (per-app taps, "which app is on the call", mono mixdown, silence
probe) — Hyprnote's `speaker/macos.rs` is a 100-line template. Skip `screencapturekit`.
Skip an AEC crate until the cheap strategy below proves insufficient (DTLN needs an ONNX
runtime; WebRTC APM via `sonora` needs a C++ build).

**Trait shape** (in `oats-core`, no platform code):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSourceKind { Mic, System }

/// 100 ms of 16 kHz mono f32 from one source. Both captures share one Sender.
pub struct AudioChunk { pub source: AudioSourceKind, pub samples: Vec<f32> }
```

`oats-audio`: `Capture::start(sink: Sender<AudioChunk>)` becomes
`Capture::start_mic(sink)` (today's code, chunks tagged `Mic`) and
`SystemCapture::start(sink)` = `default_output_device()` + `default_output_config()` +
`build_input_stream` through the same `Converter` (mix-down + FIR resample), chunks tagged
`System`; a terminal error on either closes only that lane (a dropped lane is reported as
`PipelineEvent::SourceClosed(kind)`, the session continues with the other). One channel keeps
the worker's single blocking `recv_timeout`; `FileSource` gains a `source` so tests drive two
lanes from two WAVs. `oats_ui::AudioStarter` returns both guards.

**Engine contract**: VAD state must be per lane. Add
`fn vad_lane(&mut self, lane: AudioSourceKind, samples) -> Result<Vec<VadFrame>>` (default
impl forwards to `vad`); the FluidAudio engine keeps one `VadStream` per lane (two Silero
LSTM states, same CoreML model); `NullEngine` keeps one `EnergyVad` per lane. `transcribe`
is shared and unchanged.

**Pipeline**: `Session` holds `lanes: [Lane; 2]` (`pending`, `Segmenter`, `SampleClock`,
`last_level`), dispatches each `AudioChunk` to its lane, skips VAD on an all-zero system
chunk, and funnels closed segments into `finish_segment(lane, seg)` → `insert_segment(..,
source)` → `PipelineEvent::Segment(row)`. `Level` carries both lanes (`rms_db: [f32; 2]`,
`in_speech: [bool; 2]`), still ≤ 4 Hz. `Command::Listen` takes the one receiver plus
`sources: EnumSet` so "mic only" stays the fallback when the tap can't open.

**Echo / duplication, with fallback**:
1. Headphones first: if the default output device is not the built-in speakers (cpal device
   name / later `kAudioDevicePropertyTransportType` ∈ Bluetooth, USB, headphones), the mic
   cannot hear the system audio → no dedup at all.
2. Otherwise, text-level dedup on the pipeline thread before insert (screenpipe's rule,
   tightened): a `mic` segment whose time span overlaps a `system` segment stored in the last
   45 s and whose text is ≥ 0.8 word-Jaccard similar or contained in it is dropped (logged at
   debug); under 4 words exact match only. Pure function in `oats-core`, unit-tested. The mic
   segment is usually the *later* one (room echo + a longer hangover), so the comparison
   mostly runs mic-against-stored-system; the reverse case (system row lands second) marks the
   stored mic row `source = 'system'` instead of inserting.
3. Fallback knob: `OATS_SYSTEM_DEDUP=off|text|always` and a menu toggle; if a user on speakers
   still sees doubles, the escalation path is VPIO *only while the system lane has speech*
   (ducking is acceptable during a call, not all day) or Hyprnote's DTLN crate — both deferred.

**Meeting signal**: `system` segments present ⇒ meeting (episode cutter + prompt hint).
Later, the Core Audio process list (14.2+, no permission) names the app and starts the system
lane only while something holds the mic — saves the tap's idle cost and makes "auto-detected
meeting" explicit.

**Permissions / bundling**: develop from Terminal.app (iTerm / VS Code: add the terminal under
*System Audio Recording Only* by hand); the `.app` gets `NSAudioCaptureUsageDescription` +
`NSMicrophoneUsageDescription`, ad-hoc signing is enough locally. Because denial is silent,
the pipeline reports "system audio: no signal (check System Audio Recording permission)" in
the status strip when the lane has been all-zero for 10 s while any process is running output
(or, before the process-list code exists, simply after 60 s of zeros following a user
toggle-on). Known edge: cpal binds the tap to the output device UID at creation, so a default
output change (AirPods connect) means restarting the system lane — poll the default output
device id every 2 s from the UI thread (cheap) and restart the lane on change.

**UI**: a "me / them" marker per row, "Capture system audio" toggle in the Listen menu
(pref), status strip "listening · mic + system" / "· call", level meter shows the max of the
lanes. Nothing structural.

**Phases and effort** (one developer, release builds, tests included):
1. `SystemCapture` via cpal loopback + `levels --system` example; verify on macOS 27: the
   permission prompt, all-zero behaviour before the grant, sample rate, output-device switch.
   **1–2 days.**
2. Lanes: `AudioChunk`, `vad_lane`, per-lane segmenter/clock, `segments.source` (schema v3),
   `TranscriptRow.source`, two-lane `Level`, two-`FileSource` pipeline test. **2–3 days.**
3. UI marker + toggle + status; brain `me:`/`them:` prefixes, `meeting` hint in `interpret`,
   episode cutter treats system segments as meeting evidence; `--selftest` seeds a them row.
   **1 day.**
4. Dedup: headphone check + overlap/text rule + knob + tests; perf run with two WAVs at 1×
   and 4× to confirm the numbers in §3 and update `docs/performance-*.md`. **1–2 days.**
5. Later / optional: `oats-audio-macos` over cidre (process list → app name, start the lane
   on demand, per-app tap), VPIO-during-calls or DTLN if dedup is not enough; Linux: PipeWire
   host selection when the Linux engine crate lands. **3–5 days when needed.**

Total for the usable feature (phases 1–4): about **one to one and a half weeks**.
