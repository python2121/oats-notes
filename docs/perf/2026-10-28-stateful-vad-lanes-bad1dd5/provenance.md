# Provenance
date: 2026-10-29T04:00:44Z
commit: bad1dd5 (dirty)
perf.sh: 8045a3c
binary: <repo>/target/release/oats-notes (2026-10-28T20:54:14)
profile: release
machine: Apple M2 · 8 GB
os: macOS 27.0.1 (26A434)
env: OATS_BRAIN= OATS_ENGINE=
speech run date: 2026-10-29T04:02:40Z

Notes (added by hand):
- "(dirty)" = uncommitted docs only (CLAUDE.md status line, docs/verification-2026-10-28.md);
  no code change; the binary was built from bad1dd5 at 20:54.
- Two invocations of scripts/perf.sh (shortened protocol, on request):
  idle: PERF_DURATION=90, PERF_SPEECH_WAV=long.wav (327 s, so the silence file outlasts the window);
  speech: PERF_SPEECH_WAV=long90.wav = the first 90.0 s of the same long.wav (Python wave cut),
  so 3 segments, not 11 as in the full-file runs.
- System lane ABSENT in both: --audio-file without --system-audio-file opens no system capture
  (log: "listening started (mic)"), and the System Audio Recording grant was not effective anyway
  (docs/verification-2026-10-28.md). Idle cost = mic lane only.
- Machine NOT quiet: waited the 5 min maximum; 1-min load never fell below 2.0 (5.9 at the end of the wait, 5.5–12.9 during the runs).
  idle start load: { 5.46 8.98 8.84 } top: 210.3 mediaanalysisd;32.5 TGOnDeviceInferenceProviderService;17.0 spotlightknowledged.updater;
  speech start load: { 9.24 9.10 8.88 } top: 201.1 mediaanalysisd;15.6 spotlightknowledged.updater;4.5 plugin-container;
  end load: { 12.85 10.74 9.57 }
  mediaanalysisd ran at ~200 % CPU throughout (Photos analysis), so absolute numbers are contaminated.
