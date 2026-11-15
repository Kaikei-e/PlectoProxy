#!/usr/bin/env python3
"""Pre-run and per-phase host-contention check for the perf harness.

Runs three checks over a sampling window: (1) 1-minute load average (/proc/loadavg),
(2) aggregate non-idle busy fraction (/proc/stat) across --cpus and optional --gen-cpus,
and (3) single-process CPU cores (/proc/<pid>/stat utime+stime delta). Used by
run-perf.sh as a fail-fast gate preflight (exit 3 -> 75) and per-phase recording.
"""

import argparse
import os
import re
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


_COMM_BAD_RE = re.compile(r"[,:=\s]")


def sanitize_comm(comm: str) -> str:
    """Replace ',', ':', '=', and whitespace in process comm with '_'."""
    return _COMM_BAD_RE.sub("_", comm)


def parse_pid_stat(text: str) -> Tuple[int, str, int]:
    """Parse /proc/<pid>/stat into (pid, comm, utime + stime jiffies)."""
    line = text.rstrip("\n")
    lparen = line.find("(")
    rparen = line.rfind(")")
    if lparen <= 0 or rparen <= lparen:
        raise ValueError(f"malformed /proc/<pid>/stat line: {line!r}")
    pid = int(line[:lparen].strip())
    comm = line[lparen + 1 : rparen]
    rest = line[rparen + 1 :].split()
    # Field 3 (state) is rest[0]; fields 14 (utime) and 15 (stime) are rest[11] and rest[12].
    if len(rest) <= 12:
        raise ValueError(f"truncated /proc/<pid>/stat line for pid {pid}: {line!r}")
    return pid, comm, int(rest[11]) + int(rest[12])


def read_proc_times(proc_root: str = "/proc") -> Dict[int, Tuple[str, int]]:
    """Read {pid: (comm, utime+stime jiffies)} across numeric /proc/<pid>/stat entries."""
    procs: Dict[int, Tuple[str, int]] = {}
    for entry in os.listdir(proc_root):
        if not entry.isdigit():
            continue
        stat_file = os.path.join(proc_root, entry, "stat")
        try:
            with open(stat_file, "r") as f:
                pid, comm, jiffies = parse_pid_stat(f.read())
        except (OSError, ValueError):
            # Processes may vanish or be unreadable mid-scan; skip them.
            continue
        procs[pid] = (comm, jiffies)
    return procs


def top_processes_between(
    before: Dict[int, Tuple[str, int]],
    after: Dict[int, Tuple[str, int]],
    elapsed_s: float,
    clk_tck: Optional[int] = None,
    exclude_pids: Optional[Set[int]] = None,
    limit: int = 3,
) -> List[Tuple[int, str, float]]:
    """Return up to `limit` heaviest processes as (pid, sanitized_comm, cores) descending."""
    if elapsed_s <= 0:
        return []
    tck = os.sysconf("SC_CLK_TCK") if clk_tck is None else clk_tck
    if tck <= 0:
        raise ValueError(f"invalid SC_CLK_TCK: {tck}")
    excluded = exclude_pids or set()
    denom = tck * elapsed_s
    usage: List[Tuple[int, str, float]] = []
    for pid, (comm, j1) in after.items():
        if pid in excluded or pid not in before:
            continue
        _, j0 = before[pid]
        dj = j1 - j0
        if dj <= 0:
            continue
        usage.append((pid, sanitize_comm(comm), dj / denom))
    usage.sort(key=lambda item: (-item[2], item[0]))
    return usage[:limit]


