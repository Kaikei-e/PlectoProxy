#!/usr/bin/env python3
"""Tests for gate_verdict.py pure judgment and exit code aggregation."""

import importlib.util
from io import StringIO
import json
import math
from pathlib import Path
import statistics
import sys
import tempfile
import unittest

# Import gate_verdict (and its sibling proc_cpu) by path
_here = Path(__file__).resolve().parent
sys.path.insert(0, str(_here))
_target = _here / "gate_verdict.py"
_spec = importlib.util.spec_from_file_location("gate_verdict", _target)
gate_verdict = importlib.util.module_from_spec(_spec)
sys.modules["gate_verdict"] = gate_verdict
_spec.loader.exec_module(gate_verdict)
import proc_cpu  # noqa: E402


class TestGateVerdictJudge(unittest.TestCase):
    def test_inside_pass(self):
        # [2.0, 5.0] band; value 3.5 +/- 0.5 is [3.0, 4.0], fully inside
        self.assertEqual(gate_verdict.judge(3.5, 2.0, 5.0, 0.5), "pass")
        # Touching boundaries from inside
        self.assertEqual(gate_verdict.judge(2.5, 2.0, 5.0, 0.5), "pass")
        self.assertEqual(gate_verdict.judge(4.5, 2.0, 5.0, 0.5), "pass")

    def test_straddling_low_edge_inconclusive(self):
        # [2.0, 5.0] band; value 2.1 +/- 0.3 is [1.8, 2.4], straddles band_lo
        self.assertEqual(gate_verdict.judge(2.1, 2.0, 5.0, 0.3), "inconclusive")
        self.assertEqual(gate_verdict.judge(1.9, 2.0, 5.0, 0.2), "inconclusive")

    def test_straddling_high_edge_inconclusive(self):
        # [2.0, 5.0] band; value 4.9 +/- 0.3 is [4.6, 5.2], straddles band_hi
        self.assertEqual(gate_verdict.judge(4.9, 2.0, 5.0, 0.3), "inconclusive")
        self.assertEqual(gate_verdict.judge(5.1, 2.0, 5.0, 0.2), "inconclusive")

    def test_straddling_both_edges_inconclusive(self):
        # Value 3.5 +/- 2.0 is [1.5, 5.5], wider than band [2.0, 5.0]
        self.assertEqual(gate_verdict.judge(3.5, 2.0, 5.0, 2.0), "inconclusive")

    def test_fully_outside_fail(self):
        # Fully below band_lo
        self.assertEqual(gate_verdict.judge(1.0, 2.0, 5.0, 0.5), "fail")
        # Fully above band_hi
        self.assertEqual(gate_verdict.judge(6.0, 2.0, 5.0, 0.5), "fail")

    def test_nan_fails(self):
        nan = float("nan")
        self.assertEqual(gate_verdict.judge(nan, 2.0, 5.0, 0.5), "fail")
        self.assertEqual(gate_verdict.judge(3.0, 2.0, 5.0, nan), "fail")
        self.assertEqual(gate_verdict.judge(nan, 2.0, 5.0), "fail")

    def test_no_ci_half_binary(self):
        # None -> binary check
        self.assertEqual(gate_verdict.judge(3.0, 2.0, 5.0, None), "pass")
        self.assertEqual(gate_verdict.judge(2.0, 2.0, 5.0, None), "pass")
        self.assertEqual(gate_verdict.judge(5.0, 2.0, 5.0, None), "pass")
        self.assertEqual(gate_verdict.judge(1.9, 2.0, 5.0, None), "fail")
        self.assertEqual(gate_verdict.judge(5.1, 2.0, 5.0, None), "fail")

        # Omitted ci_half
        self.assertEqual(gate_verdict.judge(3.0, 2.0, 5.0), "pass")
        self.assertEqual(gate_verdict.judge(1.0, 2.0, 5.0), "fail")

        # ci_half <= 0
        self.assertEqual(gate_verdict.judge(3.0, 2.0, 5.0, 0), "pass")
        self.assertEqual(gate_verdict.judge(1.0, 2.0, 5.0, 0), "fail")


