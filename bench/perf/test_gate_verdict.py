#!/usr/bin/env python3
"""Tests for gate_verdict.py pure judgment and exit code aggregation."""

import importlib.util
from io import StringIO
from pathlib import Path
import sys
import unittest

# Import gate_verdict by path
_target = Path(__file__).resolve().parent / "gate_verdict.py"
_spec = importlib.util.spec_from_file_location("gate_verdict", _target)
gate_verdict = importlib.util.module_from_spec(_spec)
sys.modules["gate_verdict"] = gate_verdict
_spec.loader.exec_module(gate_verdict)


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
        self.assertEqual(lines[0], "invariant,value,ci_half,band_lo,band_hi,verdict")
        self.assertEqual(len(lines), 4)
        self.assertEqual(err.getvalue().strip(), "1 pass, 1 fail, 1 inconclusive")


if __name__ == "__main__":
    unittest.main()
