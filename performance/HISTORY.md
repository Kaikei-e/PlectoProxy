# Measurement history — older generations

[`README.md`](README.md)'s TL;DR keeps only the two newest measurement generations (the current
numbers plus the delta they were judged against); everything older moves here verbatim, newest
first. Method changes are recorded in [`bench/methodology.md`](../bench/methodology.md); per-pass
CSVs are regenerable working data (`performance/data/`, untracked).

## 2026-09-28 (allocator arena default transition: 4 → 32)

Transition of the glibc malloc arena default cap from 4 to 32 based on the 2026-09-27 isolated measurements (T3; `performance/data/runs/arena-isolated-{memory,throughput}-20260927`, see [`bench/methodology.md`](../bench/methodology.md#2026-09-27--アリーナ既定値を判断するための分離計測t3) and [ADR 000118](../docs/ADR/000118.md)):
- **Rationale & trade-offs**: 2-round isolated sweeps on the reference host demonstrated that N=32 limited the throughput gap relative to glibc's own default to 2.3% on auth and 1.9% on rate limiting, while preserving meaningful body-buffering RSS containment compared to glibc default (proxy peak RSS -22.4%, post-load 15s RSS -23.0%; conversely, glibc default peak RSS is ~29% higher than N=32). Compared to N=32, N=4 throughput was ~24.2% lower on auth and ~15.1% lower on rate limiting (and ~26.0% lower on auth, ~16.7% lower on rate limiting relative to glibc default). In exchange, moving from cap 4 to 32 increases body peak RSS from ~167 to ~260 MiB (+55%) and post-load settled RSS from ~140 to ~198 MiB (+42%).
- **Heuristic scope**: 32 is a host-calibrated heuristic balancing throughput against RSS retention, not an assertion of universal optimality or zero contention on all hosts and topologies. Intermediate caps (16, 24) remain viable tuning starting points for memory-constrained environments and are not dismissed.
- **Configuration & compatibility**:
  - In production, `PLECTO_MALLOC_ARENA_MAX=4` preserves previous default cap 4 (ADR 000038 / ADR 000118) for footprint comparison (the runner removes startup `MALLOC_ARENA_MAX` injection, so exact reproduction of historical runs is not asserted). In benchmarks, `BENCH_MALLOC_ARENA_MAX=4` reproduces this configuration as `run-perf.sh` scrubs inherited `PLECTO_*` variables.
  - Values `4`–`16` (or `24`) serve as an operational tuning range and starting point when memory footprint is prioritized, not a universal recommendation across all workloads.
  - `0` leaves glibc's external configuration untouched (system environment variables, tunables, or built-in defaults; delegates to glibc and is not an assertion of "unlimited").
  - Negative values remain backwards-compatible no-ops; unset or invalid strings default to 32.
  - Active on Linux GNU targets only; compile-time no-op on other platforms.
- **Harness & runner alignment**:
  - All four standard proxy harnesses (`bench-server`, `load-balancing`, `tls-http`, `swap-bench`) invoke `cap_malloc_arenas()` before spawning the Tokio runtime.
  - `run-perf.sh` leaves `PLECTO_MALLOC_ARENA_MAX` unset when `BENCH_MALLOC_ARENA_MAX` is omitted (testing Rust default 32 directly). Explicit values (4/16/24/32/0) inject `PLECTO_MALLOC_ARENA_MAX` without setting startup `MALLOC_ARENA_MAX`. Inherited allocator env (`MALLOC_*`, `glibc.malloc.*`) is scrubbed and `LD_PRELOAD` is refused.
  - `arena_sweep.py` supports `--arenas default,32` for measured comparison between shipped default and explicit 32.
- **T3 validation (default vs explicit32, 2-round averages)**:
  - Both conditions completed with `complete` files and zero HTTP failures.
  - `/trusted` auth: default 96,281.3 / explicit32 94,982.7 rps.
  - `/ratelimit`: default 82,305.3 / explicit32 81,917.9 rps.
  - `/body`: default 2,090.1 / explicit32 2,098.5 rps.
  - Proxy peak RSS (1MiB x 50 VUs): default 256.51 / explicit32 256.00 MiB.
  - Post-load 15s settled RSS: default 185.93 / explicit32 199.21 MiB.
  - (Baseline is not averaged across mixed oha/k6 models). These 2-round figures provide a measured comparison; statistical equivalence was not established.
- **T1 gate measurements & recalibration**:
  - Measured two default32 T1 runs on the same release binary (`performance/data/runs/gate-default-run1-20260928`, `gate-default-run2-20260928`): run1 `dispatch_floor_us` 4.5397 ± 0.0540 µs (pass) / `apikey_cost_us` 1.4053 ± 0.0716 µs (fail under old band 0.3..1.2); run2 `dispatch_floor_us` 4.5860 ± 0.0600 µs (pass) / `apikey_cost_us` 1.4210 ± 0.0451 µs (fail). All other 9 invariants passed on both runs (micro layer skipped without saved criterion baseline main).
  - Explicit cap 0 control on the same binary/runner (`performance/data/runs/gate-glibc-control-20260928`): `apikey_cost_us` landed at 1.1052 ± 0.1744 µs, straddling old hi 1.2 to produce **INCONCLUSIVE (exit code 2)**, not PASS (other 9 invariants passed).
  - These observed deltas reflect the specific benchmark setup and host conditions; a single cap 0 control and host variations do not prove causal attribution solely to the arena cap. Recalibration is an operational adjustment on the reference host (not a statistical confidence guarantee): max center 1.421 + 3x max half-range 0.0716 ≈ 1.636 rounded up to 1.7 (lo 0.3 and all other bands unchanged).
- **Holdout validation completed**: An independent `explicit32` T1 gate holdout run (`performance/data/runs/gate-explicit32-validation-20260928`) completed with gate exit code 0 (10 pass, 0 fail, 0 inconclusive). Measured invariants: `dispatch_floor_us` 4.5796 ± 0.1892 µs (pass); `apikey_cost_us` 1.2994 ± 0.2035 µs (pass under recalibrated band 0.3..1.7); `ratelimit_tax_us` 4.3764 ± 0.0745 µs (pass); `pooled_tail_p50_ms` 0.1262 ms (pass); `apikey_tail_p50_ms` 0.0146 ms (pass); `respctx_tail_p50_ms` -0.0288 ms (pass); `enforce_allowed_ratio` 0.9166 (pass); `rr_spread_req` 0 (pass); `ejection_transition_s` 1 s (pass); `ejection_stray_failed` 0 (pass). Informational: `pooled_tail_p99_ms` 0.1517 ms, `respctx_tail_p99_ms` -0.0009 ms, `enforce_limited_frac` 0.7800; criterion micro informational layer skipped (saved `main` baseline absent).

## 2026-09-26 (methodology update: pinning, arenas, data retention, three-valued gate)

Methodology update establishing a new comparable series for upcoming runs:
- **Core isolation by pinning**: The proxy is pinned at exec time so every thread inherits the mask and the per-thread affinity is verified after start. The default proxy/generator split partitions by whole physical cores (`bench/perf/cpu_split.py`). The dev host is hybrid (P-cores 0-15 as adjacent SMT pairs, E-cores 16-23), so the default GEN set 12-23 mixes P and E cores (informational).
- **Allocator arena cap**: Every proxy launch runs with the shipped arena cap of 4 (`bench-server` now calls `cap_malloc_arenas` like the `plecto` binary).
- **Raw data retention & host fingerprint**: Raw per-round JSON outputs, proxy logs, CSV copies, and a host fingerprint (`host.txt`) are preserved under `performance/data/runs/<run-id>/`, and `just perf-archive` packages them into a tarball for GitHub Release attachments.
- **Three-valued gate verdict**: The T1 perf gate evaluates conservative uncertainty intervals (`[value - ci_half, value + ci_half]`) producing three-valued verdicts: `pass` (exit code 0), `fail` (exit code 1; dominates inconclusive), or `inconclusive` (exit code 2; interval straddles band edge, requiring a re-run).

Numbers measured before this change (the 2026-09-25 snapshot and earlier) were taken with partial pinning and uncapped arenas outside the body phase, so footprint KB/conn and gate spreads are not directly comparable. The next T1 runs start a new comparable series.

## 2026-07-20 (v0.5.1/v0.5.2 patch confirmation)

A full refresh: T1 `gate` (**PASS**, every invariant in band), a full `bash bench/perf/run-perf.sh
all` (T2), and `v03` (T3). Measured at commit `c635ed3` (tag **v0.5.1**); tag **v0.5.2** landed
on top moments later as an unintended early release — version strings and three reference-filter
patch bumps only (`filter-cors` / `filter-apikey` / `filter-extauthz` 0.1.1 → 0.1.2), no
`plecto-server` / `plecto-control` / `plecto-host` source changed, so every figure stands for
v0.5.2 as shipped too. The entire load run executed inside an unprivileged network namespace
(`unshare -rn`, `ip link set lo up`, no default route) rather than relying only on the runbook's
own `REQUIRE_OFFLINE=1` self-check — a kernel-enforced guarantee that nothing left the host during
the run, verified beforehand (`curl http://example.com` fails at DNS resolution inside the
namespace, before any route is even consulted). **New finding this pass** — ADR 000092's
per-source-IP connection cap (**256** concurrent connections/IP, landed 2026-07-15, after the
prior 07-11 snapshot) now intersects several k6 open-loop scenarios whose `preAllocatedVUs` pool
exceeds 256, because the generator and Plecto Proxy share one loopback source IP on this harness.
Confirmed two ways: the closed-loop **sweep** fails cleanly above the threshold (0 % at VU ≤ 200,
**28 % / 47 %** at VU 400/800 — reproduced identically with and without the netns sandbox, ruling
the isolation method out as the cause), and the **rate-limit enforcement / fairness (hot key)**
scenarios silently drop **43–49 %** of offered load from their own accepted/limited accounting (a
refused connection returns no HTTP status, so k6's `status === 200 | 429` branches never see it).
Affected numbers are flagged inline; every oha-driven section (ceiling, WASM ladder, TLS,
footprint — all `-c 50`), the low-VU k6 scenarios (body, rate-limit overhead — `VUS=50`), and every
`plecto-loadgen` scenario (open-loop, round-robin, ejection, swap, WebSocket — all ≤ 64 workers)
stay well under the cap and are clean, comparable figures. *(The harness half of that finding is
fixed in the 08-15 pass; the numbers it flagged have been re-measured.)*

## 2026-07-11 (v0.3.0 feature costs) — targeted `v03` pass

Targeted `bash bench/perf/run-perf.sh v03` (not a full `all` refresh): fills the previously-unmeasured
ADR 000073 response-context / `replace` rungs and the ADR 000074/075 compression opt-in row.
Method: same adjacent-delta ladder + oha fixed-rate CO-safe tails as the WASM plane; see
[`bench/methodology.md`](../bench/methodology.md) § v0.3.0 response / compression. Track
**µs/req** (and fixed-rate p50), not %-of-baseline.

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
