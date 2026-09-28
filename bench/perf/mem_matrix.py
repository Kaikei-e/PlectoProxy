#!/usr/bin/env python3
"""Memory matrix for the body-tax investigation (docs/plans/performance_evolution_001.md, Stage 2).

Reproduces the ~317 MB "buffer-then-decide" RSS regime under controlled load and decomposes it into
the three hypotheses:
  (A) host<->guest body-copy round-trip  -> RSS scales with payload AND concurrency (/body vs /baseline).
  (B) pooling-allocator residency        -> trusted (pooled, cap ~cores/≤8) vs untrusted (fresh/req) delta.
  (C) glibc arena retention              -> settled RSS after load stays high; drops under jemalloc /
                                            MALLOC_ARENA_MAX=1 (the allocator sweep).

Confound controls (all three the user asked for):
  * upstream runs as its OWN process on its OWN cores, so /proc/<proxy>/smaps_rollup is proxy-only.
  * RSS is sampled as a TIME SERIES (every 200 ms) during load AND for TAIL seconds after load stops,
    so `peak` and `settled` (does-not-come-back) are separated and the 294/317 snapshot variance is
    resolved into a curve.
  * a fresh proxy per (route,size), so a large body's grown linear memory never contaminates a
    smaller cell.

Run:  taskset-friendly, fully loopback.
  python3 bench/perf/mem_matrix.py           # full matrix + allocator sweep
  DUR=12s TAIL=10 python3 bench/perf/mem_matrix.py
Output: prints a summary table and writes summary.csv + worst-cell timeseries.csv to OUTDIR
(default: a scratch dir; override with OUTDIR=...).
"""
import csv
import json
import math
import os
import pathlib
import subprocess
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[2]
WS = ROOT / "plecto"
EX = WS / "target" / "release" / "examples"
K6 = os.environ.get("K6", "k6")
K6_SCRIPT = ROOT / "bench" / "k6-wasm" / "body-transform.js"

OUTDIR = pathlib.Path(os.environ.get("OUTDIR", "/tmp/mem-matrix"))

NCPU = os.cpu_count() or 24
PROXY_CPUS = os.environ.get("PROXY_CPUS", "0-7")
UP_CPUS = os.environ.get("UP_CPUS", "8-11")
GEN_CPUS = os.environ.get("GEN_CPUS", f"12-{NCPU - 1}")

PROXY_ADDR = os.environ.get("PLECTO_PROXY_ADDR", "127.0.0.1:28086")
UPSTREAM_ADDR = os.environ.get("UPSTREAM_ADDR", "127.0.0.1:28090")

SIZES = [int(x) for x in os.environ.get("SIZES", "1024,102400,1048576").split(",")]
VUS = [int(x) for x in os.environ.get("VUS", "1,50").split(",")]
ROUTES = os.environ.get("ROUTES", "baseline,body,body-headeronly").split(",")
DUR = os.environ.get("DUR", "12s")
TAIL_S = int(os.environ.get("TAIL", "10"))
SAMPLE_S = 0.2


def smaps_rollup(pid):
    """Return {Rss,Pss,Private_Dirty,Referenced} in kB for pid, or {} if gone."""
    out = {}
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                p = line.split()
                if len(p) >= 2 and p[0] in ("Rss:", "Pss:", "Private_Dirty:", "Referenced:"):
                    out[p[0][:-1]] = int(p[1])
    except (FileNotFoundError, ProcessLookupError):
        pass
    return out


class Sampler(threading.Thread):
    """Poll smaps_rollup every SAMPLE_S; keep (t, rss_kb, pd_kb) rows until stopped."""

    def __init__(self, pid):
        super().__init__(daemon=True)
        self.pid = pid
        self.rows = []
        self.status_rows = []
        self.started = time.monotonic()
        self.error = None
        self._stopping = threading.Event()

    def run(self):
        try:
            while not self._stopping.is_set():
                m = smaps_rollup(self.pid)
                if not m:
                    raise RuntimeError(f"proxy {self.pid}: missing smaps_rollup sample")
                t = time.monotonic() - self.started
                self.rows.append((t, m["Rss"], m["Private_Dirty"]))
                status = pathlib.Path(f"/proc/{self.pid}/status").read_text()
                threads = next(int(line.split()[1]) for line in status.splitlines()
                               if line.startswith("Threads:"))
                self.status_rows.append((t, threads))
                self._stopping.wait(SAMPLE_S)
        except Exception as exc:
            self.error = exc

    def stop(self):
        # Thread.join() calls Thread._stop(); never shadow it with an Event.
        self._stopping.set()
        self.join(timeout=2)
        if self.is_alive():
            raise RuntimeError("RSS sampler did not stop")
        if self.error:
            raise self.error


