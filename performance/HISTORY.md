# Measurement history — older generations

[`README.md`](README.md)'s TL;DR keeps only the two newest measurement generations (the current
numbers plus the delta they were judged against); everything older moves here verbatim, newest
first. Method changes are recorded in [`bench/methodology.md`](../bench/methodology.md); per-pass
CSVs are regenerable working data (`performance/data/`, untracked).

## 2026-10-31 (`v0.15.5` / `HEAD` `983cbed`: gungraun instruction-count reduction on Rust 1.99.1 & contended-host T2/T1 verification)

Re-measured at `HEAD` `983cbed` (one commit past **`v0.15.5`**, `6a8959a`; Rust `1.99.1`, `zstd 0.14.3`, unused `wasmtime` `cache` feature removed, `Atomic::try_update` migration; `PROXY_CPUS=0-11`, `GEN_CPUS=12-23`):
- **gungraun instruction-count micro-benchmarks (`crates/host/benches/wasm_inst.rs`, `crates/control/benches/fastpath_inst.rs`, frequency- and neighbour-invariant)**:
  - `on_request/trusted_pooled`: **28,290** instructions (down from `30,448` on `v0.15.2` / `rustc 1.98.0`, **−7.1 %** / `−2,158 Ir`), L1 hits `42,318` (was `45,406`), LL hits `1,357` (was `1,416`), RAM hits `19` (was `20`), total read/write `43,694` (was `46,842`), est. cycles **49,768** (was `53,186`, **−6.4 %**).
  - `on_request/untrusted_fresh`: **101,983** instructions (down from `112,951`, **−9.7 %** / `−10,968 Ir`; **`3.60×`** `trusted_pooled`), L1 hits `148,700` (was `164,118`), LL hits `6,090` (was `6,375`), RAM hits `41` (was `42`), total read/write `154,831` (was `170,535`), est. cycles **180,585** (was `197,463`, **−8.5 %**).
  - `lb_pick/round_robin`: **223** instructions (was `225`), L1 `310`, LL `1`, RAM `2`, total r/w `313`, est. cycles `385` (was `417`).
  - `lb_pick/least_request_p2c`: **335** instructions (was `340`), L1 `453`, LL `1`, RAM `2`, total r/w `456`, est. cycles `528` (was `569`).
  - `lb_pick_maglev/source_ip`: **278** instructions (was `282`), L1 `378`, LL `0`, RAM `3`, total r/w `381`, est. cycles `483` (was `489`).
  - [`bench/perf/ipc_check.py`](../bench/perf/ipc_check.py) passed all 5 pairs (`ok` across `on_request/trusted_pooled` IPC `1.34`, `on_request/untrusted_fresh` IPC `0.65`, `lb_pick/round_robin` IPC `1.37`, `lb_pick/least_request_p2c` IPC `1.33`, `lb_pick/maglev` IPC `2.00`, limit `5.0`).
- **Criterion wall-clock micro-benchmarks (`fastpath.rs`, `wasm.rs` at `983cbed`, pinned `0-11`, under external host load `load average ≈ 28`)**:
  - `fastpath.rs`: `lb_pick/round_robin` `33.2 → 39.4 → 37.1 ns` (`3 → 8 → 32`), `least_request_p2c` `44.2 → 51.6 → 83.0 ns`, `maglev` `25.7 → 29.1 → 24.3 ns`; `pick_under_swap_churn` `133.3 → 51.1 → 54.8 ns`; `find_route` `57.1 → 115.7 → 254.2 ns` (`1 → 16 → 64`); `normalize_path` `53.9 ns` (`plain`), `36.4 ns` (`with_query`), `145.1 ns` (`dot_segments`).
  - `wasm.rs` (`--features outbound-http,outbound-tcp,fat-guest,streaming-body`): `on_request/trusted_pooled` `3.88 µs` `[3.874 .. 3.883 µs]`, `on_request/untrusted_fresh` `29.60 µs` `[28.63 .. 30.65 µs]`, `filter_load/trusted_cold` `15.22 ms` `[14.82 .. 15.63 ms]` (elevated vs idle `2.09 µs` / `28.53 µs` / `7.89 ms` due to external CPU contention on the host).
