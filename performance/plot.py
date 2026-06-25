#!/usr/bin/env python3
"""Render Plecto's performance charts (WebP) from the measured CSVs in ``data/``.

Outputs three figures into ``performance/img/``:

  * throughput_by_scenario.webp  — sustained requests/s per scenario (bar)
  * latency_by_scenario.webp     — request-duration percentiles per scenario (grouped bar)
  * ejection_timeline.webp       — per-upstream traffic + failed/s over the resilience run

The CSVs were produced by driving the bundled ``examples/load-balancing`` with k6; the
timeline CSV is a 1-second aggregation of k6 metrics (per-instance counts come from the
``X-Instance`` response header tagged onto a custom counter). See ``data/`` and the report.

Usage:
    python3 plot.py                # reads ./data, writes ./img
Requires: matplotlib (pulls numpy + Pillow; Pillow provides WebP encoding).
"""
from __future__ import annotations

import csv
import pathlib

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = pathlib.Path(__file__).resolve().parent
DATA = HERE / "data"
IMG = HERE / "img"
IMG.mkdir(exist_ok=True)

# A restrained, readable palette (instances a/b/c, plus a failure red).
C_A, C_B, C_C = "#4C9F70", "#E1A53D", "#3D7AB5"
C_FAIL = "#C0392B"
DPI = 150


def _read(path: pathlib.Path) -> list[dict[str, str]]:
    with path.open() as fh:
        return list(csv.DictReader(fh))


def _save(fig, name: str) -> None:
    out = IMG / name
    fig.savefig(out, format="webp", dpi=DPI, bbox_inches="tight")
    plt.close(fig)
    print(f"wrote {out.relative_to(HERE)}")


def throughput() -> None:
    rows = _read(DATA / "throughput_by_scenario.csv")
    labels = [r["scenario"] for r in rows]
    vals = [int(r["rps"]) for r in rows]
    fig, ax = plt.subplots(figsize=(7.2, 4.0))
    bars = ax.bar(labels, vals, color=[C_C, C_A, C_B], width=0.6)
    ax.bar_label(bars, fmt="%d", padding=3, fontsize=10)
    ax.set_ylabel("sustained requests / second")
    ax.set_title("Throughput by scenario (single host, loopback — relative baseline)")
    ax.set_ylim(0, max(vals) * 1.15)
    ax.margins(x=0.08)
    ax.grid(axis="y", alpha=0.3)
    ax.tick_params(axis="x", labelsize=9)
    _save(fig, "throughput_by_scenario.webp")


def latency() -> None:
    rows = _read(DATA / "latency_by_scenario.csv")
    pct = ["p50", "p90", "p95", "p99"]
    x = range(len(pct))
    fig, ax = plt.subplots(figsize=(7.2, 4.0))
    n = len(rows)
    width = 0.8 / n
    colors = [C_C, C_B]
    for i, r in enumerate(rows):
        vals = [float(r[p]) for p in pct]
        off = (i - (n - 1) / 2) * width
        bars = ax.bar([xi + off for xi in x], vals, width=width,
                      label=r["scenario"], color=colors[i % len(colors)])
        ax.bar_label(bars, fmt="%.1f", padding=2, fontsize=8)
    ax.set_xticks(list(x))
    ax.set_xticklabels(pct)
    ax.set_ylabel("request duration (ms)")
    ax.set_title("Latency percentiles by scenario")
    ax.legend(fontsize=9)
    ax.grid(axis="y", alpha=0.3)
    _save(fig, "latency_by_scenario.webp")


