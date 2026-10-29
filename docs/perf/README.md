# Performance runs — keep them apples to apples

Every profiling run uses `scripts/perf.sh` (release build, `OATS_BRAIN=off`, models cached,
`OATS_NO_STATUS_ITEM=1`, deterministic `--audio-file` input: silence for `idle`, the same
327 s synthetic speech file for `speech`/`stress`). The script writes `provenance.md` (commit,
dirty flag, binary, profile, machine, OS) and `summary.md` next to the per-second samples (`<scenario>/ps.csv`).

Archive each run here as `docs/perf/<YYYY-MM-DD>-<label>-<commit>/` with `provenance.md`,
`summary.md` and each scenario's `ps.csv`, `meta.txt` and `segments.txt` (not the logs, stacks or WAVs). Compare runs only when
provenance matches on machine, OS major version, profile and env; otherwise note the
difference next to the numbers. `docs/performance-<date>.md` holds the narrative report.

Runs:
- `2026-10-28-baseline-e678c2c` — first pass: at rest 0.0 %, idle-listening 2.1 %, speech 4.6 %, stress 14.7 % CPU (one core).
- `2026-10-28-stateful-vad-8045a3c` — stateful VAD stream (vendored fluidaudio-rs, uncommitted tree on 8045a3c), stop-gaps removed: idle 2.1 %, idle-null 1.8 % → Silero path ≈ 0.3 points (baseline A/B: 1.1). Busy machine (a Firefox plugin process at ~70 % CPU; WindowServer ~10 %, GPU ~2 %), so compare the idle − idle-null delta, not the absolute numbers.
- `2026-10-28-stateful-vad-lanes-bad1dd5` — after the system-audio lane work (bad1dd5), shortened protocol (idle 90 s window, speech = first 90 s of long.wav, 3 segments), system lane absent (`--audio-file` only): idle 1.2 % (p95 1.8), speech 2.8 % mean (3 transcription spikes; p95 2.0), RSS steady 41 / 45 MB. Busy machine (mediaanalysisd ~200 %, load 5–13): contaminated, indicative only.
