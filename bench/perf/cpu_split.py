#!/usr/bin/env python3
"""CPU topology-aware partitioning and affinity validation for perf benchmarks."""

import collections
import os
import re
import sys
from typing import Iterable, List, Optional, Set, Tuple


def parse_cpuset(s: str) -> Set[int]:
    """Parse cpuset string like '0-5,8,10-12' into a set of integer CPU ids."""
    cpus: Set[int] = set()
    s = s.strip()
    if not s:
        return cpus
    for part in s.split(","):
        part = part.strip()
        if not part:
            continue
        if "-" in part:
            lo_s, hi_s = part.split("-", 1)
            lo, hi = int(lo_s.strip()), int(hi_s.strip())
            if lo > hi:
                raise ValueError(f"Invalid range: {part}")
            cpus.update(range(lo, hi + 1))
        else:
            cpus.add(int(part))
    return cpus


def format_cpuset(cpus: Iterable[int]) -> str:
    """Format an iterable of CPU ids into a compressed range string (e.g. '0-5,12-17')."""
    sorted_cpus = sorted(set(cpus))
    if not sorted_cpus:
        return ""
    ranges: List[str] = []
    start = sorted_cpus[0]
    end = start
    for c in sorted_cpus[1:]:
        if c == end + 1:
            end = c
        else:
            ranges.append(f"{start}-{end}" if start != end else f"{start}")
            start = c
            end = c
    ranges.append(f"{start}-{end}" if start != end else f"{start}")
    return ",".join(ranges)


def cpusets_equal(a: str, b: str) -> bool:
    """Return True if both strings represent the exact same set of CPU ids."""
    return parse_cpuset(a) == parse_cpuset(b)


def _resolve_devices_dir(sysfs_root: str) -> str:
    """Resolve sysfs path accommodating either a direct sysfs root or nested /sys."""
    if os.path.exists(os.path.join(sysfs_root, "devices")):
        return sysfs_root
    if os.path.exists(os.path.join(sysfs_root, "sys", "devices")):
        return os.path.join(sysfs_root, "sys")
    return sysfs_root


def get_online_cpus(sysfs_root: str = "/sys") -> List[int]:
    """Read online logical CPU ids from sysfs."""
    base = _resolve_devices_dir(sysfs_root)
    cpu_dir = os.path.join(base, "devices", "system", "cpu")
    online_file = os.path.join(cpu_dir, "online")
    if os.path.isfile(online_file):
        try:
            with open(online_file, "r") as f:
                content = f.read().strip()
            if content:
                return sorted(parse_cpuset(content))
        except OSError:
            pass

    cpus: List[int] = []
    if os.path.isdir(cpu_dir):
        for entry in os.listdir(cpu_dir):
            m = re.match(r"^cpu(\d+)$", entry)
            if not m:
                continue
            cpu_id = int(m.group(1))
            cpu_online_file = os.path.join(cpu_dir, entry, "online")
            if os.path.isfile(cpu_online_file):
                try:
                    with open(cpu_online_file, "r") as f:
                        if f.read().strip() == "0":
                            continue
                except OSError:
                    pass
            cpus.append(cpu_id)
    return sorted(cpus)


def get_physical_cores(sysfs_root: str = "/sys") -> List[List[int]]:
    """Group online logical CPUs into physical cores ordered by lowest CPU id."""
    base = _resolve_devices_dir(sysfs_root)
    cpu_dir = os.path.join(base, "devices", "system", "cpu")
    online_cpus = get_online_cpus(sysfs_root)

    cores_map: dict[Tuple[int, int], List[int]] = collections.defaultdict(list)
    for cpu_id in online_cpus:
        topo_dir = os.path.join(cpu_dir, f"cpu{cpu_id}", "topology")
        pkg_file = os.path.join(topo_dir, "physical_package_id")
        core_file = os.path.join(topo_dir, "core_id")

        pkg_id = 0
        if os.path.isfile(pkg_file):
            try:
                with open(pkg_file, "r") as f:
                    pkg_id = int(f.read().strip())
            except (ValueError, OSError):
                pkg_id = 0

        core_id = cpu_id
        if os.path.isfile(core_file):
            try:
                with open(core_file, "r") as f:
                    core_id = int(f.read().strip())
            except (ValueError, OSError):
                core_id = cpu_id

        cores_map[(pkg_id, core_id)].append(cpu_id)

    cores_list = [sorted(cpus) for cpus in cores_map.values()]
    cores_list.sort(key=lambda core: core[0])
    return cores_list


