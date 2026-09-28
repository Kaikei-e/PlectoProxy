#!/usr/bin/env python3
"""Compare allocator caps with an external upstream and proxy-only RSS time series.

Build: cd plecto && cargo build --release --locked -p plecto-server \
    --features bench-harnesses --example bench-server --example upstream
Run memory first, then throughput into a DIFFERENT output directory:
  python3 bench/perf/arena_sweep.py --phase memory --out /tmp/arena-memory
  python3 bench/perf/arena_sweep.py --phase throughput --out /tmp/arena-throughput

PROXY_CPUS / UP_CPUS / GEN_CPUS must be disjoint whole physical cores. Defaults suit the
24-logical-CPU reference host and preserve its original 12-logical-CPU proxy partition.
This is a T3 experiment: reports absolute throughput and memory, never applies T1 gate bands.
"""

import argparse
from contextlib import contextmanager
import csv
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import time
import urllib.request

from allocator_env import clean_env
from cpu_split import get_physical_cores, parse_cpuset
from mem_matrix import Sampler, memory_summary, read_k6, smaps_rollup

ROOT = Path(__file__).resolve().parents[2]
EX = ROOT / "plecto/target/release/examples"


def parse_arenas(spec: str) -> list:
    arenas = []
    for part in spec.split(","):
        p = part.strip()
        if not p:
            continue
        if p == "default":
            arenas.append("default")
        else:
            val = int(p)
            if val < 0:
                raise ValueError(f"nonnegative arenas required, got {val}")
            arenas.append(val)
    if not arenas:
        raise ValueError("empty arenas specification")
    return arenas


def build_proxy_env(proxy_addr: str, up_addr: str, arena) -> dict:
    env = {"PLECTO_PROXY_ADDR": proxy_addr, "UPSTREAM_ADDR": up_addr}
    if arena != "default":
        env["PLECTO_MALLOC_ARENA_MAX"] = str(arena)
    return env


def validate_cpus(groups):
    sets = [parse_cpuset(g) for g in groups]
    available = os.sched_getaffinity(0)
    for cpus in sets:
        if not cpus or not cpus <= available:
            raise ValueError(f"empty/unavailable CPU partition: {cpus}")
    for core in get_physical_cores():
        if sum(bool(set(core) & cpus) for cpus in sets) > 1:
            raise ValueError(f"partitions share physical core {core}")


def check_affinity(pid, expected):
    tasks = list(Path(f"/proc/{pid}/task").iterdir())
    if not tasks:
        raise RuntimeError(f"no threads for {pid}")
    for task in tasks:
        try:
            actual = os.sched_getaffinity(int(task.name))
        except ProcessLookupError:
            continue
        if actual != parse_cpuset(expected):
            raise RuntimeError(f"wrong affinity for thread {task.name}: {actual}")


@contextmanager
def server(binary, cpus, env, url, log):
    with log.open("w") as out:
        proc = subprocess.Popen(["taskset", "-c", cpus, str(EX / binary)],
                                env=clean_env(env), stdout=out, stderr=out)
        try:
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            for _ in range(100):
                if proc.poll() is not None:
                    raise RuntimeError(f"{binary} exited; see {log}")
                try:
                    with opener.open(url, timeout=1) as response:
                        if response.status == 200:
                            break
                except OSError:
                    pass
                time.sleep(0.1)
            else:
                raise RuntimeError(f"{binary} not ready; see {log}")
            if proc.poll() is not None:
                raise RuntimeError(f"{binary} exited during readiness; see {log}")
            check_affinity(proc.pid, cpus)
            yield proc
            if proc.poll() is not None:
                raise RuntimeError(f"{binary} exited during measurement; see {log}")
            check_affinity(proc.pid, cpus)
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()


def read_oha(path):
    d = json.loads(path.read_text())
    rps = d["summary"]["requestsPerSec"]
    errors = dict(d["errorDistribution"])
    # oha -z cancels the at-most-50 in-flight requests when its window ends. Those
    # cancellations are reported separately and do not lower summary.successRate.
    cancelled = errors.pop("aborted due to deadline", 0)
    if (not math.isfinite(rps) or rps <= 0 or d["summary"]["successRate"] != 1
            or errors or not 0 <= cancelled <= 50 or set(d["statusCodeDistribution"]) != {"200"}):
        raise ValueError(f"invalid/failed oha measurement: {path}")
    return {"rps": rps, "p50_ms": d["latencyPercentiles"]["p50"] * 1000,
            "p99_ms": d["latencyPercentiles"]["p99"] * 1000}