def memory_summary(rows, load_start, load_end, tail_end):
    """Separate loaded peak from the final two seconds of the post-load tail."""
    loaded = [r for r in rows if load_start <= r[0] <= load_end]
    settled = [r for r in rows if max(load_end, tail_end - 2) <= r[0] <= tail_end]
    if not loaded or len(settled) < 2 or tail_end - load_end < 2:
        raise ValueError("missing loaded samples or post-load tail shorter than two seconds")
    return {
        "peak_kb": max(r[1] for r in loaded),
        "peak_pd_kb": max(r[2] for r in loaded),
        "settled_kb": round(sum(r[1] for r in settled) / len(settled)),
    }


def read_k6(path):
    """Do not let a failed generator or failed HTTP traffic look like a cheap allocator."""
    d = json.loads(pathlib.Path(path).read_text())
    if not math.isfinite(d["rps"]) or d["rps"] <= 0 or d["failed_rate"] != 0:
        raise ValueError(f"invalid/failed k6 measurement: {path}")
    return d


def taskset(cpus, argv, env_extra):
    env = {**os.environ, **env_extra}
    env.pop("MALLOC_ARENA_MAX", None)  # an inherited value would silently cap the "glibc" cells
    return subprocess.Popen(
        ["taskset", "-c", cpus, *argv],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def wait_healthy(url, tries=100):
    for _ in range(tries):
        try:
            r = subprocess.run(["curl", "-fsS", url], capture_output=True, timeout=2)
            if r.returncode == 0:
                return True
        except Exception:
            pass
        time.sleep(0.25)
    return False


def launch_proxy(binary, env_extra):
    env = {"PLECTO_PROXY_ADDR": PROXY_ADDR, "UPSTREAM_ADDR": UPSTREAM_ADDR, "RUST_LOG": "warn", **env_extra}
    p = taskset(PROXY_CPUS, [str(EX / binary)], env)
    if not wait_healthy(f"http://{PROXY_ADDR}/baseline/x"):
        p.terminate()
        raise RuntimeError(f"proxy {binary} did not become healthy")
    return p


def run_k6(route, size, vus, out_json):
    env = {
        "BASE": f"http://{PROXY_ADDR}",
        "ROUTE_PATH": f"/{route}",
        "SIZE": str(size),
        "VUS": str(vus),
        "DUR": DUR,
        "OUT": str(out_json),
        "K6_NO_USAGE_REPORT": "true",
    }
    subprocess.run(
        ["taskset", "-c", GEN_CPUS, K6, "run", "-q", *sum([["-e", f"{k}={v}"] for k, v in env.items()], []), str(K6_SCRIPT)],
        env={**os.environ, **env},
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=True,
    )
    d = read_k6(out_json)
    return d["rps"], d["req_mbps"], d["failed_rate"]


def measure(pid, route, size, vus, tag):
    """Run one loaded cell: sample RSS through the load + a post-load tail; return the metrics."""
    s = Sampler(pid)
    s.start()
    time.sleep(0.5)
    idle = smaps_rollup(pid).get("Rss", 0)
    out_json = OUTDIR / f"k6_{tag}.json"
    load_start = time.monotonic() - s.started
    try:
        rps, mbps, failed = run_k6(route, size, vus, out_json)
        load_end = time.monotonic() - s.started
        # post-load tail: keep sampling to see what does NOT come back (the (C) retention signal).
        time.sleep(TAIL_S)
        tail_end = time.monotonic() - s.started
    finally:
        s.stop()
    summary = memory_summary(s.rows, load_start, load_end, tail_end)
    with (OUTDIR / f"timeseries_{tag}.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t_s", "rss_kb", "private_dirty_kb", "phase", "threads"])
        for (t, rss, pd), (_, threads) in zip(s.rows, s.status_rows, strict=True):
            phase = "idle" if t < load_start else "load" if t <= load_end else "tail"
            w.writerow([round(t, 3), rss, pd, phase, threads])
    return {
        "idle_kb": idle,
        **summary,
        "rps": round(rps, 1),
        "req_mbps": round(mbps, 2),
        "failed": round(failed, 4),
        "series": s.rows,
    }


