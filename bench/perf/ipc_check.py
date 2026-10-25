#!/usr/bin/env python3
"""Cross-check the two micro layers: gungraun instruction counts vs Criterion wall-clock.

    ipc_check.py [--target-dir plecto/target] [--ghz 5.4] [--max-ipc 5]

For each mapped bench pair it reads

* gungraun: `<target>/gungraun/<crate>/<bench>/<bench>/<id>/callgrind.<id>.t*.p*.out`, the first
  field of the `summary:` line (= Ir, instructions executed in the measured region), summed over
  every thread file;
* Criterion: `<target>/criterion/<group>/<id>/new/estimates.json`, `mean.point_estimate` (ns);

and computes the implied IPC = Ir / (ns * GHz). One call cannot retire more instructions than the
core can issue in the time Criterion measured: a value above `--max-ipc` (default 5, already
above what a 6-wide core sustains on this code) means the instruction-count bench is measuring
something the wall-clock bench is not — fixture construction, teardown or lazy initialisation
inside the callgrind region — and its CI soft limit is then judged against the wrong quantity.

Exit 0 when every pair with data is consistent, 1 on any violation, 0 with a note when no pair has
both layers (nothing to judge; CI runs only the instruction-count layer). `--ghz` is the clock the
Criterion run was taken at. The default is the reference host's P-core maximum turbo (5.4 GHz):
any lower real clock means fewer cycles and a HIGHER true IPC, so the default errs toward never
flagging a consistent pair (false negatives over false positives).
"""

import argparse
import glob
import json
import os
import sys

# (label, gungraun dir relative to <target>/gungraun, Criterion dir relative to <target>/criterion)
PAIRS = (
    ("on_request/trusted_pooled",
     "plecto-host/wasm_inst/wasm_inst/on_request.trusted_pooled",
     "on_request/trusted_pooled"),
    ("on_request/untrusted_fresh",
     "plecto-host/wasm_inst/wasm_inst/on_request.untrusted_fresh",
     "on_request/untrusted_fresh"),
    # fastpath_inst builds a 3-instance pool, so it maps to the n = 3 point of Criterion's sweep.
    ("lb_pick/round_robin",
     "plecto-control/fastpath_inst/fastpath_inst/lb_pick.round_robin",
     "lb_pick/round_robin/3"),
    ("lb_pick/least_request_p2c",
     "plecto-control/fastpath_inst/fastpath_inst/lb_pick.least_request_p2c",
     "lb_pick/least_request_p2c/3"),
    ("lb_pick/maglev",
     "plecto-control/fastpath_inst/fastpath_inst/lb_pick_maglev.source_ip",
     "lb_pick/maglev/3"),
)


def gungraun_ir(bench_dir):
    """Sum of Ir over every per-thread callgrind file in `bench_dir`, or None if there are none."""
    files = sorted(glob.glob(os.path.join(bench_dir, "callgrind.*.out")))
    if not files:
        return None
    total = 0
    for path in files:
        with open(path) as f:
            for line in f:
                if line.startswith("summary:"):
                    fields = line.split()[1:]
                    total += int(fields[0]) if fields else 0
                    break
    return total


def criterion_ns(bench_dir):
    path = os.path.join(bench_dir, "new", "estimates.json")
    if not os.path.exists(path):
        return None
    with open(path) as f:
        return float(json.load(f)["mean"]["point_estimate"])


def check(target, ghz, max_ipc, out=sys.stdout):
    """Return (violations, judged) and print one line per pair."""
    violations = judged = 0
    for label, g_rel, c_rel in PAIRS:
        ir = gungraun_ir(os.path.join(target, "gungraun", g_rel))
        ns = criterion_ns(os.path.join(target, "criterion", c_rel))
        if ir is None or ns is None or ns <= 0:
            missing = [name for name, v in (("gungraun", ir), ("criterion", ns)) if v is None]
            print(f"skip  {label}: no {' / '.join(missing) or 'positive criterion time'} data", file=out)
            continue
        judged += 1
        ipc = ir / (ns * ghz)
        ok = ipc <= max_ipc
        violations += not ok
        print(f"{'ok  ' if ok else 'FAIL'}  {label}: {ir} Ir / ({ns:.1f} ns x {ghz} GHz) "
              f"= IPC {ipc:.2f} (limit {max_ipc})", file=out)
    return violations, judged


def main(argv=None):
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--target-dir", default=os.path.join(here, "..", "..", "plecto", "target"))
    ap.add_argument("--ghz", type=float, default=5.4)
    ap.add_argument("--max-ipc", type=float, default=5.0)
    args = ap.parse_args(argv)
    violations, judged = check(os.path.abspath(args.target_dir), args.ghz, args.max_ipc)
    if judged == 0:
        print("ipc_check: no pair has both gungraun and Criterion data — nothing judged")
        return 0
    return 1 if violations else 0


if __name__ == "__main__":
    sys.exit(main())