def ejection() -> None:
    rows = _read(DATA / "ejection_timeline.csv")
    t = [int(r["t"]) for r in rows]
    a = [int(r["a"]) for r in rows]
    b = [int(r["b"]) for r in rows]
    c = [int(r["c"]) for r in rows]
    failed = [int(r["failed"]) for r in rows]

    fig, ax = plt.subplots(figsize=(9.5, 4.6))
    ax.stackplot(t, a, b, c, labels=["instance a", "instance b", "instance c"],
                 colors=[C_A, C_B, C_C], alpha=0.9)
    ax.plot(t, failed, color=C_FAIL, lw=2.2, label="failed / s (HTTP 503)")

    events = [(12, "eject b"), (24, "rejoin b"), (36, "eject all"), (46, "restore all")]
    top = max(max(a[i] + b[i] + c[i], failed[i]) for i in range(len(t)))
    for xt, lbl in events:
        ax.axvline(xt, color="#444", ls="--", lw=1, alpha=0.6)
        ax.text(xt + 0.4, top * 0.97, lbl, rotation=90, va="top", ha="left",
                fontsize=8, color="#333")

    ax.set_xlim(min(t), max(t))
    ax.set_ylim(0, top * 1.05)
    ax.set_xlabel("time (s)")
    ax.set_ylabel("requests / second")
    ax.set_title("Load balancing under fault injection — per-upstream traffic & failures")
    ax.legend(loc="upper right", fontsize=9, framealpha=0.9)
    ax.grid(axis="y", alpha=0.25)
    _save(fig, "ejection_timeline.webp")


def wasm_throughput() -> None:
    rows = _read(DATA / "wasm_overhead.csv")
    labels = [r["route"] for r in rows]
    vals = [int(round(float(r["rps"]))) for r in rows]
    fig, ax = plt.subplots(figsize=(7.2, 4.0))
    bars = ax.bar(labels, vals, color=[C_C, C_A, C_B][: len(rows)], width=0.6)
    ax.bar_label(bars, fmt="%d", padding=3, fontsize=10)
    ax.set_ylabel("requests / second")
    ax.set_title("Throughput at fixed concurrency (50 VUs, 0 ms backend)")
    ax.set_ylim(0, max(vals) * 1.15)
    ax.grid(axis="y", alpha=0.3)
    ax.tick_params(axis="x", labelsize=9)
    _save(fig, "wasm_throughput.webp")


def wasm_latency() -> None:
    rows = _read(DATA / "wasm_overhead.csv")
    pct = ["p50", "p90", "p95", "p99"]
    x = range(len(pct))
    fig, ax = plt.subplots(figsize=(7.6, 4.2))
    n = len(rows)
    width = 0.8 / n
    palette = [C_C, C_A, C_B]
    for i, r in enumerate(rows):
        vals = [float(r[p]) for p in pct]
        off = (i - (n - 1) / 2) * width
        bars = ax.bar([xi + off for xi in x], vals, width=width,
                      label=r["route"], color=palette[i % len(palette)])
        ax.bar_label(bars, fmt="%.2f", padding=2, fontsize=7)
    ax.set_xticks(list(x))
    ax.set_xticklabels(pct)
    ax.set_yscale("log")
    ax.set_ylabel("request duration (ms, log scale)")
    ax.set_title("Per-request latency: native vs pooled vs on-demand filter (0 ms backend)")
    ax.legend(fontsize=9)
    ax.grid(axis="y", alpha=0.3, which="both")
    _save(fig, "wasm_latency.webp")


def wasm_shortcircuit() -> None:
    rows = {r["outcome"]: r for r in _read(DATA / "wasm_mixed.csv")}
    pct = ["p50", "p95", "p99"]
    order = [("accept", C_A), ("reject", C_FAIL)]
    x = range(len(pct))
    fig, ax = plt.subplots(figsize=(7.2, 4.2))
    width = 0.38
    for i, (outcome, color) in enumerate(order):
        r = rows.get(outcome)
        if not r:
            continue
        vals = [float(r[p]) for p in pct]
        off = (i - 0.5) * width
        n = r.get("count", "")
        bars = ax.bar([xi + off for xi in x], vals, width=width,
                      label=f"{outcome} (n={n})", color=color)
        ax.bar_label(bars, fmt="%.2f", padding=2, fontsize=8)
    ax.set_xticks(list(x))
    ax.set_xticklabels(pct)
    ax.set_ylabel("request duration (ms)")
    ax.set_title("Accept vs reject under realistic traffic (15 ms backend, 90/10 mix)")
    ax.legend(fontsize=9)
    ax.grid(axis="y", alpha=0.3)
    _save(fig, "wasm_shortcircuit.webp")


def _maybe(fn) -> None:
    try:
        fn()
    except FileNotFoundError as e:
        print(f"skip {fn.__name__}: missing {e.filename}")


if __name__ == "__main__":
    for fn in (throughput, latency, ejection,
               wasm_throughput, wasm_latency, wasm_shortcircuit):
        _maybe(fn)
    print("done")
