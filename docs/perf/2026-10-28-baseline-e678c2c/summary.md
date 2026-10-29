# OatsNotes perf summary: perf-run1

Steady state only (engine ready + 5 s → end of file / window). CPU % per core.

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
| energy p95 | 0.0 | 2.5 | 13.1 | 62.3 |
| context switches/s | 1 | 127 | 330 | 952 |
| aned CPU % | 0.3 | 0.8 | 0.0 | 0.0 |
| WindowServer CPU % | 0.8 | 0.8 | 0.6 | 0.5 |
| GPU util % (system) | 0.0 | 0.4 | 0.0 | 0.0 |
| engine ready s | 2.2 | 3.4 | 2.3 | 2.2 |
| startup CPU s | 0.8 | 1.1 | 1.0 | 1.2 |
| segments | 0 | 0 | 11 | 11 |
| transcribe ms mean | – | – | 561 | 321 |
| transcribe ms p95 | – | – | 716 | 431 |
| transcribe ms max | – | – | 734 | 487 |
| segment length ms mean | – | – | 29719 | 29719 |
| RTF (proc/audio) | – | – | 0.0189 | 0.0108 |
| drain after file end s | – | – | 1.1 | 0.0 |
