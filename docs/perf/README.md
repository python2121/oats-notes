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