def run_oha(args, base, route, tag, warm=False):
    path = args.out / f"{tag}.json"
    cmd = [args.oha, "-z", f"{args.warmup if warm else args.duration}s", "-c", "50",
           "--no-tui", "--output-format", "json"]
    if route == "trusted":
        cmd += ["-H", "x-api-key: alice-secret"]
    cmd += [f"http://{base}/{route}/x"]
    with path.open("w") as out, path.with_suffix(".log").open("w") as log:
        subprocess.run(["taskset", "-c", args.gen_cpus, *cmd], env=clean_env({}),
                       stdout=out, stderr=log, check=True, timeout=args.duration + args.warmup + 30)
    return read_oha(path)


def run_k6(args, base, route, tag, body=False):
    path = args.out / f"{tag}.json"
    values = {"BASE": f"http://{base}", "ROUTE_PATH": f"/{route}", "VUS": "50",
              "DUR": str(args.body_duration if body else args.duration),
              "WARMUP_S": str(args.warmup), "OUT": str(path),
              "SIZE": "1048576", "KEYS": "1000"}
    script = "body-transform.js" if body else "ratelimit-overhead.js"
    cmd = [args.k6, "run", "-q"]
    for k, v in values.items():
        cmd += ["-e", f"{k}={v}"]
    cmd += [str(ROOT / "bench/k6-wasm" / script)]
    with path.with_suffix(".log").open("w") as log:
        subprocess.run(["taskset", "-c", args.gen_cpus, *cmd], env=clean_env({}),
                       stdout=log, stderr=log, check=True,
                       timeout=args.body_duration + args.duration + args.warmup + 30)
    d = read_k6(path)
    return {"rps": d["rps"], "p50_ms": d["p50"], "p99_ms": d["p99"]}


