# OatsNotes performance: what listening costs (2026-10-28)

Machine: M2 MacBook Air, 8 GB, macOS 27. Release build (`cargo build --release -p oats-notes`;
`opt-level 3`, thin LTO). Engine: Parakeet TDT v3 + Silero VAD on CoreML via FluidAudio 0.14.8,
models already cached. Every run: `OATS_BRAIN=off OATS_NO_STATUS_ITEM=1`, a fresh
`OATS_DATA_DIR`, no `claude` calls.

## Headline

| | at rest | idle-listening (silence) | continuous speech (1×) | stress (4×) |
|---|---:|---:|---:|---:|
| CPU, mean (% of one core) | **0.0** | **2.1** | **4.6** | 14.7 |
| CPU, p95 of 1 s samples | 0.0 | 2.7 | 6.6 | 61.4 |
| Memory footprint, peak (MB) | 121 | 144 | 169 | 165 |
| Energy impact (top `POWER`), mean | 0.0 | 2.1 | 5.0 | 15.2 |
| Neural Engine requests | 0 | **0** | ~6 per segment | ~6 per segment |
| GPU (system-wide utilisation) | 0 % | 0–0.4 % | 0 % | 0 % |

- At rest the app is free: 0.0 % CPU, about 1 context switch a second. The "never request
  continuous repaints" rule holds.
- Idle-listening (the all-day case) costs about **2 % of one core**. About half of that is the
  Silero VAD path. The other half is the UI repainting at the Level-event rate plus the audio
  plumbing.
- Continuous speech adds one Parakeet call per ~30 s segment: **0.56 s mean (0.73 s max) on
  the ANE per 30 s of audio** (real-time factor 0.019). That brings the mean to 4.6 %.
- 4× real time loads the pipeline without falling behind: the same 11 segments as at 1×, and
  the session closed within the 1 s sampling resolution of the file ending.
- The Neural Engine runs only the Parakeet encoder. **Silero VAD runs on the CPU** (CoreML's
  BNNS backend), and so do the TDT decoder and joint network. See "Is the ANE used?" below.
- The engine is ready **2.2 s** after launch when the ANE compile cache is warm. The first
  launch after every rebuild took **21 s**, because CoreML recompiles the ANE model for a new
  binary.

## Method

### The deterministic audio source

`oats_audio::FileSource` (`crates/oats-audio/src/file_source.rs`) plays a PCM 16-bit mono
16 kHz WAV into the same `Sender<Vec<f32>>` the microphone capture feeds:

- chunks are 1600 samples (100 ms);
- pacing is real time by default, on an absolute monotonic schedule (chunk *n* is due at
  `start + n × 100 ms / speed`), so lateness never accumulates;
- `speed` multiplies the rate and `looping` restarts the file at its end;
- at the end of the file the sender drops, the pipeline sees `Disconnected` and listening stops,
  exactly like a lost microphone;
- dropping the guard stops playback at once and joins the thread.

The binary takes `--audio-file <wav> [--audio-speed N] [--audio-loop]` (see `--help`), and the
file then replaces the microphone in the `AudioStarter`. Unit tests cover the reader (round
trip, unknown and odd-sized chunks, rejected formats), the schedule, chunking and looping,
end-of-file disconnect, and a prompt stop on drop.

Test material:

- `long.wav`: 327 s of synthetic speech, 16 kHz mono i16;
- a same-length all-zero `silence.wav`, written by `scripts/perf.sh` itself.

The synthetic speech has no pause longer than the segmenter's 800 ms hangover, so every segment
runs to the 30 s cap: 11 segments in 327 s. That is the worst case for transcription latency
per call, but natural conversation would give more, shorter segments.

### `scripts/perf.sh`

```bash
scripts/perf.sh                          # rest idle speech stress (≈ 12 min)
scripts/perf.sh idle idle-null           # optional idle-null: OATS_ENGINE=null (energy VAD instead of Silero)
PERF_FRONT=1 PERF_DURATION=90 scripts/perf.sh idle   # bring the window to the front first
```

