#!/usr/bin/env python3
"""Unit tests for cpu_split.py topology splitting and cpuset validation."""

from io import StringIO
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from cpu_split import (
    check_split,
    cpusets_equal,
    format_cpuset,
    parse_cpuset,
    split_cpus,
)


def create_fake_sysfs(
    root: Path,
    topology: dict[int, tuple[int, int]],
    online: str = "",
    core_cpus: str = "",
    atom_cpus: str = "",
) -> None:
    """Populate a temporary directory with a mock sysfs CPU hierarchy."""
    cpu_dir = root / "devices" / "system" / "cpu"
    cpu_dir.mkdir(parents=True, exist_ok=True)

    if online:
        (cpu_dir / "online").write_text(online + "\n")

    for cpu_id, (pkg_id, core_id) in topology.items():
        topo = cpu_dir / f"cpu{cpu_id}" / "topology"
        topo.mkdir(parents=True, exist_ok=True)
        (topo / "physical_package_id").write_text(f"{pkg_id}\n")
        (topo / "core_id").write_text(f"{core_id}\n")

    if core_cpus:
        p_dir = root / "devices" / "cpu_core"
        p_dir.mkdir(parents=True, exist_ok=True)
        (p_dir / "cpus").write_text(core_cpus + "\n")

    if atom_cpus:
        e_dir = root / "devices" / "cpu_atom"
        e_dir.mkdir(parents=True, exist_ok=True)
        (e_dir / "cpus").write_text(atom_cpus + "\n")


class TestCpuSplit(unittest.TestCase):
    def test_adjacent_siblings_24cpu_hybrid(self):
        # i7-13700K layout: 8 P-cores (pairs 0-1 ... 14-15), 8 E-cores (16-23 singletons)
        topo: dict[int, tuple[int, int]] = {}
        for p_idx in range(8):
            topo[p_idx * 2] = (0, p_idx * 4)
            topo[p_idx * 2 + 1] = (0, p_idx * 4)
        for e_idx in range(8):
            topo[16 + e_idx] = (0, 32 + e_idx)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            create_fake_sysfs(root, topo, online="0-23", core_cpus="0-15", atom_cpus="16-23")
            proxy, gen = split_cpus(str(root))
            self.assertEqual(proxy, "0-11")
            self.assertEqual(gen, "12-23")

            # Check hybrid report and no shared-core warning
            with patch("sys.stderr", new=StringIO()) as fake_err:
                msgs = check_split(proxy, gen, str(root))
                err_output = fake_err.getvalue()
                self.assertNotIn("WARNING", err_output)
                self.assertIn("P-core", err_output)
                self.assertIn("E-core", err_output)

    def test_interleaved_siblings_16cpu(self):
        # 8 cores / 16 logical CPUs where n and n+8 are siblings
        topo: dict[int, tuple[int, int]] = {}
        for i in range(8):
            topo[i] = (0, i)
            topo[i + 8] = (0, i)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            create_fake_sysfs(root, topo, online="0-15")
            proxy, gen = split_cpus(str(root))
            self.assertEqual(proxy, "0-3,8-11")
            self.assertEqual(gen, "4-7,12-15")

            with patch("sys.stderr", new=StringIO()) as fake_err:
                check_split(proxy, gen, str(root))
                self.assertNotIn("WARNING", fake_err.getvalue())

    def test_no_smt(self):
        # 4 cores / 4 logical CPUs, 1 thread per core
        topo: dict[int, tuple[int, int]] = {i: (0, i) for i in range(4)}

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            create_fake_sysfs(root, topo, online="0-3")
            proxy, gen = split_cpus(str(root))
            self.assertEqual(proxy, "0-1")
            self.assertEqual(gen, "2-3")

            with patch("sys.stderr", new=StringIO()) as fake_err:
                check_split(proxy, gen, str(root))
                self.assertNotIn("WARNING", fake_err.getvalue())

    def test_shared_core_warning(self):
        # 2 cores, 2 threads each: 0-1 sibling, 2-3 sibling
        topo = {0: (0, 0), 1: (0, 0), 2: (0, 1), 3: (0, 1)}

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            create_fake_sysfs(root, topo, online="0-3")

            # Clean split: cores kept intact
            with patch("sys.stderr", new=StringIO()) as fake_err:
                check_split("0-1", "2-3", str(root))
                self.assertNotIn("WARNING", fake_err.getvalue())

            # Shared core split: PROXY holds 0, GEN holds 1 (core 0 shared)
            with patch("sys.stderr", new=StringIO()) as fake_err:
                check_split("0,2", "1,3", str(root))
                err_val = fake_err.getvalue()
                self.assertIn("WARNING: physical core(s) shared between PROXY and GEN", err_val)
                self.assertIn("0-1", err_val)

    def test_set_normalization_and_equality(self):
        self.assertEqual(format_cpuset(parse_cpuset("0,1,2,3")), "0-3")
        self.assertEqual(format_cpuset(parse_cpuset("0-3,8,9,10,11")), "0-3,8-11")
        self.assertEqual(format_cpuset(parse_cpuset("8-11,0-3")), "0-3,8-11")
        self.assertEqual(format_cpuset(parse_cpuset("")), "")

        self.assertTrue(cpusets_equal("0-3,8-11", "0,1,2,3,8,9,10,11"))
        self.assertTrue(cpusets_equal("12-23", "12,13,14,15,16-23"))
        self.assertFalse(cpusets_equal("0-3", "0-4"))
        self.assertFalse(cpusets_equal("0-3", "0,1,2,4"))


if __name__ == "__main__":
    unittest.main()
