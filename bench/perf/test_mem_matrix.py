"""Measurement integrity checks: phase boundaries, failed load, and sampler shutdown."""

import json
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

from arena_sweep import build_proxy_env, clean_env, parse_arenas, read_oha
from mem_matrix import Sampler, memory_summary, read_k6


class MemoryMeasurementTests(unittest.TestCase):
    def test_peak_excludes_tail_and_settled_excludes_load(self):
        rows = [(0, 50, 10), (1, 100, 80), (2, 200, 150),
                (3, 999, 900), (4, 80, 50), (5, 60, 40), (6, 40, 30)]
        summary = memory_summary(rows, 1, 2, 6)
        self.assertEqual(summary, {"peak_kb": 200, "peak_pd_kb": 150, "settled_kb": 60})

    def test_missing_load_or_short_tail_cannot_produce_result(self):
        rows = [(0, 50, 10), (1, 100, 80), (2, 200, 150)]
        for boundaries in [(5, 6, 10), (0, 1, 2)]:
            with self.assertRaises(ValueError):
                memory_summary(rows, *boundaries)

    def test_sampler_can_join_and_records_threads(self):
        sampler = Sampler(os.getpid())
        sampler.start()
        time.sleep(0.25)
        sampler.stop()
        self.assertFalse(sampler.is_alive())
        self.assertGreater(len(sampler.rows), 0)
        self.assertEqual(len(sampler.rows), len(sampler.status_rows))

    def test_missing_process_fails_measurement(self):
        sampler = Sampler(999999999)
        sampler.start()
        time.sleep(0.05)
        with self.assertRaises(RuntimeError):
            sampler.stop()

    def test_failed_or_empty_load_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "k6.json"
            for result in [{"rps": 0, "failed_rate": 0},
                           {"rps": 100, "failed_rate": 0.01},
                           {"rps": float("nan"), "failed_rate": 0}]:
                path.write_text(json.dumps(result))
                with self.assertRaises(ValueError):
                    read_k6(path)
            path.write_text('{"rps":100,"failed_rate":0}')
            self.assertEqual(read_k6(path)["rps"], 100)

    def test_allocator_settings_do_not_leak_between_cells(self):
        with patch.dict(os.environ, {"MALLOC_ARENA_MAX": "1", "MALLOC_MMAP_THRESHOLD_": "131072",
                                     "PLECTO_MALLOC_ARENA_MAX": "1",
                                     "GLIBC_TUNABLES": "glibc.malloc.arena_max=2:glibc.cpu.hwcaps=-AVX2"}):
            env = clean_env({"PLECTO_MALLOC_ARENA_MAX": "0"})
        self.assertNotIn("MALLOC_ARENA_MAX", env)
        self.assertNotIn("MALLOC_MMAP_THRESHOLD_", env)
        self.assertEqual(env["PLECTO_MALLOC_ARENA_MAX"], "0")
        self.assertEqual(env["GLIBC_TUNABLES"], "glibc.cpu.hwcaps=-AVX2")

    def test_default_arena_omits_env_variable(self):
        with patch.dict(os.environ, {"MALLOC_ARENA_MAX": "4", "PLECTO_MALLOC_ARENA_MAX": "4",
                                     "GLIBC_TUNABLES": "glibc.malloc.arena_max=4:glibc.cpu.hwcaps=-AVX2"}):
            proxy_env = build_proxy_env("127.0.0.1:8080", "127.0.0.1:8081", "default")
            env = clean_env(proxy_env)
        self.assertNotIn("PLECTO_MALLOC_ARENA_MAX", env)
        self.assertNotIn("MALLOC_ARENA_MAX", env)
        self.assertEqual(env["GLIBC_TUNABLES"], "glibc.cpu.hwcaps=-AVX2")

    def test_explicit_arena_and_zero_pass_env_variable(self):
        with patch.dict(os.environ, {"MALLOC_ARENA_MAX": "4", "PLECTO_MALLOC_ARENA_MAX": "4"}):
            env32 = clean_env(build_proxy_env("127.0.0.1:8080", "127.0.0.1:8081", 32))
            env0 = clean_env(build_proxy_env("127.0.0.1:8080", "127.0.0.1:8081", 0))
        self.assertEqual(env32["PLECTO_MALLOC_ARENA_MAX"], "32")
        self.assertNotIn("MALLOC_ARENA_MAX", env32)
        self.assertEqual(env0["PLECTO_MALLOC_ARENA_MAX"], "0")
        self.assertNotIn("MALLOC_ARENA_MAX", env0)

    def test_parse_arenas_supports_integers_and_default(self):
        self.assertEqual(parse_arenas("4,16,24,32,0"), [4, 16, 24, 32, 0])
        self.assertEqual(parse_arenas("default,32,0"), ["default", 32, 0])
        self.assertEqual(parse_arenas("default"), ["default"])
        with self.assertRaises(ValueError):
            parse_arenas("-1")
        with self.assertRaises(ValueError):
            parse_arenas("invalid")
        with self.assertRaises(ValueError):
            parse_arenas("")

    def test_oha_end_of_window_cancellation_is_not_a_transport_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "oha.json"
            report = {"summary": {"requestsPerSec": 100, "successRate": 1},
                      "statusCodeDistribution": {"200": 1000},
                      "latencyPercentiles": {"p50": 0.001, "p99": 0.002},
                      "errorDistribution": {"aborted due to deadline": 50}}
            path.write_text(json.dumps(report))
            self.assertEqual(read_oha(path)["rps"], 100)
            report["errorDistribution"]["connection reset"] = 1
            path.write_text(json.dumps(report))
            with self.assertRaises(ValueError):
                read_oha(path)


if __name__ == "__main__":
    unittest.main()
