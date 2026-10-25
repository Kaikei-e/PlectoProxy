#!/usr/bin/env python3
"""Judge the gate phase's raw measurements against gate_tolerances.toml.

Reads the per-round oha JSONs, k6 JSONs, CPU snapshots and loadgen CSVs that run-perf.sh's
phase_gate wrote into its raw dir, and prints one CSV row per invariant:

    invariant,value,ci_half,band_lo,band_hi,verdict,n

Round-based invariants (dispatch floor, apikey cost, rate-limit tax, CPU-µs/req) are PAIRED
designs: every interleave round measures the arms back to back (order counterbalanced across
rounds), the delta is taken inside the round, and the estimate is the mean of the n per-round
deltas. `ci_half` is the two-sided 95% Student-t half-width of that mean,
`t_{0.975, n-1} * s / sqrt(n)` (s = sample SD of the per-round deltas), and `n` is the number of
rounds it was computed from. With n = 1 no interval can be estimated, so `ci_half` is `inf` and the
verdict is `inconclusive`. Single-window invariants (fixed-rate tails, enforcement, RR, ejection)
have empty `ci_half` / `n` and are judged as a point inside/outside the band.

Pooling: if the raw dir holds `pooled_from.txt` (written by run-perf.sh from GATE_POOL_FROM), the
per-round raw data of every listed earlier run — and, transitively, of the runs THOSE pooled — is
merged with this run's rounds into one estimate. A re-run after INCONCLUSIVE therefore adds
evidence; it never replaces the earlier run. Pooling is refused (exit 64) unless every run carries
the same `build_id.txt` (the measured binaries' SHA-256): rounds of different builds are not samples
of the same quantity.

verdict ∈ pass / fail / inconclusive / skipped (input missing, e.g. no k6) / info (reported, never
judged). An info row is promoted to a judged one by adding its band to gate_tolerances.toml.
Exit code: 1 if any fail, else 2 if any inconclusive, else 0 — the machine gate for `just gate` /
pre-push hooks; 64 on an invalid pooling request.
"""

import csv
import glob
import json
import math
import os
import statistics
import sys
import tomllib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from proc_cpu import cpu_ns_between  # noqa: E402 — sibling module, resolved via the line above

US = 1e6  # µs per second

# Two-sided 95% Student-t critical values t_{0.975, df}, df = 1..30.
T975 = {
    1: 12.706, 2: 4.303, 3: 3.182, 4: 2.776, 5: 2.571, 6: 2.447, 7: 2.365, 8: 2.306,
    9: 2.262, 10: 2.228, 11: 2.201, 12: 2.179, 13: 2.160, 14: 2.145, 15: 2.131, 16: 2.120,
    17: 2.110, 18: 2.101, 19: 2.093, 20: 2.086, 21: 2.080, 22: 2.074, 23: 2.069, 24: 2.064,
    25: 2.060, 26: 2.056, 27: 2.052, 28: 2.048, 29: 2.045, 30: 2.042,
}
Z975 = 1.959964


def t975(df):
    """t_{0.975, df}. Table for df <= 30; beyond, the Cornish-Fisher expansion of the t quantile
    in 1/df (Abramowitz & Stegun, Handbook of Mathematical Functions, 26.7.5), which meets the
    table at df = 30 (2.0423 vs 2.042) and converges to the normal 1.96 from above — a hard step
    to 1.96 at df = 31 would understate the interval by ~4%, and pooled re-runs reach that df."""
    if df < 1:
        return math.inf
    if df in T975:
        return T975[df]
    z = Z975
    g1 = (z**3 + z) / 4
    g2 = (5 * z**5 + 16 * z**3 + 3 * z) / 96
    g3 = (3 * z**7 + 19 * z**5 + 17 * z**3 - 15 * z) / 384
    return z + g1 / df + g2 / df**2 + g3 / df**3


def oha(path):
    with open(path) as f:
        return json.load(f)


def load_json(path):
    with open(path) as f:
        return json.load(f)


