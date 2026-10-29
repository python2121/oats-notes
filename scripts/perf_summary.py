"""Summarise a scripts/perf.sh results directory as Markdown (stdout).

    python3 -I scripts/perf_summary.py <results dir> [scenario ...]

Uses the `steady` rows only (after engine ready + 5 s, before the audio file ended, outside
the `sample` profiling window). CPU % is per core (100 % = one core fully busy).
"""

import csv
import math
import os
import re
import sys


def pct(values, q):
    if not values:
        return float("nan")
    v = sorted(values)
    k = (len(v) - 1) * q
    lo, hi = math.floor(k), math.ceil(k)
    return v[lo] + (v[hi] - v[lo]) * (k - lo)


def mean(values):
    return sum(values) / len(values) if values else float("nan")


def mem_mb(s):
    """top's MEM column: '123M', '1024K', '1.2G', with an optional +/- suffix."""
    m = re.match(r"([\d.]+)([BKMG])", s)
    if not m:
        return float("nan")
    n, unit = float(m.group(1)), m.group(2)
    return n * {"B": 1 / 2**20, "K": 1 / 1024, "M": 1, "G": 1024}[unit]


def read_meta(d):
    meta = {}
    try:
        with open(os.path.join(d, "meta.txt")) as f:
            for line in f:
                k, _, v = line.strip().partition("=")
                meta[k] = v
    except OSError:
        pass
    return meta


def read_ps(d):
    with open(os.path.join(d, "ps.csv")) as f:
        return list(csv.DictReader(f))


def read_top(d, pids):
    """{pid: [(sample index, cpu, mem_mb, power, csw_cumulative)]}; sample 0 is dropped
    (top's first sample has no CPU delta)."""
    out = {p: [] for p in pids if p}
    idx = -1
    header = None
    try:
        f = open(os.path.join(d, "top.txt"))
    except OSError:
        return out
    with f:
        for line in f:
            parts = line.split()
            if parts[:1] == ["PID"]:
                idx += 1
                header = parts
                continue
            if header and parts and parts[0] in out and idx >= 1:
                row = dict(zip(header, parts))
                try:
                    out[parts[0]].append(
                        (
                            idx,
                            float(row.get("%CPU", "nan")),
                            mem_mb(row.get("MEM", "")),
                            float(row.get("POWER", "nan")),
                            float(re.sub(r"[^\d.]", "", row.get("CSW", "")) or "nan"),
                        )
                    )
                except ValueError:
                    pass
    return out


def segments(d):
    try:
        with open(os.path.join(d, "segments.txt")) as f:
            n, chars, first, last = f.read().strip().split("|")
            return int(n), int(chars), int(first), int(last)
    except (OSError, ValueError):
        return 0, 0, 0, 0


def transcription_times(d):
    durs, procs = [], []
    try:
        with open(os.path.join(d, "app.log"), errors="replace") as f:
            for line in f:
                m = re.search(r"segment (\d+) ms transcribed in (\d+) ms", line)
                if m:
                    durs.append(int(m.group(1)))
                    procs.append(int(m.group(2)))
    except OSError:
        pass
    return durs, procs


def fmt(x, nd=1):
    return "–" if x is None or (isinstance(x, float) and math.isnan(x)) else f"{x:.{nd}f}"


