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
