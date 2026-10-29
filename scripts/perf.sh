#!/usr/bin/env bash
# Resource cost of OatsNotes while it listens: CPU, memory, energy impact, GPU utilisation,
# transcription timings, across four scenarios. Release build, no `claude` calls.
#
#   scripts/perf.sh [scenario ...]        # default: rest idle speech stress
#
# Scenarios (each a fresh data dir; OATS_BRAIN=off OATS_NO_STATUS_ITEM=1):
#   rest    app open, window visible, not listening                    PERF_DURATION s
#   idle    --listen --audio-file <silence>.wav (VAD on silence)       PERF_DURATION s
#   speech  --listen --audio-file <speech>.wav at real time            until the file ends
#   stress  --listen --audio-file <speech>.wav --audio-speed 4         until the file ends
# Optional (not in the default set):
#   idle-null  idle with OATS_ENGINE=null: the core's energy VAD instead of Silero, so
#              idle − idle-null ≈ the cost of the Silero VAD path (CoreML + Swift bridge)
#
# Every second: `top` (CPU %, memory footprint, energy impact "POWER", context switches) for the
# app, `aned` (the Neural Engine daemon) and WindowServer; `ps` (cumulative CPU time, RSS,
# VSZ) for the app; the GPU's system-wide "Device Utilization %" from ioreg (no root needed).
# Rows are labelled by phase — startup (until 5 s after "speech engine ready"), steady,
# profiling (a 10 s `sample` stack capture, PERF_SAMPLE=1), drain (after the audio file ended)
# — and the summary uses the steady rows only.
#
# Env: PERF_OUT (results dir; default target/perf/<timestamp>), PERF_DURATION (120),
#      PERF_SPEECH_WAV (default $SCRATCH/long.wav or ./long.wav), PERF_BIN
#      (target/release/oats-notes; built if missing), PERF_SAMPLE (1: capture `sample` stacks
#      in idle and speech), PERF_STRESS_SPEED (4), PERF_FRONT (1: bring the window to the
#      front once the engine is ready, via osascript; needs Automation permission for the
#      terminal — an occluded window is cheaper to repaint, so say which you measured).
# Output: <out>/<scenario>/{samples.csv,app.log,oslog.txt,sample.txt,data/}, <out>/summary.md
# GPU / ANE power needs root: see scripts/perf-powermetrics.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

DURATION=${PERF_DURATION:-120}
STRESS_SPEED=${PERF_STRESS_SPEED:-4}
OUT=${PERF_OUT:-$ROOT/target/perf/$(date +%Y%m%d-%H%M%S)}