def usreq(d):
    return US / d["summary"]["requestsPerSec"]


def p50_ms(d):
    return d["latencyPercentiles"]["p50"] * 1000


def p99_ms(d):
    return d["latencyPercentiles"]["p99"] * 1000


def spread(xs):
    """Return (mean, ci_half, n) of the per-round paired deltas `xs`.

    ci_half is the 95% Student-t half-width t_{0.975, n-1} * s / sqrt(n) with s the sample SD;
    `inf` for n = 1 (no spread is estimable from one round, so the verdict must be inconclusive).
    """
    n = len(xs)
    if n == 0:
        raise ValueError("spread() of an empty sample")
    mean = sum(xs) / n
    if n == 1:
        return (mean, math.inf, 1)
    return (mean, t975(n - 1) * statistics.stdev(xs) / math.sqrt(n), n)


def judge(value, band_lo, band_hi, ci_half=None):
    if math.isnan(value) or (ci_half is not None and math.isnan(ci_half)):
        return "fail"
    # ci_half is the 95% t half-width of a paired round mean (see spread()); the verdict compares
    # the whole interval [value - ci_half, value + ci_half] against the band.
    if ci_half is not None and ci_half > 0:
        lo_v = value - ci_half
        hi_v = value + ci_half
        if band_lo <= lo_v and hi_v <= band_hi:
            return "pass"
        elif hi_v < band_lo or lo_v > band_hi:
            return "fail"
        else:
            return "inconclusive"
    return "pass" if band_lo <= value <= band_hi else "fail"


# Row layout: (invariant, value, ci_half, band_lo, band_hi, verdict, n) — the CSV column order.
VERDICT = 5


def _verdict(item):
    return item[VERDICT] if isinstance(item, (tuple, list)) else item


def exit_code(rows_or_verdicts):
    verdicts = {_verdict(item) for item in rows_or_verdicts}
    if "fail" in verdicts:
        return 1
    if "inconclusive" in verdicts:
        return 2
    return 0


def count_verdicts(rows_or_verdicts):
    c = {"pass": 0, "fail": 0, "inconclusive": 0}
    for item in rows_or_verdicts:
        v = _verdict(item)
        if v in c:
            c[v] += 1
    return c


def format_summary(counts):
    return f"{counts['pass']} pass, {counts['fail']} fail, {counts['inconclusive']} inconclusive"


class Report:
    def __init__(self, bands):
        self.bands = bands
        self.rows = []

    def judge(self, name, value, ci_half=None, n=None):
        band = self.bands[name]  # a missing band is a bug in the tolerances file: fail loudly
        verdict = judge(value, band["lo"], band["hi"], ci_half)
        self.rows.append((name, value, ci_half, band["lo"], band["hi"], verdict, n))

    def info(self, name, value, ci_half=None, n=None):
        self.rows.append((name, value, ci_half, "", "", "info", n))

    def judge_if_banded(self, name, value, ci_half=None, n=None):
        # Calibration path: reported as info until gate_tolerances.toml gains a band for it.
        if name in self.bands:
            self.judge(name, value, ci_half, n)
        else:
            self.info(name, value, ci_half, n)

    def skipped(self, name):
        band = self.bands.get(name, {})
        self.rows.append((name, "", "", band.get("lo", ""), band.get("hi", ""), "skipped", None))

    @property
    def failed(self):
        return any(row[VERDICT] == "fail" for row in self.rows)

    def counts(self):
        return count_verdicts(self.rows)

    def summary(self):
        return format_summary(self.counts())

    def exit_code(self):
        return exit_code(self.rows)

    def dump(self, out=sys.stdout, err=sys.stderr):
        w = csv.writer(out)
        w.writerow(["invariant", "value", "ci_half", "band_lo", "band_hi", "verdict", "n"])

        def fmt(x):
            return f"{x:.4f}" if isinstance(x, float) else x

        for name, value, ci, lo, hi, verdict, n in self.rows:
            w.writerow(
                [name, fmt(value), fmt(ci) if ci is not None else "", lo, hi, verdict,
                 n if n is not None else ""]
            )
        print(self.summary(), file=err)