class TestExitCodeAggregation(unittest.TestCase):
    def test_all_pass_returns_0(self):
        self.assertEqual(gate_verdict.exit_code(["pass"]), 0)
        self.assertEqual(gate_verdict.exit_code(["pass", "pass"]), 0)

    def test_inconclusive_returns_2(self):
        self.assertEqual(gate_verdict.exit_code(["inconclusive"]), 2)
        self.assertEqual(gate_verdict.exit_code(["pass", "inconclusive"]), 2)

    def test_fail_returns_1(self):
        self.assertEqual(gate_verdict.exit_code(["fail"]), 1)
        self.assertEqual(gate_verdict.exit_code(["pass", "fail"]), 1)

    def test_fail_dominates_inconclusive(self):
        self.assertEqual(gate_verdict.exit_code(["inconclusive", "fail"]), 1)
        self.assertEqual(gate_verdict.exit_code(["fail", "inconclusive"]), 1)
        self.assertEqual(gate_verdict.exit_code(["pass", "inconclusive", "fail"]), 1)

    def test_non_judged_verdicts(self):
        self.assertEqual(gate_verdict.exit_code([]), 0)
        self.assertEqual(gate_verdict.exit_code(["info", "skipped"]), 0)
        self.assertEqual(gate_verdict.exit_code(["pass", "info", "skipped"]), 0)


class TestReport(unittest.TestCase):
    def test_report_pure_execution(self):
        bands = {
            "b_pass": {"lo": 2.0, "hi": 5.0},
            "b_inconclusive": {"lo": 2.0, "hi": 5.0},
            "b_fail": {"lo": 2.0, "hi": 5.0},
        }
        rep = gate_verdict.Report(bands)
        rep.judge("b_pass", 3.5, 0.5)
        rep.judge("b_inconclusive", 2.1, 0.3)
        self.assertEqual(rep.exit_code(), 2)
        self.assertEqual(rep.counts(), {"pass": 1, "fail": 0, "inconclusive": 1})

        rep.judge("b_fail", 1.0, 0.2)
        self.assertEqual(rep.exit_code(), 1)
        self.assertEqual(rep.counts(), {"pass": 1, "fail": 1, "inconclusive": 1})

        out = StringIO()
        err = StringIO()
        rep.dump(out, err=err)
        lines = out.getvalue().strip().splitlines()
        self.assertEqual(lines[0], "invariant,value,ci_half,band_lo,band_hi,verdict,n")
        self.assertEqual(len(lines), 4)
        self.assertEqual(err.getvalue().strip(), "1 pass, 1 fail, 1 inconclusive")

    def test_enforce_discrete_refill_ticks(self):
        bands = {
            "enforce_allowed_ratio": {
                "capacity": 2000,
                "refill_per_s": 1000,
                "window_s": 10,
                "lo": 0.90,
                "hi": 1.10,
            }
        }
        rep = gate_verdict.Report(bands)
        with tempfile.TemporaryDirectory() as tmp:
            enforce_path = Path(tmp) / "enforce.json"
            enforce_path.write_text(
                json.dumps({"allowed_rps": 1100.0, "limited_frac": 0.78}),
                encoding="utf-8",
            )
            gate_verdict.enforce(tmp, rep)

        self.assertEqual(len(rep.rows), 2)
        name, value, ci_half, lo, hi, verdict, n = rep.rows[0]
        self.assertEqual(name, "enforce_allowed_ratio")
        self.assertAlmostEqual(value, 1.0)
        self.assertIsNone(ci_half)
        self.assertIsNone(n)
        self.assertEqual((lo, hi), (0.90, 1.10))
        self.assertEqual(verdict, "pass")


