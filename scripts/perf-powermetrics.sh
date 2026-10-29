#!/usr/bin/env bash
# Summarise a `powermetrics` capture: mean / peak CPU, GPU, ANE and combined power (mW), GPU
# active residency and frequency. powermetrics needs root, so capture it yourself in another
# terminal while a scripts/perf.sh scenario runs (speech: start both, capture ≥ 60 s):
#
#   sudo powermetrics --samplers cpu_power,gpu_power,ane_power -i 1000 -n 60 > pm-speech.txt
#   scripts/perf-powermetrics.sh pm-speech.txt [pm-idle.txt ...]
#
# Power is SYSTEM-WIDE (the whole SoC), not per process: take a baseline capture with the app
# closed (pm-baseline.txt) and read the app's cost as the difference. The first sample is
# skipped (powermetrics' first interval is often partial).
set -euo pipefail
[ $# -ge 1 ] || { sed -n '2,12p' "$0"; exit 2; }

for f in "$@"; do
    awk -v file="$f" '
        function add(key, v) {
            if (!(key in seen)) { order[++nk] = key; seen[key] = 1 }
            n[key]++; sum[key] += v; if (!(key in max) || v > max[key]) max[key] = v
        }
        /^\*+ Sampled system activity/ { sample++ }
        sample < 2 { next }
        /^CPU Power: /                       { add("CPU power mW", $3) }
        /^GPU Power: /                       { add("GPU power mW", $3) }
        /^ANE Power: /                       { add("ANE power mW", $3) }
        /^Combined Power \(CPU \+ GPU \+ ANE\): / { add("Combined power mW", $(NF-1)) }
        /^GPU HW active residency: /         { v = $5; sub("%", "", v); add("GPU active residency %", v) }
        /^GPU HW active frequency: /         { add("GPU active frequency MHz", $5) }
        /^E-Cluster HW active residency: /   { v = $5; sub("%", "", v); add("E-cluster active residency %", v) }
        /^P-Cluster HW active residency: /   { v = $5; sub("%", "", v); add("P-cluster active residency %", v) }
        END {
            printf "%s: %d samples (first skipped)\n", file, (sample > 0 ? sample - 1 : 0)
            if (nk == 0) { print "  no power lines found (was it captured with --samplers cpu_power,gpu_power,ane_power?)"; exit }
            printf "  %-30s %10s %10s\n", "metric", "mean", "peak"
            for (i = 1; i <= nk; i++) {
                k = order[i]
                printf "  %-30s %10.1f %10.1f\n", k, sum[k] / n[k], max[k]
            }
        }
    ' "$f"
done
