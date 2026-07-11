#!/usr/bin/env python3
"""Judge the gate phase's raw measurements against gate_tolerances.toml.

Reads the per-round oha JSONs, k6 JSONs and loadgen CSVs that run-perf.sh's phase_gate wrote into
a temp dir, reduces each invariant to `mean ± half-range` (half-range across interleave rounds —
the honest run-to-run spread on an unpinned host), and prints one CSV row per invariant:

    invariant,value,ci_half,band_lo,band_hi,verdict

verdict ∈ pass / fail / skipped (input missing, e.g. no k6) / info (reported, never judged).
Exit code 0 iff no row is `fail` — the machine gate for `just gate` / pre-push hooks.
"""

import csv
import glob
import json
import os
import sys
import tomllib

US = 1e6  # µs per second


def oha(path):
    with open(path) as f:
        return json.load(f)


def usreq(d):
    return US / d["summary"]["requestsPerSec"]


def p50_ms(d):
    return d["latencyPercentiles"]["p50"] * 1000


def p99_ms(d):
    return d["latencyPercentiles"]["p99"] * 1000


def spread(xs):
    return (sum(xs) / len(xs), (max(xs) - min(xs)) / 2)


class Report:
    def __init__(self, bands):
        self.bands = bands
        self.rows = []
        self.failed = False

    def judge(self, name, value, ci_half=None):
        band = self.bands[name]  # a missing band is a bug in the tolerances file: fail loudly
        ok = band["lo"] <= value <= band["hi"]
        self.failed |= not ok
        self.rows.append((name, value, ci_half, band["lo"], band["hi"], "pass" if ok else "fail"))

    def info(self, name, value, ci_half=None):
        self.rows.append((name, value, ci_half, "", "", "info"))

    def skipped(self, name):
        band = self.bands.get(name, {})
        self.rows.append((name, "", "", band.get("lo", ""), band.get("hi", ""), "skipped"))

    def dump(self):
        w = csv.writer(sys.stdout)
        w.writerow(["invariant", "value", "ci_half", "band_lo", "band_hi", "verdict"])
        for name, value, ci, lo, hi, verdict in self.rows:
            fmt = lambda x: f"{x:.4f}" if isinstance(x, float) else x
            w.writerow([name, fmt(value), fmt(ci) if ci is not None else "", lo, hi, verdict])


def ladder_deltas(tmp, rep):
    # r[0-9]*: don't match the rate-limit pair's rl_r*_baseline.json
    rounds = sorted(glob.glob(os.path.join(tmp, "r[0-9]*_baseline.json")))
    floors, apikeys = [], []
    for base in rounds:
        prefix = base[: -len("baseline.json")]
        b, n, t = (usreq(oha(prefix + r + ".json")) for r in ("baseline", "noop-pooled", "trusted"))
        floors.append(n - b)
        apikeys.append(t - n)
    rep.judge("dispatch_floor_us", *spread(floors))
    rep.judge("apikey_cost_us", *spread(apikeys))


def tail_deltas(tmp, rep):
    t = {r: oha(os.path.join(tmp, f"tail_{r}.json"))
         for r in ("baseline", "noop-pooled", "trusted", "resp-ctx")}
    rep.judge("pooled_tail_p50_ms", p50_ms(t["noop-pooled"]) - p50_ms(t["baseline"]))
    rep.judge("apikey_tail_p50_ms", p50_ms(t["trusted"]) - p50_ms(t["noop-pooled"]))
    rep.judge("respctx_tail_p50_ms", p50_ms(t["resp-ctx"]) - p50_ms(t["noop-pooled"]))
    # p99 at a fixed rate is host-noise-dominated on an unpinned host: report, don't judge.
    rep.info("pooled_tail_p99_ms", p99_ms(t["noop-pooled"]) - p99_ms(t["baseline"]))
    rep.info("respctx_tail_p99_ms", p99_ms(t["resp-ctx"]) - p99_ms(t["noop-pooled"]))


def ratelimit(tmp, rep):
    rounds = sorted(glob.glob(os.path.join(tmp, "rl_r*_baseline.json")))
    if not rounds:
        rep.skipped("ratelimit_tax_us")
        return
    taxes = []
    for base in rounds:
        b = json.load(open(base))
        r = json.load(open(base.replace("_baseline.json", "_ratelimit.json")))
        taxes.append(US / r["rps"] - US / b["rps"])
    rep.judge("ratelimit_tax_us", *spread(taxes))


def enforce(tmp, rep):
    path = os.path.join(tmp, "enforce.json")
    if not os.path.exists(path):
        rep.skipped("enforce_allowed_ratio")
        return
    d = json.load(open(path))
    band = rep.bands["enforce_allowed_ratio"]
    expected = band["refill_per_s"] + band["capacity"] / band["window_s"]
    rep.judge("enforce_allowed_ratio", d["allowed_rps"] / expected)
    rep.info("enforce_limited_frac", d["limited_frac"])


def rr(tmp, rep):
    counts = [int(row["count"]) for row in csv.DictReader(open(os.path.join(tmp, "rr.csv")))]
    rep.judge("rr_spread_req", float(max(counts) - min(counts)))


def ejection(tmp, rep):
    timeline = {int(r["t"]): r for r in csv.DictReader(open(os.path.join(tmp, "ej_timeline.csv")))}
    events = [(int(r["t"]), r["label"]) for r in csv.DictReader(open(os.path.join(tmp, "ej_events.csv")))]
    ev = dict((label, t) for t, label in events)

    def first(t0, cond, horizon=6):
        for t in range(t0, t0 + horizon):
            row = timeline.get(t)
            if row and cond(row):
                return t - t0
        return horizon  # never settled inside the horizon -> guaranteed out of band

    n = lambda row, k: int(row[k])
    transitions = [
        first(ev["eject b"], lambda r: n(r, "b") == 0),
        first(ev["rejoin b"], lambda r: n(r, "b") > 0),
        # fail-closed: every instance ejected -> the full offered rate 503s into `failed`
        first(ev["eject all"], lambda r: n(r, "a") + n(r, "b") + n(r, "c") == 0 and n(r, "failed") > 0),
        first(ev["restore all"], lambda r: all(n(r, k) > 0 for k in "abc") and n(r, "failed") == 0),
    ]
    rep.judge("ejection_transition_s", float(max(transitions)))

    # failed responses are legitimate ONLY while eject-all fail-closed is in force (plus a 2 s
    # grace after every event while probes converge); anything else is a dropped request the
    # single-instance ejection path must not produce.
    grace = set()
    for t, _ in events:
        grace.update(range(t, t + 3))
    grace.update(range(ev["eject all"], ev["restore all"] + 3))
    stray = sum(int(r["failed"]) for t, r in timeline.items() if t not in grace)
    rep.judge("ejection_stray_failed", float(stray))


def main():
    tmp, tol = sys.argv[1], sys.argv[2]
    with open(tol, "rb") as f:
        bands = tomllib.load(f)
    rep = Report(bands)
    ladder_deltas(tmp, rep)
    tail_deltas(tmp, rep)
    ratelimit(tmp, rep)
    enforce(tmp, rep)
    rr(tmp, rep)
    ejection(tmp, rep)
    rep.dump()
    sys.exit(1 if rep.failed else 0)


if __name__ == "__main__":
    main()
