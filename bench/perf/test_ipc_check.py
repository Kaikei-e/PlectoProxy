#!/usr/bin/env python3
"""Tests for ipc_check.py (synthetic gungraun / Criterion trees)."""

from io import StringIO
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import ipc_check  # noqa: E402


def _gungraun(target, rel, per_thread_ir):
    d = Path(target) / "gungraun" / rel
    d.mkdir(parents=True)
    name = rel.rsplit("/", 1)[-1]
    for i, ir in enumerate(per_thread_ir, 1):
        (d / f"callgrind.{name}.t{i:02d}.p1.out").write_text(
            f"events: Ir Dr Dw\nsummary: {ir} 10 5\n" if ir else "summary: 0\n", encoding="utf-8"
        )
    # A saved baseline next to the live files must not be summed.
    (d / f"callgrind.{name}.t01.p1.out.base@main").write_text("summary: 999999\n", encoding="utf-8")


def _criterion(target, rel, ns):
    d = Path(target) / "criterion" / rel / "new"
    d.mkdir(parents=True)
    (d / "estimates.json").write_text(json.dumps({"mean": {"point_estimate": ns}}), encoding="utf-8")


class TestIpcCheck(unittest.TestCase):
    def test_sums_thread_files_and_ignores_baselines(self):
        with tempfile.TemporaryDirectory() as t:
            _gungraun(t, "plecto-host/wasm_inst/wasm_inst/on_request.trusted_pooled", [30448, 0, 0])
            ir = ipc_check.gungraun_ir(Path(t) / "gungraun/plecto-host/wasm_inst/wasm_inst/on_request.trusted_pooled")
        self.assertEqual(ir, 30448)

    def test_consistent_pair_passes(self):
        with tempfile.TemporaryDirectory() as t:
            # 30,448 Ir in 2,090 ns at 5.4 GHz -> IPC 2.70
            _gungraun(t, "plecto-host/wasm_inst/wasm_inst/on_request.trusted_pooled", [30448])
            _criterion(t, "on_request/trusted_pooled", 2090.0)
            out = StringIO()
            violations, judged = ipc_check.check(t, 5.4, 5.0, out)
        self.assertEqual((violations, judged), (0, 1))
        self.assertIn("IPC 2.70", out.getvalue())

    def test_teardown_contaminated_count_is_flagged(self):
        with tempfile.TemporaryDirectory() as t:
            # The pre-fix v0.9.0 shape: 507,977 Ir against 2,880 ns -> IPC ~32.7
            _gungraun(t, "plecto-host/wasm_inst/wasm_inst/on_request.trusted_pooled", [507977])
            _criterion(t, "on_request/trusted_pooled", 2880.0)
            violations, judged = ipc_check.check(t, 5.4, 5.0, StringIO())
        self.assertEqual((violations, judged), (1, 1))

    def test_missing_layers_are_skipped_not_failed(self):
        with tempfile.TemporaryDirectory() as t:
            _gungraun(t, "plecto-control/fastpath_inst/fastpath_inst/lb_pick.round_robin", [225])
            out = StringIO()
            violations, judged = ipc_check.check(t, 5.4, 5.0, out)
            self.assertEqual(ipc_check.main(["--target-dir", t]), 0)
        self.assertEqual((violations, judged), (0, 0))
        self.assertIn("skip  lb_pick/round_robin: no criterion data", out.getvalue())


if __name__ == "__main__":
    unittest.main()
