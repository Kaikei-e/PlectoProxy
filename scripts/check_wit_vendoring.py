#!/usr/bin/env python3
"""Check that files vendored into crates for crates.io publishing (`cargo package` only
includes files under a crate's own root) stay byte-identical to their canonical source.

Canonical -> vendored pairs checked:
- plecto/wit/world.wit           -> plecto/crates/host/wit/world.wit
- plecto/wit/v0.1.0/world.wit    -> plecto/crates/host/wit/v0.1.0/world.wit
- plecto/wit/v0.2.0/world.wit    -> plecto/crates/host/wit/v0.2.0/world.wit
- plecto/wit-streaming/streaming.wit -> plecto/crates/host/wit-streaming/streaming.wit
- plecto/examples/filters/filter-template/Cargo.toml
    -> plecto/crates/server/templates/filter-template/Cargo.toml
- plecto/examples/filters/filter-template/src/lib.rs
    -> plecto/crates/server/templates/filter-template/src/lib.rs

Exit 0 on success, 1 on any drift or missing file.
"""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PLECTO = ROOT / "plecto"

PAIRS = [
    (PLECTO / "wit" / "world.wit", PLECTO / "crates/host/wit/world.wit"),
    (PLECTO / "wit/v0.1.0/world.wit", PLECTO / "crates/host/wit/v0.1.0/world.wit"),
    (PLECTO / "wit/v0.2.0/world.wit", PLECTO / "crates/host/wit/v0.2.0/world.wit"),
    (
        PLECTO / "wit-streaming/streaming.wit",
        PLECTO / "crates/host/wit-streaming/streaming.wit",
    ),
    (
        PLECTO / "examples/filters/filter-template/Cargo.toml",
        PLECTO / "crates/server/templates/filter-template/Cargo.toml.template",
    ),
    (
        PLECTO / "examples/filters/filter-template/src/lib.rs",
        PLECTO / "crates/server/templates/filter-template/src/lib.rs",
    ),
]


def main() -> int:
    violations: list[str] = []
    for canonical, vendored in PAIRS:
        rel_canonical = canonical.relative_to(ROOT)
        rel_vendored = vendored.relative_to(ROOT)
        if not canonical.exists():
            violations.append(f"missing canonical source: {rel_canonical}")
            continue
        if not vendored.exists():
            violations.append(f"missing vendored copy: {rel_vendored}")
            continue
        if canonical.read_bytes() != vendored.read_bytes():
            violations.append(
                f"drift: {rel_vendored} no longer matches {rel_canonical} "
                f"(copy {rel_canonical} -> {rel_vendored})"
            )

    if violations:
        print("check_wit_vendoring: FAILED", file=sys.stderr)
        for v in violations:
            print(f"  - {v}", file=sys.stderr)
        return 1

    print(f"check_wit_vendoring: {len(PAIRS)} vendored copies OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