# ---- pooling -----------------------------------------------------------------------------------


class PoolError(Exception):
    pass


POOL_FILE = "pooled_from.txt"
BUILD_FILE = "build_id.txt"
ROUND_GLOBS = ("r[0-9]*_baseline.json", "rl_r*_baseline.json", "cpu_r*_baseline.json")


def gate_dir(path):
    """A pooled run may be named by its RUN_DIR or by RUN_DIR/gate."""
    sub = os.path.join(path, "gate")
    return sub if os.path.isdir(sub) else path


def _has_rounds(d):
    return any(glob.glob(os.path.join(d, g)) for g in ROUND_GLOBS)


def _read_build_id(d):
    try:
        with open(os.path.join(d, BUILD_FILE)) as f:
            return f.read().strip()
    except FileNotFoundError:
        return None


def resolve_sources(tmp):
    """[tmp, *every run tmp pools from, transitively], de-duplicated, tmp first."""
    seen = [os.path.realpath(tmp)]
    queue = [tmp]
    while queue:
        cur = queue.pop(0)
        pool_file = os.path.join(cur, POOL_FILE)
        if not os.path.exists(pool_file):
            continue
        with open(pool_file) as f:
            entries = [ln.strip() for ln in f if ln.strip() and not ln.lstrip().startswith("#")]
        for entry in entries:
            d = gate_dir(entry)
            real = os.path.realpath(d)
            if real in seen:
                continue
            if not os.path.isdir(d):
                raise PoolError(f"pool source {entry!r} does not exist (mistyped RUN_DIR?)")
            if not _has_rounds(d):
                raise PoolError(f"pool source {entry!r} has no gate round data")
            seen.append(real)
            queue.append(d)
    return seen


def check_pool_builds(sources):
    if len(sources) < 2:
        return
    ids = {d: _read_build_id(d) for d in sources}
    own = ids[sources[0]]
    if own is None:
        raise PoolError(f"{sources[0]} has no {BUILD_FILE}: cannot prove the pooled runs share a build")
    for d, bid in ids.items():
        if bid != own:
            raise PoolError(
                f"refusing to pool {d}: build {bid or 'unknown'} != {own} "
                "(rounds of different builds are not samples of the same quantity)"
            )


# ---- invariants ----------------------------------------------------------------------------------


def _rounds(sources, pattern):
    out = []
    for d in sources:
        out.extend(sorted(glob.glob(os.path.join(d, pattern))))
    return out


def ladder_deltas(sources, rep):
    if isinstance(sources, str):
        sources = [sources]
    # r[0-9]*: don't match the rate-limit pair's rl_r*_baseline.json. Each round is one paired
    # baseline / noop-pooled / trusted triple; the deltas are taken inside the round.
    floors, apikeys = [], []
    for base in _rounds(sources, "r[0-9]*_baseline.json"):
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


def ratelimit(sources, rep, err=sys.stderr):
    if isinstance(sources, str):
        sources = [sources]
    taxes = []
    for base in _rounds(sources, "rl_r*_baseline.json"):
        partner = base.replace("_baseline.json", "_ratelimit.json")
        try:
            b, r = load_json(base), load_json(partner)
            taxes.append(US / r["rps"] - US / b["rps"])
        except (OSError, ValueError, KeyError, ZeroDivisionError) as e:
            # An incomplete pair is not a sample: drop the round, say so, keep judging the rest.
            print(f"warning: dropping rate-limit round {os.path.basename(base)}: {e!r}", file=err)
    if not taxes:
        rep.skipped("ratelimit_tax_us")
        return
    rep.judge("ratelimit_tax_us", *spread(taxes))