def measure_memory(args, proc, route, tag):
    idle = smaps_rollup(proc.pid)["Rss"]
    sampler = Sampler(proc.pid)
    sampler.start()
    start = time.monotonic() - sampler.started
    try:
        result = run_k6(args, args.proxy_addr, route, tag, body=True)
        end = time.monotonic() - sampler.started
        check_affinity(proc.pid, args.proxy_cpus)
        time.sleep(args.tail)
        tail_end = time.monotonic() - sampler.started
    finally:
        sampler.stop()
    with (args.out / f"{tag}-rss.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t_s", "rss_kb", "private_dirty_kb", "phase", "threads"])
        for (t, rss, pd), (_, threads) in zip(sampler.rows, sampler.status_rows, strict=True):
            w.writerow([round(t, 3), rss, pd, "load" if t <= end else "tail", threads])
    return {**result, "idle_kb": idle, **memory_summary(sampler.rows, start, end, tail_end),
            "threads_max": max(n for t, n in sampler.status_rows if start <= t <= end)}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--phase", choices=["memory", "throughput"], required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--arenas", default="4,16,24,32,0")
    p.add_argument("--rounds", type=int, default=2)
    p.add_argument("--inner-rounds", type=int, default=3)
    p.add_argument("--duration", type=int, default=10)
    p.add_argument("--body-duration", type=int, default=15)
    p.add_argument("--warmup", type=int, default=5)
    p.add_argument("--tail", type=int, default=15)
    p.add_argument("--routes", default="baseline,body,body-headeronly")
    args = p.parse_args()
    args.out = args.out.resolve()
    args.proxy_cpus = os.environ.get("PROXY_CPUS", "0-11")
    args.up_cpus = os.environ.get("UP_CPUS", "16-19")
    args.gen_cpus = os.environ.get("GEN_CPUS", "12-15,20-23")
    args.proxy_addr = os.environ.get("PLECTO_PROXY_ADDR", "127.0.0.1:29086")
    args.up_addr = os.environ.get("UPSTREAM_ADDR", "127.0.0.1:29090")
    args.oha = os.environ.get("OHA", shutil.which("oha"))
    args.k6 = os.environ.get("K6", shutil.which("k6"))
    try:
        arenas = parse_arenas(args.arenas)
    except ValueError as e:
        p.error(str(e))
    if min(args.rounds, args.inner_rounds, args.duration, args.body_duration,
           args.warmup) < 1 or args.tail < 3:
        p.error("positive windows/rounds, and tail >= 3 required")
    if os.environ.get("LD_PRELOAD"):
        p.error("unset LD_PRELOAD before allocator measurements")
    validate_cpus([args.proxy_cpus, args.up_cpus, args.gen_cpus])
    args.out.mkdir(parents=True, exist_ok=False)
    metadata = {**vars(args), "out": str(args.out), "date": datetime.now(timezone.utc).isoformat(),
                "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                "glibc": subprocess.check_output(["getconf", "GNU_LIBC_VERSION"], text=True).strip(),
                "binary_sha256": {name: hashlib.sha256((EX / name).read_bytes()).hexdigest()
                                  for name in ["bench-server", "upstream"]}}
    metadata["harness_sha256"] = {name: hashlib.sha256((Path(__file__).parent / name).read_bytes()).hexdigest()
                                  for name in ["arena_sweep.py", "mem_matrix.py"]}
    metadata["tracked_diff_sha256"] = hashlib.sha256(
        subprocess.check_output(["git", "diff", "HEAD"], cwd=ROOT)).hexdigest()
    metadata["kernel"] = ".".join(os.uname().release.split(".")[:2])
    metadata["cpu_topology"] = get_physical_cores()
    metadata["governors"] = {p.parent.parent.name: p.read_text().strip()
                              for p in Path("/sys/devices/system/cpu").glob("cpu*/cpufreq/scaling_governor")}
    metadata["tools"] = {name: subprocess.check_output([binary, "--version"], text=True).strip()
                         for name, binary in [("oha", args.oha), ("k6", args.k6)]}
    (args.out / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    columns = ["round", "arena", "kind", "route", "sample", "rps", "p50_ms", "p99_ms",
               "idle_kb", "peak_kb", "settled_kb", "peak_pd_kb", "threads_max"]
    with (args.out / "summary.csv").open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=columns)
        w.writeheader()

        def record(round_n, arena, kind, route, sample, metrics):
            row = dict(round=round_n, arena=arena, kind=kind, route=route, sample=sample, **metrics)
            w.writerow(row)
            f.flush()
            print(json.dumps(row), flush=True)

        for round_n in range(1, args.rounds + 1):
            order = arenas if round_n % 2 else list(reversed(arenas))
            for arena in order:
                tag = f"r{round_n}-n{arena}"
                # Backend uses identical glibc defaults for EVERY cell, independent of proxy cap.
                with server("upstream", args.up_cpus, {"UPSTREAM_ADDR": args.up_addr,
                            "RESP_BYTES": "16", "BACKEND_LATENCY_MS": "0"},
                            f"http://{args.up_addr}/", args.out / f"{tag}-upstream.log"):
                    if arena == order[0]:
                        # Check generator/backend headroom without the proxy in each round.
                        if args.phase == "memory":
                            metrics = run_k6(args, args.up_addr, "baseline", f"r{round_n}-direct-body", body=True)
                            record(round_n, "", "direct-body", "baseline", 1, metrics)
                        else:
                            run_oha(args, args.up_addr, "baseline", f"r{round_n}-direct-warm", warm=True)
                            metrics = run_oha(args, args.up_addr, "baseline", f"r{round_n}-direct-oha")
                            record(round_n, "", "direct-oha", "baseline", 1, metrics)
                            metrics = run_k6(args, args.up_addr, "baseline", f"r{round_n}-direct-k6")
                            record(round_n, "", "direct-k6", "baseline", 1, metrics)
                    proxy_env = build_proxy_env(args.proxy_addr, args.up_addr, arena)
                    # Match the executable's startup mallopt; no extra MALLOC_ARENA_MAX at exec.
                    if args.phase == "memory":
                        for route in args.routes.split(","):
                            with server("bench-server", args.proxy_cpus, proxy_env,
                                        f"http://{args.proxy_addr}/baseline/x",
                                        args.out / f"{tag}-{route}-proxy.log") as proc:
                                metrics = measure_memory(args, proc, route, f"{tag}-{route}")
                                record(round_n, arena, "body-1MiB", route, 1, metrics)
                    else:
                        with server("bench-server", args.proxy_cpus, proxy_env,
                                    f"http://{args.proxy_addr}/baseline/x",
                                    args.out / f"{tag}-proxy.log") as proc:
                            sampler = Sampler(proc.pid)
                            sampler.start()
                            try:
                                for i in range(1, args.inner_rounds + 1):
                                    for route in ["baseline", "noop-pooled", "trusted"]:
                                        sample_tag = f"{tag}-s{i}-{route}"
                                        if i == 1:
                                            run_oha(args, args.proxy_addr, route, sample_tag + "-warm", warm=True)
                                        metrics = run_oha(args, args.proxy_addr, route, sample_tag)
                                        record(round_n, arena, "oha", route, i, metrics)
                                for i in range(1, 3):
                                    for route in ["baseline", "ratelimit"]:
                                        metrics = run_k6(args, args.proxy_addr, route, f"{tag}-rl{i}-{route}")
                                        record(round_n, arena, "k6", route, i, metrics)
                            finally:
                                sampler.stop()
                            with (args.out / f"{tag}-threads.csv").open("w", newline="") as tf:
                                tw = csv.writer(tf)
                                tw.writerow(["t_s", "threads"])
                                tw.writerows(sampler.status_rows)
    (args.out / "complete").write_text(datetime.now(timezone.utc).isoformat() + "\n")


if __name__ == "__main__":
    main()
