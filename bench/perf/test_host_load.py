#!/usr/bin/env python3
"""Unit tests for host_load.py pre-run host-contention checks."""

from io import StringIO
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import host_load


STAT_BEFORE = """\
cpu  1000 100 500 8000 400 0 0 0 0 0
cpu0 100 0 50 800 50 0 0 0 0 0
cpu1 200 0 50 700 50 0 0 0 0 0
cpu2 500 0 100 300 100 0 0 0 0 0
cpu3 400 0 100 400 100 0 0 0 0 0
intr 12345 0
ctxt 67890
"""

# cpu0: +50 user, +50 sys, +700 idle, +50 iowait -> d_total=850, d_idle=750, d_busy=100
# cpu1: +0 user, +0 sys, +850 idle, +0 iowait -> d_total=850, d_idle=850, d_busy=0
# cpu2: +400 user, +400 sys, +50 idle, +0 iowait -> d_total=850, d_idle=50, d_busy=800 (heavy non-proxy CPU)
# cpu3: +100 user, +0 sys, +750 idle, +0 iowait -> d_total=850, d_idle=750, d_busy=100
STAT_AFTER = """\
cpu  1550 100 950 10350 450 0 0 0 0 0
cpu0 150 0 100 1500 100 0 0 0 0 0
cpu1 200 0 50 1550 50 0 0 0 0 0
cpu2 900 0 500 350 100 0 0 0 0 0
cpu3 500 0 100 1150 100 0 0 0 0 0
"""


class TestHostLoadParsingAndMath(unittest.TestCase):
    def test_parse_loadavg1(self):
        self.assertAlmostEqual(host_load.parse_loadavg1("1.71 1.75 1.80 6/6098 2411923\n"), 1.71)
        self.assertAlmostEqual(host_load.parse_loadavg1("  13.04 9.12 5.40 14/2048 999 "), 13.04)
        with self.assertRaises(ValueError):
            host_load.parse_loadavg1("   \n")
        with self.assertRaises(ValueError):
            host_load.parse_loadavg1("not-a-float 1.0 1.0")

    def test_proc_stat_busy_fraction_and_cpu_filtering(self):
        before = host_load.parse_proc_stat(STAT_BEFORE)
        after = host_load.parse_proc_stat(STAT_AFTER)

        # Aggregate cpu0 + cpu1: d_busy = 100 + 0 = 100, d_total = 850 + 850 = 1700
        # Busy cpu2 is excluded by CPU list filtering.
        busy_01 = host_load.busy_fraction_between(before, after, [0, 1])
        self.assertAlmostEqual(busy_01, 100.0 / 1700.0)

        # cpu2 alone: 800 / 850
        busy_2 = host_load.busy_fraction_between(before, after, [2])
        self.assertAlmostEqual(busy_2, 800.0 / 850.0)

        # cpu0 + cpu3: (100 + 100) / (850 + 850) = 200 / 1700
        busy_03 = host_load.busy_fraction_between(before, after, [0, 3])
        self.assertAlmostEqual(busy_03, 200.0 / 1700.0)

    def test_iowait_and_guest_accounting(self):
        # Verify iowait counts as non-busy and guest/guest_nice (cols 8..9) are not double-counted.
        s0 = host_load.parse_proc_stat("cpu0 100 0 0 100 0 0 0 0 10 5\n")
        s1 = host_load.parse_proc_stat("cpu0 200 0 0 200 100 0 0 0 50 10\n")
        # d_total (cols 0..7) = 100 (user) + 100 (idle) + 100 (iowait) = 300
        # d_idle = 100 (idle) + 100 (iowait) = 200 -> busy = 100 / 300
        self.assertAlmostEqual(host_load.busy_fraction_between(s0, s1, [0]), 1.0 / 3.0)

    def test_minimal_four_column_stat_and_zero_delta(self):
        s0 = host_load.parse_proc_stat("cpu0 100 0 50 850\n")
        s1 = host_load.parse_proc_stat("cpu0 150 0 50 900\n")
        self.assertAlmostEqual(host_load.busy_fraction_between(s0, s1, [0]), 50.0 / 100.0)
        self.assertEqual(host_load.busy_fraction_between(s0, s0, [0]), 0.0)

    def test_missing_or_empty_cpu_list_raises(self):
        before = host_load.parse_proc_stat(STAT_BEFORE)
        after = host_load.parse_proc_stat(STAT_AFTER)
        with self.assertRaises(ValueError):
            host_load.busy_fraction_between(before, after, [])
        with self.assertRaises(ValueError):
            host_load.busy_fraction_between(before, after, [99])