def cpu_us_per_req(prefix, err=sys.stderr):
    """Proxy CPU µs per completed request for one fixed-rate arm (`<prefix>.json` = the k6
    summary, `<prefix>.snap0.json` / `.snap1.json` = proc_cpu snapshots around it)."""
    k6 = load_json(prefix + ".json")
    ns, source = cpu_ns_between(load_json(prefix + ".snap0.json"), load_json(prefix + ".snap1.json"))
    target = k6.get("target_rps") or 0
    achieved = k6.get("achieved_rps") or 0
    # k6 may drop a handful of iterations at start-up; only flag a window whose completed count is
    # materially off the fixed rate (> 0.5 % dropped, any failure, or < 95 % of target throughput).
    reqs = k6.get("reqs") or 0
    if (k6.get("dropped", 0) > 0.005 * max(reqs, 1) or k6.get("failed", 0)
            or (target and achieved < 0.95 * target)):
        print(f"warning: {os.path.basename(prefix)} did not hold its fixed rate "
              f"(target {target}, achieved {achieved:.0f}, dropped {k6.get('dropped', 0)}, "
              f"failed {k6.get('failed', 0)}) — CPU/req is not at the intended load",
              file=err)
    if source != "schedstat":
        print(f"note: {os.path.basename(prefix)} fell back to tick-resolution utime+stime "
              "(a proxy thread exited mid-window)", file=err)
    if not k6.get("reqs"):
        raise ValueError(f"{os.path.basename(prefix)} completed no requests")
    return ns / 1e3 / k6["reqs"]


def ratelimit_cpu(sources, rep, err=sys.stderr):
    if isinstance(sources, str):
        sources = [sources]
    rounds = _rounds(sources, "cpu_r*_baseline.json")
    if not rounds:
        rep.skipped("baseline_cpu_us_per_req")
        rep.skipped("ratelimit_cpu_us_per_req")
        return
    bases, deltas = [], []
    for base in rounds:
        try:
            b = cpu_us_per_req(base[: -len(".json")], err)
            r = cpu_us_per_req(base[: -len("baseline.json")] + "ratelimit", err)
        except (OSError, ValueError, KeyError, ZeroDivisionError) as e:
            print(f"warning: dropping CPU round {os.path.basename(base)}: {e!r}", file=err)
            continue
        bases.append(b)
        deltas.append(r - b)
    if not deltas:
        rep.skipped("baseline_cpu_us_per_req")
        rep.skipped("ratelimit_cpu_us_per_req")
        return
    # Absolute CPU/req includes the in-process mock upstream; the paired delta cancels it.
    rep.info("baseline_cpu_us_per_req", *spread(bases))
    rep.judge_if_banded("ratelimit_cpu_us_per_req", *spread(deltas))


def enforce(tmp, rep):
    path = os.path.join(tmp, "enforce.json")
    if not os.path.exists(path):
        rep.skipped("enforce_allowed_ratio")
        return
    with open(path) as f:
        d = json.load(f)
    band = rep.bands["enforce_allowed_ratio"]
    # In plecto-host (backend.rs::apply_bucket), token refills happen in discrete integer
    # multiples of refill_interval_ms = 1000 (intervals = elapsed / 1000). Over window_s,
    # after the initial capacity burst at t=0, the bucket experiences window_s - 1 discrete
    # 1 s refill ticks because the tick at t = window_s coincides with the end of the window.
    refill_ticks = max(0, band["window_s"] - 1)
    expected = (band["capacity"] + refill_ticks * band["refill_per_s"]) / band["window_s"]
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

    def n(row, k):
        return int(row[k])

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
    try:
        sources = resolve_sources(tmp)
        check_pool_builds(sources)
    except PoolError as e:
        print(f"gate_verdict: {e}", file=sys.stderr)
        sys.exit(64)
    if len(sources) > 1:
        print("pooling rounds from: " + ", ".join(sources[1:]), file=sys.stderr)
    rep = Report(bands)
    ladder_deltas(sources, rep)
    tail_deltas(tmp, rep)
    ratelimit(sources, rep)
    ratelimit_cpu(sources, rep)
    enforce(tmp, rep)
    rr(tmp, rep)
    ejection(tmp, rep)
    rep.dump()
    sys.exit(rep.exit_code())


if __name__ == "__main__":
    main()