class TestSpread(unittest.TestCase):
    def test_t_table_and_tail(self):
        self.assertAlmostEqual(gate_verdict.t975(1), 12.706)
        self.assertAlmostEqual(gate_verdict.t975(7), 2.365)
        self.assertAlmostEqual(gate_verdict.t975(30), 2.042)
        # Past the table the expansion continues smoothly from above toward 1.96.
        self.assertLess(gate_verdict.t975(31), gate_verdict.t975(30))
        self.assertGreater(gate_verdict.t975(31), 2.03)
        self.assertGreater(gate_verdict.t975(10_000), gate_verdict.Z975)
        self.assertAlmostEqual(gate_verdict.t975(10_000), gate_verdict.Z975, places=3)
        self.assertEqual(gate_verdict.t975(0), math.inf)

    def test_spread_is_95pct_t_halfwidth(self):
        xs = [1.0, 1.2, 1.1, 1.3]
        mean, ci, n = gate_verdict.spread(xs)
        self.assertAlmostEqual(mean, 1.15)
        self.assertEqual(n, 4)
        self.assertAlmostEqual(ci, 3.182 * statistics.stdev(xs) / 2)

    def test_two_rounds_interval_is_12_7_se_not_half_range(self):
        # The historical ratelimit confirmation rounds (1.133 / 1.257 µs): the old half-range was
        # 0.062; the 95% t interval at n = 2 is t_{0.975,1} * s / sqrt(2) = 12.706 * 0.062.
        mean, ci, n = gate_verdict.spread([1.133, 1.257])
        self.assertAlmostEqual(mean, 1.195)
        self.assertAlmostEqual(ci, 12.706 * 0.062, places=6)
        self.assertEqual(gate_verdict.judge(mean, 0.9, 2.4, ci), "inconclusive")

    def test_single_round_is_inconclusive(self):
        mean, ci, n = gate_verdict.spread([1.2])
        self.assertEqual((mean, n), (1.2, 1))
        self.assertEqual(ci, math.inf)
        self.assertEqual(gate_verdict.judge(mean, 0.9, 2.4, ci), "inconclusive")

    def test_empty_raises(self):
        with self.assertRaises(ValueError):
            gate_verdict.spread([])


def _oha(path, rps):
    path.write_text(json.dumps({"summary": {"requestsPerSec": rps}}), encoding="utf-8")


def _ladder_round(d, rnd, base, noop, trusted):
    for rung, rps in (("baseline", base), ("noop-pooled", noop), ("trusted", trusted)):
        _oha(Path(d) / f"r{rnd}_{rung}.json", rps)


class TestPairedLadder(unittest.TestCase):
    def test_deltas_are_taken_within_each_round(self):
        bands = {"dispatch_floor_us": {"lo": 0.0, "hi": 10.0}, "apikey_cost_us": {"lo": 0.0, "hi": 10.0}}
        with tempfile.TemporaryDirectory() as tmp:
            # Host drift: round 2 is uniformly slower, but each round's deltas are identical
            # (1 µs floor, 1 µs apikey), so the paired estimate has zero spread.
            _ladder_round(tmp, 1, 1e6 / 5.0, 1e6 / 6.0, 1e6 / 7.0)
            _ladder_round(tmp, 2, 1e6 / 8.0, 1e6 / 9.0, 1e6 / 10.0)
            rep = gate_verdict.Report(bands)
            gate_verdict.ladder_deltas(tmp, rep)
        floor, apikey = rep.rows
        self.assertAlmostEqual(floor[1], 1.0)
        self.assertAlmostEqual(floor[2], 0.0)
        self.assertEqual(floor[6], 2)
        self.assertAlmostEqual(apikey[1], 1.0)


