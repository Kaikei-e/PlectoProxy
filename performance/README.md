# Plecto Proxy Performance

An honest performance snapshot of Plecto Proxy's two halves: the **native load-balancing fast
path** and the **WASM extension plane** (per-request filters, host-enforced rate limiting, the
request-body hook). The goal is **transparency about method**, not a leaderboard. Every number
here is an internal **regression baseline** — not a capacity guide, and not a comparison against
other proxies.

All components — load generator, Plecto Proxy, the upstream instances, and any tooling — run
**co-resident on a single commodity developer host over loopback**, so absolute figures are
bounded by that host and by the generator, not by Plecto Proxy in isolation. Read them as **relative**
signals — ratios, curve shapes and time-constants, not headline throughput.

## Measurement setup

- **Core isolation by pinning (and co-resident in-process upstream in T1/T2).** The proxy is pinned at exec time so every thread inherits the mask
  and the per-thread affinity is verified after start; the default proxy/generator split is by whole
  physical cores (`bench/perf/cpu_split.py`). On the reference host (`Intel Core i7-13700K`: 8 Raptor Cove P-cores as adjacent SMT pairs on CPUs `0-15`, 8 Gracemont E-cores on CPUs `16-23`), the proxy runs on `PROXY_CPUS=0-11` (6 P-cores / 12 threads) and the load generator runs on `GEN_CPUS=12-23` (2 P-cores / 4 threads + 8 E-cores / 8 threads). The generator therefore never steals a core from the proxy, though generator-heavy phases can be bounded by the hybrid P+E generator core set. **Important co-residency note:** the four standard benchmark harnesses (`bench-server`, `load-balancing`, `tls-http`, `swap-bench`) spawn their mock HTTP upstream **in-process** on the same OS process and Tokio runtime (`PROXY_CPUS=0-11`). Consequently, standard T1/T2 absolute throughput, CPU/req, and RSS figures (`body_rss.csv`, `footprint.txt`, `ws_footprint.csv`) measure **`proxy + in-process mock upstream` together** (adjacent route deltas cancel the shared upstream work, whereas absolute numbers and percentages include it). By contrast, `bench/perf/arena_sweep.py` (T3) isolates proxy, upstream, and generator into three separate OS processes on disjoint physical cores (`PROXY_CPUS=0-11`, `UP_CPUS=16-19`, `GEN_CPUS=12-15,20-23`).