- **Contended-host T2 `all` (`20261030T164100Z-983cbed`, all 14 phases `ok`) & T1 `gate` (`20261030T171451Z-983cbed-dirty`, `INCONCLUSIVE` exit 2: `7 pass, 0 fail, 3 inconclusive`)**:
  - Unpinned external containers/processes on the reference host consumed ~1,500–1,700 % CPU across all 24 logical CPUs (`load average ≈ 22–33`) throughout the macro runs.
  - All functional and load-invariant checks held identically to idle runs:
    - `rr_spread_req`: **0** (`40,000 / 40,000 / 40,000` across `120,000` requests in both T2 and T1).
    - `ejection`: `300,000` responses over 75 s in T2 (`60,120` failed = `20.04 %` during total ejection) and `160,000` over 40 s in T1 (`24,040` failed = `15.03 %`), `ejection_transition_s` = **1.0 s**, `ejection_stray_failed` = **0**.
    - `swap`: `240,000` responses over 60 s, **0 failed (0.00 %)**, 1 s transition at `t = 15 s`.
    - `ratelimit` enforcement & fairness: T1 `enforce_allowed_ratio` = **0.9999** (`11,000` allowed = `1,100.0/s`, `39,003` 429s = `78.0 %` shed, `0` status-less over 10 s); T2 `enforce` **1,033.3/s** allowed (`31,000` accepted, `119,000` limited = `79.3 %` shed, `0` status-less over 30 s); T2 `fairness` hot key `1,033.3/s` allowed (`74.17 %` shed, `0` status-less), light key **500.0/s allowed (`0 %` shed, `0` status-less)**.
    - `wasm_mixed` short-circuit: `108,084` accepted (`1,801.4/s`, `90.05 %`, p50 `16.344 ms`, p95 `17.365 ms`, p99 `18.732 ms`) / `11,945` rejected (`199.1/s`, `9.95 %`, p50 `0.105 ms`, p95 `1.070 ms`, p99 `1.512 ms`), **0 status-less**.
    - `footprint` & `ws`: `footprint.txt` idle `45,308 kB` → `51,616 kB` over 250 conns (**`25,838 B/conn`**); `ws_footprint.csv` fresh-process idle `41,144 kB` → `46,364 kB` over 250 held tunnels (**`21,381 B/tunnel`**); `ws_handshake` `10,000 / 10,000` (`500.0/s`, `0` failed); `h3` `status=200 http_version=3`; `body_rss.csv` (1 MB × 50 VUs) `148,620 / 286,640 / 134,000 kB` (`/baseline` / `/body` / `/body-headeronly`).
    - T1 fixed-rate 2,000 rps tails: `pooled_tail_p50_ms` **+0.0430 ms** (`pass`, p99 `+0.3082 ms`), `apikey_tail_p50_ms` **−0.0083 ms** (`pass`), `respctx_tail_p50_ms` **−0.0178 ms** (`pass`, p99 `−0.0216 ms`).
  - Contended throughput / latency figures (archived in `performance/data/runs/20261030T164100Z-983cbed/csv/` and `20261030T171451Z-983cbed-dirty/csv/`, not used to overwrite [`README.md`](README.md)'s idle-host macro tables and charts):
    - T2 `ceiling`: keep-alive `90,801.9 rps` (p50 `0.45 ms`, p99 `1.99 ms`), cold CRR `46,376.2 rps` (`51.1 %` of keep-alive, p50 `0.94 ms`, p99 `3.21 ms`).
    - T2 `wasm` ladder: `/baseline` `90,801.9` → `/noop-pooled` `72,706.1` → `/trusted` `65,318.9` → `/noop-fresh` `8,037.4` / `/ondemand` `8,329.2 rps`; fixed-rate `4,822 rps` tails p50 `0.144 / 0.157 / 0.196 ms` (`baseline` / `noop-pooled` / `trusted`).
    - T2 `tls`: keep-alive `77,235.5 rps`, handshake/req `11,046.3 rps`, h2 `59,702.0 rps`; `ws_echo`: 1 KB `56,209.2 msg/s` (`57.56 MB/s`), 64 KB `23,648.5 msg/s` (`1,549.82 MB/s`); `sweep`: VU 50/100/200/400/800 = `47,992 / 55,816 / 50,062 / 48,527 (27.8 % failed) / 43,230 rps (50.6 % failed)`; `openloop` at `39,070/s`: `39,070/s` achieved, `0` dropped, p50 `0.556 ms`, p99 `48.671 ms`; `body`: 1 KB `51,628 / 48,185 / 42,794 rps`, 100 KB `14,260 / 9,569 / 14,655 rps`, 1 MB `3,513 / 1,254 / 3,749 rps`; `mix` at 20k/s: `read-only` dropped `4,379`, `mix` dropped `9,996` (`0` status-less).
    - T1 `gate` (`20261030T171451Z-983cbed-dirty`): `dispatch_floor_us` `2.7291 ± 1.6192 µs` (`inconclusive`, n = 8), `apikey_cost_us` `4.3395 ± 3.5856 µs` (`inconclusive`, n = 8), `ratelimit_tax_us` `4.0742 ± 5.2226 µs` (`inconclusive`, n = 8); info `baseline_cpu_us_per_req` `46.5172 ± 4.5292 µs`, `ratelimit_cpu_us_per_req` `22.8146 ± 2.1891 µs` (n = 4, with rate-hold warnings on 7 of 8 arms: `achieved 19,199–19,937 rps, dropped 185–2,382`).

## 2026-10-25 (ADR 000122 re-measurement: Upgrade per-IP fix, open-loop pacing, 8-round paired gate)

Targeted re-runs on the same reference host after [ADR 000122](../docs/ADR/000122.md) (`9c0b84b` + working tree; `PROXY_CPUS=0-11`, `GEN_CPUS=12-23`):
- **`ws`** (`20261025T051805Z`): fresh `bench-server`, tunnel footprint measured first — idle RSS 42,080 kB → 47,292 kB with 250 held tunnels (`ws hold: 250 tunnels`), **21,348 B/tunnel** (supersedes the 2026-10-24 `8,778 B/tunnel`, which was taken on a post-burst heap while upgraded tunnels escaped the per-IP cap). Handshake 10,000/10,000 at 500/s; echo 219,566 msg/s (1 KB, p99 1.06 ms) / 60,133 msg/s (64 KB, 3,941 MB/s, p99 1.65 ms).
- **`openloop`** (`20261025T051926Z`): 103,026/s achieved, 0 dropped — **p50 0.050 ms** (was 0.84 ms), p95 0.508, p99 2.091, p99.9 4.567 ms; mean 0.132 ms; 89.6 % of samples in the first 100 µs bin. The 2026-10-24 p50 was generator timer lag.
- **`gate`** (`20261025T052103Z`, first 8-round paired run, 95 % t interval): `INCONCLUSIVE (exit 2)` — `dispatch_floor_us` 1.2422 ± 0.1234 (pass), `apikey_cost_us` 1.7365 ± 1.1332 (**inconclusive**; rounds 3–4 disturbed: `/trusted` 124.8k / 80.6k rps vs ~140–146k in the other rounds), `ratelimit_tax_us` 1.0768 ± 0.1538 (pass), tails `pooled` +0.0131 / `apikey` +0.0128 / `respctx` +0.0009 ms (pass), `enforce_allowed_ratio` 0.9999, `rr_spread_req` 0, `ejection_transition_s` 1, `ejection_stray_failed` 0; info: `baseline_cpu_us_per_req` 32.66 ± 1.13, `ratelimit_cpu_us_per_req` 14.29 ± 1.54 (n = 4). Re-run `20261025T053124Z` (same `build_id.txt`) — its `GATE_POOL_FROM` path was mistyped (`9c0b8b`), so the in-run verdict refused to pool (exit 64); the path was corrected in its `pooled_from.txt` and the rounds re-judged offline with `gate_verdict.py`. That run alone: `dispatch_floor_us` 1.2865 ± 0.1032, `apikey_cost_us` 1.0568 ± 0.1377 (pass), `ratelimit_tax_us` 1.0183 ± 0.2610 (inconclusive). **Pooled (n = 16): `INCONCLUSIVE` — `dispatch_floor_us` 1.2644 ± 0.0711 (pass), `apikey_cost_us` 1.3966 ± 0.5309 (inconclusive), `ratelimit_tax_us` 1.0475 ± 0.1328 (pass)**, all other invariants pass; info `ratelimit_cpu_us_per_req` 15.247 ± 1.586 over `baseline_cpu_us_per_req` 32.566 ± 0.523 (n = 8). `run-perf.sh` now validates `GATE_POOL_FROM` (existence, round data, build id) before measuring.
- **`gate`** third run `20261025T054222Z`, pooling both earlier runs (n = 24): **`PASS` (every invariant in band)** — `dispatch_floor_us` 1.2434 ± 0.0508, `apikey_cost_us` 1.2996 ± 0.3456, `ratelimit_tax_us` 1.0638 ± 0.1378, tails `pooled` +0.0097 / `apikey` +0.0136 / `respctx` +0.0025 ms, `enforce_allowed_ratio` 0.9999, `rr_spread_req` 0, `ejection_transition_s` 1, `ejection_stray_failed` 0; info `ratelimit_cpu_us_per_req` 15.270 ± 0.972 over `baseline_cpu_us_per_req` 32.345 ± 0.387 (n = 12). That run alone was inconclusive on `ratelimit_tax_us` (1.0964 ± 0.3878) under the old `lo = 0.9` edge.
- **`gate`** independent holdout run `20261025T061734Z` (n = 8, standalone after moving `ratelimit_tax_us` `lo` from `0.9` to `0.5` and adding an 11 s pre-window settle before `ratelimit-cpu` so idle blocking-pool threads finish retiring before `schedstat` snapshots): **`PASS` (`10 pass, 0 fail, 0 inconclusive`)** — `dispatch_floor_us` 1.1652 ± 0.1237, `apikey_cost_us` 1.0562 ± 0.0918, `ratelimit_tax_us` 1.4863 ± 0.5407, tails `pooled` +0.0247 / `apikey` +0.0017 / `respctx` −0.0081 ms, `enforce_allowed_ratio` 1.0000, `rr_spread_req` 0, `ejection_transition_s` 1, `ejection_stray_failed` 0; info `ratelimit_cpu_us_per_req` 18.3423 ± 0.7996 over `baseline_cpu_us_per_req` 34.5189 ± 0.6782 (n = 4, zero tick-clock fallbacks across all 8 CPU arms).

## 2026-10-24 (v0.15.2 full T2 snapshot, T1 gate & Criterion confirmation — wasmtime 50.0.0 + WASI 0.3)

Full T2 release report (`bash bench/perf/run-perf.sh all`, `RUN_DIR: 20261024T035518Z-1061a79`), T1 regression gate (`bash bench/perf/run-perf.sh gate`), and Criterion WASM micro-benchmarks (`crates/host/benches/wasm.rs`) measured at tag **`v0.15.2`** (`1061a79`, plus pre-tag verification at `3b8ca45`) after landing the lockstep Wasmtime 50.0.0 bump ([ADR 000120](../docs/ADR/000120.md)), full WASI 0.3 (`@0.3.0`) capability host support alongside WASI 0.2 (`@0.2.x`) across `fat-guest`, `outbound-http`, `outbound-tcp`, and `streaming-body` ([ADR 000121](../docs/ADR/000121.md), amending ADR 000036, ADR 000060, and ADR 000063), inline header hooks ([ADR 000119](../docs/ADR/000119.md)), and the default allocator arena cap transition (`4 → 32`, [ADR 000118](../docs/ADR/000118.md)):
- **Hot-path execution model unchanged**: Tier A zero-WASI filters (`wasi_minimal = false`, no outbound) continue to execute on `pollster::block_on` (and inline on the Tokio worker when eligible under [ADR 000119](../docs/ADR/000119.md)). Wasmtime's ambient Tokio runtime (`block_on_wasi`) is entered only for `wasi = "minimal"` (`fat-guest`) and `streaming-body` invocations so WASI 0.3 clock waits (`wasi:clocks/monotonic-clock@0.3.0#wait-for`) have a timer driver even when called outside Tokio.
- **Criterion WASM micro-benchmarks (`cargo bench -p plecto-host --bench wasm --features outbound-http,outbound-tcp,fat-guest,streaming-body` at `1061a79` `v0.15.2`)**:
  - `on_request/trusted_pooled`: **2.09 µs** `[2.080 µs .. 2.104 µs]` (`3b8ca45`: 2.20 µs; v0.9.0 baseline: ~2.88 µs)
  - `on_request/untrusted_fresh`: **28.53 µs** `[28.05 µs .. 29.08 µs]` (`3b8ca45`: 27.92 µs; v0.9.0 baseline: ~44.0 µs)
  - `filter_load/trusted_cold`: **7.89 ms** `[7.79 ms .. 7.99 ms]` (`3b8ca45`: 7.75 ms; v0.9.0 baseline: ~27.3 ms)
- **T1 gate (`bash bench/perf/run-perf.sh gate`, `PROXY_CPUS=0-11`, `GEN_CPUS=12-23`, 3 interleaved rounds)**:
  - Two pre-T2 runs at `1061a79` returned `INCONCLUSIVE (exit 2: 9 pass, 0 fail, 1 inconclusive)` solely on `ratelimit_tax_us` (`1.2976 ± 0.5181 µs` and `1.1216 ± 0.3806 µs`, where `value - ci_half` straddled the fast-side band edge `0.9` of `0.9..2.4`; `dispatch_floor_us` `1.1383 ± 0.0809` / `1.2628 ± 0.0073 µs`, `apikey_cost_us` `1.1582 ± 0.0576` / `1.1066 ± 0.0325 µs`).
  - The post-T2 confirmation run (`20261024T042039Z-1061a79`, `gate.csv`) and the pre-tag run (`20261024T025042Z-3b8ca45`) both returned **PASS (`10 pass, 0 fail, 0 inconclusive`)**:
    - `dispatch_floor_us`: **1.2878 ± 0.0970 µs** (`3b8ca45`: `1.1714 ± 0.0330 µs`; band `0.9..2.1`, pass)
    - `apikey_cost_us`: **1.0640 ± 0.1228 µs** (`3b8ca45`: `1.1694 ± 0.0710 µs`; band `0.3..1.7`, pass)
    - `pooled_tail_p50_ms`: **0.0131 ms** (`3b8ca45`: `0.0135 ms`; band `0.00..0.20`, pass; informational `pooled_tail_p99_ms`: `0.0265 ms`)
    - `apikey_tail_p50_ms`: **0.0106 ms** (`3b8ca45`: `0.0095 ms`; band `-0.02..0.15`, pass)
    - `respctx_tail_p50_ms`: **0.0010 ms** (`3b8ca45`: `0.0011 ms`; band `-0.05..0.25`, pass; informational `respctx_tail_p99_ms`: `-0.0020 ms`)
    - `ratelimit_tax_us`: **1.1967 ± 0.0630 µs** (`3b8ca45`: `1.1452 ± 0.0297 µs`; band `0.9..2.4`, pass)
    - `enforce_allowed_ratio`: **0.9166** (band `0.90..1.10`, pass; informational `enforce_limited_frac` `0.7800`)
    - `rr_spread_req`: **0** (40,000 / 40,000 / 40,000 across 120,000 responses, pass)
    - `ejection_transition_s`: **1.0000 s** (band `0..2`, pass)
    - `ejection_stray_failed`: **0** (band `0..0`, pass)
  - *Addendum (2026-10-25, [ADR 000122](../docs/ADR/000122.md)):* the `±` figures above are the then-current sample half-range `(max - min) / 2` over 2–3 rounds (≈ 1 SE at n = 2), not 95 % intervals. Re-judging the archived raw rounds with the gate's current 95 % Student-t interval gives, for the confirmation run, `dispatch_floor_us` 1.2878 ± 0.2621 (pass), `apikey_cost_us` 1.0640 ± 0.3400 (pass) and `ratelimit_tax_us` 1.1967 ± 0.8007 µs (**inconclusive**); against the then-current `0.9..2.4` band, `ratelimit_tax_us` is likewise inconclusive in all five archived post-ADR-000119 gate runs (`20261021T134514Z-c253da5-dirty`, `20261023T182045Z-57e6f8e-dirty`, and the three `1061a79` runs); against the `0.5..2.4` band adopted on 2026-10-25, the 10-21 and 10-23 runs pass and the three `1061a79` runs remain inconclusive. `enforce_allowed_ratio` re-reads as 1.0000 under the discrete-refill expectation. No run directory named `20261024T025042Z-3b8ca45` is archived; the pre-tag figures quoted above match `20261023T182045Z-57e6f8e-dirty` exactly.
- **T2 full suite (`bash bench/perf/run-perf.sh all`, `20261024T035518Z-1061a79`, all 14 phases `ok`)**:
  - First full T2 refresh since inline header hooks ([ADR 000119](../docs/ADR/000119.md)) and default arena cap 32 ([ADR 000118](../docs/ADR/000118.md)) landed: `/baseline` keep-alive **212,730 req/s** (cold CRR **106,165 req/s**), `/noop-pooled` **170,316 req/s** (`1.17 µs/req` over baseline), `/trusted` **142,813 req/s** (`1.13 µs/req` over noop-pooled), `/noop-fresh` **13,102 req/s**, `/ondemand` **12,609 req/s**; open-loop **103,026/s** (p50 0.84 ms, p99 2.83 ms, 0 dropped); TLS keep-alive **144,025 req/s**, handshake/req **26,229 req/s**, h2 **129,587 req/s**; WebSocket echo **211,337 msg/s** (1 KB) / **59,500 msg/s** (64 KB, 3,899 MB/s); footprint idle RSS **41,992 kB** (`~25.4 KB/conn`). All 21 charts in `performance/img/*.png` regenerated with `flint-chart-mcp` 1.0.1.

## 2026-09-29 (inline header hooks: dispatch floor reduction)

Execution of header hooks (`on-request` / `on-response`) for trusted, non-blocking filters was moved inline onto the async Tokio worker instead of `tokio::task::spawn_blocking` ([ADR 000119](../docs/ADR/000119.md), amending ADR 000013 and ADR 000021):
- **Rationale & execution model**: wasmtime 49's `Store<T>` is `Send` when `T: Send`; the historical reason for off-worker execution was that guest code runs synchronously until its epoch deadline. For trusted filters without blocking operations, this bounded synchronous execution is safe on the async worker. The inline path takes only an idle pooled instance without waiting or building on the worker; if none is idle (pool at capacity, not yet built, recycle pending, breaker open) or a filter is ineligible, dispatch pauses and the remainder of the chain continues on the blocking pool from that filter (resumable dispatch). Body hooks remain on the blocking pool.
- **Eligibility criteria**: Evaluated at load time (`LoadedFilter::runs_inline`): declared `isolation = "trusted"`, imports only non-blocking host APIs (`host-log`, `host-clock`, `host-config`, types), state interfaces (`host-kv`, `host-counter`, `host-ratelimit`) only when using the in-memory backend (`!KvBackend::may_block()`), and no attached blocking capabilities (no outbound HTTP/TCP, no WASI).
- **Operational consequence**: An inline trusted filter occupies its worker thread for its own CPU time, bounded by the per-request epoch deadline (default 100 ms). When configured with `[state] backend = "redb"` (or custom backends that may block), state-using filters retain the blocking-pool execution path.
- **Measurements (reference host i7-13700K, proxy pinned to 6 P-cores/12 threads, generator on the rest, loopback, bench-server, 50 connections, oha; before = HEAD 22f94ad binary, after = this change; 3 interleaved launches each, medians)**:
  - **Dispatch floor** (`noop-pooled − baseline`, closed-loop inverse throughput 1e6/rps): before 4.06–4.21 µs/req, after 1.41–1.44 µs/req.
  - **Ceiling**: `noop-pooled` ceiling 100.2k → 146.6k rps; trusted (`apikey`, memory backend) ceiling 87.9k → 123.1k rps; apikey's own cost (`trusted − noop-pooled`) 1.38–1.42 → 1.30 µs (unchanged: filter work).
  - **Fixed-rate p50 (latency-corrected)**: at 500 rps `noop-pooled` 0.352 → 0.256 ms, trusted 0.391 → 0.291 ms (baseline 0.216–0.222); at 2,000 rps `noop-pooled` 0.406 → 0.361 ms, trusted 0.451 → 0.398 ms (baseline 0.291–0.292).
  - **Per-request scheduling** (summed over proxy threads via `/proc/<pid>/task/*/schedstat` + `status`, `noop-pooled` at ceiling): voluntary context switches 5.09 → 0.79 per request (baseline 0.87–0.92), CPU 103 → 76 µs/req, runqueue wait 76 → 0.7 µs/req, thread count 80 → 35.
  - All responses 200 in both arms.
- **T1 gate measurements & recalibration**:
  - Two same-session runs (`performance/data/runs/gate-inline-hooks-{1,2}`): `dispatch_floor_us` 1.4386 ± 0.1045 and 1.4987 ± 0.1843 µs (old band 2.0..5.0 FAIL); `ratelimit_tax_us` 1.7348 ± 0.0405 and 1.7771 ± 0.0232 µs (old band 2.2..5.0 FAIL).
  - All other invariants passed: `apikey_cost_us` 1.3879 ± 0.0420 / 1.3778 ± 0.0463 µs; `pooled_tail_p50_ms` 0.0721 / 0.0816 ms; `apikey_tail_p50_ms` 0.0567 / 0.0606 ms; `respctx_tail_p50_ms` 0.0095 / 0.0030 ms; `enforce_allowed_ratio` 0.9166; `rr_spread_req` 0; `ejection_transition_s` 1.0 s, `ejection_stray_failed` 0.
  - Recalibrated bands in `bench/perf/gate_tolerances.toml`: `dispatch_floor_us` 0.9..2.1 µs, `ratelimit_tax_us` 1.2..2.4 µs (PROVISIONAL pending more runs).
- **Trusted pool cap 8 (default) vs 12 (= worker count)**, interleaved 3 launches each, medians, via a temporary bench-only `pool_size` override (not committed): `noop-pooled` ceiling 145.2k vs 145.7k rps; trusted 122.1k vs 123.6k rps (≈ noise); dispatch floor 1.34–1.42 vs 1.39–1.47 µs/req. `plecto_filter_inline_fallbacks_total{reason="pool_exhausted"}` at the trusted ceiling: 0.74 % vs 0.01 % of requests; `pool_filling` ≈ 0.7 % of requests at the `noop-pooled` ceiling in both (instance recycle every 65,536 calls rebuilds only on the blocking path). The default cap costs nothing measurable; kept.
- **Residual**: after inlining, `noop-pooled` still uses ~19 µs/req more CPU than baseline (12 logical CPUs at saturation; inverse throughput × 12 ≈ 17 µs agrees). Two guest calls (~3 µs each) and pool fallbacks (~0.3 µs/req) leave ~12 µs unexplained (candidates: header conversion/copies, span emission, pool lock); not yet profiled because `perf` needs root on the reference host.
- **T1 gate after the follow-up** (panic parity, inline metrics, `dispatch` key; `performance/data/runs/gate-inline-hooks-3`): PASS on the recalibrated bands — `dispatch_floor_us` 1.4201 ± 0.0458, `ratelimit_tax_us` 1.5831 ± 0.0304, `apikey_cost_us` 1.3042 ± 0.0830, `pooled_tail_p50_ms` 0.0598, `apikey_tail_p50_ms` 0.0086, `respctx_tail_p50_ms` −0.0049, `enforce_allowed_ratio` 0.9166, `rr_spread_req` 0, `ejection_transition_s` 1.0 / `ejection_stray_failed` 0.

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

## 2026-08-15 (v0.9.0 snapshot)

A full refresh of the fast path at commit `b2a89be` (**v0.9.0**): T2 `all`, T3 `v03`, the T1 `gate` three times, and both micro layers (criterion + gungraun instruction counts, each saved as a local `main` baseline). The harness repairs described next landed on top of that commit and touch no crate source, so every figure stands for v0.9.0 as shipped. **The 07-20 pass's harness contamination is fixed, and the rows it invalidated are trustworthy again.** Two defects compounded: the k6 scenarios' VU pools exceeded ADR 000092's per-source-IP connection cap (**256**/IP), and the scripts folded a status-less response — a connection the proxy never accepted — into a meaningful bucket (`mixed.js` counted anything not 200 as a short-circuit "reject"; `weighted-mix.js` fed its near-zero duration straight into the published percentiles). Both are repaired: every pool is now bounded below the cap (Little's law leaves them ample at loopback service times) and every scenario counts status-less responses in their own `no_status` bucket, reported in the CSVs so the artifact can never pass as signal again. **Every re-measured scenario reports `no_status` = 0 this pass**, and the corrected figures land back where the pre-cap 07-11 snapshot had them: the short-circuit mix reads its designed **90.0 % / 10.0 %** split (was 76 %/24 %), enforcement accounts for **all 150,000** offered requests (48.8 % were missing) and sheds **79.3 %** — the 07-11 figure to the decimal — and the light rate-limit key passes **500/500 untouched** (was 140/500). The `footprint` phase was repaired the same way — it asked for 1,000 connections, got the cap's 256, and then divided by 1,000 — and its corrected marginal cost (**~25.4 KB/conn**) returns to the historical figure. The one scenario still crossing the cap is the closed-loop **sweep** at VU 400/800, which does so *deliberately* (its whole point is the concurrency curve); under the cap it is failure-free. **Verdicts this pass**: T1 `gate` **PASS**, then **FAIL**, then **PASS** — the single excursion was `ratelimit_tax_us` at 4.88 µs against a 2.2–4.2 band, with the phase's other three same-day measurements at 3.11 / 3.42 / 3.79 µs. This host was *not* idle (a browser, cadvisor and a clickhouse-server were resident; 15-min load average ~10), which is the honest explanation for a between-session excursion the interleave cannot cancel; the band was left untouched.

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
