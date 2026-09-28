"""Regression tests for bench/perf allocator environment sanitization."""

import json
import os
from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]
RUN_PERF = ROOT / "bench/perf/run-perf.sh"
ALLOCATOR_ENV = ROOT / "bench/perf/allocator_env.py"


class AllocatorEnvTests(unittest.TestCase):
    def _run_allocator_args(self, extra_env):
        env = dict(os.environ)
        env.update(extra_env)
        proc = subprocess.run(
            ["python3", str(ALLOCATOR_ENV), "--env-args"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            proc.returncode, 0, f"allocator_env failed: {proc.stderr}\nstdout: {proc.stdout}"
        )
        return proc.stdout.splitlines()

    def _run_env_capture(self, extra_env):
        args = self._run_allocator_args(extra_env)
        # Execute actual `env` command with generated args to verify:
        # 1. '-u' options precede assignments without error.
        # 2. Resulting environment has variables correctly set / unset.
        cmd = ["env", *args, "python3", "-c", "import json, os; print(json.dumps(dict(os.environ)))"]
        env = dict(os.environ)
        env.update(extra_env)
        proc = subprocess.run(
            cmd,
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            proc.returncode, 0, f"env execution failed: {proc.stderr}\nstdout: {proc.stdout}"
        )
        return json.loads(proc.stdout)

    def test_default_launch_omits_arena_override_and_cleans_allocator_env(self):
        extra = {
            "MALLOC_ARENA_MAX": "4",
            "MALLOC_MMAP_THRESHOLD_": "131072",
            "PLECTO_MALLOC_ARENA_MAX": "4",
            "GLIBC_TUNABLES": "glibc.malloc.arena_max=2:glibc.cpu.hwcaps=-AVX2",
        }
        captured = self._run_env_capture(extra)
        self.assertNotIn("MALLOC_ARENA_MAX", captured)
        self.assertNotIn("MALLOC_MMAP_THRESHOLD_", captured)
        self.assertNotIn("PLECTO_MALLOC_ARENA_MAX", captured)
        self.assertEqual(captured.get("GLIBC_TUNABLES"), "glibc.cpu.hwcaps=-AVX2")

    def test_explicit_arena_with_malloc_only_tunables_orders_unsets_before_sets(self):
        # Regression test for host review: BENCH_MALLOC_ARENA_MAX=32 with malloc-only tunables
        # must not produce 'env: -u: No such file or directory' caused by -u following an assignment.
        extra = {
            "BENCH_MALLOC_ARENA_MAX": "32",
            "GLIBC_TUNABLES": "glibc.malloc.arena_max=2:glibc.malloc.tcache_count=64",
            "MALLOC_ARENA_MAX": "4",
        }
        args = self._run_allocator_args(extra)
        saw_assignment = False
        for arg in args:
            if "=" in arg:
                saw_assignment = True
            if arg == "-u":
                self.assertFalse(saw_assignment, "Found '-u' after an assignment in env args")

        captured = self._run_env_capture(extra)
        self.assertEqual(captured.get("PLECTO_MALLOC_ARENA_MAX"), "32")
        self.assertNotIn("MALLOC_ARENA_MAX", captured)
        self.assertNotIn("GLIBC_TUNABLES", captured)

    def test_explicit_zero_passes_zero_and_strips_startup_malloc_arena_max(self):
        extra = {
            "BENCH_MALLOC_ARENA_MAX": "0",
            "MALLOC_ARENA_MAX": "4",
        }
        captured = self._run_env_capture(extra)
        self.assertEqual(captured.get("PLECTO_MALLOC_ARENA_MAX"), "0")
        self.assertNotIn("MALLOC_ARENA_MAX", captured)

    def test_allocator_env_rejects_ld_preload(self):
        env = dict(os.environ)
        env["LD_PRELOAD"] = "/tmp/fake.so"
        proc = subprocess.run(
            ["python3", str(ALLOCATOR_ENV), "--env-args"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(proc.returncode, 64)
        self.assertIn("unset LD_PRELOAD", proc.stderr)

    def test_run_perf_rejects_ld_preload_at_startup(self):
        env = dict(os.environ)
        env["LD_PRELOAD"] = "/tmp/fake.so"
        proc = subprocess.run(
            ["bash", str(RUN_PERF), "cpus"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(proc.returncode, 64)
        self.assertIn("unset LD_PRELOAD", proc.stderr)


if __name__ == "__main__":
    unittest.main()