def split_cpus(sysfs_root: str = "/sys") -> Tuple[str, str]:
    """Assign whole physical cores to proxy (>= ncpu/2 logical cpus) and generator."""
    cores = get_physical_cores(sysfs_root)
    total_cpus = sum(len(c) for c in cores)
    target = (total_cpus + 1) // 2

    proxy_cpus: List[int] = []
    gen_cpus: List[int] = []

    for core in cores:
        if len(proxy_cpus) < target:
            proxy_cpus.extend(core)
        else:
            gen_cpus.extend(core)

    return format_cpuset(proxy_cpus), format_cpuset(gen_cpus)


def check_split(proxy_str: str, gen_str: str, sysfs_root: str = "/sys") -> List[str]:
    """Check for shared physical cores and report hybrid core types.

    Writes WARNING to stderr if physical cores are shared between sets.
    Writes informational note to stderr if hybrid core types exist.
    """
    proxy_set = parse_cpuset(proxy_str)
    gen_set = parse_cpuset(gen_str)
    cores = get_physical_cores(sysfs_root)

    messages: List[str] = []

    shared_cores: List[List[int]] = []
    for core in cores:
        core_set = set(core)
        if (core_set & proxy_set) and (core_set & gen_set):
            shared_cores.append(core)

    if shared_cores or (proxy_set & gen_set):
        shared_desc = ", ".join(format_cpuset(c) for c in shared_cores) if shared_cores else format_cpuset(proxy_set & gen_set)
        msg = f"WARNING: physical core(s) shared between PROXY and GEN: {shared_desc}"
        sys.stderr.write(msg + "\n")
        messages.append(msg)

    base = _resolve_devices_dir(sysfs_root)
    core_p = os.path.join(base, "devices", "cpu_core", "cpus")
    atom_p = os.path.join(base, "devices", "cpu_atom", "cpus")
    if os.path.isfile(core_p) and os.path.isfile(atom_p):
        try:
            with open(core_p, "r") as f:
                p_cpus = parse_cpuset(f.read())
            with open(atom_p, "r") as f:
                e_cpus = parse_cpuset(f.read())

            def describe_set(s: Set[int]) -> str:
                types = []
                if s & p_cpus:
                    types.append("P-core (cpu_core)")
                if s & e_cpus:
                    types.append("E-core (cpu_atom)")
                return ", ".join(types) if types else "none"

            p_desc = describe_set(proxy_set)
            g_desc = describe_set(gen_set)
            note = f"note: hybrid CPU layout: PROXY ({proxy_str}) contains {p_desc}; GEN ({gen_str}) contains {g_desc}"
            sys.stderr.write(note + "\n")
            messages.append(note)
        except OSError:
            pass

    return messages


def main(argv: Optional[List[str]] = None) -> int:
    if argv is None:
        argv = sys.argv[1:]

    sysfs_root = os.environ.get("SYSFS_ROOT", "/sys")

    # Optional leading --sysfs <path>
    if len(argv) >= 2 and argv[0] == "--sysfs":
        sysfs_root = argv[1]
        argv = argv[2:]

    if not argv:
        proxy_ranges, gen_ranges = split_cpus(sysfs_root)
        print(f"PROXY_CPUS={proxy_ranges}")
        print(f"GEN_CPUS={gen_ranges}")
        return 0

    mode = argv[0]
    if mode == "--same":
        if len(argv) != 3:
            sys.stderr.write("Usage: cpu_split.py [--sysfs ROOT] --same <CPUS1> <CPUS2>\n")
            return 2
        return 0 if cpusets_equal(argv[1], argv[2]) else 1

    if mode == "--check":
        if len(argv) != 3:
            sys.stderr.write("Usage: cpu_split.py [--sysfs ROOT] --check <PROXY_CPUS> <GEN_CPUS>\n")
            return 2
        check_split(argv[1], argv[2], sysfs_root)
        return 0

    sys.stderr.write(f"Unknown option: {mode}\n")
    return 2


if __name__ == "__main__":
    sys.exit(main())