def main():
    OUTDIR.mkdir(parents=True, exist_ok=True)
    up = taskset(UP_CPUS, [str(EX / "upstream")], {"UPSTREAM_ADDR": UPSTREAM_ADDR, "RESP_BYTES": "16"})
    time.sleep(1.0)
    rows = []
    worst_series = None
    try:
        # ---- Full matrix (glibc default allocator) --------------------------------------------
        for route in ROUTES:
            for size in SIZES:
                for vus in VUS:
                    proxy = launch_proxy("bench-server", {"PLECTO_MALLOC_ARENA_MAX": "0"})
                    try:
                        tag = f"{route}_{size}_{vus}_glibc"
                        m = measure(proxy.pid, route, size, vus, tag)
                        rows.append(("glibc", route, size, vus, m))
                        print(
                            f"[glibc] {route:<15} {size:>8}B vus={vus:<3} "
                            f"idle={m['idle_kb']//1024}M peak={m['peak_kb']//1024}M "
                            f"settled={m['settled_kb']//1024}M pd={m['peak_pd_kb']//1024}M "
                            f"rps={m['rps']} fail={m['failed']}",
                            flush=True,
                        )
                        if route == "body" and size == 1048576 and vus == 50:
                            worst_series = m["series"]
                    finally:
                        proxy.terminate()
                        proxy.wait(timeout=5)
        # ---- Allocator sweep on the worst cell (body, 1 MB, 50 VUs) ----------------------------
        sweep = [
            ("glibc", "bench-server", {"PLECTO_MALLOC_ARENA_MAX": "0"}),
            ("arena4", "bench-server", {"PLECTO_MALLOC_ARENA_MAX": "4"}),
            ("arena1", "bench-server", {"PLECTO_MALLOC_ARENA_MAX": "1"}),
            ("jemalloc", "bench-server-jemalloc", {"PLECTO_MALLOC_ARENA_MAX": "0"}),
        ]
        for alloc, binary, env_extra in sweep:
            proxy = launch_proxy(binary, env_extra)
            try:
                tag = f"sweep_body_1048576_50_{alloc}"
                m = measure(proxy.pid, "body", 1048576, 50, tag)
                rows.append((f"sweep:{alloc}", "body", 1048576, 50, m))
                print(
                    f"[sweep:{alloc}] body 1MB vus=50 peak={m['peak_kb']//1024}M "
                    f"settled={m['settled_kb']//1024}M (retained={100*m['settled_kb']//max(1,m['peak_kb'])}% of peak)",
                    flush=True,
                )
            finally:
                proxy.terminate()
                proxy.wait(timeout=5)
    finally:
        up.terminate()
        up.wait(timeout=5)

    # ---- Write CSVs ---------------------------------------------------------------------------
    with open(OUTDIR / "summary.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["alloc", "route", "size", "vus", "idle_kb", "peak_kb", "settled_kb", "peak_pd_kb", "rps", "req_mbps", "failed"])
        for alloc, route, size, vus, m in rows:
            w.writerow([alloc, route, size, vus, m["idle_kb"], m["peak_kb"], m["settled_kb"], m["peak_pd_kb"], m["rps"], m["req_mbps"], m["failed"]])
    if worst_series:
        with open(OUTDIR / "timeseries_worst.csv", "w", newline="") as f:
            w = csv.writer(f)
            w.writerow(["t_s", "rss_kb", "private_dirty_kb"])
            for t, rss, pd in worst_series:
                w.writerow([round(t, 2), rss, pd])
    print(f"\nwrote {OUTDIR}/summary.csv ({len(rows)} rows)")


if __name__ == "__main__":
    main()