class TestPooling(unittest.TestCase):
    def _run(self, root, name, build, rounds):
        d = Path(root) / name / "gate"
        d.mkdir(parents=True)
        (d / "build_id.txt").write_text(build + "\n", encoding="utf-8")
        for rnd, (b, n, t) in enumerate(rounds, 1):
            _ladder_round(d, rnd, b, n, t)
        return d

    def test_pooled_rounds_merge_into_one_estimate(self):
        bands = {"dispatch_floor_us": {"lo": 0.0, "hi": 10.0}, "apikey_cost_us": {"lo": 0.0, "hi": 10.0}}
        with tempfile.TemporaryDirectory() as root:
            first = self._run(root, "run1", "abc", [(1e6 / 5.0, 1e6 / 6.0, 1e6 / 7.0)] * 2)
            second = self._run(root, "run2", "abc", [(1e6 / 5.0, 1e6 / 7.0, 1e6 / 8.0)] * 2)
            # The pool file may name the earlier RUN_DIR (its gate/ subdir is resolved).
            (second / "pooled_from.txt").write_text(str(first.parent) + "\n", encoding="utf-8")
            sources = gate_verdict.resolve_sources(str(second))
            gate_verdict.check_pool_builds(sources)
            rep = gate_verdict.Report(bands)
            gate_verdict.ladder_deltas(sources, rep)
        self.assertEqual(len(sources), 2)
        floor = rep.rows[0]
        self.assertEqual(floor[6], 4)
        self.assertAlmostEqual(floor[1], 1.5)  # mean of 1, 1, 2, 2

    def test_pooling_refuses_a_different_build(self):
        with tempfile.TemporaryDirectory() as root:
            first = self._run(root, "run1", "abc", [(1.0, 2.0, 3.0)])
            second = self._run(root, "run2", "xyz", [(1.0, 2.0, 3.0)])
            (second / "pooled_from.txt").write_text(str(first) + "\n", encoding="utf-8")
            sources = gate_verdict.resolve_sources(str(second))
            with self.assertRaises(gate_verdict.PoolError):
                gate_verdict.check_pool_builds(sources)

    def test_pooling_refuses_a_source_without_rounds(self):
        with tempfile.TemporaryDirectory() as root:
            second = self._run(root, "run2", "abc", [(1.0, 2.0, 3.0)])
            empty = Path(root) / "empty"
            empty.mkdir()
            (second / "pooled_from.txt").write_text(str(empty) + "\n", encoding="utf-8")
            with self.assertRaises(gate_verdict.PoolError):
                gate_verdict.resolve_sources(str(second))

    def test_pooling_refuses_a_missing_source_with_a_clear_message(self):
        with tempfile.TemporaryDirectory() as root:
            second = self._run(root, "run2", "abc", [(1.0, 2.0, 3.0)])
            (second / "pooled_from.txt").write_text(str(Path(root) / "typo") + "\n", encoding="utf-8")
            with self.assertRaisesRegex(gate_verdict.PoolError, "does not exist"):
                gate_verdict.resolve_sources(str(second))

    def test_pooling_is_transitive_and_cycle_safe(self):
        with tempfile.TemporaryDirectory() as root:
            a = self._run(root, "a", "abc", [(1.0, 2.0, 3.0)])
            b = self._run(root, "b", "abc", [(1.0, 2.0, 3.0)])
            c = self._run(root, "c", "abc", [(1.0, 2.0, 3.0)])
            (c / "pooled_from.txt").write_text(f"{b}\n# comment\n{a}\n", encoding="utf-8")
            (b / "pooled_from.txt").write_text(f"{a}\n{c}\n", encoding="utf-8")  # cycle back to c
            (a / "pooled_from.txt").write_text(f"{b}\n", encoding="utf-8")
            sources = gate_verdict.resolve_sources(str(c))
        self.assertEqual(len(sources), 3)
        self.assertEqual(len(set(sources)), 3)

    def test_pooling_requires_the_own_build_id(self):
        with tempfile.TemporaryDirectory() as root:
            first = self._run(root, "run1", "abc", [(1.0, 2.0, 3.0)])
            second = self._run(root, "run2", "abc", [(1.0, 2.0, 3.0)])
            (second / "build_id.txt").unlink()
            (second / "pooled_from.txt").write_text(str(first) + "\n", encoding="utf-8")
            sources = gate_verdict.resolve_sources(str(second))
            with self.assertRaises(gate_verdict.PoolError):
                gate_verdict.check_pool_builds(sources)


