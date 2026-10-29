# Hardware verification — system-audio lane (2026-10-28)

Commit `bad1dd5` (clean tree), release build (`cargo build --release -p oats-notes`,
`cargo build --release -p oats-audio --example levels`). MacBook Air (M2), terminal launched by
ClaudeShip (`/Applications/ClaudeShip.app`), which had been asked to be granted "System Audio
Recording Only". Scratch data dir, `OATS_BRAIN=off`.

## Result: NOT verified — the system lane delivered nothing (permission not effective)

### Devices

- `say -a '?'` lists one output: `71 MacBook Air Speakers` (the default output).
- `system_profiler SPBluetoothDataType`: all AirPods (several headsets, none connected)
  under **Not Connected**.

### Step 1 — `levels --system` with speech on the speakers

```
./target/release/examples/levels --system &   # ~12 s, killed afterwards
sleep 2; say -a "MacBook Air Speakers" "The quarterly budget review is scheduled for Thursday afternoon in the main conference room."
```

Output (excerpt; ~100 lines, one per ~0.1 s):

```
mic:    "MacBook Air Microphone" (48000 Hz, 1 ch) → 16 kHz mono
system: opening the tap (macOS may ask for permission to record system audio; the lane delivers nothing until that is answered)
mic   -68.2 dBFS  system     (no chunks yet)  #####
...
mic   -58.1 dBFS  system     (no chunks yet)  ##########
mic   -34.2 dBFS  system     (no chunks yet)  ######################   <- say starts (mic hears the speakers)
mic   -29.9 dBFS  system     (no chunks yet)  #########################
...
mic   -45.6 dBFS  system     (no chunks yet)  #################        <- say ends
mic   -63.2 dBFS  system     (no chunks yet)  ########
...
system lane: 0 chunks, 0 with signal
```

- The mic lane works and picks the built-in microphone (≈ −63 dBFS room, ≈ −30 dBFS while
  `say` plays).
- The system lane printed **"(no chunks yet)" for the whole run and ended with 0 chunks** — not
  "all zeros" (which a denied-but-open tap gives) but no chunks at all: the tap open stayed
  blocked, which is what Core Audio does while the permission prompt is pending / the grant is not
  effective for the responsible app. An earlier run today (scratchpad `levels.txt`, 20:34)
  ended identically (`system lane: 0 chunks, 0 with signal`).
- The TCC database is not readable from this shell (no Full Disk Access), so the exact grant
  state could not be read. Likely causes: the grant was not given to ClaudeShip, a prompt is still
  waiting on screen, or ClaudeShip was not restarted after the grant (permission changes need a
  restart of the responsible app).

Per the task, Part 1 stopped here: steps 2 (live `--listen` mic + system, echo marking) and 3
(AirPods — none connected anyway) were **not run**; no workaround was attempted.

### To finish the verification

1. System Settings → Privacy & Security → Screen & System Audio Recording → "System Audio
   Recording Only": enable ClaudeShip (or the terminal you use); quit and reopen it.
2. `./target/release/examples/levels --system` with sound playing: expect non-zero system levels.
3. `RUST_LOG=info OATS_BRAIN=off OATS_DATA_DIR=<scratch> ./target/release/oats-notes --listen`,
   two `say -a "MacBook Air Speakers"` sentences 8 s apart; expect `audio input (BuiltIn):
   "MacBook Air Microphone"`, `listening started (mic + system)`, system rows and mic rows with
   `echo_of` set; then the same on AirPods with no echo rows and no AirPods input opened.

## 2026-10-29 — system audio verified on hardware

Permission "System Audio Recording Only" granted to ClaudeShip (the app that launched the terminal), ClaudeShip restarted.

`./target/release/examples/levels --system` (12 s, a video playing through "MacBook Air Speakers"):
mic −37…−54 dBFS, system −13…−31 dBFS while sound played, then both fell to −60; `system lane: 99 chunks, 94 with signal`.

`./target/release/oats-notes --listen` (release, `OATS_BRAIN=off`, scratch data dir), log:

```
audio input (BuiltIn): "MacBook Air Microphone" at 48000 Hz, 1 channel(s), F32 → 16000 Hz mono
listening started (mic + system)
system audio: "MacBook Air Speakers" at 48000 Hz, 2 channel(s), F32 → 16000 Hz mono (speakers)
echo rule on (output "MacBook Air Speakers", Auto)
system audio: first signal
FluidAudio VAD stream for the system lane
mic segment 1 marked as the echo of system segment 3
mic segment 2 marked as the echo of system segment 3
mic segment 5 stored as the echo of system segment 4
```

Segments (id, lane, echo_of, start, seconds, text):

```
3  system  -  08:57:14  18.9  But oh yeah, this is a beautiful revelation. Now we can start our speedrun. As soon as we click on a million, …
1  mic     3  08:57:14  11.5  But uh yeah, this is a beautiful revelation. Now we can start a speedrun. As soon as we click on a million, we…
2  mic     3  08:57:29   3.3  With that, the next thing we're gonna upgrade is
4  system  -  09:01:29   5.6  Also remember that the apartment lease renewal paperwork is due at the start of February.
5  mic     4  09:01:29   6.1  Also remember that the apartment lease renewal paperwork is due at the start of February.
```

Rows 3 and 4 came from the system lane (a video the user played, and a `say` sentence); the microphone's copies 1, 2 and 5 are stored as echoes and hidden from the views. Both the reverse-order path (1 and 2 marked after 3 arrived) and the forward path (5 stored as an echo at insert) were exercised. Harness note: the first `say` sentence never played and the second ran ~4 min late while the user's audio was playing — a `say` quirk, not the app. Still unchecked: AirPods as the output device (none connected).

## 2026-10-29 — echo-cancellation spike (VoiceProcessingIO)

`scripts/aec-spike.sh --output "MacBook Air Speakers"`, quiet room, built-in speakers, a `say` sentence 2 s into a 12 s capture:

| | plain mic | VoiceProcessingIO |
|---|---:|---:|
| room (lead-in) | −54.9 dBFS | −60.9 dBFS |
| during playback | −30.6 dBFS | −51.9 dBFS |
| playback over room | +24.3 dB | +9.0 dB |
| envelope correlation peak | 0.157 | 0.178 |
| unit format | 48 kHz | 44.1 kHz (the unit's choice) |

Verdict: **partial** — VPIO attenuates audio played by other apps by ~15 dB (short of the 20 dB bar) and the
envelope correlation does not collapse, so a faint echo remains. Decision: the text-level echo rule stays
the primary mechanism; VoiceProcessing stays opt-in (`OATS_MIC_MODE=vpio`) as a possible attenuation
add-on. Not yet measured: whether Parakeet still transcribes the −52 dBFS residual, and the effect of
VPIO's processing on the user's own voice.
