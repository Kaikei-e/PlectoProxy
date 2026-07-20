# Measurement history — older generations

[`README.md`](README.md)'s TL;DR keeps only the two newest measurement generations (the current
numbers plus the delta they were judged against); everything older moves here verbatim, newest
first. Method changes are recorded in [`bench/methodology.md`](../bench/methodology.md); per-pass
CSVs are regenerable working data (`performance/data/`, untracked).

## 2026-07-11 (release confirmation) — v0.3.0 release gate

A second full `bash bench/perf/run-perf.sh all` refresh (plus a fresh `cargo bench` criterion
pass) ahead of the **v0.3.0** contract release, after landing native response compression
(ADR 000074 / ADR 000075) and the `plecto:filter@0.3.0` response-context / `replace` contract
(ADR 000073). Compression is opt-in (`[route.compression]`) and off by default, so it touched none
of the routes the `all` suite measures — this pass confirmed the regression invariants with the
features *present but unused* (pooled WASM floor **+0.11 ms p50 / +0.26 ms p99**, apikey **≈0.86 µs
/ −9.8 %**, rate-limit **1,033/s at 79.3 % shed**, round-robin exact).

## 2026-07-11 (earlier same day) — industry-methodology pass

First full refresh after the industry-methodology pass
([`bench/methodology.md`](../bench/methodology.md)): authoritative open-loop is now
**`plecto-loadgen openloop`** (wrk2 schedule-latency), so the auto 70 %-of-peak target achieved
**0 dropped** — the earlier k6-pinned `OPENLOOP_RATE=60000` workaround is no longer needed for
the published figure. Ceiling CSV carries **RR/CRR** KPI labels.

## 2026-07-09 — post feature batch

Re-measured after KvQuota striping, PROXY protocol v2, body-retry, H3 GOAWAY, outbound-TCP,
two-tier rate-limit, shared ticket keys, and fat-guest (unmeasured in the default build).
Open-loop still needed a pinned 60k/s under k6.

## 2026-07-05 — TLS resumption + hot-path fixes

Re-measured after ADR 000052 (stateless TLS 1.3 session resumption) plus three hot-path fixes
landed alongside it: a control-plane outlier-ejection race fix that also cut a per-request
**route-lookup** allocation and the chain's per-filter HashMap re-resolution (the LB *pick* path
is untouched), a host quota-accounting race fix + new untrusted-instance breaker, and fail-closed
handling for a buffer-permit error. This run filled the TLS section's previously-pending
resumption gap with a clean `plecto-loadgen tls --mode full|resumed` measurement, which confirms
oha's `handshake/req` row was already silently resumption-contaminated.

## 2026-07-04 — aws-lc-rs baseline + harness consolidation

Re-measured post ADR 000050/000051 (TLS crypto provider moved to **aws-lc-rs**, a new baseline,
not a `ring` delta); `wasm-bench` / `edge-bench` consolidated into one `bench-server` harness so
the plain-HTTP/1.1 ceiling is measured once and every other section reads it; added
endpoint-set-swap (ADR 000044) and WebSocket (ADR 000048) scenarios.

## 2026-07-02 — plecto-loadgen rebuild

Harness rebuilt onto `plecto-loadgen` (Rust), warm-up excluded from every window. Every figure
was refreshed; the **µs/req deltas are what to track across snapshots**, not raw throughput.