class TestIncompleteRounds(unittest.TestCase):
    def test_incomplete_ratelimit_pair_is_dropped_not_fatal(self):
        with tempfile.TemporaryDirectory() as tmp:
            for rnd in (1, 2, 3):
                Path(tmp, f"rl_r{rnd}_baseline.json").write_text(json.dumps({"rps": 150000.0}), encoding="utf-8")
            for rnd in (1, 2):  # round 3 lost its /ratelimit arm
                Path(tmp, f"rl_r{rnd}_ratelimit.json").write_text(json.dumps({"rps": 130000.0}), encoding="utf-8")
            rep = gate_verdict.Report({"ratelimit_tax_us": {"lo": 0.5, "hi": 2.4}})
            err = StringIO()
            gate_verdict.ratelimit(tmp, rep, err=err)
        self.assertEqual(rep.rows[0][6], 2)
        self.assertIn("dropping rate-limit round rl_r3_baseline.json", err.getvalue())

    def test_zero_request_cpu_arm_is_dropped(self):
        with tempfile.TemporaryDirectory() as tmp:
            for route in ("baseline", "ratelimit"):
                prefix = Path(tmp) / f"cpu_r1_{route}"
                Path(f"{prefix}.json").write_text(json.dumps({"reqs": 0}), encoding="utf-8")
                Path(f"{prefix}.snap0.json").write_text(json.dumps(_snap(7, {"1": 0}, 0)), encoding="utf-8")
                Path(f"{prefix}.snap1.json").write_text(json.dumps(_snap(7, {"1": 5}, 0)), encoding="utf-8")
            rep = gate_verdict.Report({})
            gate_verdict.ratelimit_cpu(tmp, rep, err=StringIO())
        self.assertEqual([r[5] for r in rep.rows], ["skipped", "skipped"])


def _snap(pid, tasks, ticks):
    return {"pid": pid, "tasks": tasks, "stat_ticks": ticks, "clk_tck": 100}


class TestCpuPerRequest(unittest.TestCase):
    def test_schedstat_used_when_no_thread_exited(self):
        before = _snap(1, {"1": 1_000, "2": 2_000}, 10)
        after = _snap(1, {"1": 4_000, "2": 3_000, "3": 500}, 11)
        self.assertEqual(proc_cpu.cpu_ns_between(before, after), (4_500.0, "schedstat"))

    def test_falls_back_to_ticks_when_a_thread_exited(self):
        before = _snap(1, {"1": 1_000, "2": 2_000}, 10)
        after = _snap(1, {"1": 4_000}, 13)
        self.assertEqual(proc_cpu.cpu_ns_between(before, after), (3 * 1e9 / 100, "stat"))

    def test_ratelimit_cpu_is_info_until_banded(self):
        with tempfile.TemporaryDirectory() as tmp:
            for rnd in (1, 2):
                for route, per_req_ns in (("baseline", 10_000), ("ratelimit", 11_000 + rnd)):
                    prefix = Path(tmp) / f"cpu_r{rnd}_{route}"
                    reqs = 1000
                    Path(f"{prefix}.json").write_text(
                        json.dumps({"reqs": reqs, "target_rps": 20000, "achieved_rps": 20000}),
                        encoding="utf-8",
                    )
                    Path(f"{prefix}.snap0.json").write_text(json.dumps(_snap(7, {"1": 0}, 0)), encoding="utf-8")
                    Path(f"{prefix}.snap1.json").write_text(
                        json.dumps(_snap(7, {"1": per_req_ns * reqs}, 0)), encoding="utf-8"
                    )
            rep = gate_verdict.Report({})
            gate_verdict.ratelimit_cpu(tmp, rep, err=StringIO())
        rows = {r[0]: r for r in rep.rows}
        self.assertEqual(rows["baseline_cpu_us_per_req"][5], "info")
        delta = rows["ratelimit_cpu_us_per_req"]
        self.assertEqual(delta[5], "info")
        self.assertAlmostEqual(delta[1], 1.0015)
        self.assertEqual(delta[6], 2)


if __name__ == "__main__":
    unittest.main()