def measure_host_load(
    cpus: Iterable[int],
    max_load: float,
    max_busy: float,
    window: float = 3.0,
    loadavg_path: str = "/proc/loadavg",
    stat_path: str = "/proc/stat",
    sleep_fn: Callable[[float], None] = time.sleep,
    gen_cpus: Optional[Iterable[int]] = None,
    max_gen_busy: Optional[float] = None,
    max_proc_cores: float = 0.5,
    proc_root: str = "/proc",
    monotonic_fn: Callable[[], float] = time.monotonic,
    clk_tck: Optional[int] = None,
) -> dict:
    """Measure loadavg1, aggregate CPU busy fractions, and top per-process CPU over `window`."""
    cpus_set = set(cpus)
    if not cpus_set:
        raise ValueError("CPU list must not be empty")
    gen_cpus_set: Optional[Set[int]] = None
    if gen_cpus is not None:
        gen_cpus_set = set(gen_cpus)
        if not gen_cpus_set:
            raise ValueError("gen CPU list must not be empty")
    if window < 0:
        raise ValueError(f"window must be >= 0, got {window}")
    if max_proc_cores < 0:
        raise ValueError(f"max_proc_cores must be >= 0, got {max_proc_cores}")
    effective_max_gen_busy = max_busy if max_gen_busy is None else max_gen_busy
    if effective_max_gen_busy < 0:
        raise ValueError(f"max_gen_busy must be >= 0, got {effective_max_gen_busy}")

    before = read_proc_stat(stat_path)
    before_procs = read_proc_times(proc_root) if window > 0 else {}
    t0 = monotonic_fn()
    if window > 0:
        sleep_fn(window)
    t1 = monotonic_fn()
    after = read_proc_stat(stat_path)
    after_procs = read_proc_times(proc_root) if window > 0 else {}
    loadavg1 = read_loadavg1(loadavg_path)

    proxy_busy = busy_fraction_between(before, after, cpus_set)
    gen_busy: Optional[float] = (
        busy_fraction_between(before, after, gen_cpus_set)
        if gen_cpus_set is not None
        else None
    )
    # No CPU-affinity filtering: an unpinned hog is the case that matters. A process pinned away
    # from both sets can false-positive, and the operator can override.
    top_cpu = (
        top_processes_between(
            before_procs,
            after_procs,
            t1 - t0,
            clk_tck=clk_tck,
            exclude_pids={os.getpid()},
        )
        if window > 0
        else []
    )
    max_proc_cores_seen = top_cpu[0][2] if top_cpu else 0.0

    reasons: List[str] = []
    if loadavg1 > max_load:
        reasons.append("loadavg")
    if proxy_busy > max_busy:
        reasons.append("proxy_busy")
    if gen_busy is not None and gen_busy > effective_max_gen_busy:
        reasons.append("gen_busy")
    if max_proc_cores_seen > max_proc_cores:
        reasons.append("proc")
    contended = 1 if reasons else 0

    record: dict = {
        "loadavg1": loadavg1,
        "proxy_cpus": format_cpuset(cpus_set),
        "proxy_busy": proxy_busy,
        "top_cpu": top_cpu,
        "max_proc_cores_seen": max_proc_cores_seen,
        "max_load": max_load,
        "max_busy": max_busy,
        "max_proc_cores": max_proc_cores,
        "contended_reasons": reasons,
        "contended": contended,
    }
    if gen_cpus_set is not None and gen_busy is not None:
        record["gen_cpus"] = format_cpuset(gen_cpus_set)
        record["gen_busy"] = gen_busy
        record["max_gen_busy"] = effective_max_gen_busy
    return record


def format_record(record: dict) -> str:
    """Format a measurement dict as key=value lines."""
    top_str = ",".join(f"{pid}:{comm}:{cores:.2f}" for pid, comm, cores in record["top_cpu"])
    lines = [
        f"loadavg1={record['loadavg1']:.2f}",
        f"proxy_cpus={record['proxy_cpus']}",
        f"proxy_busy={record['proxy_busy']:.4f}",
    ]
    if "gen_cpus" in record:
        lines.extend([
            f"gen_cpus={record['gen_cpus']}",
            f"gen_busy={record['gen_busy']:.4f}",
            f"max_gen_busy={record['max_gen_busy']:g}",
        ])
    lines.extend([
        f"top_cpu={top_str}",
        f"max_proc_cores_seen={record['max_proc_cores_seen']:.2f}",
        f"max_load={record['max_load']:g}",
        f"max_busy={record['max_busy']:g}",
        f"max_proc_cores={record['max_proc_cores']:g}",
        f"contended_reasons={','.join(record['contended_reasons'])}",
        f"contended={record['contended']}",
    ])
    return "\n".join(lines) + "\n"


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        description="Check host load average, per-CPU busy fraction, and per-process CPU usage."
    )
    parser.add_argument("--cpus", required=True, help="CPU list to aggregate (e.g. 0-11)")
    parser.add_argument("--gen-cpus", default=None, help="Optional generator CPU list (e.g. 12-23)")
    parser.add_argument("--max-load", required=True, type=float, help="Max 1-min loadavg")
    parser.add_argument("--max-busy", required=True, type=float, help="Max busy fraction (0..1)")
    parser.add_argument(
        "--max-gen-busy",
        type=float,
        default=None,
        help="Max generator busy fraction (0..1; defaults to --max-busy)",
    )
    parser.add_argument(
        "--max-proc-cores",
        type=float,
        default=0.5,
        help="Max single-process CPU cores (default 0.5)",
    )
    parser.add_argument("--window", type=float, default=3.0, help="Sampling window in seconds")
    parser.add_argument("--out", default=None, help="Optional path to write key=value record")
    args = parser.parse_args(argv)

    try:
        cpus = parse_cpuset(args.cpus)
        if not cpus:
            raise ValueError("empty --cpus list")
        gen_cpus: Optional[Set[int]] = None
        if args.gen_cpus is not None:
            gen_cpus = parse_cpuset(args.gen_cpus)
            if not gen_cpus:
                raise ValueError("empty --gen-cpus list")
        if args.window < 0:
            raise ValueError("--window must be >= 0")
        if args.max_load < 0 or args.max_busy < 0:
            raise ValueError("--max-load and --max-busy must be >= 0")
        if args.max_gen_busy is not None and args.max_gen_busy < 0:
            raise ValueError("--max-gen-busy must be >= 0")
        if args.max_proc_cores < 0:
            raise ValueError("--max-proc-cores must be >= 0")
    except ValueError as e:
        sys.stderr.write(f"host_load.py: {e}\n")
        return 64

    try:
        record = measure_host_load(
            cpus=cpus,
            max_load=args.max_load,
            max_busy=args.max_busy,
            window=args.window,
            gen_cpus=gen_cpus,
            max_gen_busy=args.max_gen_busy,
            max_proc_cores=args.max_proc_cores,
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
