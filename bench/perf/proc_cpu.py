#!/usr/bin/env python3
"""Process CPU-time snapshots for the gate's CPU-µs/req invariant.

    proc_cpu.py snapshot <pid>   # JSON to stdout

A snapshot records two independent clocks for the whole process:

* `tasks` — per-thread on-CPU time in ns (`/proc/<pid>/task/<tid>/schedstat`, field 1). ns
  resolution, but a thread that EXITS between two snapshots takes its time with it.
* `stat_ticks` — process-wide utime + stime (`/proc/<pid>/stat`, fields 14 + 15) in clock ticks.
  Includes threads that already exited, but only at 1/CLK_TCK (typically 10 ms) resolution.

`cpu_ns_between()` uses the ns-resolution per-thread sum whenever every thread present in the
first snapshot is still alive in the second (threads born in between count from 0, which is exact),
and falls back to the tick clock otherwise, so an exited thread can never silently deflate the
number. The source used is returned alongside the value.
"""

import json
import os
import sys


def _stat_ticks(proc_root, pid):
    with open(os.path.join(proc_root, str(pid), "stat")) as f:
        raw = f.read()
    # comm (field 2) may contain spaces/parens: split after the LAST ')'. The remaining fields
    # start at field 3 (state), so utime/stime (fields 14/15) sit at offsets 11/12.
    rest = raw[raw.rindex(")") + 2 :].split()
    return int(rest[11]) + int(rest[12])


def snapshot(pid, proc_root="/proc"):
    tasks = {}
    task_dir = os.path.join(proc_root, str(pid), "task")
    for tid in os.listdir(task_dir):
        try:
            with open(os.path.join(task_dir, tid, "schedstat")) as f:
                tasks[tid] = int(f.read().split()[0])
        except (FileNotFoundError, ProcessLookupError):
            continue  # thread exited while listing: it is simply absent from this snapshot
    return {
        "pid": int(pid),
        "tasks": tasks,
        "stat_ticks": _stat_ticks(proc_root, pid),
        "clk_tck": os.sysconf("SC_CLK_TCK"),
    }


def cpu_ns_between(before, after):
    """Return (cpu_ns, source) consumed by the process between two snapshots."""
    if before["pid"] != after["pid"]:
        raise ValueError(f"snapshots are of different processes ({before['pid']} vs {after['pid']})")
    exited = set(before["tasks"]) - set(after["tasks"])
    if not exited:
        # A TID reused by a new thread would show a counter BELOW its old value; count such a
        # thread from 0, like any thread born between the snapshots.
        ns = sum(
            v - before["tasks"].get(tid, 0) if v >= before["tasks"].get(tid, 0) else v
            for tid, v in after["tasks"].items()
        )
        return float(ns), "schedstat"
    ticks = after["stat_ticks"] - before["stat_ticks"]
    return ticks * 1e9 / after["clk_tck"], "stat"


def main(argv):
    if len(argv) != 3 or argv[1] != "snapshot":
        print("usage: proc_cpu.py snapshot <pid>", file=sys.stderr)
        return 64
    json.dump(snapshot(argv[2]), sys.stdout)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