class TestHostLoadVerdictAndCLI(unittest.TestCase):
    def _run_cli_with_synthetic_proc(self, loadavg_text, stat_seq, argv):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            load_path = root / "loadavg"
            stat_path = root / "stat"
            out_path = root / "host_load.txt"
            load_path.write_text(loadavg_text)
            stat_path.write_text(stat_seq[0])

            slept = []

            def fake_sleep(secs):
                slept.append(secs)
                stat_path.write_text(stat_seq[1])

            orig_measure = host_load.measure_host_load

            def wrapped_measure(**kwargs):
                kwargs["loadavg_path"] = str(load_path)
                kwargs["stat_path"] = str(stat_path)
                kwargs["sleep_fn"] = fake_sleep
                return orig_measure(**kwargs)

            with (
                patch.object(host_load, "measure_host_load", side_effect=wrapped_measure),
                patch("sys.stdout", new=StringIO()) as fake_out,
                patch("sys.stderr", new=StringIO()) as fake_err,
            ):
                rc = host_load.main([*argv, "--out", str(out_path)])
                stdout_text = fake_out.getvalue()
                stderr_text = fake_err.getvalue()
            out_text = out_path.read_text() if out_path.exists() else ""
            return rc, stdout_text, stderr_text, out_text, slept

    def test_within_thresholds_exits_0_and_writes_record(self):
        # cpu0-1 busy = 100/1700 ≈ 0.0588 <= 0.10; loadavg1 = 1.71 <= 4.0
        rc, stdout_text, stderr_text, out_text, slept = self._run_cli_with_synthetic_proc(
            "1.71 1.75 1.80 6/6098 2411923\n",
            (STAT_BEFORE, STAT_AFTER),
            ["--cpus", "0-1", "--max-load", "4.0", "--max-busy", "0.10", "--window", "3"],
        )
        self.assertEqual(rc, 0, stderr_text)
        self.assertEqual(slept, [3.0])
        self.assertEqual(stdout_text, out_text)
        kv = dict(line.split("=", 1) for line in out_text.strip().splitlines())
        self.assertEqual(kv["loadavg1"], "1.71")
        self.assertEqual(kv["proxy_cpus"], "0-1")
        self.assertEqual(kv["proxy_busy"], "0.0588")
        self.assertEqual(kv["max_load"], "4")
        self.assertEqual(kv["max_busy"], "0.1")
        self.assertEqual(kv["contended"], "0")

    def test_loadavg_exceeded_exits_3(self):
        rc, stdout_text, _, out_text, _ = self._run_cli_with_synthetic_proc(
            "4.01 3.50 2.00 5/1000 123\n",
            (STAT_BEFORE, STAT_AFTER),
            ["--cpus", "0-1", "--max-load", "4.0", "--max-busy", "0.10"],
        )
        self.assertEqual(rc, 3)
        self.assertEqual(stdout_text, out_text)
        self.assertIn("contended=1\n", out_text)

    def test_proxy_busy_exceeded_exits_3(self):
        # cpu0+cpu3 busy = 200/1700 ≈ 0.1176 > 0.10
        rc, stdout_text, _, out_text, _ = self._run_cli_with_synthetic_proc(
            "0.50 0.50 0.50 1/100 123\n",
            (STAT_BEFORE, STAT_AFTER),
            ["--cpus", "0,3", "--max-load", "4.0", "--max-busy", "0.10"],
        )
        self.assertEqual(rc, 3)
        self.assertIn("proxy_cpus=0,3\n", out_text)
        self.assertIn("proxy_busy=0.1176\n", out_text)
        self.assertIn("contended=1\n", out_text)

    def test_exact_threshold_equality_is_within(self):
        # cpu0+cpu1 = 100/1700; test exact boundary loadavg1 == max_load
        rc, _, _, out_text, _ = self._run_cli_with_synthetic_proc(
            "4.00 2.00 1.00 1/100 123\n",
            (STAT_BEFORE, STAT_AFTER),
            ["--cpus", "0-1", "--max-load", "4.0", "--max-busy", "0.10"],
        )
        self.assertEqual(rc, 0)
        self.assertIn("contended=0\n", out_text)

    def test_invalid_cpus_exits_64(self):
        with patch("sys.stderr", new=StringIO()):
            rc = host_load.main(["--cpus", "", "--max-load", "4.0", "--max-busy", "0.10"])
        self.assertEqual(rc, 64)


if __name__ == "__main__":
    unittest.main()
