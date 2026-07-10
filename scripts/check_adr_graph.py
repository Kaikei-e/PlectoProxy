#!/usr/bin/env python3
"""Validate Plecto ADR frontmatter graph invariants (append-only decision history).

Checks:
- frontmatter parses (YAML between --- markers)
- status is one of proposed | accepted | superseded
- amends / supersedes reference existing ADR files
- amended_by reverse edges are consistent with amends
- no cycles in amends+supersedes edges
- wikilinks [[000NNN]] resolve to files

Exit 0 on success, 1 on violations.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ADR_DIR = ROOT / "docs" / "ADR"

ADR_FILE_RE = re.compile(r"^(\d{6})\.md$")
WIKILINK_RE = re.compile(r"\[\[(\d{6})\]\]")
FRONTMATTER_RE = re.compile(r"^---\n(.*?)\n---\n", re.DOTALL)


def parse_frontmatter(text: str) -> dict[str, str | list[str]]:
    m = FRONTMATTER_RE.match(text)
    if not m:
        raise ValueError("missing frontmatter")
    block = m.group(1)
    out: dict[str, str | list[str]] = {}
    for line in block.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("status:"):
            out["status"] = line.split(":", 1)[1].strip()
        elif line.startswith(("amends:", "supersedes:", "amended_by:")):
            key, rest = line.split(":", 1)
            inner = rest.strip()
            if inner.startswith("[") and inner.endswith("]"):
                inner = inner[1:-1]
                out[key] = [
                    x.strip().strip('"').strip("'")
                    for x in inner.split(",")
                    if x.strip()
                ]
    return out


def normalize_adr_ref(target: str) -> str:
    t = target.replace("ADR-", "").replace("ADR", "").strip()
    if t.isdigit():
        return f"{int(t):06d}"
    return t


def adr_id(path: Path) -> str:
    return path.stem


def load_adrs() -> dict[str, Path]:
    adrs: dict[str, Path] = {}
    for p in sorted(ADR_DIR.glob("*.md")):
        if ADR_FILE_RE.match(p.name):
            adrs[adr_id(p)] = p
    return adrs


def edges(adrs: dict[str, Path]) -> tuple[dict[str, list[tuple[str, str]]], list[str]]:
    """node -> [(target, kind)]"""
    graph: dict[str, list[tuple[str, str]]] = {k: [] for k in adrs}
    errors: list[str] = []
    for aid, path in adrs.items():
        text = path.read_text(encoding="utf-8")
        try:
            fm = parse_frontmatter(text)
        except ValueError as e:
            errors.append(f"{path.name}: {e}")
            continue
        status = fm.get("status")
        if status and status not in {"proposed", "accepted", "superseded"}:
            errors.append(f"{path.name}: invalid status {status!r}")
        for kind in ("amends", "supersedes"):
            targets = fm.get(kind, [])
            if isinstance(targets, str):
                targets = [targets]
            for target in targets:
                t = normalize_adr_ref(target)
                if t not in adrs:
                    errors.append(f"{path.name}: {kind} references missing ADR {target!r}")
                else:
                    graph[aid].append((t, kind))
        amended_by = fm.get("amended_by", [])
        if isinstance(amended_by, str):
            amended_by = [amended_by]
        for target in amended_by:
            t = normalize_adr_ref(target)
            if t not in adrs:
                errors.append(f"{path.name}: amended_by references missing ADR {target!r}")
            elif ("amends", aid) not in [(k, n) for n, edges_list in graph.items() for _, k in edges_list]:
                # check target actually amends this ADR
                target_fm = parse_frontmatter(adrs[t].read_text(encoding="utf-8"))
                amends_list = target_fm.get("amends", [])
                if isinstance(amends_list, str):
                    amends_list = [amends_list]
                if aid not in [normalize_adr_ref(x) for x in amends_list]:
                    errors.append(
                        f"{path.name}: amended_by {target!r} but {t} does not list amends: [\"{aid}\"]"
                    )
        for m in WIKILINK_RE.finditer(text):
            target = m.group(1)
            if target not in adrs:
                errors.append(f"{path.name}: wikilink [[{target}]] has no file")
    return graph, errors


def find_cycles(graph: dict[str, list[tuple[str, str]]]) -> list[str]:
    cycles: list[str] = []
    visited: set[str] = set()
    stack: set[str] = set()

    def dfs(node: str, path: list[str]) -> None:
        if node in stack:
            cycles.append(" -> ".join(path + [node]))
            return
        if node in visited:
            return
        visited.add(node)
        stack.add(node)
        for nxt, kind in graph.get(node, []):
            dfs(nxt, path + [f"{node}({kind})"])
        stack.remove(node)

    for n in graph:
        dfs(n, [])
    return cycles


def main() -> int:
    adrs = load_adrs()
    if not adrs:
        print("no ADRs found", file=sys.stderr)
        return 1
    graph, errors = edges(adrs)
    errors.extend(find_cycles(graph))

    if errors:
        print("ADR graph check FAILED:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        return 1
    print(f"ADR graph check OK ({len(adrs)} files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