def summarise(root, name):
    d = os.path.join(root, name)
    if not os.path.isdir(d):
        return None
    meta = read_meta(d)
    rows = read_ps(d)
    steady = [r for r in rows if r["phase"] == "steady"]
    t = [float(r["t"]) for r in steady]
    ct = [float(r["cputime_s"]) for r in steady]
    per_s = []
    for i in range(1, len(steady)):
        dt = t[i] - t[i - 1]
        if dt > 0:
            per_s.append((ct[i] - ct[i - 1]) / dt * 100)
    cpu_mean = (ct[-1] - ct[0]) / (t[-1] - t[0]) * 100 if len(t) > 1 and t[-1] > t[0] else float("nan")
    rss_all = [int(r["rss_kb"]) / 1024 for r in rows]
    rss_steady = [int(r["rss_kb"]) / 1024 for r in steady]
    gpu = [float(r["gpu_util"]) for r in steady if r["gpu_util"]]
    total_cpu_s = float(rows[-1]["cputime_s"]) if rows else float("nan")
    startup_cpu_s = next((float(r["cputime_s"]) for r in steady[:1]), float("nan"))

    # top samples → phase by time (top starts ~0.6 s after t0, one sample per second)
    phase_at = {}
    for r in rows:
        phase_at[int(float(r["t"]))] = r["phase"]
    app, aned, ws = meta.get("app_pid"), meta.get("aned_pid"), meta.get("ws_pid")
    top = read_top(d, [app, aned, ws])

    def top_steady(pid):
        return [s for s in top.get(pid, []) if phase_at.get(s[0]) == "steady"]

    a = top_steady(app)
    top_cpu = [s[1] for s in a]
    footprint_peak = max((s[2] for s in top.get(app, [])), default=float("nan"))
    footprint_steady = mean([s[2] for s in a])
    power = [s[3] for s in a]
    wake = []
    for i in range(1, len(a)):
        if a[i][0] == a[i - 1][0] + 1:
            wake.append(a[i][4] - a[i - 1][4])
    aned_cpu = [s[1] for s in top_steady(aned)]
    ws_cpu = [s[1] for s in top_steady(ws)]

    durs, procs = transcription_times(d)
    nseg, chars, first, last = segments(d)

    def f(k):
        try:
            return float(meta.get(k) or "nan")
        except ValueError:
            return float("nan")

    ready, ended, closed = f("ready_s"), f("ended_s"), f("closed_s")
    return {
        "name": name,
        "steady_s": (t[-1] - t[0]) if len(t) > 1 else 0,
        "cpu_mean": cpu_mean,
        "cpu_p95": pct(per_s, 0.95),
        "cpu_max": max(per_s, default=float("nan")),
        "top_cpu_mean": mean(top_cpu),
        "top_cpu_p95": pct(top_cpu, 0.95),
        "rss_peak": max(rss_all, default=float("nan")),
        "rss_steady": mean(rss_steady),
        "footprint_peak": footprint_peak,
        "footprint_steady": footprint_steady,
        "power_mean": mean(power),
        "power_p95": pct(power, 0.95),
        "wakeups": mean(wake),
        "aned_cpu": mean(aned_cpu),
        "ws_cpu": mean(ws_cpu),
        "gpu_mean": mean(gpu),
        "gpu_p95": pct(gpu, 0.95),
        "ready": ready,
        "startup_cpu_s": startup_cpu_s,
        "total_cpu_s": total_cpu_s,
        "segments": nseg,
        "chars": chars,
        "tx_n": len(procs),
        "tx_mean": mean(procs),
        "tx_p95": pct(procs, 0.95),
        "tx_max": max(procs, default=float("nan")),
        "seg_mean_ms": mean(durs),
        "rtf": (sum(procs) / sum(durs)) if durs else float("nan"),
        "drain_s": closed - ended,
    }


def main():
    root = sys.argv[1]
    names = sys.argv[2:] or ["rest", "idle", "speech", "stress"]
    res = [r for r in (summarise(root, n) for n in names) if r]
    print(f"# OatsNotes perf summary: {os.path.basename(os.path.abspath(root))}\n")
    print("Steady state only (engine ready + 5 s → end of file / window). CPU % per core.\n")
    cols = [
        ("scenario", lambda r: r["name"]),
        ("steady s", lambda r: fmt(r["steady_s"], 0)),
        ("CPU mean %", lambda r: fmt(r["cpu_mean"])),
        ("CPU p95 %", lambda r: fmt(r["cpu_p95"])),
        ("top CPU mean %", lambda r: fmt(r["top_cpu_mean"])),
        ("RSS peak MB", lambda r: fmt(r["rss_peak"], 0)),
        ("RSS steady MB", lambda r: fmt(r["rss_steady"], 0)),
        ("footprint peak MB", lambda r: fmt(r["footprint_peak"], 0)),
        ("energy impact mean", lambda r: fmt(r["power_mean"])),
        ("energy p95", lambda r: fmt(r["power_p95"])),
        ("context switches/s", lambda r: fmt(r["wakeups"], 0)),
        ("aned CPU %", lambda r: fmt(r["aned_cpu"])),
        ("WindowServer CPU %", lambda r: fmt(r["ws_cpu"])),
        ("GPU util % (system)", lambda r: fmt(r["gpu_mean"])),
        ("engine ready s", lambda r: fmt(r["ready"])),
        ("startup CPU s", lambda r: fmt(r["startup_cpu_s"])),
        ("segments", lambda r: str(r["segments"])),
        ("transcribe ms mean", lambda r: fmt(r["tx_mean"], 0)),
        ("transcribe ms p95", lambda r: fmt(r["tx_p95"], 0)),
        ("transcribe ms max", lambda r: fmt(r["tx_max"], 0)),
        ("segment length ms mean", lambda r: fmt(r["seg_mean_ms"], 0)),
        ("RTF (proc/audio)", lambda r: fmt(r["rtf"], 4)),
        ("drain after file end s", lambda r: fmt(r["drain_s"])),
    ]
    print("| metric | " + " | ".join(r["name"] for r in res) + " |")
    print("|---|" + "---:|" * len(res))
    for title, get in cols[1:]:
        print(f"| {title} | " + " | ".join(get(r) for r in res) + " |")


if __name__ == "__main__":
    main()
