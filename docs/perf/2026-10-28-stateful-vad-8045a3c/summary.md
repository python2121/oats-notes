# Provenance
date: 2026-10-29T02:23:00Z
commit: 8045a3c (dirty)
perf.sh: 8045a3c
binary: <repo>/target/release/oats-notes (2026-10-28T19:07:53)
profile: release
machine: Apple M2 · 8 GB
os: macOS 27.0.1 (26A434)
env: OATS_BRAIN= OATS_ENGINE=

# OatsNotes perf summary: stateful-vad-ab

Steady state only (engine ready + 5 s → end of file / window). CPU % per core.

| metric | idle | idle-null |
|---|---:|---:|
| steady s | 131 | 131 |
| CPU mean % | 2.1 | 1.8 |
| CPU p95 % | 3.6 | 2.7 |
| top CPU mean % | 2.1 | 1.8 |
| RSS peak MB | 203 | 194 |
| RSS steady MB | 48 | 79 |
| footprint peak MB | 180 | 176 |
| energy impact mean | 2.1 | 1.8 |
| energy p95 | 3.2 | 2.4 |
| context switches/s | 283 | 273 |
| aned CPU % | 0.1 | 0.3 |
| WindowServer CPU % | 11.1 | 10.2 |
| GPU util % (system) | 2.1 | 2.3 |
| engine ready s | 2.3 | 1.2 |
| startup CPU s | 0.8 | 0.6 |
| segments | 0 | 0 |
| transcribe ms mean | – | – |
| transcribe ms p95 | – | – |
| transcribe ms max | – | – |
| segment length ms mean | – | – |
| RTF (proc/audio) | – | – |
| drain after file end s | – | – |