| scenario | command line | runs for |
|---|---|---|
| rest | (no flags) | `PERF_DURATION` (120 s) of steady state |
| idle | `--listen --audio-file silence.wav` | 120 s |
| speech | `--listen --audio-file long.wav` | the whole file (327 s) |
| stress | `… --audio-speed 4` | the whole file (82 s) |

Each second the script records:

- **top**: CPU %, memory footprint, the `POWER` column (Activity Monitor's "Energy Impact") and
  context switches, for the app, `aned` (the ANE daemon) and WindowServer;
- **ps**: cumulative CPU time, RSS and VSZ;
- **ioreg**: the GPU's system-wide `Device Utilization %` (no root needed).

CPU mean is total CPU time over wall time, and p95 is taken over the 1 s deltas. Both are a
percentage of one core.

Rows are labelled by phase: `startup` (until 5 s after "speech engine ready"), `steady`,
`profiling` and `drain` (after the file ended). The `profiling` phase is a 10 s `sample(1)`
stack capture, taken in idle and speech 60 s into steady state. Only `steady` rows are
summarised.

Other sources:

- **Unified log**: a `log stream` of the `com.fluidinference`, `com.apple.coreml`,
  `com.apple.espresso` and `com.apple.ane` subsystems for the process (`oslog.txt`). This is
  how ANE use was checked.
- **App log**: with `RUST_LOG=info,oats_core::pipeline=debug`, the app log carries the
  engine-ready line and the pipeline's existing per-segment line,
  `segment <len> ms transcribed in <ms> ms`. No logging change was needed.
- **Segments stored**: counted with `sqlite3` at the end of each run.

Output per scenario: `samples` CSV (`ps.csv`), `top.txt`, `app.log`, `oslog.txt`, `sample.txt`
and `data/`. The table is written to `summary.md` by `scripts/perf_summary.py`, which can be
re-run on any results directory.

## Results

Run 1 (`scripts/perf.sh`, default scenarios, window open but not frontmost):

| metric | rest | idle | speech | stress |
|---|---:|---:|---:|---:|
| steady s | 120 | 131 | 320 | 75 |
| CPU mean % | 0.0 | 2.1 | 4.6 | 14.7 |
| CPU p95 % | 0.0 | 2.7 | 6.6 | 61.4 |
| top CPU mean % | 0.0 | 2.1 | 5.0 | 15.2 |
| RSS peak MB | 186 | 165 | 203 | 176 |
| RSS steady MB | 28 | 29 | 63 | 91 |
| footprint peak MB | 121 | 144 | 169 | 165 |
| energy impact mean | 0.0 | 2.1 | 5.0 | 15.2 |
| energy impact p95 | 0.0 | 2.5 | 13.1 | 62.3 |
| context switches/s | 1 | 127 | 330 | 952 |
| aned CPU % | 0.3 | 0.8 | 0.0 | 0.0 |
| WindowServer CPU % | 0.8 | 0.8 | 0.6 | 0.5 |
| GPU utilisation % (system) | 0.0 | 0.4 | 0.0 | 0.0 |
| engine ready s (warm cache) | 2.2 | 3.4 | 2.3 | 2.2 |
| CPU s spent in startup | 0.8 | 1.1 | 1.0 | 1.2 |
| segments stored | 0 | 0 | 11 | 11 |
| transcribe ms mean / p95 / max | – | – | 561 / 716 / 734 | 321 / 431 / 487 |
| segment length ms mean | – | – | 29 719 | 29 719 |
| real-time factor (processing / audio) | – | – | 0.019 | 0.011 |
| file end → session closed | – | – | ≤ 1 s | ≤ 1 s |

Run 2 (`PERF_FRONT=1 PERF_DURATION=90 scripts/perf.sh idle idle-null rest`, window frontmost):

| metric | idle (Silero) | idle-null (energy VAD) | rest |
|---|---:|---:|---:|
| CPU mean % | 2.3 | 1.2 | 0.0 |
| CPU p95 % | 3.0 | 1.8 | 0.0 |
| footprint peak MB | 160 | 117 | 121 |
| energy impact mean | 2.2 | 1.1 | 0.0 |
| context switches/s | 115 | 83 | 1 |

Notes on the numbers:

- **Memory.** The footprint (top `MEM`, what Activity Monitor shows) peaks at 120–170 MB.
  RSS falls to 30–90 MB in steady state because CoreML maps the weights and the ANE holds the
  compiled encoder outside the process. Neither number counts the ANE's copy of the model. On
  8 GB this is comfortable.
- **Transcription is faster at 4×** (321 ms against 561 ms per 30 s segment). Calls 7.5 s
  apart find the ANE and CPU clusters still clocked up. Calls 30 s apart pay the ramp-up each
  time. This costs nothing that matters; it is just why the 1× latency is the higher one.
- **Stress keeps up.** The channel is unbounded and nothing is dropped, so "keeping up" means
  the backlog drains. The pipeline closed the session within a second of the last chunk, and
  the segment count and text matched the 1× run.
- **Spikes in speech** (p95 6.6 %, energy p95 13) are the transcription seconds. A 30 s segment
  costs about 0.6 s of ANE time plus CPU for the preprocessor, decoder and joint network.
- **First launch after a build.** The unified log shows `compiledModelExistsFor … success = 0`
  then an E5 compile, and the engine was ready after 21.3 s. Warm launches take 2.2 s. An app
  update will pay that once.

### Is the ANE used?

FluidAudio requests `.cpuAndNeuralEngine` for every model it loads. That is the default in
`MLModelConfigurationUtils` and in `AsrModels.defaultConfiguration()`, and VAD inherits it
through `VadConfig.computeUnits`. The one exception is the Parakeet preprocessor, which is
deliberately `.cpuOnly`. FluidAudio logs the chosen units ("Loaded … with compute units:
cpuAndNeuralEngine"), but only to the unified log, as `<private>` strings in a release build,
so the app log and `log stream` show nothing useful from it.

What CoreML actually did, from the `com.apple.ane` and `com.apple.coreml` log lines:

- **5 models are loaded** (VAD, preprocessor, encoder, decoder, joint), each "BNNSGraph as a
  preferred cpu backend". Only **one ANE `loadModel`** happens, which is the Parakeet encoder.
- **Idle-listening makes 0 ANE requests** in about 130 s. That covers about 500 VAD calls, or
  1 500 Silero inferences counting the context re-feed. Silero therefore runs on the CPU
  (BNNS), on Swift's cooperative thread pool (`libBNNS` frames in `sample.txt`).
- **Speech makes 64 ANE requests for 11 segments**, about 6 per 30 s segment (the encoder).
  The decoder and joint network run on the CPU.

So the ANE is in use for the expensive part, the encoder, and there is no GPU fallback: the GPU
shows no utilisation beyond the UI. The small recurrent models are placed on the CPU by
CoreML, which is usually right for a 4096-sample LSTM. ANE dispatch overhead would eat the
gain.

### Where idle-listening's 2 % goes

Sources:

- the idle and idle-null A/B in run 2;
- a 10 s `sample` capture at ~1 ms intervals (busy, non-waiting leaves only);
- the code.

| share of idle CPU | what | evidence |
|---|---|---|
| ≈ 1.1 points (≈ 50 %) | **Silero VAD path.** About 3.9 calls/s, each a Swift `Task` and semaphore round trip, copying 12 288 samples (2 chunks of re-fed context + 1 new) and running **3 BNNS inferences**: about 11.7 inferences/s where 3.9 would do. | idle − idle-null = 2.3 − 1.2. The cooperative-queue threads with `libBNNS` leaves are the largest busy group after the main thread. |
| ≈ 0.6–0.7 points | **UI repaint at the Level rate.** Every 100 ms chunk sends `PipelineEvent::Level`, which wakes the UI: a full egui pass, tessellation, and a wgpu submit and present. | Main-thread `run_ui_and_paint` plus the Metal command-queue threads make about 70 of 260 busy samples. At rest, with no events, the cost is 0.0 %. |
| ≈ 0.25 points | Audio delivery: the file-source thread (timer, 6.4 kB copy and send ten times a second) and the pipeline's channel wake-ups. | `oats-file-source` busy samples. With a microphone, the cpal callback and the FIR resampler do this job instead (not measured here; see below). |
| small | Segmenter, RMS, store | not visible in the profile |

GPU and WindowServer costs are negligible: under 0.5 % utilisation, and WindowServer at
0.6–0.8 % whether listening or not. The 1100×720 window repaints only a little at 10 Hz.

**The VAD context re-feed, quantified.** `FluidAudioEngine::vad` prepends `WARMUP_CHUNKS = 2`
previous 4096-sample chunks to every call. The bridge's `vadProcessSamples` starts from a fresh
LSTM state, so this is how the recurrent context survives. With one 256 ms frame per call, that
means:

- **3 Silero inferences per 256 ms of audio instead of 1** (11.7/s instead of 3.9/s);
- 12 288 samples copied per call into Swift `[Float]` instead of 4 096.

The inference share of the 1.1-point VAD path is about 0.8 points (the BNNS threads); the rest
is the Swift `Task` and semaphore hop. Dropping the re-feed should save about two-thirds of
the inference share: **≈ 0.5 points, about 25 % of idle-listening CPU.** Behaviour was not
changed here, as asked.

## GPU and ANE power (needs root)

`top`'s energy impact is a unitless estimate, and the ioreg GPU figure is system-wide. Per-unit
power needs `powermetrics`, and `sudo -n powermetrics …` asked for a password here, so it was
not captured. To measure it, start a scenario and then, in another terminal, capture about
60 s:

```bash
# terminal 1
PERF_OUT=/tmp/oats-perf scripts/perf.sh speech          # or idle
# terminal 2, once "engine ready" is printed
sudo powermetrics --samplers cpu_power,gpu_power,ane_power -i 1000 -n 60 > pm-speech.txt
# a baseline with the app closed, for the difference
sudo powermetrics --samplers cpu_power,gpu_power,ane_power -i 1000 -n 60 > pm-baseline.txt
scripts/perf-powermetrics.sh pm-baseline.txt pm-speech.txt
```

`scripts/perf-powermetrics.sh` skips the first sample. It prints mean and peak for:

- CPU, GPU and ANE power, and their sum (Combined), in mW;
- GPU active residency and frequency;
- E-cluster and P-cluster active residency.

powermetrics is system-wide, so the app's cost is the difference from the baseline. Expect the
ANE line to read about 0 mW while idle-listening (no ANE requests) and to pulse during each
transcription.

## Recommendations, ranked by expected saving

Savings are in points of one core, against idle-listening's ≈ 2.1 points. Idle-listening is
what the app does for most of the day, so it is the number that matters.

1. **Energy pre-gate in front of Silero: up to ≈ 1.1 points (≈ 50 %) in a quiet room.**
   The idle-null run shows the core's `EnergyVad` costs nothing measurable.
   - Run Silero only while the chunk RMS is above a floor (e.g. −55 dBFS), with a hangover of a
     second or two so speech onsets still go through Silero, and the pre-roll keeps onsets intact.
   - In a silent room Silero would then barely run. In a noisy room the gain shrinks towards 0.
   - It needs a threshold check against quiet speakers. Pipeline-only change.
2. **Stop re-feeding VAD context: ≈ 0.5 points (≈ 25 %).** There are two ways to do it.
   - (a) Expose FluidAudio's stateful API through the bridge. `VadManager.processChunk(_:inputState:)`
     and `processStreamingChunk(_:state:…)` exist in 0.14.8, but `fluidaudio-rs` only exposes
     the stateless `vadProcessSamples`. That needs a small patch or fork of `fluidaudio-rs`;
     the cost goes from 3 inferences per frame to 1.
   - (b) Without touching the bridge: **batch VAD calls.** Feed 4 frames (1 s) per call. With
     2 chunks of context, that is 6 inferences per 4 frames (1.5×, down from 3×) and one Swift
     `Task` hop per second, down from four. It saves about 0.4 points and adds at most 1 s of
     segmentation latency, which doesn't matter for ambient capture.
   - (a) and (b) combine.
3. **Lower the Level-event rate and skip it when no one can see it: ≈ 0.3–0.6 points.**
   - The meter does not need 10 Hz. Send `Level` at 4 Hz, or only when `in_speech` flips or
     the level moves by more than ~2 dB, and the repaints fall proportionally.
   - When the window is hidden (status-item mode) or minimised, the pipeline's `wake` still
     forces a `logic` pass for every Level event. Let the UI tell the pipeline to stop sending
     Level while the window isn't visible, or drop Level in `wake` when hidden.
   - Repaints are already event-only, so this is the whole UI cost.
4. **Transcription: nothing to do.** 0.56 s of mostly-ANE time per 30 s segment
   (RTF 0.019) is about 2 % duty, and the 4× run shows a ceiling far above real time.
   Shorter `max_segment_ms` would not reduce total work.
5. **First launch after an update (≈ 20 s, one-off):** CoreML recompiles the ANE program for a
   new binary path or hash. Saving nothing would make this cheaper, but the loading status
   line should say "Optimising the speech model for this Mac…" so it doesn't look hung.
6. **Release-profile flags (`lto = "fat"`, `codegen-units = 1`): ≈ 0 points.** The busy time is
   in CoreML/BNNS, libdispatch, AppKit and Metal, not Rust code, so this would mostly shrink
   the binary. Not worth slower release builds for CPU reasons.
7. **Resampler (microphone path), not measured here.** The file source bypasses cpal and the
   FIR resampler. It runs one polyphase FIR at 48 kHz in, with buffers reused and no
   allocation except the 100 ms chunk. That should be well under the VAD cost, but
   `cargo run -p oats-audio --example levels` under `top` would confirm it on the real
   microphone.

What the user can expect today: **about 2 % of one core while listening to a quiet room,
about 5 % during continuous speech, 0 % when not listening**. Memory is around 150–170 MB, the
ANE is used only in short bursts per segment, and the GPU is essentially idle. Recommendations
1–3 together should take idle-listening below 1 %.

## Addendum (later on 2026-10-28): stateful VAD

Recommendation 2(a) is done: `fluidaudio-rs` is vendored (`vendor/fluidaudio-rs`, see its
`VENDORED.md`) with a stateful Silero stream over FluidAudio 0.14.8's public
`VadManager.processStreamingChunk`. `FluidAudioEngine::vad` now runs **one inference per
256 ms chunk: 3.9 inferences/s while listening, down from 11.7/s**, with 4 096 samples copied
per call instead of 12 288. The pipeline calls the engine once per frame again; the 2(b)
batching stop-gap (`VAD_BATCH_FRAMES`) and the quiet-room gate (`QUIET_FLOOR_DB`,
`QUIET_HANGOVER_BATCHES`) are removed, so there is no added decision latency.

- **Parity:** `cargo run --release -p oats-engine-fluidaudio --example vad_parity -- long.wav`
  — the stream reproduces FluidAudio's batch VAD on all 1 278 chunks of `long.wav`: 100 %
  `is_speech` agreement, max probability delta 0.0 (thresholds 0.6 and 0.85). About 0.22 ms per
  chunk either way (CPU/BNNS).
- **CPU** (`scripts/perf.sh idle idle-null`, archived as
  `docs/perf/2026-10-28-stateful-vad-8045a3c`): idle 2.1 %, idle-null 1.8 %, so the Silero
  path now costs **≈ 0.3 points, down from ≈ 1.1** in run 2 above. The machine was busy
  during this run (another app at ~70 % CPU; WindowServer ~10 %, GPU ~2 % even for
  idle-null), which inflates both absolute numbers. Compare the delta, not the totals. Two
  more idle-only runs under the same load gave 2.0 % and 2.7 %. Re-measure on a quiet machine
  before quoting a new headline.

## Before/after (late 2026-10-28, after the system-audio lane, bad1dd5)

Archive: `docs/perf/2026-10-28-stateful-vad-lanes-bad1dd5/`. Shortened protocol (on request):
idle = 90 s steady window; speech = the first 90 s of `long.wav` at real time (3 segments, so
segment counts are not comparable with the 327 s runs). `--audio-file` only, so the **system
lane was absent** (log: `listening started (mic)`); the System Audio Recording grant was not
effective on this machine anyway (`docs/verification-2026-10-28.md`). **The machine was busy**
(mediaanalysisd ~200 % CPU, 1-min load 5.5–12.9 after a 5 min wait), so treat these as indicative.

| run | idle CPU mean / p95 % | speech CPU mean / p95 % | RSS steady MB (idle / speech) | energy mean (idle / speech) | segments stored (speech) |
|---|---:|---:|---:|---:|---:|
| baseline e678c2c | 2.1 / 2.7 | 4.6 / 6.6 | 29 / 63 | 2.1 / 5.0 | 11 (327 s file) |
| stop-gaps c2c1b31 | 1.7 / – | – (partial run) | – | – | – |
| stateful VAD 8045a3c (busy) | 2.1 / 3.6 | – | 48 / – | 2.1 / – | – |
| **lanes bad1dd5 (busy, shortened)** | **1.2 / 1.8** | **2.8 / 2.0** | **41 / 45** | **1.1 / 4.1** | **3 (90 s cut)** |

Speech mean > p95 because ps's `%cpu` shows three transcription bursts (20.8, 21.5, 67.4 %)
in the 83 s window — one per ~30 s segment (transcribe ≈ 750 ms each, RTF 0.025); `top`'s
per-second mean is 4.1 %. Interpretation: idle listening with the stateful VAD now costs about
1.2 % of one core (down from 2.1 % at baseline) even on a loaded machine, and the lane split did
not add measurable mic-only cost; the system lane's own idle cost (a silent tap, all-zero frames
skip VAD) is still unmeasured until the permission works.

## Addendum (2026-10-29): audio-level echo check — expected saving, not yet measured

In a real meeting on the built-in speakers, 55 of 69 microphone segments were echoes of the
system lane, and two-lane listening cost ≈ 15 % of a core against ≈ 3 % single-lane: most of
the difference was Parakeet transcribing microphone audio that the text rule
(`oats_core::echo`) then hid. The pipeline now checks each closing microphone segment against
the system audio it already holds (`oats_core::echo_audio`: 10 ms envelope correlation over
± 500 ms lags, plus gain and coverage guards) and does not transcribe a suspected echo. The
check itself is a few hundred thousand multiply-adds per segment (envelopes of ≤ 31 s of audio,
≤ 3 000 frames × 101 lags) — negligible next to one transcription — and the system ring is a
memcpy of every system chunk into a 45 s buffer (≈ 2.9 MB).

Expected: two-lane speech on speakers should approach the single-lane cost (system lane
transcribed, plus the user's own segments), i.e. from ≈ 15 % toward ≈ 3–5 % of a core, to the
extent the echoes are recognised. To be confirmed by a live measurement: a call on speakers
with `RUST_LOG=oats_core=info` (the session's last log line gives `mic segments transcribed /
skipped by the audio check (linked, discarded) / marked by the text rule`) and
`scripts/perf.sh`. The thresholds are calibrated on synthetic signals only; if real rooms
correlate lower, segments are simply transcribed as before (no saving, no loss) — the
counters show which. `OATS_ECHO_AUDIO=never` gives the old behaviour for an A/B run.
