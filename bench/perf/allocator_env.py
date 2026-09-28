#!/usr/bin/env python3
"""Allocator environment sanitization and configuration for Plecto benchmarks.

Shared between run-perf.sh launch and arena_sweep.py.
Enforces:
- Rejection of LD_PRELOAD.
- Stripping of inherited MALLOC_* and glibc.malloc.* tunables.
- BENCH_MALLOC_ARENA_MAX translation to PLECTO_MALLOC_ARENA_MAX.
- Ordering of env unsets (-u) before assignments.
"""

import os
import sys


def filter_glibc_tunables(raw: str) -> str:
    """Filter out glibc.malloc.* settings, keeping other tunables."""
    parts = [x for x in raw.split(":") if x and not x.startswith("glibc.malloc.")]
    return ":".join(parts)


def clean_env(extra=None, base_env=None):
    """Sanitize environment dictionary for Python subprocess execution (used by arena_sweep)."""
    if base_env is None:
        base_env = os.environ
    if base_env.get("LD_PRELOAD"):
        sys.stderr.write("unset LD_PRELOAD before running perf benchmarks\n")
        sys.exit(64)

    env = {
        k: v
        for k, v in base_env.items()
        if not k.startswith("MALLOC_")
        and k not in {"PLECTO_MALLOC_ARENA_MAX", "TOKIO_WORKER_THREADS", "GLIBC_TUNABLES", "LD_PRELOAD"}
    }
    raw_tunables = base_env.get("GLIBC_TUNABLES", "")
    if raw_tunables:
        cleaned = filter_glibc_tunables(raw_tunables)
        if cleaned:
            env["GLIBC_TUNABLES"] = cleaned
    env.update({"NO_COLOR": "true", "K6_NO_USAGE_REPORT": "true", "RUST_LOG": "warn"})
    if extra:
        env.update(extra)
    return env


def get_env_args(base_env=None):
    """Generate command-line arguments for `env` command in bash launch().

    Returns a list of arguments where all '-u <VAR>' options precede any
    'KEY=VALUE' assignments.
    """
    if base_env is None:
        base_env = os.environ
    if base_env.get("LD_PRELOAD"):
        sys.stderr.write("unset LD_PRELOAD before running perf benchmarks\n")
        sys.exit(64)

    unsets = ["MALLOC_ARENA_MAX", "LD_PRELOAD"]
    for k in base_env:
        if k.startswith("MALLOC_") and k not in unsets:
            unsets.append(k)

    sets = []
    if "BENCH_MALLOC_ARENA_MAX" in base_env:
        sets.append(f"PLECTO_MALLOC_ARENA_MAX={base_env['BENCH_MALLOC_ARENA_MAX']}")
    else:
        unsets.append("PLECTO_MALLOC_ARENA_MAX")

    if "GLIBC_TUNABLES" in base_env:
        cleaned = filter_glibc_tunables(base_env["GLIBC_TUNABLES"])
        if cleaned:
            sets.append(f"GLIBC_TUNABLES={cleaned}")
        else:
            unsets.append("GLIBC_TUNABLES")

    args = []
    for u in unsets:
        args.extend(["-u", u])
    args.extend(sets)
    return args


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "--env-args":
        args = get_env_args()
        for a in args:
            print(a)
    else:
        print("Usage: allocator_env.py --env-args", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
