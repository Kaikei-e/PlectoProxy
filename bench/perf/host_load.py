#!/usr/bin/env python3
"""Pre-run host-contention check for the T1 perf gate.

Samples the 1-minute load average (/proc/loadavg) and per-CPU busy fraction
(/proc/stat delta over a short window) aggregated across a given CPU list
(typically PROXY_CPUS). Used by run-perf.sh before launching proxies or
generators so contended-host runs fail fast (exit 3 here -> exit 75 in gate).
"""

import argparse
import os
import sys
import time
from typing import Callable, Dict, Iterable, List, Optional, Set, Tuple

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cpu_split import format_cpuset, parse_cpuset  # noqa: E402


def parse_loadavg1(text: str) -> float:
    """Parse the 1-minute load average from /proc/loadavg text."""
    parts = text.strip().split()
    if not parts:
        raise ValueError("empty /proc/loadavg content")
    return float(parts[0])


def read_loadavg1(path: str = "/proc/loadavg") -> float:
    """Read the 1-minute load average from /proc/loadavg."""
    with open(path, "r") as f:
        return parse_loadavg1(f.read())


def parse_proc_stat(text: str) -> Dict[int, Tuple[int, int]]:
    """Parse /proc/stat per-CPU lines into {cpu_id: (total_jiffies, idle_jiffies)}.

    `idle_jiffies` combines `idle` (col 4) and `iowait` (col 5).
    `total_jiffies` sums up to the first 8 columns (user, nice, system, idle,
    iowait, irq, softirq, steal); Linux already folds guest/guest_nice into
    user/nice.
    """
    cpus: Dict[int, Tuple[int, int]] = {}
    for line in text.splitlines():
        parts = line.split()
        if not parts:
            continue
        label = parts[0]
        if label.startswith("cpu") and label[3:].isdigit():
            cpu_id = int(label[3:])
            vals = [int(x) for x in parts[1:]]
            if len(vals) < 4:
                raise ValueError(f"malformed /proc/stat line for {label}: {line!r}")
            idle = vals[3] + (vals[4] if len(vals) > 4 else 0)
            total = sum(vals[:8])
            cpus[cpu_id] = (total, idle)
    return cpus


def read_proc_stat(path: str = "/proc/stat") -> Dict[int, Tuple[int, int]]:
    """Read per-CPU jiffie counters from /proc/stat."""
    with open(path, "r") as f:
        return parse_proc_stat(f.read())


def busy_fraction_between(
    before: Dict[int, Tuple[int, int]],
    after: Dict[int, Tuple[int, int]],
    cpus: Iterable[int],
) -> float:
    """Compute aggregate non-idle, non-iowait jiffies / total across `cpus`."""
    target: Set[int] = set(cpus)
    if not target:
        raise ValueError("CPU list must not be empty")
    common = target & set(before) & set(after)
    if not common:
        raise ValueError(
            f"none of requested CPUs ({format_cpuset(target)}) found in /proc/stat"
        )

    total_delta = 0
    idle_delta = 0
    for cpu_id in common:
        t0, i0 = before[cpu_id]
        t1, i1 = after[cpu_id]
        dt = max(0, t1 - t0)
        di = max(0, min(dt, i1 - i0))
        total_delta += dt
        idle_delta += di

    if total_delta <= 0:
        return 0.0
    return (total_delta - idle_delta) / total_delta


def measure_host_load(
    cpus: Iterable[int],
    max_load: float,
    max_busy: float,
    window: float = 3.0,
    loadavg_path: str = "/proc/loadavg",
    stat_path: str = "/proc/stat",
    sleep_fn: Callable[[float], None] = time.sleep,
) -> dict:
    """Measure loadavg1 and aggregate CPU busy fraction over `window` seconds."""
    cpus_set = set(cpus)
    if not cpus_set:
        raise ValueError("CPU list must not be empty")
    if window < 0:
        raise ValueError(f"window must be >= 0, got {window}")

    before = read_proc_stat(stat_path)
    if window > 0:
        sleep_fn(window)
    after = read_proc_stat(stat_path)
    loadavg1 = read_loadavg1(loadavg_path)
    proxy_busy = busy_fraction_between(before, after, cpus_set)
    contended = 1 if (loadavg1 > max_load or proxy_busy > max_busy) else 0

    return {
        "loadavg1": loadavg1,
        "proxy_cpus": format_cpuset(cpus_set),
        "proxy_busy": proxy_busy,
        "max_load": max_load,
        "max_busy": max_busy,
        "contended": contended,
    }


def format_record(record: dict) -> str:
    """Format a measurement dict as key=value lines."""
    lines = [
        f"loadavg1={record['loadavg1']:.2f}",
        f"proxy_cpus={record['proxy_cpus']}",
        f"proxy_busy={record['proxy_busy']:.4f}",
        f"max_load={record['max_load']:g}",
        f"max_busy={record['max_busy']:g}",
        f"contended={record['contended']}",
    ]
    return "\n".join(lines) + "\n"


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        description="Check host load average and per-CPU busy fraction before running the perf gate."
    )
    parser.add_argument("--cpus", required=True, help="CPU list to aggregate (e.g. 0-11)")
    parser.add_argument("--max-load", required=True, type=float, help="Max 1-min loadavg")
    parser.add_argument("--max-busy", required=True, type=float, help="Max busy fraction (0..1)")
    parser.add_argument("--window", type=float, default=3.0, help="Sampling window in seconds")
    parser.add_argument("--out", default=None, help="Optional path to write key=value record")
    args = parser.parse_args(argv)

    try:
        cpus = parse_cpuset(args.cpus)
        if not cpus:
            raise ValueError("empty --cpus list")
        if args.window < 0:
            raise ValueError("--window must be >= 0")
        if args.max_load < 0 or args.max_busy < 0:
            raise ValueError("--max-load and --max-busy must be >= 0")
    except ValueError as e:
        sys.stderr.write(f"host_load.py: {e}\n")
        return 64

    try:
        record = measure_host_load(
            cpus=cpus,
            max_load=args.max_load,
            max_busy=args.max_busy,
            window=args.window,
        )
    except (OSError, ValueError) as e:
        sys.stderr.write(f"host_load.py: {e}\n")
        return 1

    output = format_record(record)
    sys.stdout.write(output)
    if args.out:
        out_dir = os.path.dirname(os.path.abspath(args.out))
        if out_dir:
            os.makedirs(out_dir, exist_ok=True)
        with open(args.out, "w") as f:
            f.write(output)

    return 3 if record["contended"] else 0


if __name__ == "__main__":
    sys.exit(main())