- **Allocator configuration.** Every proxy launch runs with the shipped arena cap (default **32**, revised from the historical cap of 4; all 4 standard harnesses call `cap_malloc_arenas` before runtime initialization). Legacy behavior is reproducible in production with `PLECTO_MALLOC_ARENA_MAX=4` (or in benchmarks via `BENCH_MALLOC_ARENA_MAX=4`, as `run-perf.sh` scrubs inherited `PLECTO_*` variables). Note that both the historical `MALLOC_ARENA_MAX=4` body RSS table (`~114 / ~196 / ~122 MB`) and the current default-32 `body_rss.csv` table (`~122 / ~296 / ~117 MB`, i.e. `+51%` on `/body` traded for `+32%` auth and `+18%` rate-limit throughput per [ADR 000118](../docs/ADR/000118.md)) are **co-resident** mid-load samples (`proxy + in-process upstream` under `phase_body`), whereas isolated **proxy-only** RSS (`~256 MiB` peak / `~186–199 MiB` post-load 15 s settled at cap 32, vs `~167 / ~140 MiB` at cap 4) is measured separately by `arena_sweep.py` (T3); see [`bench/methodology.md`](../bench/methodology.md#2026-09-27--アリーナ既定値を判断するための分離計測t3), [`performance/HISTORY.md`](HISTORY.md#2026-09-28-allocator-arena-default-transition-4--32), and [ADR 000118](../docs/ADR/000118.md).
- **Run data retention & host fingerprinting.** Raw per-round JSON, proxy logs, CSV copies and a host
  fingerprint (`host.txt`, allowlisted for publication: no hostname, kernel build string or per-CVE
  mitigation lines) are kept under `performance/data/runs/<run-id>/` and `just perf-archive` tars
  one for attaching to a GitHub Release.
- **Historical comparability note.** Numbers measured before this change (the 2026-09-25 snapshot and
  earlier) were taken with partial pinning and uncapped arenas outside the body phase, so footprint
  KB/conn and gate spreads are not directly comparable.
- **No host tuning.** CPU governor / turbo are left at their defaults — no fixed-frequency lock.
  Absolute throughput shifts run-to-run with clock; the **ratios, shapes and time-constants** are
  the durable signal, so those are what we read.
- **Generators, by phase.** [k6](https://grafana.com/docs/k6/latest/) drives the closed-loop
  concurrency sweep (`constant-vus`), the mixed short-circuit run, and the rate-limit / body
  scenarios; **`plecto-loadgen openloop`** is the **authoritative** open-loop tail driver
  (constant arrival rate with **schedule-based latency** — the wrk2 / Gil Tene model; see
  [`bench/methodology.md`](../bench/methodology.md)); `OPENLOOP_GEN=k6` keeps the older
  `constant-arrival-rate` path for A/B. `plecto-loadgen` also runs the fault-injection timeline,
  the endpoint-set swap timeline, the round-robin count, and the WebSocket / TLS-handshake
  scenarios; and [oha](https://github.com/hatoo/oha) drives the single-route ceiling (plain h1,
  WASM W1, TLS) runs. Different generators have different ceilings — **numbers are comparable
  within a section, and across same-generator sections, but not blindly across all of them**.
  Each section names its generator.
- **Warm-up excluded.** Every measured window starts after a short warm-up (default 5 s) that
  sends load but is not recorded: in-script for k6 and plecto-loadgen, a discarded pre-run for
  oha. Cold-start seconds (route tables, upstream pools, allocator state) never enter a
  percentile. The rate-limit enforcement / fairness runs are the deliberate exception — their
  initial token-bucket burst *is* the measured signal.
- **Ceilings vs tails.** Closed-loop full-throttle runs (oha, `constant-vus`) are read as
  *throughput ceilings*; their latencies are queueing-at-saturation, not service latency
  ("never measure latency at max load"). Honest tails come from the fixed-rate runs:
  **`plecto-loadgen openloop`** (schedule-latency) and oha `-q` + `--latency-correction`, both
  coordinated-omission-safe. The plain-h1 ceiling reports **RR** (keep-alive) and **CRR**
  (cold TCP/req) KPIs in `ceiling.csv`.
- **Fully local.** Generators, proxy and upstreams talk only over loopback; generator telemetry and
  the optional dashboard's phone-home are disabled. Nothing leaves the host during a load run —
  load traffic stays on loopback; `REQUIRE_OFFLINE=1` can refuse a default IPv4 route for a
  netns-style lab (see [`bench/methodology.md`](../bench/methodology.md)).
- **PMU not collected.** The runbook's optional micro-architectural attribution (cycles/req, IPC,
  LLC / branch misses via `perf`) needs a lowered `kernel.perf_event_paranoid` (privileged); it
  was not enabled on this run, so the WASM / rate-limit tax is reported as throughput / latency /
  **µs-per-req**, not a cycles breakdown.

## TL;DR

> **Latest snapshot (`2026-10-24`, tag `v0.15.2`, commit `1061a79`, Wasmtime 50.0.0 + dual WASI 0.2/0.3 `@0.3.0`, inline header hooks [ADR 000119](../docs/ADR/000119.md), default arena cap 32 [ADR 000118](../docs/ADR/000118.md)):**
> - **T2 report (`RUN_DIR: 20261024T035518Z-1061a79`) & T1 gate (`20261024T042039Z-1061a79`):** All 14 T2 phases `ok`. The gate run reported `10 pass` under the half-range rule in force at the time; **re-judged from its archived raw rounds with the gate's current 95 % Student-t interval ([ADR 000122](../docs/ADR/000122.md)) it is `9 pass, 1 inconclusive`**: `dispatch_floor_us` **1.29 ± 0.26 µs** (n = 3, pass), `apikey_cost_us` **1.06 ± 0.34 µs** (n = 3, pass), `ratelimit_tax_us` **1.20 ± 0.80 µs** (n = 2, **inconclusive** — the interval straddles both the then-current lower edge `0.9` and the current `0.5`); fixed-rate 2,000 rps `pooled_tail_p50_ms` **+0.013 ms**, `rr_spread_req` **0**, `ejection_transition_s` **1.0 s**. Against the then-current `0.9..2.4` band the same re-judgment makes `ratelimit_tax_us` inconclusive in all five archived post-ADR-000119 gate runs (2026-10-21 … 10-24); against the current `0.5..2.4` band, in three of the five — the n = 2–3 intervals are simply too wide, which is why the gate now runs 8 short paired rounds.
> - **In-process micro-benchmarks (`HEAD`):** Criterion `on_request/trusted_pooled` **2.09 µs** vs `untrusted_fresh` **28.53 µs** (`13.65×`), `filter_load/trusted_cold` **7.89 ms**; gungraun teardown-excluded instruction counts ([ADR 000113](../docs/ADR/000113.md), [ADR 000122](../docs/ADR/000122.md)): `on_request` **30,448** vs **112,951** instructions (`3.71×`), `lb_pick` **225 / 340 / 282** instructions.
> - **Re-measured 2026-10-25 on the ADR 000122 tree (same host, `9c0b84b` + working tree):** open-loop at 103,026/s with sub-millisecond pacing — **p50 0.050 ms** (was 0.84 ms), p95 0.51 ms, p99 2.09 ms, p99.9 4.57 ms, 0 dropped — so the old p50 was generator-side timer lag; WebSocket tunnels on a fresh process, under the per-IP cap: **250 held, ~21.3 KB/tunnel** (`21,348 B`); 8-round paired gate, three runs pooled (`20261025T052103Z` + `053124Z` + `054222Z`, n = 24, same build): **PASS, every invariant in band** — `dispatch_floor_us` **1.24 ± 0.05 µs**, `apikey_cost_us` **1.30 ± 0.35 µs** (the first run's two disturbed rounds kept in, see the [WASM ladder](#the-wasm-cost-ladder--isolating-each-cost) note), `ratelimit_tax_us` **1.06 ± 0.14 µs**, fixed-rate CPU `ratelimit_cpu_us_per_req` **+15.3 ± 1.0 µs** over a `baseline` of 32.3 µs/req (info, n = 12). The two intermediate verdicts were inconclusive; none was replaced. An independent standalone holdout gate run (`20261025T061734Z`, n = 8, no pooling, with 11 s pre-CPU settle) also returned **PASS on all 10 invariants** (`dispatch_floor_us` **1.17 ± 0.12 µs**, `apikey_cost_us` **1.06 ± 0.09 µs**, `ratelimit_tax_us` **1.49 ± 0.54 µs**, `ratelimit_cpu_us_per_req` **+18.3 ± 0.8 µs** over `34.5 ± 0.7 µs`, n = 4).
> - **Full per-run verdict logs & historical snapshots (`2026-09-29` inline hooks, `2026-09-28` arena 32, `2026-09-25` v0.11.5+ … `2026-07-02`):** see [`performance/HISTORY.md`](HISTORY.md).
>
> **µs/req deltas are what to track across snapshots**, not raw throughput — and the tracked
> invariant set is machine-checked by the T1 gate (`bash bench/perf/run-perf.sh gate`, bands in
> `bench/perf/gate_tolerances.toml`). The gate produces a three-valued verdict: `pass` (exit code 0,
> every invariant's interval inside its band), `fail` (exit code 1, interval outside band; dominates inconclusive), or
> `inconclusive` (exit code 2, interval straddles a band edge => re-run with `GATE_POOL_FROM=<that RUN_DIR>`, which **pools** the earlier rounds with the new ones instead of replacing them). In `gate.csv`, `ci_half` is the **two-sided 95 % Student-t half-width** of the mean of the n per-round paired deltas, $t_{0.975,n-1}\cdot s/\sqrt{n}$, with `n` in the last column. Gate runs before ADR 000122 recorded the 2–3-round sample half-range `(max - min) / 2` in that column instead — at $n=2$ exactly $1\,\text{SE}$ (50 % coverage), not a 95 % interval — so `±` figures quoted from those runs are not comparable to new ones.
>
> **Metric definition — µs/req**: µs/req = 1e6 / rps, i.e. inverse throughput. In a closed loop with C connections it equals mean latency / C (Little's law); it is neither latency nor CPU time. Note that criterion's per-call µs (single-thread, in-process) is a different quantity.

**Load-balancing fast path** (plaintext HTTP/1.1, 3 upstreams, trivial 0 ms backend; k6 / loadgen / oha):

- Closed-loop throughput peaks at **~147.2k req/s** (VU 100 this run) with **p99 ≈ 1.25–4.52 ms** and
  zero failures through VU 200. **VU 400/800 show 27.8 % / 50.5 % "failed"** — this is **not** proxy
  overload: it is the sweep's own concurrency (400/800 simultaneous connections, all from the
  generator's single loopback IP) crossing ADR 000092's **256-connections-per-source-IP**
  admission cap, which those two rungs cross deliberately — see
  [the sweep section](#throughput--latency-vs-concurrency). Under the
  cap (VU ≤ 200) the curve still declines gracefully with no cliff.
- Open-loop at the auto **103.0k/s** (70 % of closed-loop peak ≈147,181 req/s) **achieves 103,026/s exactly** with
  **p50 0.050 ms, p95 0.51 ms, p99 2.09 ms, p99.9 4.57 ms, 0 dropped, 0 % failed** — schedule-latency
  (`plecto-loadgen openloop`, 64 workers — well under the per-IP cap, unaffected; re-measured 2026-10-25 after the loadgen's sub-millisecond pacing fix). The 2026-10-24 run read p50 0.84 ms because the generator's 1 ms timer granularity delayed sends — see [open-loop tail](#tail-latency-under-open-loop-load).
- Round-robin across three upstreams is **even to within zero requests** (33.3 % each, 40,000 / 40,000 / 40,000, 120,000 reqs).
- **Resilience is as designed**: ejecting one upstream drops its share to zero in ~1 s and the
  survivors absorb the load with **no client-visible errors**; a *total* outage **fails closed
  with HTTP 503** and the pool **recovers within ~1 s** of health returning.
- TLS termination (**aws-lc-rs**, ADR 000051): within-TLS, keep-alive **~144.0k** (~68 % of the
  plaintext ceiling) vs handshake/req **~26.2k** (~18 % of keep-alive) and h2 **~129.6k** (~90 % of
  keep-alive) — the path is **crypto-/TLS-I/O-bound**, ordering clean. A resumption-isolated
  measurement (carried from 07-05, not re-run this pass) puts a **true full handshake at ~22.1k/s**
  vs **~29.8k/s resumed (93 %)** — see [TLS](#tls-termination).
- A **kept-alive** connection (**RR**) serves **~212.7k req/s** this run; forcing a **TCP
  handshake per request** (**CRR**, 106.2k req/s) costs **~50 % throughput and +0.71 ms p99** — connection
  reuse is still load-bearing (see [the plain HTTP/1.1 ceiling](#plain-http11-ceiling)).

**WASM extension plane** (the cost of running a decision as a sandboxed component; oha / k6):

- A **cost ladder** isolates each cost by adjacent delta (oha, `-c 50` — well under the per-IP cap,
  clean). With inline header hooks ([ADR 000119](../docs/ADR/000119.md)) and Wasmtime 50.0.0 ([ADR 000120](../docs/ADR/000120.md)),
  this run's full-throttle ceiling is clean (`baseline` **>** every WASM rung): **baseline → noop-pooled costs ~20 % throughput** full-throttle
  (**≈ 1.17 µs/req** inverse-throughput delta, matching the T1 gate's **`1.14–1.29 µs`** `dispatch_floor_us` across the five archived post-ADR-000119 gate runs, 2026-10-21 … 10-24); the **fixed-rate tail** (7,565 req/s, the portable queueing-honest read)
  puts it at **+0.01 ms p50 / +0.01 ms p99** over native (`+0.013 ms p50 / +0.027 ms p99` in the T1 gate's 2,000 req/s tail). A **real filter's own work**
  (`filter-apikey` on top of the pooled no-op) is **≈ 1.13 µs** — matching the gate's **`1.06–1.17 µs`** `apikey_cost_us` across the same five runs; running that filter **fresh-per-request** instead of pooled
  costs **~11–13×** throughput — the price of re-paying `init` every request.
- These macro deltas **reconcile with the criterion [micro-benchmarks](#0-micro-benchmarks-in-process-criterion)**
  in direction and order of magnitude, and with the T1 gate (`bash bench/perf/run-perf.sh gate`, bands in
  `bench/perf/gate_tolerances.toml`; re-judged with the 95 % t interval, the confirmation run's `dispatch_floor_us` and `apikey_cost_us` pass and `ratelimit_tax_us` is inconclusive — see the [TL;DR](#tldr)).
- **v0.3.0 response / compression (opt-in `v03` phase, measured 2026-08-15 on v0.9.0, pre-ADR-000119):** reading the
  as-forwarded request snapshot on `on-response` costs **≈ +0.22 µs/req** over the same-session v0.9.0 `/noop-pooled` baseline (`112,955 rps`, `8.85 µs/req`; on current post-ADR-000119 `HEAD`, the T1 gate tracks the 2,000 rps fixed-rate tail delta `respctx_tail_p50_ms` at `−0.0008 … +0.0011 ms` across the five archived post-ADR-000119 gate runs — see
  [the section itself](#v030-response-ladder--compression)); gzip on a 4 KiB compressible body costs
  **≈ −31 % ceiling / +2.3 µs/req** vs the same body uncompressed, within ~0.25 µs across snapshots.
- A rejected request (**HTTP 401 short-circuit**) is decided in **~0.075 ms (p50) / ~0.178 ms (p95) and never reaches the
  backend** — bad traffic is shed **~215× faster at p50 (~95× at p95)** than good traffic is forwarded (`16.097 ms` p50 / `16.900 ms` p95) through a 15 ms
  backend. The split matches the design mix — **90.1 % accepted /
  9.9 % rejected, zero status-less responses** — see
  [the section](#short-circuit-rejecting-bad-traffic-at-the-edge).

**Host-enforced rate limiting** (token bucket, spec host-configured in the manifest; k6):

- With inline header hooks ([ADR 000119](../docs/ADR/000119.md)), the T1 gate measures `ratelimit_tax_us` at
  **`1.12–1.30 µs/req`** across the five archived post-ADR-000119 gate runs (`1.1967` in the confirmation run's `gate.csv`; re-judged with the 95 % t interval at n = 2, all five straddle the then-current `0.9` edge and three of five still straddle today's `0.5`, e.g. `1.20 ± 0.80 µs`) over a no-filter
  baseline when the bucket never denies (`VUS=50`, well under the per-IP cap, clean). This k6 closed-loop delta reads *below* the same run's oha `dispatch_floor_us` in 7 of the 9 gate runs since ADR 000119 (4 of the 5 archived, 3 of the 4 on 2026-10-25), so it cannot be read as "floor + bucket consult": k6's `/baseline` arm runs at ~117k–155k rps (≈151k in the 10-24 confirmation run) vs oha's ~212.7k, near k6's own ceiling, which plausibly compresses $\Delta(1/\text{RPS})$, and deltas from different generators cannot be added or subtracted. The gate now also records proxy CPU-µs/req at a fixed 20k/s (`ratelimit_cpu_us_per_req`, info row; +15.3 ± 1.0 µs over three pooled runs, +18.3 ± 0.8 µs in the holdout run) — see [rate-limit overhead](#overhead--the-cost-of-consulting-the-bucket).
- Offered **5× over the configured rate**, the **allowed throughput converges to the
  bucket's discrete refill schedule** (**1,033.3/s** over 30 s for a `capacity = 2,000`, `1,000 tok / 1,000 ms` bucket: initial `2,000` burst + `29 × 1,000` discrete 1 s refills = **31,000** allowed) and the run accounts for
  **every** offered request — 31,000 allowed + 119,002 shed = 150,002 accounted for over 30 s, **0
  status-less** — putting the shed fraction at **79.3 %**, the same figure every post-repair
  snapshot has measured.
- Buckets are **per key**: a hot key offered 4× its limit is throttled to its own refill rate
  (1,033/s, 74.2 % shed) while a **light key on the same filter passes completely untouched —
  500/s offered, 500/s allowed, zero 429s** — no cross-key starvation, now measured rather than
  inferred.

**Request-body hook** (buffer-then-decide, ADR 000025; export-presence zero-copy bypass, ADR 000038; k6):

- A filter that **reads** the body (`/body`, filter-hello) costs **~29 % throughput at 1 KB** and
  scales with payload: **~55 % at 100 KB**, **~64 % at 1 MB**, versus the streaming passthrough
  (`VUS=50`, well under the per-IP cap, clean).
  A **header-only filter** (`/body-headeronly`) **streams the body through**: at 1 MB it lands
  **within ~2.2 % of `/baseline`** (ADR 000038, within noise); at 100 KB the gap is **~2.7 %**
  and at 1 KB the gap is the ordinary **WASM dispatch floor** on a tiny request, not a body cost.
- Co-resident `proxy + in-process upstream` RSS at 1 MB × 50 VUs (`body_rss.csv`, one mid-load sample under `phase_body`, default arena cap 32 per [ADR 000118](../docs/ADR/000118.md)): **~122 MB `/baseline` · ~296 MB `/body` · ~117 MB
  `/body-headeronly`** (up from `~114 / ~196 / ~122 MB` under legacy arena cap 4, i.e. `+51%` on `/body`; isolated proxy-only RSS via `arena_sweep.py` is **`~256 MiB` peak / `~186–199 MiB` settled**). The header-only bypass stays near baseline; the buffer stays bounded (16 MiB
  cap, fail-closed 413).

## Scope & honesty notes

- **Reference host disclosed; treat absolute figures as host-specific.** All runs execute on a single commodity reference host (`Intel Core i7-13700K`, 8 Raptor Cove P-cores / 16 threads on CPUs `0-15` + 8 Gracemont E-cores / 8 threads on CPUs `16-23`, 24 logical CPUs, default `powersave` governor, loopback, co-resident). While absolute figures depend on this reference host and its unpinned clock/governor (and `GEN_CPUS=12-23` mixes 2 P-cores + 8 E-cores), the benchmark structure isolates relative deltas, ratios, and time-constants on the same run.
- **Single host and pinning method.** Measured on the same single commodity host over loopback with
  the core-pinning setup described above (`PROXY_CPUS=0-11`, `GEN_CPUS=12-23`). At `1061a79` (`v0.15.2`), two pre-T2 T1 `gate` runs returned `INCONCLUSIVE (exit 2)` solely on `ratelimit_tax_us` (`1.30 ± 0.52` and `1.12 ± 0.38 µs` as old half-ranges, straddling the edge `0.9` of `0.9..2.4`), while the post-T2 confirmation run (`20261024T042039Z`) and the pre-tag run (`20261023T182045Z-57e6f8e-dirty`) reported `10 pass` under the half-range rule then in force. Re-judged from their archived rounds with the 95 % t interval, all four (and the 2026-10-21 run) are inconclusive on `ratelimit_tax_us` against that `0.9..2.4` band (three of the five against today's `0.5..2.4`): two k6 rounds cannot bound a ~1.2 µs delta tightly.
  Ratios, shapes and time-constants — what this report actually tracks — remain the durable signal.
- **Generator-bound where noted.** The closed-loop sweep tops out near the *generator's* ceiling on
  its cores, not the proxy's: absolute peaks move with host/generator noise (this run ~147.2k k6
  peak vs ~212.7k oha ceiling keep-alive — different generators, different ceilings). The sweep
  curve's *shape* is the signal, not
  its absolute peak (below the per-IP admission cap — see [the sweep section](#throughput--latency-vs-concurrency) for VU ≥ 400).
  Open-loop tails use `plecto-loadgen` so they are no longer k6-VU-bound.
- **Trivial upstreams** (tiny static responses, 0 ms latency by default, running in-process inside the four standard T1/T2 harnesses and isolated in a separate OS process in T3 `arena_sweep.py`) deliberately isolate
  **proxy + LB + filter overhead** rather than backend work. A 15 ms synthetic backend is used
  where realistic proportions matter (WASM short-circuit); a sized-body backend for the body sweep.
- The LB figures are **plaintext HTTP/1.1**, except the dedicated [TLS run](#tls-termination).
- **No comparative claims.** Mature proxies are referenced only for shared methodology, never ranking.
- Charts compiled with Flint (flint-chart) from the CSVs in performance/data; an optional InfluxDB + Grafana stack (`INFLUX=1`) provides
  live dashboards during k6 runs (its images are a one-time setup pull; the load stays on loopback).

---

# 0. Micro-benchmarks (in-process, criterion)

A deterministic, network-free layer (`cargo bench`, criterion) that isolates the **per-function** cost
of the hot path with low noise — complementary to the end-to-end macro scenarios below, and the basis
for the CI regression gate (`--save-baseline` / `--baseline`). Micro-cost × calls-per-request should
roughly explain the macro deltas, and it does (the WASM ladder is the worked example).

**Fast path** (`crates/control/benches/fastpath.rs`):

| bench | cost | note |
| --- | --- | --- |
| LB pick — round-robin | 36 → 27 → 31 ns (32 → 8 → 3 instances) | ~O(1) over the eligible set |
| LB pick — P2C weighted-least-request | 36 → 43 → 69 ns (3 → 8 → 32 instances) | two eligibility passes + the sampled compare |
| LB pick — weighted Maglev | ~22 ns (3, 8) → ~21 ns (32) | + one table lookup |
| LB pick under swap churn (`pick_under_swap_churn`) | 85 → 60 → 65 ns (3 → 8 → 32 instances) | round-robin pick while a background thread continuously calls `update_endpoints` (ADR 000044) — the per-pick `ArcSwap<Endpoints>` load cost under worst-case concurrent churn |
| route match (`find_route`) | 47 ns → 242 ns (1 → 64 routes) | scans by specificity, allocation-free |
| ingress path normalization | ~52–74 ns clean / ~187 ns dot-segments | ADR 000027; a clean path is borrowed, no allocation |

All three LB algorithms are covered here; the macro suite only load-tests round-robin. The `n=3`
`pick_under_swap_churn` cell reads slower than `n=8`/`n=32` — under continuous churn the eligible
set is tiny (2 instances) relative to the fixed cost of the concurrent `update_endpoints`
allocation contending for the same cache lines every tick; reported as measured, not smoothed.

**Extension plane** (`crates/host/benches/wasm.rs`):

| bench | cost (per-call µs) | isolates |
| --- | --- | --- |
| `on_request` — pooled instance | ~2.09 µs | dispatch + call (init amortized) |
| `on_request` — fresh instance / request | ~28.5 µs | + per-request instantiation (the pool's value) |
| cold `load` (verify + instantiate + init) | ~7.89 ms | cosign signature + SBOM verification + compile |

*(Note: Criterion clocks wall-clock duration per call in a single-thread, in-process benchmark. This per-call µs is a different quantity from the macro inverse-throughput µs/req = 1e6 / rps; see [metric definition](#1-load-balancing-fast-path).)*

The ~14× pooled→fresh gap here is the same one the [macro ladder](#the-wasm-cost-ladder--isolating-each-cost)
shows end-to-end (~13× in the T2 snapshot, with the HTTP layer and its own run-to-run noise around it) —
the two layers agree in direction and order of magnitude, so a divergence between them would be a
real bug. (Extension-plane Criterion table re-measured 2026-10-24 at `1061a79` (tag **v0.15.2**, ADR 000120/000121),
wasmtime 50.0.0 with `outbound-http,outbound-tcp,fat-guest,streaming-body` enabled — down from
~2.88 µs / ~44.0 µs / ~27.3 ms on 2026-08-15 `v0.9.0`; fast-path Criterion table above measured
2026-08-15 at commit `b2a89be` (`v0.9.0`). Day-to-day drift of ±10–20 % is expected per
[`bench/methodology.md`](../bench/methodology.md) § Measurement tiers.)

**The frequency-invariant twin — instruction counts** (gungraun/callgrind, feature
`instruction-bench`; `crates/host/benches/wasm_inst.rs`, `crates/control/benches/fastpath_inst.rs`).
Instruction counts don't move with clock, thermals or a noisy neighbour, which is what makes them
the *judged* layer in CI (`ir=5%` soft limit against main's baseline) while criterion stays
informational:

| bench (`HEAD` post-ADR-000113, teardown-excluded) | instructions | L1 hits | LL hits | RAM hits | total read/write | est. cycles |
| --- | --- | --- | --- | --- | --- | --- |
| `on_request/trusted_pooled` (`wasm_inst`) | **30,448** | 45,406 | 1,416 | 20 | 46,842 | 53,186 |
| `on_request/untrusted_fresh` (`wasm_inst`) | **112,951** | 164,118 | 6,375 | 42 | 170,535 | 197,463 |
| LB pick — round-robin (`fastpath_inst`) | **225** | 312 | 0 | 3 | 315 | 417 |
| LB pick — P2C weighted-least-request (`fastpath_inst`) | **340** | 464 | 0 | 3 | 467 | 569 |
| LB pick — weighted Maglev, source IP (`fastpath_inst`) | **282** | 384 | 0 | 3 | 387 | 489 |

- **Why the old v0.9.0 (`2026-08-15`) instruction table (`507,977` vs `563,824`, `1.11×` for `wasm_inst`; `2,114` / `2,377` / `2,277` for `fastpath_inst`) was stale and physically inconsistent with Criterion:** In v0.9.0, `on_request(fx: Fx)` and `lb_pick(group: Arc<UpstreamGroup>)` consumed and dropped their fixtures inside the callgrind-measured block, so **~90 % of the ~508k / ~564k instructions in `wasm_inst` was `Host`/`Engine` teardown** and **~86–89 % of the ~2,114–2,377 instructions in `fastpath_inst` was `UpstreamGroup` teardown** rather than per-request execution (and `507,977` instructions in `2.09–2.88 µs` at ~5.4 GHz implied an impossible IPC > 30). [ADR 000113](../docs/ADR/000113.md) fixed `wasm_inst.rs` by warming one-time lazy paths inside `setup` and returning `-> Fx` so `Drop` runs outside the measured region, and `fastpath_inst.rs` now likewise warms one pick in `group()` and returns `-> Arc<UpstreamGroup>` (`2,114 → 225` on round-robin, `2,377 → 340` on P2C, `2,277 → 282` on Maglev).
- **Reconciling `wasm_inst` and `fastpath_inst` with Criterion on `HEAD`:** With teardown excluded, `on_request/trusted_pooled` executes **`30,448` instructions**, which in `2.09–2.88 µs` on a ~5.4 GHz Raptor Cove P-core (~11,300–15,550 cycles) corresponds to **IPC ≈ 1.96–2.70** — physically consistent with Criterion's single-thread wall-clock. `lb_pick` at `225 / 340 / 282` instructions against Criterion's 3-instance points (`31 / 36 / 22 ns`, measured on `v0.9.0`) gives IPC ≈ 1.3–2.4. (The table's **est. cycles** column is callgrind's cache-weighted cost model — `L1 hits + 5 × LL hits + 35 × RAM hits`, e.g. `45,406 + 5 × 1,416 + 35 × 20 = 53,186` — not hardware cycles, so it is not the denominator of the IPC above; the same holds for `lb_pick`'s `417–569`.) [`bench/perf/ipc_check.py`](../bench/perf/ipc_check.py) automates this cross-check (`Ir / (ns × GHz) > 5` fails) whenever both layers have been run locally. Across the two WASM modes, `on_request/untrusted_fresh` executes **`3.71×` more user-space instructions** (`112,951` vs `30,448`) and **`3.71×` more model cost** (`197,463` vs `53,186`), while Criterion wall-clock is **`13.65×` slower** (`28.53 µs` vs `2.09 µs`). The remaining gap between `3.71×` user-space instructions and `13.65×` wall-clock is work callgrind does not count as user-space instructions — kernel time (page faults, `madvise`, `mmap`/`munmap`) and allocator behaviour around per-request store/linear-memory setup — plus **`4.5×` more LL cache hits** (`6,375` vs `1,416`) and **`2.1×` more RAM hits** (`42` vs `20`); the split between those terms has not been profiled. Instruction-count invariance is not wall-clock invariance; both layers are kept for that reason.

---

# 1. Load-balancing fast path

> **Metric definition — µs/req**: `µs/req = 1e6 / rps`, i.e. inverse throughput. In a closed loop with *C* connections it equals mean latency / *C* (Little's law); it is neither latency nor CPU time. Criterion's per-call µs in [§0 Micro-benchmarks](#0-micro-benchmarks-in-process-criterion) is a single-thread, in-process quantity and dimensionally distinct.

Subject: one Plecto Proxy route forwarding to an upstream pool of **3 instances**, round-robin pick
over the healthy set, active health probe every **500 ms** with eject after **2** consecutive
failures (≈ ~1 s to detect). The three upstream nodes are three loopback backends, so the run
needs no external network.

## Plain HTTP/1.1 ceiling

The canonical reference figure every other section in this report reads from — measured **once**,
on `bench-server`'s filter-less `/baseline` route (oha; keep-alive vs a fresh TCP handshake per
request, `--disable-keepalive`). Before the `bench-server` harness merge this same route was
measured independently by three different processes (the WASM ladder's own server, the TLS run's
plaintext control, and a standalone churn run) — three numbers for one thing, differing only by
host noise. The `ceiling` phase now produces `ceiling.csv`; the [WASM ladder](#the-wasm-cost-ladder--isolating-each-cost)'s
`baseline` row and [TLS termination](#tls-termination)'s `plain (h1)` row cite it instead of
re-measuring.

![Plain HTTP/1.1 ceiling](img/ceiling.png)
![Plain HTTP/1.1 ceiling, tail latency](img/ceiling_tail.png)

| Variant | KPI | req/s | p50 | p99 |
| --- | --- | --- | --- | --- |
| keep-alive       | RR  | 212,730 | 0.22 ms | 0.50 ms |
| cold (TCP/req)   | CRR | 106,165 | 0.41 ms | 1.21 ms |

*(Re-measured 2026-10-24 (commit `1061a79`, tag **v0.15.2**) — `bash bench/perf/run-perf.sh all` / `ceiling`.
Cold/keep-alive **ratio** and the RR/CRR split are the durable signal.)*

A TCP handshake per request costs **~50 % throughput and +0.71 ms p99** even on loopback (where the
handshake is nearly free) — over a real network the gap widens with RTT. Connection reuse is
load-bearing; this is the plaintext analogue of the [TLS handshake-per-request row](#tls-termination) below.

> **A note on a latency bug this scenario caught.** An early body run showed a ~40 ms p99 cliff on
> medium streamed bodies — the signature of a delayed-ACK stall. The upstream client had Nagle's
> algorithm on (no `TCP_NODELAY`), so a streamed request body sent in several writes stalled on the
> peer's delayed-ACK timer. Disabling Nagle on the upstream sockets — standard practice for L7
> proxies — removed it (100 KB streamed p99 42.9 ms → 4.2 ms). The numbers here are post-fix.

## Throughput & latency vs concurrency

Closed-loop sweep (k6 `constant-vus`) — a fixed number of virtual users, each issuing its next
request only after the previous response. Rising concurrency walks the load curve.

![Throughput vs concurrency](img/throughput_vs_concurrency.png)
![Latency percentiles vs concurrency](img/latency_vs_concurrency.png)

| VUs | req/s | p50 | p95 | p99 | p99.9 | failed |
| --- | --- | --- | --- | --- | --- | --- |
| 50  | 143,126 | 0.24 ms | 0.71 ms | 1.25 ms | 2.48 ms | 0% |
| 100 | **147,182** | 0.48 ms | 1.35 ms | 2.46 ms | 4.88 ms | 0% |
| 200 | 133,782 | 0.90 ms | 2.55 ms | 4.52 ms | 9.04 ms | 0% |
| 400 | 96,394  | 1.01 ms | 4.98 ms | 8.68 ms | 16.55 ms | **27.8%** |
| 800 | 86,259  | 1.25 ms | 6.03 ms | 10.80 ms | 20.41 ms | **50.5%** |

Throughput peaks at **~147.2k at VU 100 this run** (the k6 generator's own ceiling on its cores —
which VU count wins the peak is host/generator noise, not a proxy change) and, **through VU 200**,
declines gracefully with latency rising in proportion and zero failures — the shape this section has
always shown.

> **A newly-added admission-control cap now surfaces at VU >= 400 (measured 2026-07-20).** This pass
> is the first since ADR 000092 landed (2026-07-15, commit `36006c7`): Plecto Proxy now refuses a new
> connection outright once a single source IP holds `MAX_CONNECTIONS_PER_IP` = **256** concurrent
> connections (`crates/server/src/conn_limit.rs`, amending [ADR 000027](../docs/ADR/000027.md)) - a CWE-770/CWE-400 hardening
> measure so one source can no longer monopolize every connection permit. `constant-vus` runs every
> VU as a genuinely concurrent connection from the **same** loopback source IP as the proxy itself,
> so VU 400 and VU 800 are, from the cap's point of view, one source opening 400 / 800 connections -
> well past the 256 threshold the 07-11 snapshot never crossed (its highest rung was also VU 800, but
> that pre-dates the cap by ten days). The refused connections surface as k6 `status !== 200` (no
> HTTP response at all - `res.status` reads `0`), which is exactly what the `failed` column counts.
> Confirmed as the cap, not host noise or the netns sandbox: the failure fractions reproduce within
> 0.1 pp whether this phase runs inside the isolated network namespace or in the host's normal
> namespace (28.0 %/47.0 % outside vs 27.9 %/47.1 % inside, back-to-back on the same host state), and
> the threshold crossing lines up exactly with the cap - 0 % at VU 200 (under 256), a jump at VU 400
> (over 256). This is a **benchmark-harness / feature interaction**, not a regression in the load
> balancer itself. **Reproduced on 2026-09-25 and 2026-10-24** (27.8 % / 50.5 %), and *kept* here rather
> than tuned away: unlike the open-loop scenarios — whose oversized VU pools were pure artifact and
> are now bounded below the cap (see the [short-circuit](#short-circuit-rejecting-bad-traffic-at-the-edge), [rate-limit](#enforcement--does-it-actually-hold-the-rate), and [weighted-mix](#weighted-request-mix--with-its-own-baseline) notes below) — these two rungs exist to walk the concurrency curve, so
> crossing the cap is the honest thing for them to do. Read VU 400/800 as "one source IP past its
> admission budget", not as proxy overload; every other phase in this report keeps its own
> concurrency under 256 and reads clean.

## Tail latency under open-loop load

Open-loop sends at a **constant arrival rate** regardless of how fast responses come back, so
queueing surfaces in the tail instead of being hidden — the *coordinated-omission-safe* model.

| Model | target | achieved | p50 | p95 | p99 | p99.9 | dropped | failed |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| open-loop, 0 ms backend (`plecto-loadgen`, 2026-10-25, `yield_now()` pacing) | 103,026/s | **103,026/s** | **0.050 ms** | 0.508 ms | 2.09 ms | 4.57 ms | **0** | 0% |
| *(superseded: 2026-10-24, `sleep_until` pacing)* | 103,026/s | 103,026/s | 0.84 ms | 1.65 ms | 2.83 ms | 5.58 ms | 0 | 0% |

The auto target (70 % of the closed-loop peak, **103.0k/s** this run, based on closed-loop peak ≈147,181 req/s) is **achieved exactly** (`9,272,340` requests over 90 s) with
**zero dropped slots** under schedule-latency measurement (`plecto-loadgen openloop`, 64 workers —
well under ADR 000092's per-IP cap, unaffected; wrk2 model — see
[`bench/methodology.md`](../bench/methodology.md)). A co-resident Rust generator sustains the
auto rate without dropping slots. With the corrected pacing, **89.6 % of requests complete within 100 µs of their scheduled send time** (mean `0.132 ms` in `openloop_hist.csv`, well under the 0.621 ms Little's-law bound below); the ~2.09 ms p99 is the queueing tail to track.

> **Why the 2026-10-24 p50 was 0.84 ms — generator-side send lag, confirmed by re-measurement.**
> With `C = 64` single-in-flight worker connections sustaining $\lambda = 103{,}026\text{ req/s}$ (`0 dropped`), Little's law bounds the mean wire response time (`write_all` + `read_response`) at $W_{\text{wire}} \le 64 / 103{,}026 = 0.621\text{ ms}$, and a right-skewed distribution has its median below its mean — yet schedule-latency (`done − scheduled`) reads p50 `0.84 ms` / mean `0.895 ms`. At least ~0.2–0.3 ms of it is therefore time spent *before* the request was written. Binned at 100 µs, `openloop_hist.csv` shows a flat plateau from ~0.2 to ~1.4 ms (≈ 6–9 % per bin) rather than a geometric tail — the shape a uniform 0–1 ms delay convolved with the wire latency would give. That points at the pacing: the run used `tokio::time::sleep_until(scheduled)`, and Tokio's timer wheel has 1 ms granularity, so a wake-up can land up to 1 ms late. The alternative explanation — per-connection queueing at 1,610 req/s per worker — would produce a geometric tail, which the histogram does not show. `bench/loadgen/src/openloop.rs` now sleeps only until `scheduled − 1 ms` (and only when more than 2 ms remain) and covers the last stretch with `tokio::task::yield_now()`, keeping the coordinated-omission-safe `done − scheduled` accounting. Re-measured on 2026-10-25 at the same 103,026/s: p50 fell from 0.84 to **0.050 ms**, the mean from 0.895 to 0.132 ms, and the plateau is gone (89.6 % of samples in the first 100 µs bin, then a geometric decay) — the old p50 was the generator's timer, not the proxy. One confound is not yet bounded: the yield loop keeps the generator cores busy, which can shift the package power budget shared with the proxy's P-cores. That would bias the proxy *slower*, not faster, so it cannot explain the improvement, but the closed-loop ceiling and proxy CPU-µs/req under open-loop load have not been compared with and without it.

## Round-robin distribution

![Round-robin distribution](img/rr_distribution.png)

Over a steady window with all three upstreams healthy, **120,000** requests split **40,000 /
40,000 / 40,000** — exact across all three instances (33.3 % each, 0 spread). Round-robin holds under load.
(Re-measured 2026-10-24; `plecto-loadgen rr`, 48 workers — well under the per-IP cap, unaffected.)

## Resilience: ejection & fail-closed

A steady open-loop rate (~4k req/s, `plecto-loadgen ejection` with a 5 s unrecorded warm-up so
t=0 is already steady state) while a controller drives a fault timeline (`eject b` → `rejoin b` →
`eject all` → `restore all`) and the driver buckets each upstream's served-count and the 503/s
every second:

![Load balancing under fault injection](img/ejection_timeline.png)
![Fail-closed 503s during total ejection](img/ejection_failed.png)

- **Even baseline.** ~4k req/s split three ways while healthy (1,334/1,333/1,333 this run).
- **Graceful ejection.** When **b** is driven unhealthy its share falls to zero within ~1 s (a
  one-second mixed transition bucket — `a=1534, b=933, c=1533` at t=15, then clean) and the
  survivors (a + c) absorb the full load **with zero failed requests** — this run they split it
  **evenly** (2,000/2,000), round-robin over two survivors landing on an even split.
- **Fail-closed, not fail-open.** With **every** instance unhealthy, Plecto Proxy returns **HTTP 503**
  promptly (no hang, no blind forward); the 503/s line jumps to the full offered rate (4,000/s here).
- **Fast recovery.** Restoring health returns instances to rotation within ~1 s (a one-second mixed
  bucket at t=60 — `a=280, b=280, c=280, failed=3160`, then clean 1,333/1,334/1,333 at t=61).

*(Re-measured 2026-10-24 (commit `1061a79`, tag **v0.15.2**). Over the 75 s T2 fault-timeline run: 300,000 responses, 60,120 failed (20.04 %) during total ejection, across events eject b / rejoin b / eject all / restore all; over the 40 s T1 gate run: 160,000 responses, 24,040 failed (15.03 %), 0 stray failed.)*

## Endpoint-set swap under load (ADR 000044)

A different axis than the ejection run above: instead of an existing instance's *health* flipping,
the upstream's *configured address set itself* changes — the shape a periodic-DNS re-resolution
swap takes (`resolve_interval_ms`), reproduced here via `swap-bench`'s SIGHUP reload path (the same
`ArcSwap<Endpoints>` replacement, ADR 000044). Subject: a 4-instance harness (`a, b, c, d`) starting
with the pool `[a, b, c]`; `plecto-loadgen swap` holds a steady open-loop rate while, mid-run, the
manifest is rewritten to `[a, b, d]` (dropping `c`, adding the spare `d`) and reloaded via SIGHUP —
the same fixed-rate timeline + per-instance bucketing the ejection run uses, generalized to a
changing label set (`bench/perf/run-perf.sh`'s `swap` phase).

The per-pick cost this introduces — an `ArcSwap<Endpoints>` load on every LB pick, not just on a
reload — is isolated in the companion criterion micro-benchmark,
[`pick_under_swap_churn`](#0-micro-benchmarks-in-process-criterion), under continuous concurrent
swap churn (the worst case; an unchanged tick short-circuits to one atomic load + compare and isn't
exercised there).

![Endpoint-set swap under load](img/swap_timeline.png)

> Re-measured 2026-10-24 (commit `1061a79`, tag **v0.15.2**): a steady ~4k req/s open-loop while, at
> t=15 s (post-warmup), the manifest is rewritten `[a, b, c]` → `[a, b, d]` and SIGHUP-reloaded
> (64 workers — well under the per-IP cap, unaffected; same shape as every prior pass).

- **Zero client-visible failures.** All 240,000 responses over the 60 s run succeeded — **0 %
  failed** — even through the swap itself. Unlike a health-based ejection, nothing here ever needs
  to fail closed: `a` and `b` are unchanged addresses, so `reconcile` reuses their `Arc`s and
  health outright (ADR 000017's reuse rule), and only `d` starts pessimistic.
- **The swap completes within one second.** The transition second (t=15) shows a brief mixed
  bucket (`a=1628, b=1626, c=13, d=733`) as in-flight requests to `c` finish and the reconciled
  pool takes over mid-second; by t=16 the split is already clean — `c=0`, and `a` / `b` / `d` even
  at ~1,333 each — the same ~1 s time constant [ejection](#resilience-ejection--fail-closed) shows,
  because both paths funnel through the same `ArcSwap<Endpoints>` replacement.
- This confirms the read the [per-pick micro-benchmark](#0-micro-benchmarks-in-process-criterion)
  predicts: the swap itself is cheap and instantaneous from the client's perspective — the cost
  ADR 000044 introduces is the small continuous per-pick `ArcSwap` load, not a client-visible
  disruption at swap time.

## TLS termination

The same single-backend pass-through, re-run with rustls TLS termination, decomposed so the cost
of each layer is separable (oha; h1 client isolates the record/handshake split from h2
multiplexing). `plain (h1)` is the [plain HTTP/1.1 ceiling](#plain-http11-ceiling)'s keep-alive row,
not re-measured here.

![TLS vs plain](img/tls_vs_plain.png)
![TLS vs plain, tail latency](img/tls_tail.png)

| Variant | req/s | p50 | p99 | isolates |
| --- | --- | --- | --- | --- |
| plain (h1)               | 212,730 | 0.22 ms | 0.50 ms | [ceiling](#plain-http11-ceiling) keep-alive |
| TLS h1, keep-alive       | 144,025 | 0.33 ms | 0.71 ms | record layer + TLS I/O path |
| TLS h1, handshake/req    | 26,229  | 1.76 ms | 5.43 ms | oha, shared `ClientConfig` — see caveat below |
| TLS (h2)                 | 129,587 | 0.37 ms | 0.69 ms | h2 multiplexing over TLS |

The decomposition is the point. This run's ordering is clean — plain h1 keep-alive (212.7k) sits
above the TLS keep-alive rung (144.0k, ~68 % of plaintext): **within-TLS ratios** are the signal:
handshake/req is **~18 % of TLS keep-alive**, and **h2 is clean** (129.6k/s, ~90 % of TLS
keep-alive, p99 0.69 ms). The TLS-terminated path remains **crypto-/TLS-I/O-bound**;
native-path optimisations don't reach it. A client that funnels many VUs over a handful of
multiplexed connections can make h2 *look* far worse (head-of-line queueing, not server work);
measuring with a connection-per-concurrency client removes that artifact.

*(Re-measured 2026-10-24 (commit `1061a79`, tag **v0.15.2**) on **aws-lc-rs** (ADR 000051), `-c 50` —
well under the per-IP cap, unaffected. Qualitative story unchanged across every snapshot so far.)*

### Full vs resumed handshake (ADR 000052)

*(Not re-run this pass — `bench/perf/run-perf.sh`'s `tls` phase doesn't drive this
rung automatically; the numbers below are carried over unchanged from 2026-07-05 at commit `c635ed3`, tag **v0.5.0**.)*

The `handshake/req` row above no longer isolates a *true* full handshake: oha shares one rustls
`ClientConfig` across connections, and against a server issuing stateless TLS 1.3 session tickets
its "cold" connections silently resume once warm. `plecto-loadgen tls --mode full|resumed` gives
each rung explicit resumption control instead:

| Client resumption | req/s | p50 | p99 | resumed |
| --- | --- | --- | --- | --- |
| full (disabled)      | 22,099 | 2.06 ms | 4.36 ms | 0 % |
| resumed (enabled)     | 29,768 | 1.54 ms | 3.26 ms | 93.0 % |

A true full handshake (22.1k/s) is **~16–17 % slower** than the `handshake/req` row (26.2–26.5k/s) —
confirming it really was partly resumed. Enabling client resumption recovers **~35 % throughput**
over a true full handshake — **≈11.7 µs/connection** (`45.3 → 33.6 µs` in inverse throughput `1e6 / rps`; note that per the [metric definition](#1-load-balancing-fast-path) this is an inverse-throughput delta across concurrent connections rather than single-core server CPU time, and includes both the proxy's server-side `CertificateVerify` signing on `PROXY_CPUS=0-11` and the load generator's client-side certificate-chain verification on `GEN_CPUS=12-23`, which mixes 2 P-cores and 8 E-cores). That saving comes from skipping the
certificate chain + asymmetric signature generation/verification, **not** the ephemeral key exchange: rustls's client
hardcodes `psk_dhe_ke` and never offers plain `psk_ke` ([RFC 9846 §4.3.9](https://www.rfc-editor.org/rfc/rfc9846#section-4.3.9), obsoleting RFC 8446 where this lived at §4.2.9; for forward secrecy of PSK + (EC)DHE/KEM see [RFC 9846 §2.2](https://www.rfc-editor.org/rfc/rfc9846#section-2.2), [RFC 9257 §4.2](https://www.rfc-editor.org/rfc/rfc9257#section-4.2) and [§6](https://www.rfc-editor.org/rfc/rfc9257#section-6), and rustls `client/hs.rs`'s source comment — *"such connections don't have forward secrecy"*), so every resumed handshake here still runs a fresh ephemeral key exchange for forward secrecy. Because both `plecto` (`crates/control/src/tls.rs`) and `plecto-loadgen` build `rustls` with `aws-lc-rs`'s default `prefer-post-quantum` provider, that key exchange negotiates the hybrid post-quantum group **`X25519MLKEM768`** (ML-KEM-768 + X25519; emitted as `kx_group` by `plecto-loadgen tls`) rather than classical ECDHE alone. The ~11.7 µs inverse-throughput delta is consistent with that: skipping only the asymmetric
sign/verify + cert bytes is a much smaller saving than skipping the hybrid `X25519MLKEM768` key exchange too would be. The residual
7 % full handshakes are cold-cache misses under concurrent load.

---

# 2. WASM extension plane

Plecto Proxy runs each request's *decision* — auth, rewriting, rate limiting, policy — as a sandboxed
**WebAssembly Component Model filter**, not native proxy code. This measures what that costs,
changing only **how the decision runs**. The bundled `bench/harnesses/bench-server` serves a **ladder** of
routes — all forwarding to the **same** backend — so each adjacent delta isolates one cost (the full
table is in [the cost ladder](#the-wasm-cost-ladder--isolating-each-cost) below): a native `/baseline`,
a pure no-op WASM filter pooled vs fresh (`/noop-pooled`, `/noop-fresh`), and the real `filter-apikey`
pooled vs fresh (`/trusted`, `/ondemand`).

`filter-apikey` is a real `plecto:filter` component: it reads `x-api-key`, stamps
`x-authenticated-user` on a valid key and forwards, or returns a typed `short-circuit` **401** on a
missing/invalid key. It is cosign-signed and loaded through the production verify-then-load path
(fail-closed). `filter-noop` returns `continue` with **no host-API calls** — it exists only to expose
the irreducible dispatch floor.

## The WASM cost ladder — isolating each cost

![Throughput by decision path](img/wasm_throughput.png)
![Per-request latency by decision path](img/wasm_latency.png)

> W1 — fixed 50 connections, 0 ms backend, valid key (oha, warm-up burned in a discarded 5 s
> pre-run). Full-throttle: read these rows as **throughput ceilings**; the honest latencies are in
> the fixed-rate tail table below.

Five routes forward to the **same** backend, so each **adjacent delta isolates exactly one cost**. A
pure **no-op** WASM filter (no host-API calls) is the key addition — it separates "the WASM tax" from
"a real filter's work", which older reports conflated.

| Route | Decision path | req/s | p50 | p99 |
| --- | --- | --- | --- | --- |
| `/baseline` | native fast path (no filter) | 212,730 | 0.22 ms | 0.50 ms |
| `/noop-pooled` | a **pure no-op** WASM filter, pooled | 170,316 | 0.29 ms | 0.47 ms |
| `/noop-fresh` | the same no-op, **fresh instance / request** | 13,102  | 3.74 ms | 7.27 ms |
| `/trusted` | the real `filter-apikey`, pooled | 142,813 | 0.34 ms | 0.52 ms |
| `/ondemand` | `filter-apikey`, fresh instance / request | 12,609  | 3.86 ms | 7.33 ms |

*(Re-measured 2026-10-24 (commit `1061a79`, tag **v0.15.2**), `-c 50` — well under the per-IP cap,
clean. `/baseline` is sourced from [ceiling.csv](#plain-http11-ceiling); the other four rungs are
measured together in the same session. This is the first T2 snapshot since inline header hooks
([ADR 000119](../docs/ADR/000119.md)) and default arena cap 32 ([ADR 000118](../docs/ADR/000118.md))
landed: `baseline` > every WASM rung, and the full-throttle floor reads directly; the fixed-rate tails
below remain the honest queueing-free read.)*

- **baseline → noop-pooled** = the **irreducible extension-plane dispatch cost**. Full-throttle,
  this run shows a **~20 % throughput** cost (212.7k → 170.3k, **≈ 1.17 µs/req** inverse-throughput
  delta [see metric definition](#1-load-balancing-fast-path) — matching the T1 gate's **`1.14–1.29 µs`** `dispatch_floor_us` across the five archived post-ADR-000119 gate runs); the fixed-rate tails put the queueing-free floor at
  **+0.01 ms p50 / +0.01 ms p99** at 7,565 req/s (`+0.013 ms p50 / +0.027 ms p99` in the T1 gate at 2,000 req/s). Every WASM filter pays this floor.
- **noop-pooled → noop-fresh** = the **per-request instantiation cost**, cleanly isolated from any
  host work: throughput collapses **~13×** (170.3k → 13.1k). This is what pooling buys.
- **noop-pooled → trusted** = a **real filter's own work** on top of the no-op (header parse +
  host-KV lookup + counter): **−16.1 % (~1.13 µs this run)** — matching the T1 gate's interleaved
  **`1.06–1.17 µs`** `apikey_cost_us` across the five archived post-ADR-000119 gate runs, and inside the calibrated A/B band (0.3–1.7 µs). With inline
  header hooks ([ADR 000119](../docs/ADR/000119.md)), the dispatch floor (`~1.17 µs`) and the apikey
  filter's own work (`~1.13 µs`) are now roughly equal terms. The first 8-round gate (2026-10-25) left
  `apikey_cost_us` *inconclusive* at `1.74 ± 1.13 µs`: six of its eight paired rounds read 0.95–1.34 µs, but in
  rounds 3 and 4 the `/trusted` window ran at 124.8k and 80.6k rps (round 4's `/baseline` also dropped to 169k), giving
  2.24 and 4.93 µs. A disturbance that hits one arm of a pair does not cancel, and the t interval correctly refuses a
  verdict. Per the gate's rule the re-run (`20261025T053124Z`, whose own 8 rounds read `1.06 ± 0.14 µs`) was **pooled** with these
  rounds rather than replacing them: n = 16 gives `1.40 ± 0.53 µs`, still inconclusive against the `0.3..1.7` band; a third pooled run (n = 24, reaching the gate's 3-run pooling cap) resolves it to
  **`1.30 ± 0.35 µs`, pass** — with the two disturbed rounds still in the estimate. (Because round-level pooling treats the 24 rounds as independent and does not model between-run cluster variance — at the run-mean level $n = 3$ gives `1.30 ± 0.94 µs`, which still straddles `1.7` — the independent non-pooled holdout run `20261025T061734Z` at **`1.06 ± 0.09 µs`**, $n = 8$, is the decisive check.)
- **noop-fresh and ondemand are the same order of magnitude** (13.1k vs 12.6k req/s — within ~3.8 %),
  confirming instantiation dominates the fresh path — the filter's per-request work is
  noise next to re-paying `init` (~28.5 µs, this pass's fresh criterion figure) every request.

### The same ladder at one fixed rate — honest tails

> W1b — every rung offered the **same** fixed **7,565 req/s** this run (60 % of the slowest rung's
> ceiling, `/ondemand` at 12,609/s), 50 connections, oha `-q` + `--latency-correction`
> (coordinated-omission-safe). Identical offered load, so the latency columns are directly
> comparable.

| Route | achieved | p50 | p90 | p99 |
| --- | --- | --- | --- | --- |
| `/baseline` | 7,564/s | 0.10 ms | 0.15 ms | 0.22 ms |
| `/noop-pooled` | 7,564/s | 0.11 ms | 0.15 ms | 0.23 ms |
| `/trusted` | 7,564/s | 0.14 ms | 0.22 ms | 0.32 ms |
| `/noop-fresh` | 7,565/s | 0.60 ms | 1.42 ms | 4.19 ms |
| `/ondemand` | 7,565/s | 0.56 ms | 1.16 ms | 2.64 ms |

At a rate every rung sustains (7,565 req/s), the pooled dispatch floor costs **+0.01 ms p50 / +0.01 ms p99** over
native (`0.1006 → 0.1100 ms` p50, `0.2193 → 0.2252 ms` p99) and the real pooled filter **+0.04 ms p50 / +0.10 ms p99** — well under 0.35 ms at p99.
The fresh rungs live at **p99 ~2.6–4.2 ms** at 7,565 req/s (`~12–19×` the pooled p99), reflecting the per-request instantiation and kernel/allocator cost described below. (Note why `/ondemand` reads slightly lower than `/noop-fresh` here — `0.56 vs 0.60 ms` p50 and `2.64 vs 4.19 ms` p99: on the fresh path, per-request instantiation at `~28.5 µs` plus kernel/allocator contention is `>25×` larger than `filter-apikey`'s `~1.13 µs` guest work, so the ordering is most plausibly run-to-run jitter in concurrent instantiation swamping the tiny guest-work difference — a single fixed-rate window per rung cannot separate the two. Adjacent deltas between the fresh rungs are not a reliable isolation of one cost at this rate.)

> **Why per-request instantiation scales poorly under concurrency — and what is (and is not) known about the knee in `v0.15.2`:**
> In the original `2026-07-06` investigation (under legacy `MALLOC_ARENA_MAX=4` and blocking-pool `spawn_blocking` dispatch), a fresh instance (`Allocation::OnDemand`) triggered heavy `mmap`/`munmap` + `mmap_lock` serialization and cross-core TLB shootdowns (`~31–35` TLB shootdowns/req in `/proc/interrupts` vs `~0.3` pooled), causing p99 to blow up from `1.4 ms` at `1k/s` and `4.7 ms` at `2k/s` to `~650 ms` at `4.2k/s` and `~1.2 s` at `6k/s`. In the `2026-10-24` (`v0.15.2`) snapshot above, **7,565 req/s** now completes at **p99 = 2.64–4.19 ms** — more than 150× below the old `4.2k–6k/s` cliff, so the mechanism behind the 07-06 knee no longer describes the current fresh tail. Three changes landed in between and are candidate causes — inline header hooks ([ADR 000119](../docs/ADR/000119.md), which also cut proxy threads from 80 to 35 per its scheduling measurement), the default allocator arena cap `4 → 32` ([ADR 000118](../docs/ADR/000118.md)), and Wasmtime 50.0.0 ([ADR 000120](../docs/ADR/000120.md)) — but **none of them has been isolated as the cause**, and where the knee now sits has not been measured. Even with that shift, fresh instantiation at 7,565 req/s remains **~12–19× slower at p99** (`2.64–4.19 ms` vs `0.23–0.32 ms`) and **~11–13× lower in ceiling throughput** than Wasmtime's pooling allocator (which pre-maps slots and avoids per-request virtual-memory churn). *(Re-measuring TLB shootdowns/req from `/proc/interrupts` deltas — unprivileged — at 1k / 2k / 4k / 6k / 7.5k req/s is the prerequisite for any causal statement here.)*

**The µs/req deltas are the invariants to track for regressions, not the percentages** (which widen or
shrink whenever the *baseline* moves). These macro deltas **reconcile with the in-process
[micro-benchmarks](#0-micro-benchmarks-in-process-criterion)** in direction and general scale, while
remembering that the 1.17 µs/req is an inverse-throughput delta under 50-connection saturation (~+58.5 µs mean latency across 50 connections), whereas a single-thread criterion call (~2.09 µs) is in-process wall-clock time on one core. The fresh ~28.5 µs, by contrast, is the *uncontended* single-thread cost — criterion
instantiates sequentially, so it never pays the `mmap_lock` contention or cross-core shootdowns the
concurrent macro run exposes (the knee above). The layers agree once that kernel-side term is named.

> **Decomposed 2026-09-29 (inline header hooks, [ADR 000119](../docs/ADR/000119.md)) & confirmed in T2/T1 on 2026-10-24 (`v0.15.2`, wasmtime 50.0.0 + WASI 0.3, [ADR 000120](../docs/ADR/000120.md) / [ADR 000121](../docs/ADR/000121.md)):**
> Prior to ADR 000119, the ~4.5–4.9 µs/req dispatch floor was mostly the blocking-pool round trips (two per filtered request: on-request
> and on-response) rather than guest execution. Trusted, non-blocking filters now run inline on the
> Tokio worker. On the reference host (i7-13700K, 6 P-cores, 50 conns), dispatch floor dropped from
> 4.06–4.21 to 1.41–1.44 µs/req at landing, and per-request scheduling collapsed: voluntary context switches 5.09 → 0.79/req (baseline 0.87–0.92),
> CPU 103 → 76 µs/req, runqueue wait 76 → 0.7 µs/req, and proxy thread count 80 → 35. In the 2026-10-24 `v0.15.2` snapshot (wasmtime 50.0.0 +
> dual WASI 0.2/0.3 capability host), both the T2 cost ladder above (`1.17 µs/req` dispatch floor, `1.13 µs/req` apikey cost) and the T1 gate confirm the ~1.1–1.3 µs/req inline dispatch floor: the 2026-10-24 3-round gate (`20261024T042039Z`) re-judged under the 95 % Student-t interval ([ADR 000122](../docs/ADR/000122.md)) reads `dispatch_floor_us` **1.29 ± 0.26 µs** (n = 3, pass), `apikey_cost_us` **1.06 ± 0.34 µs** (n = 3, pass), `ratelimit_tax_us` **1.20 ± 0.80 µs** (n = 2, inconclusive; old half-ranges were `± 0.0970` / `± 0.1228` / `± 0.0630 µs`), and fixed-rate 2,000 rps `pooled_tail_p50_ms` **+0.0131 ms** (`9 pass, 1 inconclusive`), while the 2026-10-25 8-round paired gate passes all 10 invariants both in the 3-run pooled estimate (`dispatch_floor_us` **1.24 ± 0.05 µs**, `apikey_cost_us` **1.30 ± 0.35 µs**, `ratelimit_tax_us` **1.06 ± 0.14 µs**, n = 24) and in the standalone holdout run (`20261025T061734Z`, `dispatch_floor_us` **1.17 ± 0.12 µs**, `apikey_cost_us` **1.06 ± 0.09 µs**, `ratelimit_tax_us` **1.49 ± 0.54 µs**, n = 8).

## Short-circuit: rejecting bad traffic at the edge

![Accept vs reject latency](img/wasm_shortcircuit.png)

> W2 — fixed 2000 req/s, 15 ms backend, ~90 % valid / ~10 % bad keys (k6). 108,117 accepted, 11,907
> rejected, **0 status-less** — a **90.1 % / 9.9 %** split, matching the script's own key roll.

| Path | p50 | p95 | p99 |
| --- | --- | --- | --- |
| accept (200, forwarded) | 16.097 ms | 16.900 ms | 17.476 ms |
| reject (401, short-circuited) | 0.075 ms | 0.178 ms | 0.780 ms |

Accepted requests cost the 15 ms backend plus the small pooled-filter + proxy overhead. Rejected
requests are decided **at the edge in 0.075 ms (p50) / 0.178 ms (p95)** (`wasm_mixed.csv`) and never reach the upstream: bad traffic is shed
**~215× faster at p50 (`16.097 / 0.075 = 214.6×`) and ~95× faster at p95 (`16.900 / 0.178 = 94.9×`)** than good traffic is forwarded, and is harmless to the backend it would otherwise
hit. (Filter faults or deadline overruns **fail closed** - 502/504 - exercised by the test suite,
not this benchmark.)

> **The 07-20 split (76 %/24 %) was a harness artifact, fixed and re-measured on 08-15 and reconfirmed 2026-09-25 / 2026-10-24.**
> That pass's `constant-arrival-rate` executor pre-allocated **300 VUs** — above ADR 000092's
> 256-connections-per-source-IP cap — and `bench/k6-wasm/mixed.js` counted *anything* not `200` as
> a rejection, so refused connections (`res.status === 0`, no HTTP status at all) were tallied
> alongside genuine 401s. The script now bounds its pool below the cap and counts a status-less
> response in its own `no_status` bucket, which the CSV carries; this pass reports **`no_status` =
> 0** and the split returns to the designed ~90/10. The 07-20 *latency* figures were always valid —
> only the count split was contaminated.

<a id="v030-response-ladder--compression"></a>
## Response hooks & compression (ADR 000073–000075)

The `all` pass measures every route with ADR 000073/074/075 **present but
unused**. This section fills the gap: what those features cost **when exercised**. Opt-in phase
(`bash bench/perf/run-perf.sh v03`) — not part of `all`, so a full refresh stays heavy while this
row can be re-run alone. Same generators and CO-safe tail pattern as
[the WASM cost ladder](#the-wasm-cost-ladder--isolating-each-cost).

### Response-context read vs `replace` (ADR 000073)

> R1 — fixed 50 connections, 0 ms backend, tiny response (oha). Lean `filter-resp` (no host-API
> calls): `/resp-ctx` always *reads* the as-forwarded request snapshot then `continue`;
> `/resp-replace` does the same read then `replace`s with a synthesised **418** (marker header
> `x-plecto-resp-replace`). Control is same-session `/noop-pooled` (ignores the snapshot).

| Route | Decision path | req/s | p50 | p99 | µs/req |
| --- | --- | --- | --- | --- | --- |
| `/noop-pooled` | on-response unused params → continue | 112,955 | 0.42 ms | 0.83 ms | 8.85 |
| `/resp-ctx` | read as-forwarded snapshot → continue | 110,183 | 0.44 ms | 0.83 ms | 9.08 |
| `/resp-replace` | read + `replace` (418, 23 B body) | 105,742 | 0.45 ms | 0.84 ms | 9.46 |

*(Not re-run this pass — measured 2026-08-15 at commit `b2a89be` (tag **v0.9.0**) via `v03` prior to [ADR 000119](../docs/ADR/000119.md) inline header hooks and [ADR 000118](../docs/ADR/000118.md) default arena cap 32; `/noop-pooled` in this table is therefore `112,955 rps` (`8.85 µs/req`) from that v0.9.0 session rather than current `HEAD`'s `170,316 rps` (`5.87 µs/req`). Same-process adjacent deltas — do not splice onto an older or newer `wasm` CSV's noop row. On current `HEAD`, the T1 gate tracks the 2,000 rps fixed-rate tail delta `respctx_tail_p50_ms` directly (`+0.0010 ms` in the confirmation run's `gate.csv`, `−0.0008 … +0.0011 ms` across the five archived post-ADR-000119 gate runs). µs/req = 1e6 / rps inverse throughput; see [definition](#1-load-balancing-fast-path).)*

- **noop-pooled → resp-ctx ≈ +0.22 µs/req** in that v0.9.0 `v03` run — the cost of *using* the ADR 000073 request
  snapshot on `on-response` (path length + header scan), with the same continue/forward path. The
  three snapshots that have measured this rung read **+0.90 / +0.10 / +0.22 µs**, all small enough
  that the run-to-run movement of the ceiling itself is a comparable term; track the fixed-rate p50
  below alongside this figure, not this number alone. The T1 gate's `respctx_tail_p50_ms`
  invariant (**`+0.0010 ms`** over pooled in `gate.csv` on `v0.15.2`, `+0.016 ms` in v0.9.0) is the tighter read.
- **noop-pooled → resp-replace ≈ +0.60 µs/req** net at full throttle in that v0.9.0 (`2026-08-15`) `v03` session — unchanged from the preceding
  `v03` run to two decimals. `replace` synthesises a tiny body and **drops** the upstream payload on the
  wire (verified: `Content-Encoding` N/A, 23-byte 418). That wire-shape change can *under-* or
  *over-read* replace's guest/host work relative to resp-ctx run to run; do **not** read either
  run's number as a pure CPU claim. The regression signal for replace is "still within ~1 µs of
  the pooled no-op on this host," which held in that v0.9.0 `v03` session.
- Fixed-rate tails at **63,444/s** (60 % of this ladder's slowest ceiling; oha `-q`
  `--latency-correction`): all three rungs hold the offered rate; **p50 stays ~0.88–0.91 ms**. p99
  at this rate is host-noise-dominated on this session (8–18 ms) — same caveat as other
  high-rate fixed runs near a host knee; prefer µs/req + p50 for this row.

### Native response compression (ADR 000074 / 000075)

> R2 — same oha shape; backend **4 096 B** repeating `text/plain` (above the 1024-byte
> `min_length` default). Both routes send `Accept-Encoding: gzip`. `/baseline` has no
> `[route.compression]` → identity; `/compress` pins `algorithms = ["gzip"]`. Wire check:
> identity 4096 B vs gzip **45 B** + `Content-Encoding: gzip` + `Vary: accept-encoding`.

| Route | Transform | req/s | p50 | p99 | µs/req |
| --- | --- | --- | --- | --- | --- |
| `/baseline` | identity (AE advertised, opt-in off) | 192,892 | 0.25 ms | 0.46 ms | 5.18 |
| `/compress` | gzip (level 5, ADR 000075 defaults) | 133,135 | 0.36 ms | 0.63 ms | 7.51 |

- **baseline → compress ≈ −31.0 % ceiling / +2.33 µs/req** (inverse throughput; see [definition](#1-load-balancing-fast-path)) for this highly compressible 4 KiB
  filler — the third pass in a row within ~0.25 µs of the same figure (+2.11 / +2.09 / +2.33). Real
  HTML/JSON ratios and CPU will differ; this row is a **regression floor** for the opt-in path, not
  a capacity guide for production payloads. RFC 9411 §7.3-style: one object size, sustainable
  throughput, method disclosed.
- Fixed-rate at **79,881/s** (60 % of compress ceiling): both hold the rate; p50 ≈ 0.94 ms;
  p99 again host-noise-band at this offered load (87 ms identity / 116 ms gzip — this session's
  neighbour load shows up hardest in the highest-rate fixed run in the report) — µs/req from the
  ceiling table is the durable signal.

**Criterion note (not re-run here).** Day-to-day criterion absolute drift (±10–20 %) is expected
without a locked governor. To attribute ADR 000073 contract-surface cost in-process, use a
same-host baseline pair — `cargo bench -p plecto-host -- --save-baseline pre-adr73` on the
pre-landing commit, then `--baseline pre-adr73` after — per
[criterion baselines](https://bheisler.github.io/criterion.rs/book/user_guide/command_line_options.html)
([`bench/methodology.md`](../bench/methodology.md)).

## Outbound ext_authz (ADR 000036)

*(Not measured this pass — load numbers deferred; capability verified functional by host test suite.)*

A filter can call an external authorization service per request over the lent, SSRF-guarded outbound
capability (`filter-extauthz`). Per-request cost is three parts, only the first two Plecto Proxy's: the
WASM tax (the same [cost ladder](#the-wasm-cost-ladder--isolating-each-cost)), the outbound gate
(allowlist + SSRF classification — nanoseconds, negligible), and the network round-trip to the authz
endpoint, which dominates and is the *operator's* latency, not Plecto Proxy's.

Load numbers are deferred rather than faked: the SSRF guard blocks loopback by design, so a hermetic
mock authz needs a non-loopback endpoint (environment-specific), and the connector currently opens a
new connection per call (pooling is a follow-up). The capability itself is verified end-to-end by
the host's `outbound-http` test suite (allowlist deny + DNS-rebinding SSRF block).

## Host-enforced rate limiting

Plecto Proxy's rate limiter is a **host-native token bucket** (ADR 000026): the bucket spec
(`capacity` / `refill_tokens` / `refill_interval_ms`) is configured **in the operator's manifest**,
not by the filter — an untrusted filter passes only `(key, cost)` and so cannot widen its own limit.
The refill + counting stay host-side (the WASM boundary is not crossed on the hot path); the filter
only decides *whether* to consult the limiter and *on what key*. Driven through `bench/harnesses/bench-server`
(`filter-hello`, pooled); a `429` carries `retry-after-ms`.

> **Scope: single node.** Every run below drives one `plecto` instance. The bucket is **node-local**
> ([ADR 000053](../docs/ADR/000053.md)) — the enforcement and fairness numbers describe what one
> instance guarantees, not a multi-replica fleet. Behind a load balancer fanning out to N replicas,
> the fleet's effective allowed rate scales with N unless the front LB pins a key to one replica; see
> the [hardening guide](../docs/hardening.md) for the operational formula.
>
> **Scope: in-memory state backend.** These numbers (and every host-state number in this report) run
> the default `[state] backend = "memory"`. With `backend = "redb"`, the backend write happens
> **inside the process-wide quota lock** (`charge_and_apply` — the price of closing the CWE-770
> accounting race), so every host-kv / counter / rate-limit call across all filters serializes
> behind that disk write. Persistent-state throughput under concurrency is structurally different
> and **unmeasured here**.

### Overhead — the cost of consulting the bucket

> R1 — 50 VUs, 0 ms backend, a **never-deny** bucket spread across 1000 keys (k6). `/baseline` vs
> `/ratelimit`.

| Route | Interleaved T1 gate (`20261024T042039Z-1061a79`, 2 rounds) | Single-round T2 (`ratelimit_overhead.csv`) | T2 p50 | T2 p99 |
| --- | --- | --- | --- | --- |
| `/baseline` (no filter) | **150,829 / 151,434 req/s** (`6.62 µs/req`) | 123,603 req/s | 0.28 ms | 1.56 ms |
| `/ratelimit` (bucket) | **128,804 / 127,213 req/s** (`7.81 µs/req`, **`+1.20 ± 0.80 µs/req`** 95 % t, n = 2; `± 0.06` half-range) | 127,519 req/s | 0.31 ms | 1.09 ms |

![Rate-limit overhead](img/ratelimit_overhead.png)
![Rate-limit overhead, tail latency](img/ratelimit_overhead_tail.png)

With inline header hooks ([ADR 000119](../docs/ADR/000119.md)), the rate-limit tax is small enough
(**`1.12–1.30 µs/req`** across the five archived post-ADR-000119 gate runs) that an un-interleaved single-round k6 closed-loop run (`ratelimit_overhead.csv`, plotted above) can see `/baseline`
dip slightly below `/ratelimit` from between-window host variance (123.6k vs 127.5k req/s; p50 moves
from 0.28 ms to 0.31 ms). That is why the paired gate rounds are the authoritative read for this invariant: in the
confirmation gate run (`20261024T042039Z-1061a79`, `/baseline` 150.8k / 151.4k rps vs
`/ratelimit` 128.8k / 127.2k rps) `ratelimit_tax_us` is **`1.20 µs`**, but with only two rounds its 95 % t interval is
**`± 0.80 µs`** (the `± 0.0630` originally printed was the half-range, ≈ 1 SE) — the interval straddles the band edge
`0.9` then in force (and today's `0.5`), so the honest verdict is *inconclusive*; at n = 2 every archived run straddled `0.9`, and three of five still straddle `0.5` (`VUS=50`, well under ADR 000092's per-IP cap, unaffected).
The gate now runs eight paired rounds for this reason; the first such run (2026-10-25, `20261025T052103Z`) reads
**`1.08 ± 0.15 µs`** (n = 8, pass), and pooled over three runs **`1.06 ± 0.14 µs`** (n = 24, pass) — still below the pooled
`dispatch_floor_us` (`1.24 ± 0.05 µs`). Two of the three runs were inconclusive on this invariant by themselves under the old lower edge `0.9` (interval lower edges
`0.757` / `0.709`, i.e. on the *fast* side). The band's lower edge was therefore moved to `0.5` (2026-10-25,
[ADR 000122](../docs/ADR/000122.md)): that only admits a cheaper tax, the slow-side guard `2.4` is unchanged, and "the filter
stopped limiting" is caught by `enforce_allowed_ratio`, not by this edge. Under `0.5` all three runs pass on their own, and an independent holdout gate run (`20261025T061734Z`, n = 8, no pooling) confirmed the band by passing all 10 invariants on the first try (`ratelimit_tax_us` **`1.49 ± 0.54 µs`**, `dispatch_floor_us` **`1.17 ± 0.12 µs`**, `apikey_cost_us` **`1.06 ± 0.09 µs`**).

**This tax is not "dispatch floor + bucket consult".** Additively it could never be smaller than the floor, yet the k6
`ratelimit_tax_us` reads below the same run's oha `dispatch_floor_us` in 7 of the 9 gate runs since ADR 000119 (`1.20` vs
`1.29 µs` in the 10-24 confirmation run; the exceptions are `20261024T033147Z`, 1.30 vs 1.14, and the `20261025T061734Z` holdout, 1.49 vs 1.17). The two are measured by different generators: oha (`-c 50`) drives `/baseline` to ~212.7k rps, while k6 (`VUS=50`
over 1,000 tenant keys) reaches ~117k–155k rps on `GEN_CPUS=12-23` across the gate runs (≈151k in the confirmation run) — near its own ceiling. In a closed loop
whose generator is near saturation, extra proxy work is partly absorbed by the generator waiting less, which compresses
$\Delta(1/\text{RPS})$; that is the most plausible reading, not a measured decomposition. Either way, deltas from different
generators cannot be added or subtracted. A candidate generator-independent read is proxy **CPU-µs/req at a fixed rate**,
which the gate now records as `ratelimit_cpu_us_per_req` (20k/s, `bench/perf/proc_cpu.py`; `info` until calibrated). Each snapshot records both per-thread `/proc/<pid>/task/*/schedstat` (ns resolution) and process-wide `/proc/<pid>/stat` (`utime + stime`), falling back to `stat` whenever a thread present at window start has exited by window end; across all 32 windows of the four 2026-10-25 runs, `schedstat` and `stat` agree to within `~0.1 µs/req`.
Three pooled runs (2026-10-25, 12 paired rounds, no pre-window idle sleep) read **+15.3 ± 1.0 µs CPU per request** over a `/baseline` of 32.3 ± 0.4 µs, and the standalone holdout (`20261025T061734Z`, 4 paired rounds with `GATE_CPU_SETTLE_S=11` — now recorded in `gate_params.txt` / `build_id.txt` so runs with different settle times cannot be pooled) reads **+18.3 ± 0.8 µs** over a `/baseline` of 34.5 ± 0.7 µs
(absolute values include the in-process mock upstream; the delta cancels it). Note why the holdout reads ~7 % higher on both `/baseline` (`32.3 → 34.5 µs`) and `/ratelimit` (`+15.3 → +18.3 µs`) even though `schedstat` and `stat` match in both regimes: the 11 s idle settle after the ~128k req/s closed-loop phase lets the cores clock down under the `powersave` governor before entering the light 20k/s 3 s windows (lower clock frequency means more nanoseconds on-CPU per instruction). Also note why `/ratelimit` spawns a few blocking-pool threads (`26 → 29 → 32` across the first two windows, then flat at `32`): `filter-hello` (`Isolation::Trusted`, `MemoryBackend`) runs its header hooks inline on the Tokio worker (`runs_inline = true`), and only the once-per-65,536-calls instance recycle (`max_requests_per_instance = 65,536`, [ADR 000119](../docs/ADR/000119.md) Decision 2) hands off replacement instance creation (`IdleMiss::PoolFilling`) to `spawn_blocking`. Its additivity is not yet verified: at 20k/s the
proxy is far from saturated, so the figure can include per-request wake-up and recycle costs that amortize differently under load, and ~15–18 CPU-µs spread over 12 cores (≈ 1.27–1.53 µs) only matches the inverse-throughput
deltas in order of magnitude — a consistency hint, not an equivalence. What the tax *contains* is still the filter dispatch plus the host-native bucket consult,
including the per-call host-state quota check (ADR 000027) that keeps a multi-tenant filter's bucket count bounded —
only the arithmetic across generators is invalid.

### Enforcement — does it actually hold the rate?

![Rate-limit enforcement](img/ratelimit_enforce.png)

> R2 — a **tight** bucket (refill 1000 tok/s, burst 2000), offered **5000 req/s** open-loop at one
> key for 30 s (k6).

| offered | allowed (200) | shed (429) | status-less | accept p99 | 429 p99 |
| --- | --- | --- | --- | --- | --- |
| 5,000/s | **1,033/s** | 79.3% | **0** | 0.21 ms | 0.15 ms |

Offered 5× over the limit, the **allowed throughput converges to the bucket's discrete refill
schedule**: in `crates/host/src/backend.rs` (`apply_bucket`), the first request on a fresh key initializes `(tokens = capacity, last_refill = t_0)` and refills occur only at discrete integer multiples of `refill_interval_ms = 1,000 ms` (`intervals = now_ms.saturating_sub(last) / refill_interval_ms`). Over a window of $W$ seconds under continuous overload, the bucket therefore grants `capacity + (W - 1) × refill_tokens` requests:
- **30 s T2 report window**: `2,000 + 29 × 1,000 = 31,000` allowed requests (`31,000 / 30 s = 1,033.33 req/s`), **the same 1,033/s as every prior snapshot**.
- **10 s T1 gate window**: `2,000 + 9 × 1,000 = 11,000` allowed requests (`11,000 / 10 s = 1,100.0 req/s`). Historically, `gate_verdict.py` divided by a continuous approximation `(capacity + W × refill) / W = 1,200.0 req/s`, which is why every historical `gate.csv` recorded `enforce_allowed_ratio = 1,100 / 1,200 = 0.9166`; `gate_verdict.py` now uses the exact discrete-tick formula `(capacity + (W - 1) × refill) / W = 1,100.0 req/s` (`ratio = 1.0000`).
- **Operator guidance on refill granularity:** Because `refill_interval_ms = 1,000` grants all 1,000 refill tokens at once on each 1-second boundary under continuous overload, operators who want smoother sub-second pacing to upstream backends can configure a finer `refill_interval_ms` with the same rate (for example, `refill_tokens = 10` and `refill_interval_ms = 10` for 1,000 tokens/s).

The accounting is complete this pass: 31,000 allowed + 119,002 shed = **150,002**,
accounting for the 5,000/s × 30 s offered load, with **zero status-less responses** and **79.3 % shed**
(accept p50 0.10 ms, accept p99 0.21 ms, 429 p99 0.15 ms).

> **This row was contaminated on 2026-07-20 and is repaired here (2026-08-15).** That pass reported
> 59.6 % shed because `bench/k6-wasm/ratelimit-enforce.js` counted only `status === 200` and
> `status === 429`, dropping everything else — and its `preAllocatedVUs` (`max(200, RATE/10)` =
> **500** for a 5,000 req/s offer) was almost double ADR 000092's 256-per-IP cap, so **48.8 % of
> attempts produced no HTTP status at all** and vanished from both counters. The pool is now bounded
> below the cap and status-less responses have their own tracked `no_status` bucket. The repaired
> figure — **79.3 %** — reproduces the pre-cap 07-11 snapshot's 79.3 % exactly, which is the
> strongest available evidence that the cap interaction, not the proxy, was the whole story.

### Fairness — one key cannot starve another

![Rate-limit fairness](img/ratelimit_fairness.png)

> R3 — same tight bucket; two keys concurrently: a **hot** key offered 4000/s and a **light** key
> offered 500/s (k6).

| key | offered | allowed (200) | shed | status-less |
| --- | --- | --- | --- | --- |
| hot | 4,000/s | 1,033/s | 74.2% | **0** |
| light | 500/s | **500/s** | 0% | **0** |

State is **per key**, and this pass measures it end to end rather than inferring it: the hot key is
throttled to **its own** refill rate (1,033/s — the same figure the single-key
[enforcement](#enforcement--does-it-actually-hold-the-rate) run produces, so the light key's traffic
costs it nothing), while the light key **receives every request it offers** — 500/s offered, 500/s
allowed, zero 429s, zero status-less. A hot neighbour cannot starve a light one.

> **The 07-20 pass could not show this** (light read 145/s against a 500/s offer, with 0 % shed):
> the two scenarios share one k6 process and one loopback source IP, and `hot`'s pool alone
> (`max(200, 4000/10)` = **400**) exceeded ADR 000092's 256-per-IP cap, so the light key's
> connections were refused at accept — starvation by the harness's own connection budget, dressed
> up as a fairness result. With both pools bounded below the cap (150 hot / 50 light) the intended
> claim is now the measured one.

## Request body handling

The request-side **body hook** (`on-request-body`, ADR 000025) follows a *buffer-then-decide* model:
for a filtered route carrying a body, the host buffers it (bounded — 16 MiB cap, fail-closed 413),
runs the filter's `on-request-body`, and forwards the possibly-transformed body — or short-circuits
before upstream. `filter-hello` uppercases the body (a real transform) or 403s on a `deny-body`
marker. A bodyless request, a filter-less route, and — since ADR 000038 — a route whose filters are
**all header-only** (none exports `on-request-body`) keep the zero-copy streaming path: the host
decides from the component's exports whether any filter reads the body, and buffers only then.

![Request body hook](img/body.png)
![Request body hook, tail latency](img/body_tail.png)

> B — 50 VUs, POST a `SIZE`-byte body at 1 KB / 100 KB / 1 MB (k6), to `/body` (filter-hello buffers +
> transforms), `/body-headeronly` (a header-only filter — body streams through, ADR 000038), and
> `/baseline` (no filter). Default allocator arena cap of **32** ([ADR 000118](../docs/ADR/000118.md), amending ADR 000038's earlier cap 4).

| size | route | req/s | throughput | p99 |
| --- | --- | --- | --- | --- |
| 1 KB   | /baseline        | 149,041 | 153 MB/s  | 1.16 ms |
| 1 KB   | /body            | 106,002 | 109 MB/s  | 1.16 ms |
| 1 KB   | /body-headeronly | 121,852 | 125 MB/s  | 1.22 ms |
| 100 KB | /baseline        | 43,125  | 4416 MB/s | 4.05 ms |
| 100 KB | /body            | 19,525  | 1999 MB/s | 5.17 ms |
| 100 KB | /body-headeronly | 41,959  | 4297 MB/s | 4.14 ms |
| 1 MB   | /baseline        | 5,925   | 6213 MB/s | 32.5 ms |
| 1 MB   | /body            | 2,140   | 2244 MB/s | 40.3 ms |
| 1 MB   | /body-headeronly | 5,795   | 6076 MB/s | 33.0 ms |

A filter that **reads** the body pays for it, growing with payload: **~29 % throughput at 1 KB** (the
buffer + WASM transform dominate the small request), **~55 % at 100 KB**, **~64 % at 1 MB** (a
full-body copy + uppercase per request). A **header-only filter takes the zero-copy bypass** — the
body never enters guest memory: at 1 MB it lands **within ~2.2 % of `/baseline`** (ADR
000038 — the two paths are within run-to-run noise at this size); at 100 KB
the gap is **~2.7 %** (`VUS=50`, well under the per-IP cap); at 1 KB (`−18.2 %`) it reflects
the ordinary **WASM dispatch floor** on a tiny request, not a body cost. RSS at 1 MB × 50 VUs (fresh
proxy per route, default arena cap 32 per [ADR 000118](../docs/ADR/000118.md); proxy + in-process upstream, one mid-load sample): **~122 MB `/baseline` · ~296 MB `/body` · ~117 MB
`/body-headeronly`**
(`data/body_rss.csv`). The export-presence bypass keeps a header-only route near baseline. The buffer
stays bounded (16 MiB cap, fail-closed 413) for the filters that do read the body. The remaining
buffered-path copy is addressed by the opt-in `stream<u8>` streaming-body capability (ADR 000020 / ADR 000121); a per-request
time-series / allocator-sweep decomposition lives in `bench/perf/mem_matrix.py`. For choosing an
arena default, use `bench/perf/arena_sweep.py`: it isolates the upstream in a separate process and
records proxy-only loaded peaks and post-load retention for every candidate. The snapshot above
is neither a proxy-only peak nor a post-load retention measurement; see the
[measurement procedure](../bench/methodology.md#2026-09-27--アリーナ既定値を判断するための分離計測t3).

## Footprint

Idle resident set and the marginal cost of an open connection (`bench/harnesses/bench-server`):

| Metric | Value |
| --- | --- |
| idle RSS | ~42 MB |
| RSS holding keep-alive connections | ~48 MB (250 conns) |
| marginal bytes / connection | ~25.4 KB |

*(Re-measured 2026-10-24 (commit `1061a79`, tag **v0.15.2**). The phase asks for **250** connections (under
ADR 000092's 256-per-IP cap, so every connection it counts is one the proxy actually holds) and
divides by the count the generator reports as open. At 25.4 KB/conn (25,395 bytes/conn) the
figure matches the historical ~24.8–25.6 KB/conn, and idle RSS (~42 MB, 41,992 kB; 48,192 kB with 250 connections) tracks expected baseline memory.)*

---

# 3. Realistic & protocol coverage

## Weighted request mix — with its own baseline

> M1 — open-loop 20k req/s, a weighted blend across routes on one gateway (k6): read-heavy, partly
> edge-checked (per-tenant rate-limit keys, 200 tenants, never-deny bucket), occasional writes,
> rare large payloads. Paired with a **read-only control at the same arrival rate** — 100 % plain
> reads — so the per-class deltas are attributable to the traffic *blend*, not the offered load.

| Profile | Class (share) | route | p50 | p99 | p99.9 |
| --- | --- | --- | --- | --- | --- |
| read-only (control) | read 100 % | GET `/baseline` (1 KB) | 0.057 ms | 0.191 ms | 0.513 ms |
| mix | read 60 % | GET `/baseline` (1 KB) | 0.063 ms | 0.225 ms | 0.909 ms |
| mix | auth read 25 % | GET `/ratelimit` (tenant key) | 0.081 ms | 0.260 ms | — *(exported in `weighted-mix.js` / `run-perf.sh` going forward)* |
| mix | write 10 % | POST `/body` (1 KB) | 0.108 ms | 0.301 ms | — *(exported in `weighted-mix.js` / `run-perf.sh` going forward)* |
| mix | large 5 % | POST `/body` (100 KB) | 0.352 ms | 0.683 ms | — *(exported in `weighted-mix.js` / `run-perf.sh` going forward)* |

Both profiles hold ~20k/s offered (from the run log: 20,000/s read-only with 433 dropped iterations out of 600,000 — `0.072%`; in k6's `constant-arrival-rate` a dropped iteration means the 240-VU pool was momentarily exhausted, i.e. a latency event on the control side that this run did not capture further — cause not determined; 20,000/s mix
with 0 dropped; zero 429s from the never-deny bucket, **zero status-less responses** — the honest
open-loop shed signal under the cap, see the note below). With inline header hooks ([ADR 000119](../docs/ADR/000119.md))
and default arena cap 32 ([ADR 000118](../docs/ADR/000118.md)), every class remains sub-millisecond through p99 (`0.225–0.683 ms`), and the recorded `read` class remains sub-millisecond at p99.9 (`0.909 ms` in `mix` vs `0.513 ms` in `read-only`; `auth`, `write`, and `large` p99.9 values were not yet exported in the 2026-10-24 snapshot and are now exported by `weighted-mix.js` / `run-perf.sh` going forward).
The pairing is the point: at the same rate,
**the blend costs the plain reads +0.034 ms at p99 (`0.191 → 0.225 ms`, or `0.19 → 0.23 ms` at 2 decimals) and +0.396 ms at p99.9 (`0.513 → 0.909 ms`, or `+0.40 ms` at 2 decimals)** this run — head-of-line pressure
from the body classes — and the classes order exactly as their work predicts (read < auth read < write <
large, monotone in both p50 and p99: `0.063 < 0.081 < 0.108 < 0.352 ms` p50, `0.225 < 0.260 < 0.301 < 0.683 ms` p99). A single-endpoint test hides all of this; the control run
keeps it honest.

> **The 07-20 figures for this section were diluted; these are not (2026-08-15, reconfirmed 2026-10-24).** That pass's
> `record()` folded **every** response's duration into the latency Trends regardless of status,
> while its VU pool (**500**, for a 20,000 req/s offer) sat above ADR 000092's 256-per-IP cap — so
> near-instant connection refusals were pulling the published percentiles *down*. `weighted-mix.js` now drops status-less responses into their own counter
> before the trend, and holds its pool at **240** — just under the cap. Running this phase materially above 20k req/s from a single source IP is not
> possible on this harness without crossing the cap.

## HTTP/3

The fast path terminates **HTTP/3 over QUIC** (ADR 000016; `tls-http` serves h1/h2/h3 on one port). A
functional check confirms it end-to-end:

```
curl --http3-only https://…/api/hello  ->  status=200 http_version=3
```

*(Re-confirmed 2026-10-24.)*

A **rigorous, coordinated-omission-safe H3 *load* benchmark is deferred**: default/distro `oha` builds and `k6` omit native
HTTP/3 (`oha` supports `--http-version 3` experimentally only when compiled from source with `--features http3`, and `h2load --alpn-list=h3` is closed-loop concurrency-based where `--rate` paces connection creation rather than open-loop request arrivals; adding an H3 subcommand to `plecto-loadgen` or using a custom `oha --features http3` build is tracked for T3). Rather than publish process-spawn-bound `curl`-loop numbers, the H3 load figure
stays absent until that tooling is wired into the harness — server support is verified, not throughput.

## WebSocket Upgrade tunnel (ADR 000048)

Plecto Proxy's HTTP/1.1 Upgrade path (ADR 000048): a route declaring `[route.upgrade] protocols =
["websocket"]` forwards the client's handshake (controlled re-issue — hop-by-hop stripping stays
the default for every other route), and on the upstream's 101 the proxy splices the two connections
into an opaque bidirectional byte tunnel — the same post-upgrade relay shape used by typical
L7 `Upgrade` / TCP tunnel modes. This is a **different load shape**
than every other scenario in this report: a long-lived, stateful connection instead of a
short-lived request, so it exercises axes nothing else here does — connection-permit accounting
(the connection admission guards, circuit breaker, and least-request in-flight counters follow the tunnel for its whole lifetime,
not just the handshake) and an activity-based idle timeout, rather than throughput-per-request.

`bench-server`'s `/ws` route tunnels to a dedicated mock upstream that completes the RFC 6455
handshake and echoes every frame; `plecto-loadgen`'s `ws` subcommand drives three sub-scenarios
(`bench/perf/run-perf.sh`'s `ws` phase):

- **Handshake rate** — open-loop paced Upgrade attempts/sec (the 101 handshake runs through the full
  filter chain like any other request, so it pays the same per-request cost the rest of this report
  measures — only the post-101 tunnel is new).
- **Tunnel footprint** — RSS held by concurrently open (idle) tunnels, the long-lived-connection
  analogue of [Footprint](#footprint)'s keep-alive connection measurement.
- **Echo throughput** — sustained request/response frames per held tunnel, at two payload sizes
  (1 KB / 64 KB), closed-loop per connection (the same concurrency model oha's `-c N` uses).

> **Measured 2026-10-24 (commit `1061a79`, tag `v0.15.2`); re-measured 2026-10-25 after two fixes that data led to ([ADR 000122](../docs/ADR/000122.md)):**
> 1. **Why `hold --conns 1000` succeeded despite `MAX_CONNECTIONS_PER_IP = 256` ([ADR 000092](../docs/ADR/000092.md)):** In `crates/server/src/listener.rs`, `_permit` (`MAX_CONNECTIONS`) and `_ip_guard` (`PerIpConnGuard`) lived on `serve_conn`'s stack. Because `hyper`'s `serve_connection(...).with_upgrades()` returns as soon as the `101 Switching Protocols` response headers are written and hands the upgraded I/O to `OnUpgrade` (which runs in the detached `tokio::spawn` task inside `upgrade_switch`), `serve_conn` returned immediately after each 101 handshake and **dropped `_ip_guard` and `_permit` while the WebSocket tunnel stayed open**. Both guards are now bundled into `Arc<ConnGuards>` (`listener.rs` → `dispatch.rs` → `proxy.rs` `upgrade_switch`) and moved into the spawned tunnel task for the full tunnel lifetime, and `run-perf.sh` `phase_ws` now uses `--conns 250` (matching [Footprint](#footprint)) and divides by the actual held count.
> 2. **Why `idle_rss` in `ws_footprint.csv` was `76.5 MB` (vs `42.0 MB` in `footprint.txt`):** `phase_ws` previously reused one `bench-server` process across `10.1 handshake` (10,000 upgrades at 500/s) and `10.2 tunnel footprint`, so `idle_rss = 76,452 kB` was read on a heap that had just served the 10,000-upgrade burst. The most likely reading of the 34.5 MB excess is freed-but-retained allocator memory (more likely with arena cap 32), which new tunnel allocations then reuse — making the marginal `~8.8 KB/tunnel` an underestimate. That is an inference, not an allocator measurement. `phase_ws` now starts with `stop_proxy` and runs `10.2 tunnel footprint` **first** on a freshly launched `bench-server`.

| Scenario | Result |
| --- | --- |
| Handshake rate (2026-10-25) | 10,000/10,000 Upgrades succeeded at the paced 500/s target — **0 % failed** over 20 s |
| Tunnel footprint (2026-10-25, fresh process, measured first) | idle RSS 42.1 MB → 47.3 MB with **250 held tunnels** (`ws hold: 250 tunnels`) — **~21.3 KB/tunnel** (`21,348 B`) |
| *(superseded: 2026-10-24)* | 76.5 MB → 85.0 MB across 1,000 tunnels (`8,778 B/tunnel`) — measured while upgraded tunnels escaped the per-IP cap, on a post-burst heap; not reproducible on the current tree |

![WebSocket echo throughput](img/ws_echo.png)
![WebSocket echo tail latency](img/ws_echo_tail.png)

| Payload | messages/s | throughput | p50 | p99 |
| --- | --- | --- | --- | --- |
| 1 KB  | 219,566 | 225 MB/s   | 0.18 ms | 1.06 ms |
| 64 KB | 60,133  | 3,941 MB/s | 0.80 ms | 1.65 ms |

The handshake rate holds at 100 % of the paced 500/s target (`10,000/10,000`, `0 % failed` over 20 s; note this validates zero-drop stability at 500 handshakes/s rather than measuring a maximum handshake ceiling). On a fresh process a held tunnel costs **~21.3 KB** of RSS — the same order as a held keep-alive connection in [Footprint](#footprint) (`~25.4 KB/conn`), and 2.4× the superseded post-burst `~8.8 KB`, which confirms that figure was an underestimate. For scale: each tunnel's relay (`tokio::io::copy_bidirectional`) eagerly allocates one 8 KiB buffer per direction (16 KiB), plus the two `Upgraded` I/O handles and the task itself. Echo throughput at 1 KB
(219.6k msg/s) is **3.7×** the 64 KB rate (60.1k msg/s) while aggregate byte throughput rises 17.5×
(225 → 3,941 MB/s; charts regenerated 2026-10-25 from `ws_echo.csv`), consistent with a per-message dispatch floor that amortizes better over larger
frames. The *shape* — small frames dispatch-bound, large frames bandwidth-bound — is the
part that has held across every snapshot.

*(A per-request small-frame delayed-ACK stall — the exact Nagle signature the
[connection-churn history](#plain-http11-ceiling) already found once — appeared during this
scenario's development on the mock upstream's accept-side socket; disabling Nagle there fixed it.
The numbers above are post-fix. The idle timeout (default 5 min, ADR 000048) and its interaction
with the breaker permit / least-request in-flight counters are exercised by the host's own test
suite, not this benchmark; a transfer-bytes / tunnel-duration metric is a documented observability
gap (ADR 000048's re-examine condition (d)) this report will pick up once it lands.)*

---

## Methodology — why the numbers look the way they do

(Builds on [Measurement setup](#measurement-setup) above — pinning, warm-up, open/closed-loop — with
what that setup buys.)

- **Track the invariant, not the headline.** The WASM tax and the rate-limit tax are ~µs/req (not a
  %), rate-limit enforcement converges to the configured refill rate, fairness is per-key isolation,
  resilience is ~time-constants, and round-robin is exact — these hold across hosts and generators,
  so a change in them is a real regression. A change in absolute peak throughput is usually just the
  host or the generator.
- **Benchmarks find bugs.** The body scenario surfaced a delayed-ACK stall from Nagle on the upstream
  sockets (no `TCP_NODELAY`); disabling Nagle there — standard for L7 proxies — removed a ~40 ms p99
  cliff on streamed bodies. Disclosing *how* a number was produced is the point.
- **Two layers that must agree.** In-process criterion micro-benchmarks isolate per-function cost
  deterministically; the open-loop macro scenarios measure it end-to-end. Micro-cost × calls-per-request
  should explain the macro delta — the WASM ladder is the worked example — so a divergence between the
  layers is a bug in one of them, not noise. The micro layer itself is split in two: criterion for
  wall-clock direction, and instruction counts (gungraun/callgrind, feature `instruction-bench`) as
  the frequency-invariant judge for "did the contract surface get more expensive?" — see
  [Reproducing](#reproducing) and `bench/methodology.md` § Measurement tiers.
- **The local per-change gate.** `bash bench/perf/run-perf.sh gate` re-measures exactly the
  invariants this report tracks (8 short paired rounds, rung order reversed on even rounds, so a common per-round level shift cancels inside each round and a linear host drift cancels in the sample mean across counterbalanced odd/even round pairs while remaining in $s$ as alternating-sign spread; `ci_half` is the 95 % Student-t half-width of the mean per-round delta, with `n` reported) and machine-checks them
  against `bench/perf/gate_tolerances.toml` — the bands are tracked in-repo, so a deliberate
  performance change is reviewed as a diff. The gate returns a **three-valued verdict**:
  - `pass` (exit code 0): every invariant's uncertainty interval `[value - ci_half, value + ci_half]` falls within its band `[band_lo, band_hi]`.
  - `fail` (exit code 1): at least one invariant's uncertainty interval falls completely outside its band (`value + ci_half < band_lo` or `value - ci_half > band_hi`). Exit code 1 dominates inconclusive.
  - `inconclusive` (exit code 2): an invariant straddles a band edge (neither fully inside nor fully outside). Re-run with `GATE_POOL_FROM=<that RUN_DIR>`: the new rounds are **pooled** with the earlier ones into one estimate (refused with exit 64 if `build_id.txt` differs or if more than 3 runs are pooled), so a re-run adds evidence instead of replacing an unwelcome result. Because round-level pooling treats rounds as independent and sequential stop-on-pass slightly inflates nominal coverage, pooling is capped at **at most 3 runs** (`n = 24`) and any band recalibration requires an independent non-pooled holdout run.
  (Invariants without `ci_half` keep binary pass/fail judgment; `info` rows such as `ratelimit_cpu_us_per_req` are reported, never judged, until they get a band.)
  `all` stays the human-read release snapshot.
- **CI regression gate.** Per-PR, two layers with different verdict policies (`bench.yml`): the
  criterion micro-benchmarks stay *informational* (hosted-runner wall-clock is noisy-neighbour
  bound, ~2–3 % CV, so a tight threshold would false-fail), while the gungraun instruction-count
  benches are *judged* — a soft limit (`ir=5%`) against the baseline saved from main pushes.
  Instruction counts don't inherit the runner's frequency/thermal noise, which is what makes a
  machine verdict meaningful on shared VMs. The heavy k6/oha macro suite never runs in CI — the
  local T1 `gate` covers per-change macro invariants.
- **Prior art.** Disclosing open- vs closed-loop and corrected latency is standard in tools such as
  `wrk2` and k6. This report follows that spirit using only its own measurements.

## Reproducing

The tracked, in-repo subjects and the runbook that produces every CSV here:

```bash
# Build the release examples first (the runbook does not build). bench-server/swap-bench live
# outside plecto/ (bench/harnesses/), so they need --features bench-harnesses.
cargo build --release -p plecto-server --features bench-harnesses \
  --example load-balancing --example bench-server --example tls-http --example swap-bench

# T1 — the per-change regression gate (~6-7 min): interleaved invariant deltas judged against
# bench/perf/gate_tolerances.toml, written to performance/data/gate.csv.
# Exit code: 0 = pass (in band), 1 = fail (out of band), 2 = inconclusive (straddles edge => re-run).
bash bench/perf/run-perf.sh gate          # or: just gate

# T2 — the full release-snapshot suite (~22 min at the report-tier windows). Phases:
#   quick gate ceiling sweep openloop rr ejection swap wasm v03 tls h3 ws footprint ratelimit body mix all
bash bench/perf/run-perf.sh all           # or: just report

# T3 — opt-in deep phases (not part of `all`): v0.3.0 response-context / replace + compression:
bash bench/perf/run-perf.sh v03           # or: just deep v03

# T0 — a fast local sanity check (~1 min, oha only, no k6/Docker, no tracked CSV):
bash bench/perf/run-perf.sh quick

# In-process micro-benchmarks, two layers (see bench/methodology.md § Measurement tiers):
# 1) criterion (wall-clock; drifts with the unpinned governor — read direction, not absolutes).
#    For ADR-sized contract changes, prefer a named baseline on the pre-change commit:
#      git checkout <pre-adr73> && cargo bench -p plecto-host -- --save-baseline pre-adr73
#      git checkout <post>      && cargo bench -p plecto-host -- --baseline pre-adr73
cargo bench -p plecto-control -p plecto-host -- --save-baseline main   # on the base branch
cargo bench -p plecto-control -p plecto-host -- --baseline main        # on a change, to read the deltas
# 2) instruction counts (gungraun/callgrind; frequency/thermal-invariant — the deterministic
#    judge for "did the contract surface get more expensive?"). Needs valgrind + a
#    version-matched `cargo install gungraun-runner`; feature-gated so plain `cargo bench` skips it:
#    NB: gungraun needs `=`-attached values — a space-separated `--save-baseline main` is parsed
#    as a positional benchmark filter and silently runs nothing — and baseline names allow only
#    [A-Za-z0-9_] (use `pre_adr73`, not `pre-adr73`).
cargo bench -p plecto-host    --features instruction-bench --bench wasm_inst     -- --save-baseline=main
cargo bench -p plecto-control --features instruction-bench --bench fastpath_inst -- --save-baseline=main
#    ...then on a change: the same commands with `-- --baseline=main` (soft limits are also
#    available, e.g. `-- --callgrind-limits=ir=5%`).

# Optional live dashboard (images are a one-time setup pull; the load stays on loopback):
INFLUX=1 bash bench/perf/run-perf.sh all     # http://localhost:3000/d/plecto-lb-k6

# The underlying examples (default ports overridable with PLECTO_PROXY_ADDR):
cargo run --release -p plecto-server --example load-balancing   # LB fast path
BACKEND_LATENCY_MS=0 cargo run --release -p plecto-server --features bench-harnesses --example bench-server   # WASM plane + rate-limit + body hook + WS
cargo run --release -p plecto-server --example tls-http          # TLS termination
cargo run --release -p plecto-server --features bench-harnesses --example swap-bench          # endpoint-set swap under load

# TLS full-vs-resumed handshake rungs (ADR 000052; the `tls` phase doesn't drive this yet — the
# cert.pem lives in tls-http's temp dir, printed nowhere, so find it under /tmp and pass it as --ca):
plecto-loadgen tls --mode full    --target https://localhost:PORT/api/hello --ca CERT.pem --out performance/data/tls_full.csv
plecto-loadgen tls --mode resumed --target https://localhost:PORT/api/hello --ca CERT.pem --out performance/data/tls_resumed.csv
```

The k6 scenarios live in `bench/k6/` and `bench/k6-wasm/`; the round-robin counter, the open-loop
fault/swap timelines, and the WebSocket handshake/hold/echo scenarios are `plecto-loadgen`
subcommands (`bench/loadgen/`, built lazily by the runbook). The open-loop driver records
schedule-latency into an HDR histogram (fixed footprint at any rate/window) and dumps the FULL
distribution alongside the summary (`--hist-out`, written to `performance/data/openloop_hist.csv`
by the runbook) — so a p99 move can be attributed to a second mode appearing vs one mode's tail
stretching, without a re-run. Charts are compiled with Flint (flint-chart) from the CSVs in performance/data:

```bash
cd performance/flint && npm ci && npm run plot
```

(The benchmark *method* — the runbook, scenarios, the Rust loadgen, plotting — is tracked, as are the rendered charts and this
report; the measured CSVs are regenerable working data and stay untracked, like `bench/`'s raw run
artifacts. See `bench/plan.md`.)

## Non-goals

- Not a sizing or capacity guide.
- Not a comparison against other proxies, gateways, or Wasm runtimes.
- Not representative of production hardware, real networks, or non-trivial upstream work.

## References

- Gil Tene, *coordinated omission* — summarized in ScyllaDB's [On Coordinated Omission](https://www.scylladb.com/2021/04/22/on-coordinated-omission/).
- [k6 executors](https://grafana.com/docs/k6/latest/using-k6/scenarios/executors/) — closed-loop (`constant-vus`) vs open-loop (`constant-arrival-rate`) models.
- [oha](https://github.com/hatoo/oha) — the single-connection-pool HTTP load generator used for the ceiling, WASM overhead, and TLS runs.
- [criterion.rs](https://bheisler.github.io/criterion.rs/book/) — the in-process micro-benchmark harness (LB pick, route match, WASM per-request cost) and its baseline-comparison regression gate.
- HTTP/3 load-testing references for the deferred H3 *load* benchmark (see [HTTP/3](#http3)):
- [h2load](https://nghttp2.org/documentation/h2load-howto.html) — nghttp2's closed-loop concurrency-based load generator; supports HTTP/3 (`--alpn-list=h3`) with qlog output (for open-loop H3 arrival pacing, see `oha --features http3` or a future `plecto-loadgen` H3 subcommand in [HTTP/3](#http3)).
- [wrk2](https://github.com/giltene/wrk2) — constant throughput with corrected latency recording.
- [Wasmtime](https://docs.wasmtime.dev/) — the pooling allocator and epoch interruption behind pooled vs on-demand filter instances.
- [WebAssembly Component Model](https://component-model.bytecodealliance.org/) — the `plecto:filter` contract is a Component Model world.
- [RFC 6455](https://www.rfc-editor.org/rfc/rfc6455) — the WebSocket protocol `bench-server`'s `/ws` mock upstream and `plecto-loadgen`'s `ws` subcommand implement (handshake + frame codec) to drive the Upgrade tunnel scenario.