# Provenance, so runs archived under docs/perf/ stay comparable over time (apples to apples):
# commit, dirty tree, build profile, machine, OS, script version. Written before anything runs.
write_provenance() {
    mkdir -p "$OUT"
    {
        echo "# Provenance"
        echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "commit: $(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)$(git -C "$ROOT" diff --quiet 2>/dev/null || echo ' (dirty)')"
        echo "perf.sh: $(git -C "$ROOT" log -1 --format=%h -- scripts/perf.sh scripts/perf_summary.py 2>/dev/null || echo unknown)"
        echo "binary: $BIN ($(stat -f %Sm -t %Y-%m-%dT%H:%M:%S "$BIN" 2>/dev/null || echo missing))"
        echo "profile: $( [[ "$BIN" == */release/* ]] && echo release || echo debug )"
        echo "machine: $(sysctl -n machdep.cpu.brand_string 2>/dev/null) · $(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824 )) GB"
        echo "os: $(sw_vers -productName 2>/dev/null) $(sw_vers -productVersion 2>/dev/null) ($(sw_vers -buildVersion 2>/dev/null))"
        echo "env: OATS_BRAIN=${OATS_BRAIN:-} OATS_ENGINE=${OATS_ENGINE:-}"
        echo
    } > "$OUT/provenance.md"
}
BIN=${PERF_BIN:-$ROOT/target/release/oats-notes}
SPEECH=${PERF_SPEECH_WAV:-long.wav}
SAMPLE=${PERF_SAMPLE:-1}
SCENARIOS=("$@")
[ ${#SCENARIOS[@]} -eq 0 ] && SCENARIOS=(rest idle speech stress)

[ -f "$SPEECH" ] || { echo "perf: speech WAV not found: $SPEECH (set PERF_SPEECH_WAV)" >&2; exit 2; }
SPEECH=$(cd "$(dirname "$SPEECH")" && pwd)/$(basename "$SPEECH")
if [ ! -x "$BIN" ]; then
    cargo build --release -p oats-notes
fi
mkdir -p "$OUT"
write_provenance
# never leave the app, top or log stream behind (an error, ^C)
trap 'kill $(jobs -p) 2>/dev/null || true' EXIT
echo "perf: results in $OUT"

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }

# A silence WAV as long as the speech file (PCM 16-bit mono 16 kHz, all zeros).
SILENCE=$OUT/silence.wav
python3 -I - "$SPEECH" "$SILENCE" <<'PY'
import sys, wave
with wave.open(sys.argv[1], "rb") as w:
    assert (w.getnchannels(), w.getsampwidth(), w.getframerate()) == (1, 2, 16000), "need 16 kHz mono i16"
    n = w.getnframes()
with wave.open(sys.argv[2], "wb") as o:
    o.setnchannels(1); o.setsampwidth(2); o.setframerate(16000)
    o.writeframes(b"\0\0" * n)
print(f"perf: silence.wav {n/16000:.1f} s")
PY

pid_of() { pgrep -x "$1" | head -1 || true; }

# ps cputime "[[dd-]hh:]mm:ss.ss" → seconds
cpu_seconds() {
    awk -F'[:-]' '{ s=0; for (i=1;i<=NF;i++) s = s*60 + $i; if (NF==4) s = $1*86400 + $2*3600 + $3*60 + $4; printf "%.2f", s }'
}

run_scenario() {
    local name=$1; shift
    local dir=$OUT/$name
    rm -rf "$dir"; mkdir -p "$dir/data"
    local args=("$@")
    local until_end=0
    case $name in speech|stress) until_end=1 ;; esac

    # FluidAudio / CoreML log to the unified log (not stderr): keep what is visible
    log stream --style compact --level debug \
        --predicate 'process == "oats-notes" AND (subsystem BEGINSWITH "com.fluidinference" OR subsystem BEGINSWITH "com.apple.coreml" OR subsystem BEGINSWITH "com.apple.espresso" OR subsystem BEGINSWITH "com.apple.ane")' \
        > "$dir/oslog.txt" 2>&1 &
    local logpid=$!
    sleep 0.5

    local t0; t0=$(now)
    env OATS_BRAIN=off OATS_DATA_DIR="$dir/data" OATS_NO_STATUS_ITEM=1 ${EXTRA_ENV:-} \
        RUST_LOG=info,oats_core::pipeline=debug \
        "$BIN" ${args[@]+"${args[@]}"} > "$dir/app.log" 2>&1 &
    local pid=$!
    echo "perf: [$name] pid $pid: oats-notes ${args[*]-}"

    local aned ws
    aned=$(pid_of aned); ws=$(pid_of WindowServer)
    local tpids=(-pid "$pid"); [ -n "$aned" ] && tpids+=(-pid "$aned"); [ -n "$ws" ] && tpids+=(-pid "$ws")
    # top: delta CPU per 1 s sample; run long enough, killed at the end
    top -l 100000 -s 1 "${tpids[@]}" -stats pid,cpu,mem,power,csw > "$dir/top.txt" 2>&1 &
    local toppid=$!

    echo "t,phase,ps_cpu,cputime_s,rss_kb,vsz_kb,gpu_util" > "$dir/ps.csv"
    local ready="" ended="" closed="" profiled=0 phase=startup t
    local deadline=$(( DURATION ))
    while kill -0 "$pid" 2>/dev/null; do
        t=$(perl -e "printf '%.1f', $(now) - $t0")
        if [ -z "$ready" ] && grep -q "speech engine ready" "$dir/app.log"; then
            ready=$t; echo "perf: [$name] engine ready after ${ready} s"
            if [ "${PERF_FRONT:-0}" = 1 ]; then
                osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $pid) to true" \
                    > /dev/null 2>&1 && echo "perf: [$name] window brought to front" \
                    || echo "perf: [$name] could not bring the window to front (Automation permission?)"
            fi
        fi
        if [ -z "$ended" ] && grep -q "audio file ended" "$dir/app.log"; then
            ended=$t
        fi
        # the pipeline drained the channel and closed the session (the UI logs this)
        if [ -z "$closed" ] && grep -q "audio input closed" "$dir/app.log"; then
            closed=$t
        fi
        if [ -n "$ended" ]; then phase=drain
        elif [ -n "$ready" ] && perl -e "exit !($t >= $ready + 5)"; then phase=steady
        fi
        # profiling: 10 s of `sample` stacks, 60 s into the steady phase (idle, speech)
        if [ "$SAMPLE" = 1 ] && [ $profiled = 0 ] && [ -n "$ready" ] && [ -z "$ended" ] \
            && { [ "$name" = idle ] || [ "$name" = speech ] || [ "$name" = idle-null ]; } \
            && perl -e "exit !($t >= $ready + 65)"; then
            profiled=1
            sample "$pid" 10 -mayDie -file "$dir/sample.txt" > /dev/null 2>&1 &
            local samplepid=$!
            local pend; pend=$(perl -e "printf '%.1f', $t + 11")
        fi
        local cur=$phase
        if [ $profiled = 1 ] && perl -e "exit !($t < ${pend:-0})"; then cur=profiling; fi
        local row; row=$(ps -o %cpu=,cputime=,rss=,vsz= -p "$pid" 2>/dev/null || true)
        [ -z "$row" ] && break
        read -r pcpu ctime rss vsz <<< "$row"
        local csec; csec=$(echo "$ctime" | cpu_seconds)
        local gpu; gpu=$(ioreg -r -d 1 -w 0 -c IOAccelerator | grep -o '"Device Utilization %"=[0-9]*' | head -1 | cut -d= -f2)
        echo "$t,$cur,$pcpu,$csec,$rss,$vsz,${gpu:-}" >> "$dir/ps.csv"

        # stop conditions
        if [ $until_end = 1 ]; then
            # after the file ended: wait until the pipeline has drained and closed the session
            if [ -n "$closed" ] && perl -e "exit !($t >= $closed + 3)"; then break; fi
            if perl -e "exit !($t >= 1200)"; then echo "perf: [$name] 20 min cap" >&2; break; fi
        elif [ -n "$ready" ] && perl -e "exit !($t >= $ready + 5 + $deadline + ($profiled ? 11 : 0))"; then
            # rest / idle: DURATION s of steady state (plus the profiling window)
            if [ "$SAMPLE" != 1 ] || [ "$name" = rest ] || [ $profiled = 1 ]; then break; fi
        fi
        sleep 1
    done
    [ -n "${samplepid:-}" ] && wait "$samplepid" 2>/dev/null || true
    kill "$toppid" 2>/dev/null || true
    kill -TERM "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    kill -KILL "$pid" 2>/dev/null || true
    sleep 0.5
    kill "$logpid" 2>/dev/null || true
    wait 2>/dev/null || true

    printf '%s\n' "ready_s=${ready:-}" "ended_s=${ended:-}" "closed_s=${closed:-}" "aned_pid=${aned:-}" "ws_pid=${ws:-}" "app_pid=$pid" > "$dir/meta.txt"
    sqlite3 "$dir/data/oats.sqlite" \
        "select count(*), coalesce(sum(length(text)),0), coalesce(min(started_at),0), coalesce(max(ended_at),0) from segments" \
        > "$dir/segments.txt" 2>/dev/null || echo "0|0|0|0" > "$dir/segments.txt"
    echo "perf: [$name] done: $(cat "$dir/segments.txt" | cut -d'|' -f1) segments stored"
}

for s in "${SCENARIOS[@]}"; do
    case $s in
        rest)   run_scenario rest ;;
        idle)   run_scenario idle --listen --audio-file "$SILENCE" ;;
        idle-null) EXTRA_ENV=OATS_ENGINE=null run_scenario idle-null --listen --audio-file "$SILENCE" ;;
        speech) run_scenario speech --listen --audio-file "$SPEECH" ;;
        stress) run_scenario stress --listen --audio-file "$SPEECH" --audio-speed "$STRESS_SPEED" ;;
        *) echo "perf: unknown scenario $s" >&2; exit 2 ;;
    esac
    sleep 5   # let the machine settle between runs
done

{ cat "$OUT/provenance.md"; python3 -I "$ROOT/scripts/perf_summary.py" "$OUT" "${SCENARIOS[@]}"; } | tee "$OUT/summary.md"
