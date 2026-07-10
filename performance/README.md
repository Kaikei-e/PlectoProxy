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

- **Core isolation by pinning.** Plecto Proxy (and its in-process backends) is pinned to one dedicated
  set of CPU cores; **every** load generator is pinned to a separate, disjoint set. The generator
  therefore never steals a core from the proxy — the run measures Plecto Proxy, not the generator
  fighting it. (Done with `taskset`; no privileged host tuning.)
- **No host tuning.** CPU governor / turbo are left at their defaults — no fixed-frequency lock.
  Absolute throughput shifts run-to-run with clock; the **ratios, shapes and time-constants** are
  the durable signal, so those are what we read.
- **Generators, by phase.** [k6](https://grafana.com/docs/k6/latest/) drives the closed-loop
  concurrency sweep (`constant-vus`), the open-loop tail (`constant-arrival-rate`), the mixed
  short-circuit run, and the rate-limit / body scenarios; `plecto-loadgen` (a small Rust open-loop
  driver in `bench/loadgen/`, tokio + hyper — it replaced the earlier Python drivers, whose
  GIL-bound workers melted before the proxy did) runs the fault-injection timeline, the endpoint-set
  swap timeline, the round-robin count, and the WebSocket handshake/echo scenarios; and
  [oha](https://github.com/hatoo/oha) drives the single-route ceiling (plain h1, WASM W1, TLS) runs.
  Different generators have different ceilings — **numbers are comparable within a section, and
  across same-generator sections, but not blindly across all of them** (a lighter generator reveals
  a higher proxy ceiling). Each section names its generator.
- **Warm-up excluded.** Every measured window starts after a short warm-up (default 5 s) that
  sends load but is not recorded: in-script for k6 and plecto-loadgen, a discarded pre-run for
  oha. Cold-start seconds (route tables, upstream pools, allocator state) never enter a
  percentile. The rate-limit enforcement / fairness runs are the deliberate exception — their
  initial token-bucket burst *is* the measured signal.
- **Ceilings vs tails.** Closed-loop full-throttle runs (oha, `constant-vus`) are read as
  *throughput ceilings*; their latencies are queueing-at-saturation, not service latency
  ("never measure latency at max load"). Honest tails come from the fixed-rate runs: k6
  `constant-arrival-rate` and oha `-q` + `--latency-correction`, both coordinated-omission-safe.
- **Fully local.** Generators, proxy and upstreams talk only over loopback; generator telemetry and
  the optional dashboard's phone-home are disabled. Nothing leaves the host during a load run —
  the 2026-07-04 snapshot below ran inside a network namespace with only loopback up, so this is a
  kernel-enforced guarantee, not just an unexercised code path.
- **PMU not collected.** The runbook's optional micro-architectural attribution (cycles/req, IPC,
  LLC / branch misses via `perf`) needs a lowered `kernel.perf_event_paranoid` (privileged); it
  was not enabled on this run, so the WASM / rate-limit tax is reported as throughput / latency /
  **µs-per-req**, not a cycles breakdown.

## TL;DR

> **Measurement history** (newest first). **2026-07-09** — re-measured on the current branch tip
> after six days of landed work: a KvQuota striping fix (per-key atomicity, tallies kept
> I/O-free — the exact `charge_and_apply` cost [the WASM ladder note](#the-wasm-cost-ladder--isolating-each-cost)
> once flagged as a future-growth suspect), PROXY protocol v2 reception (ADR 000057), replayable
> buffered-body retry (ADR 000058), HTTP/3 GOAWAY drain (ADR 000059), the outbound-TCP capability
> (ADR 000060), the two-tier rate-limit model's completion with `filter-ratelimit-redis`
> (ADR 000061), shared TLS session-ticket keys (ADR 000062), and the fat-guest minimal-WASI grant
> for Go/TinyGo (ADR 000063) — plus a same-day fix recovering a fresh instance's final logs on
> success, not just on trap. Neither of the last two touches this report's default build (no
> `fat-guest` feature enabled, no filter declaring `wasi = "minimal"`), so they are unmeasured here
> by design. Every macro figure below is refreshed with `bash bench/perf/run-perf.sh all`; the
> open-loop phase needed `OPENLOOP_RATE=60000` pinned again, since this run's auto 70 %-of-peak
> target (~112k/s) again outran the co-resident k6 generator's own ceiling — reproducing the exact
> gap this report already documents. Ratios and shapes hold across every section; absolute figures
> move within the usual host-noise band (e.g. the plain-h1 ceiling reads ~10 % lower than 07-05).
> The in-process criterion micro-benchmarks ([§0](#0-micro-benchmarks-in-process-criterion)) were
> not re-run this pass. **2026-07-05** — re-measured after ADR 000052 (stateless
> TLS 1.3 session resumption) plus three hot-path fixes landed alongside it: a control-plane
> outlier-ejection race fix that also cut a per-request **route-lookup** allocation and the chain's
> per-filter HashMap re-resolution (the LB *pick* path is untouched), a host quota-accounting
> race fix + new untrusted-instance breaker, and fail-closed handling for a buffer-permit error. This
> run fills the [TLS section](#tls-termination)'s previously-pending resumption gap with a clean
> `plecto-loadgen tls --mode full|resumed` measurement, which confirms oha's `handshake/req` row was
> already silently resumption-contaminated. **2026-07-04** — re-measured post ADR 000050/000051 (TLS
> crypto provider moved to **aws-lc-rs**, a new baseline, not a `ring` delta); `wasm-bench` /
> `edge-bench` consolidated into one `bench-server` harness so the plain-HTTP/1.1 ceiling is measured
> once and every other section reads it; added endpoint-set-swap (ADR 000044) and WebSocket
> (ADR 000048) scenarios. **2026-07-02** — harness rebuilt onto `plecto-loadgen` (Rust), warm-up
> excluded from every window. Every figure below is refreshed; the **µs/req deltas are what to track
> across snapshots**, not raw throughput.

**Load-balancing fast path** (plaintext HTTP/1.1, 3 upstreams, trivial 0 ms backend; k6):

- Closed-loop throughput peaks at **~160k req/s** (100 VUs, this run — the k6 generator's own
  ceiling, so which VU count wins the peak shifts run to run, though it landed on 100 VUs in both
  this and the previous snapshot) with **p99 ≈ 2.25 ms** and zero failures; it degrades
  **gracefully** — still **~120k at 800 VUs** (p99 15.1 ms) with **0 failures and no latency cliff**.
- Open-loop at the pinned **60k/s** **achieves 59.3k/s (98.8 %)** with **p50 0.13 ms, p99 23.9 ms,
  1.6 % dropped, 0 % failed** — an honest queueing tail, not generator noise. The runbook's
  automatic target (70 % of the closed-loop peak, ~112k/s) still exceeds the co-resident
  generator's ceiling, which is why the pinned rate stays the published figure — this run's
  un-pinned auto-target attempt confirmed it again (43 % achieved, a p99.9 in the tens of seconds,
  discarded as generator-bound noise, not republished).
- Round-robin across three upstreams is **even to within one request** (33.3 % each).
- **Resilience is as designed**: ejecting one upstream drops its share to zero in ~1 s and the
  survivors absorb the load with **no client-visible errors**; a *total* outage **fails closed
  with HTTP 503** and the pool **recovers within ~1 s** of health returning.
- TLS termination (**aws-lc-rs**, ADR 000051; now with stateless resumption, ADR 000052) reads as
  **~57 % throughput vs plaintext this run** (h1 keep-alive ~124k vs plain ~219k) — within this
  report's usual host-noise band of the 07-04/07-05 snapshots' ~49 %; the qualitative story is
  unchanged: the TLS path is **crypto-bound**, so the native-path optimisations don't reach it. A
  clean, resumption-isolated measurement (not re-run this pass; carried over from 07-05) puts a
  **true full handshake at ~22.1k/s** vs **~29.8k/s with client resumption enabled (93 % resumed)**
  — a **~35 % throughput gain** from skipping the certificate chain and signature
  generation/verification (ECDHE still runs every time — rustls only offers `psk_dhe_ke`, never
  plain `psk_ke`, so resumption keeps forward secrecy; see [TLS](#tls-termination)).
- A **kept-alive** connection serves **~219k req/s**; forcing a **TCP handshake per request** costs
  **~47 % throughput and +0.41 ms p99** — connection reuse is load-bearing (see
  [the plain HTTP/1.1 ceiling](#plain-http11-ceiling)).

**WASM extension plane** (the cost of running a decision as a sandboxed component; oha / k6):

- A **cost ladder** isolates each cost by adjacent delta. The **irreducible dispatch floor** — a pure
  no-op WASM filter, pooled — is **≈ 3.2 µs/req (−41 % throughput)** over the native baseline; a
  **real filter's own work** (`filter-apikey`: header + host-KV + counter) adds **another
  ~0.6 µs (−7 % this run)** — matching this report's own repeated interleaved A/B measurement
  (0.6 ± 0.2 µs) almost exactly; and running that filter **fresh-per-request** instead of pooled
  costs **~27×** throughput — the price of re-paying `init` every request, and the value of
  pooling. The **µs/req is the portable figure**. A **fixed-rate tail run** (all rungs at the same
  2.9k/s this time, CO-corrected — this run's floor sits well clear of the ~4k/s knee described
  below) puts honest latency on the same ladder: the pooled no-op adds **+0.10 ms p50 / +0.17 ms
  p99** over native, the real pooled filter +0.14 ms p50 / +0.24 ms p99 — while the fresh rungs
  live at **p99 129–177 ms**, a kernel-side mmap/munmap knee (TLB-shootdown IPIs, ~100× the pooled
  rate), not CPU queueing — measured and dissected in
  [the ladder's tail note](#the-same-ladder-at-one-fixed-rate--honest-tails). (The much lower fresh
  tails than the 07-05 snapshot's 626–738 ms are themselves confirmation of that knee: this run's
  fixed rate landed well *below* it, the previous one almost exactly *on* it.)
- These macro deltas **reconcile with the criterion [micro-benchmarks](#0-micro-benchmarks-in-process-criterion)**:
  the pooled guest call is ~2.0 µs, and the remainder of the floor is the blocking-pool handoff the
  no-filter path skips entirely — the two layers agree.
- A rejected request (**HTTP 401 short-circuit**) is decided in **~0.28 ms and never reaches the
  backend** — bad traffic is shed **~57× faster** than good traffic is forwarded through a 15 ms backend.

**Host-enforced rate limiting** (token bucket, spec host-configured in the manifest; k6):

- The rate-limited route costs **~3.6 µs/req** (~36 % throughput, p99 unchanged) over a no-filter
  baseline when the bucket never denies — the filter dispatch floor plus the host-native bucket
  consult (and its multi-tenant quota check) on the hot path.
- Offered **5× over the configured rate**, the **allowed throughput converges to the bucket's refill
  rate** (≈ 1.0k/s for a 1000-token/s bucket) and **79.3 % is shed as 429** — decided at the edge in
  **~0.6 ms**, never reaching the backend — the exact same shed fraction as the 07-05 snapshot,
  since it falls out of the bucket math (refill/offered), not host timing.
- Buckets are **per key**: a hot key offered 4× its limit is throttled to its refill rate while a
  light key on the **same filter passes untouched (0 % shed)** — no cross-key starvation.

**Request-body hook** (buffer-then-decide, ADR 000025; export-presence zero-copy bypass, ADR 000038; k6):

- A filter that **reads** the body (`/body`, filter-hello) costs **~48 % throughput at 1 KB** and
  scales with payload: **~63 % at 100 KB**, **~69 % at 1 MB**, versus the streaming passthrough — a
  smoother monotonic climb this run than the previous snapshot's near-plateau at 100 KB, within the
  usual host-noise band. A **header-only filter** (`/body-headeronly` — no `on-request-body`
  export) **streams the body through**: at 100 KB and 1 MB it lands **within ~4 % of `/baseline`**
  (no body tax, ADR 000038); at 1 KB it shows **−36 %** — that gap is the ordinary **WASM dispatch
  floor** dominating a tiny request, not a body cost.
- RSS at 1 MB × 50 VUs (`MALLOC_ARENA_MAX=4`, the shipped default): **~97 MB `/baseline` · ~178 MB
  `/body` · ~104 MB `/body-headeronly`**. The arena cap roughly halves the buffered path (an uncapped
  glibc held ~317 MB); the header-only bypass stays close to baseline. The buffer stays bounded (16 MiB
  cap, fail-closed 413).

## Scope & honesty notes

- **Machine specs intentionally omitted.** Single commodity host, loopback, everything
  co-resident. Absolute throughput is contended and clock-variable; treat figures as relative /
  regression signals.
- **Generator-bound where noted.** The closed-loop sweep tops out near the *generator's* ceiling on
  its cores, not the proxy's: the same fast path serves a single route at ~219k req/s under the
  lighter oha (see the [plain HTTP/1.1 ceiling](#plain-http11-ceiling)), well above the k6 sweep's
  ~160k. The sweep curve's *shape* is the signal, not its absolute peak.
- **Trivial upstreams** (tiny static responses, 0 ms latency by default) deliberately isolate
  **proxy + LB + filter overhead** rather than backend work. A 15 ms synthetic backend is used
  where realistic proportions matter (WASM short-circuit); a sized-body backend for the body sweep.
- The LB figures are **plaintext HTTP/1.1**, except the dedicated [TLS run](#tls-termination).
- **No comparative claims.** Mature proxies are referenced only for shared methodology, never ranking.
- Charts rendered with matplotlib → WebP; an optional InfluxDB + Grafana stack (`INFLUX=1`) provides
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
| LB pick — round-robin | 29 → 36 ns (3 → 32 instances) | ~O(1) over the eligible set |
| LB pick — P2C weighted-least-request | 47 → 67 ns | two eligibility passes + the sampled compare |
| LB pick — weighted Maglev | ~25 ns | + one table lookup |
| LB pick under swap churn (`pick_under_swap_churn`) | 91 → 62 → 72 ns (3 → 8 → 32 instances) | round-robin pick while a background thread continuously calls `update_endpoints` (ADR 000044) — the per-pick `ArcSwap<Endpoints>` load cost under worst-case concurrent churn |
| route match (`find_route`) | 43 ns → 215 ns (1 → 64 routes) | scans by specificity, allocation-free |
| ingress path normalization | ~47–68 ns clean / ~162 ns dot-segments | ADR 000027; a clean path is borrowed, no allocation |

All three LB algorithms are covered here; the macro suite only load-tests round-robin. The `n=3`
`pick_under_swap_churn` cell reads slower than `n=8`/`n=32` — under continuous churn the eligible
set is tiny (2 instances) relative to the fixed cost of the concurrent `update_endpoints`
allocation contending for the same cache lines every tick; reported as measured, not smoothed.

**Extension plane** (`crates/host/benches/wasm.rs`):

| bench | cost | isolates |
| --- | --- | --- |
| `on_request` — pooled instance | ~2.0 µs/req | dispatch + call (init amortized) |
| `on_request` — fresh instance / request | ~29 µs/req | + per-request instantiation (the pool's value) |
| cold `load` (verify + instantiate + init) | ~15.2 ms | cosign signature + SBOM verification dominates |

The ~15× pooled→fresh gap here is the same one the [macro ladder](#the-wasm-cost-ladder--isolating-each-cost)
shows end-to-end (~27× in the 2026-07-09 snapshot, with the HTTP layer and its own run-to-run noise
around it) — the two layers agree in direction and order of magnitude, so a divergence between them
would be a real bug. (This criterion table itself was not re-run on 2026-07-09 — see the TL;DR note.)

---

# 1. Load-balancing fast path

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

![Plain HTTP/1.1 ceiling](img/ceiling.webp)

| Variant | req/s | p50 | p99 |
| --- | --- | --- | --- |
| keep-alive       | 218,638 | 0.21 ms | 0.55 ms |
| cold (TCP/req)   | 115,240 | 0.41 ms | 0.96 ms |

*(Re-measured 2026-07-09 — `bash bench/perf/run-perf.sh ceiling`. The prior snapshot's 242,272 /
115,894 sits within this report's usual host-noise spread of the same measurement — keep-alive
reads ~10 % lower this run, cold is within 1 %.)*

A TCP handshake per request costs **~47 % throughput and +0.41 ms p99** even on loopback (where the
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

![Throughput vs concurrency](img/throughput_vs_concurrency.webp)
![Latency percentiles vs concurrency](img/latency_vs_concurrency.webp)

| VUs | req/s | p50 | p95 | p99 | p99.9 | failed |
| --- | --- | --- | --- | --- | --- | --- |
| 50  | 157,565 | 0.21 ms | 0.67 ms | 1.10 ms | 2.09 ms | 0% |
| 100 | **160,384** | 0.41 ms | 1.36 ms | 2.25 ms | 4.22 ms | 0% |
| 200 | 151,227 | 0.67 ms | 2.27 ms | 3.73 ms | 7.34 ms | 0% |
| 400 | 132,848 | 1.26 ms | 4.36 ms | 6.89 ms | 13.06 ms | 0% |
| 800 | 120,058 | 3.21 ms | 8.79 ms | 15.10 ms | 24.32 ms | 0% |

Throughput peaks at **~160k at 100 VUs this run** (the k6 generator's own ceiling on its cores —
which VU count wins the peak is host/generator noise, not a proxy change; both this and the
previous snapshot peaked at 100 VUs) and declines **gracefully** as concurrency climbs — latency
rises in proportion with **no failures and no cliff even at 800 VUs**. The useful reading is the
shape: a flat-then-declining ceiling with an orderly latency climb, the pinned proxy never
collapsing under the generator.

## Tail latency under open-loop load

Open-loop sends at a **constant arrival rate** regardless of how fast responses come back, so
queueing surfaces in the tail instead of being hidden — the *coordinated-omission-safe* model.

| Model | target | achieved | p50 | p95 | p99 | p99.9 | dropped | failed |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| open-loop, 0 ms backend (pinned) | 60,000/s | 59,301/s | 0.13 ms | 10.37 ms | 23.9 ms | 35.6 ms | 1.6% | 0% |

The pinned `OPENLOOP_RATE=60000` is **achieved to 98.8 %** with a sub-ms p50 and a ~24 ms p99
queueing tail — a real queueing tail, not generator noise (Little's-law VU allocation with a
capped `maxVUs`). The runbook's automatic target (70 % of the closed-loop peak, ~112k/s this run)
again exceeds what the co-resident generator can sustain — an un-pinned attempt at that rate
achieved only 43 % with a p99.9 in the tens of seconds, confirming the ceiling is the generator's,
not the proxy's — so the pinned rate stays the published figure; overload past the generator's own
ceiling degrades into `dropped_iterations`, not VU explosion.

## Round-robin distribution

![Round-robin distribution](img/rr_distribution.webp)

Over a steady window with all three upstreams healthy, **120,000** requests split **40,000 /
40,000 / 40,000** — even to a single request (33.3 % each). Round-robin holds under load.

## Resilience: ejection & fail-closed

A steady open-loop rate (~4k req/s, `plecto-loadgen ejection` with a 5 s unrecorded warm-up so
t=0 is already steady state) while a controller drives a fault timeline (`eject b` → `rejoin b` →
`eject all` → `restore all`) and the driver buckets each upstream's served-count and the 503/s
every second:

![Load balancing under fault injection](img/ejection_timeline.webp)

- **Even baseline.** ~4k req/s split three ways while healthy (1,333/1,334/1,333 this run).
- **Graceful ejection.** When **b** is driven unhealthy its share falls to zero within ~1 s (a
  one-second mixed transition bucket, then clean) and the survivors (a + c) absorb the full load
  **with zero failed requests** — this run they split it **evenly** (2,000/2,000), round-robin over
  two survivors landing on an even split.
- **Fail-closed, not fail-open.** With **every** instance unhealthy, Plecto Proxy returns **HTTP 503**
  promptly (no hang, no blind forward); the 503/s line jumps to the full offered rate (4,000/s here).
- **Fast recovery.** Restoring health returns instances to rotation within ~1 s (a one-second mixed
  bucket, then clean).

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

![Endpoint-set swap under load](img/swap_timeline.webp)

> Re-measured 2026-07-09: a steady ~4k req/s open-loop while, at t=15 s (post-warmup),
> the manifest is rewritten `[a, b, c]` → `[a, b, d]` and SIGHUP-reloaded.

- **Zero client-visible failures.** All 240,000 responses over the 60 s run succeeded — **0 %
  failed** — even through the swap itself. Unlike a health-based ejection, nothing here ever needs
  to fail closed: `a` and `b` are unchanged addresses, so `reconcile` reuses their `Arc`s and
  health outright (ADR 000017's reuse rule), and only `d` starts pessimistic.
- **The swap completes within one second.** The transition second (t=15) shows a brief mixed
  bucket (`a=1581, b=1579, c=13, d=827`) as in-flight requests to `c` finish and the reconciled
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

![TLS vs plain](img/tls_vs_plain.webp)

| Variant | req/s | p50 | p99 | isolates |
| --- | --- | --- | --- | --- |
| plain (h1)               | 218,638 | 0.21 ms | 0.55 ms | [ceiling](#plain-http11-ceiling) keep-alive |
| TLS h1, keep-alive       | 124,150 | 0.39 ms | 0.75 ms | record layer + TLS I/O path = Δ vs plain |
| TLS h1, handshake/req    | 27,415  | 1.65 ms | 5.03 ms | oha, shared `ClientConfig` — see caveat below |
| TLS (h2)                 | 108,822 | 0.44 ms | 0.96 ms | h2 multiplexing over TLS |

The decomposition is the point. The kept-alive TLS delta reads **−43 % / +0.20 ms p99** vs the
plaintext ceiling this run: the TLS-terminated path is the **crypto-/TLS-I/O-bound** path that the
native-path optimisations don't reach. **h2 is clean** (109k/s, p99 0.96 ms) — a client that funnels
many VUs over a handful of multiplexed connections can make h2 *look* far worse (head-of-line
queueing, not server work); measuring with a connection-per-concurrency client removes that artifact.

*(Re-measured 2026-07-09 on **aws-lc-rs** (ADR 000051). The kept-alive ratio (~57 % of plaintext
this run vs ~49 % in the 07-04/07-05 snapshots) moved within this report's usual host-noise band;
the qualitative story — TLS I/O-bound, no native-path optimisation reaching it — is unchanged
across every snapshot so far.)*

### Full vs resumed handshake (ADR 000052)

*(Not re-run in the 2026-07-09 pass — `bench/perf/run-perf.sh`'s `tls` phase doesn't drive this
rung automatically; the numbers below are carried over unchanged from 2026-07-05.)*

The `handshake/req` row above no longer isolates a *true* full handshake: oha shares one rustls
`ClientConfig` across connections, and against a server issuing stateless TLS 1.3 session tickets
its "cold" connections silently resume once warm. `plecto-loadgen tls --mode full|resumed` gives
each rung explicit resumption control instead:

| Client resumption | req/s | p50 | p99 | resumed |
| --- | --- | --- | --- | --- |
| full (disabled)      | 22,099 | 2.06 ms | 4.36 ms | 0 % |
| resumed (enabled)     | 29,768 | 1.54 ms | 3.26 ms | 93.0 % |

A true full handshake (22.1k/s) is **~17 % slower** than the old `handshake/req` row (26.5k/s) —
confirming it really was partly resumed. Enabling client resumption recovers **~35 % throughput**
over a true full handshake — **≈11.7 µs/connection** (45.3 → 33.6 µs). That saving is the
certificate chain + signature generation/verification, **not** the ECDHE exchange: rustls's client
hardcodes `psk_dhe_ke` and never offers plain `psk_ke` (`client/hs.rs`, RFC 8446 §4.2.9 — "such
connections don't have forward secrecy"), so every resumed handshake here still runs a fresh ECDHE
exchange for forward secrecy. The ~11.7 µs matches that: skipping only the asymmetric
sign/verify + cert bytes is a much smaller saving than skipping ECDHE too would be. The residual
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

![Throughput by decision path](img/wasm_throughput.webp)
![Per-request latency by decision path](img/wasm_latency.webp)

> W1 — fixed 50 connections, 0 ms backend, valid key (oha, warm-up burned in a discarded 5 s
> pre-run). Full-throttle: read these rows as **throughput ceilings**; the honest latencies are in
> the fixed-rate tail table below.

Five routes forward to the **same** backend, so each **adjacent delta isolates exactly one cost**. A
pure **no-op** WASM filter (no host-API calls) is the key addition — it separates "the WASM tax" from
"a real filter's work", which older reports conflated.

| Route | Decision path | req/s | p50 | p99 |
| --- | --- | --- | --- | --- |
| `/baseline` | native fast path (no filter) | 218,638 | 0.21 ms | 0.55 ms |
| `/noop-pooled` | a **pure no-op** WASM filter, pooled | 128,727 | 0.37 ms | 0.75 ms |
| `/noop-fresh` | the same no-op, **fresh instance / request** | 4,770 | 7.98 ms | 27.3 ms |
| `/trusted` | the real `filter-apikey`, pooled | 119,192 | 0.40 ms | 0.77 ms |
| `/ondemand` | `filter-apikey`, fresh instance / request | 6,095 | 4.86 ms | 25.9 ms |

*(Re-measured 2026-07-09. `/baseline` is sourced from [ceiling.csv](#plain-http11-ceiling); the
other four rungs are measured together in one run, and the deltas below are computed against that
run's own baseline.)*

- **baseline → noop-pooled** = the **irreducible extension-plane dispatch cost** (chain dispatch +
  the blocking-pool hop + instance acquisition + one empty host↔guest crossing), with *no* filter
  work: **−41 % throughput, ≈ 3.2 µs/req**. Every WASM filter pays this floor.
- **noop-pooled → noop-fresh** = the **per-request instantiation cost**, now cleanly isolated from any
  host work: throughput collapses **~27×** (129k → 4.8k). This is what pooling buys.
- **noop-pooled → trusted** = a **real filter's own work** on top of the no-op (header parse +
  host-KV lookup + counter): **−7 % (~0.6 µs this run)** — landing right on this report's own
  repeated, interleaved A/B measurement (2026-07-06, 5 pairs at 50 connections: **0.6 ± 0.2 µs**),
  a tighter match than either single-draw snapshot before it (0.4 µs, then 0.8 µs). The apikey
  filter is cheap; the dispatch floor still dominates it. (The structural suspect for any future
  growth was `charge_and_apply` under process-wide quota-lock contention — a per-key striping fix
  landed 2026-07-06 specifically to shrink that; at 50 connections any residual effect stays
  inside this noise band.)
- **noop-fresh and ondemand are the same order of magnitude** (4.8k vs 6.1k req/s), confirming
  instantiation dominates the fresh path — the filter's per-request work is noise next to re-paying
  `init` (~29 µs) every request.

### The same ladder at one fixed rate — honest tails

> W1b — every rung offered the **same** fixed 2,862 req/s this run (60 % of the slowest rung's
> ceiling, `/noop-fresh` at 4,770/s), 50 connections, oha `-q` + `--latency-correction`
> (coordinated-omission-safe). Identical offered load, so the latency columns are directly
> comparable. Unlike the 07-05 snapshot's 4,189 req/s — which sat **on** the fresh rungs' knee —
> this run's rate sits comfortably below every rung's knee, pooled and fresh alike; see the
> reconciling note just after the table.

| Route | achieved | p50 | p90 | p99 |
| --- | --- | --- | --- | --- |
| `/baseline` | 2,861/s | 0.27 ms | 0.39 ms | 0.65 ms |
| `/noop-pooled` | 2,862/s | 0.37 ms | 0.50 ms | 0.82 ms |
| `/trusted` | 2,862/s | 0.41 ms | 0.55 ms | 0.89 ms |
| `/noop-fresh` | 2,860/s | 2.03 ms | 70.97 ms | 129.31 ms |
| `/ondemand` | 2,845/s | 27.01 ms | 102.22 ms | 177.38 ms |

At a rate every rung sustains, the pooled dispatch floor costs **+0.10 ms p50 / +0.17 ms p99** over
native and the real pooled filter **+0.14 ms p50 / +0.24 ms p99** — sub-millisecond even at p99.
The fresh rungs live at **p99 129–177 ms** this run — far better than the 07-05 snapshot's
626–738 ms — and the mechanism note right below explains why: that snapshot's fixed rate sat
almost exactly on the fresh rungs' ~4k/s knee, this one sits clearly below it. Per-request
instantiation is still not a tail you can operate behind near or above that knee, but the two
snapshots together are themselves a natural-experiment confirmation of where the knee actually is.

> **The fresh tail is a kernel-side knee, not CPU queueing (measured 2026-07-06).** A fresh
> instance is an mmap at instantiate and an munmap at drop, every request
> (`Allocation::OnDemand`); munmap serializes on the process's `mmap_lock` and IPIs every core
> running the process (TLB shootdown). `/proc/interrupts` deltas during fixed-rate runs: the fresh
> rung takes **~31–35 TLB shootdowns/req vs ~0.3 pooled — ~100×**. The resulting tail is sharply
> rate-dependent — p99 **1.4 ms at 1k/s, 4.7 ms at 2k/s, ~650 ms at 4.2k/s, ~1.2 s at 6k/s** (with
> shootdowns/req itself doubling as concurrency rises) — a knee near ~4k/s, roughly *half* the
> rung's closed-loop ceiling. The 07-05 snapshot's W1b fixed rate (60 % of that run's slowest
> ceiling, 4,189 req/s) landed almost exactly on that knee, which is why its fresh rows' absolute
> tails were chaotic across earlier snapshots (440 → 738 ms between runs; 83 ms vs 648 ms at the
> same rate on the same host minutes apart) while the pooled rows stayed stable; the 2026-07-09
> snapshot's slower rungs pulled W1b's rate down to 2,862 req/s, comfortably clear of the knee,
> which is exactly why *its* fresh tails (129–177 ms) read so much cleaner. Avoiding precisely this
> per-request mmap/munmap churn is why wasmtime's pooling
> allocator pre-maps slots and batches decommits — the trusted path rides that. Stated portably:
> fresh-per-request has a clean-tail operating ceiling around ~2k/s on this host, and that — not
> the 29 µs — is the pooling decision's real justification.

**The µs/req deltas are the invariants to track for regressions, not the percentages** (which widen or
shrink whenever the *baseline* moves). These macro deltas **reconcile with the in-process
[micro-benchmarks](#0-micro-benchmarks-in-process-criterion)** — with one disclosed asymmetry:
criterion clocks the pooled per-request call at ~2.0 µs, and the remaining ~2 µs of the macro floor
is the `spawn_blocking` handoff (sync wasmtime, `!Send` store) that a route with no filters skips
entirely. The fresh ~29 µs, by contrast, is the *uncontended* cost — criterion instantiates
sequentially, so it never pays the `mmap_lock` contention or cross-core shootdowns the concurrent
macro run exposes (the knee above). The layers agree once that kernel-side term is named.

## Short-circuit: rejecting bad traffic at the edge

![Accept vs reject latency](img/wasm_shortcircuit.webp)

> W2 — fixed 2000 req/s, 15 ms backend, ~90 % valid / ~10 % bad keys (k6). 108,016 accepted, 12,012 rejected.

| Path | p50 | p95 | p99 |
| --- | --- | --- | --- |
| accept (200, forwarded) | 16.31 ms | 17.11 ms | 17.44 ms |
| reject (401, short-circuited) | 0.28 ms | 0.46 ms | 0.65 ms |

Accepted requests cost the 15 ms backend plus the small pooled-filter + proxy overhead. Rejected
requests are decided **at the edge in ~0.28 ms** and never reach the upstream: bad traffic is shed
**~57× faster** than good traffic is forwarded, and is harmless to the backend it would otherwise
hit. (Filter faults or deadline overruns **fail closed** — 502/504 — exercised by the test suite,
not this benchmark.)

## Outbound ext_authz (ADR 000036)

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

| Route | req/s | p50 | p99 |
| --- | --- | --- | --- |
| /baseline (no filter) | 158,964 | 0.21 ms | 1.20 ms |
| /ratelimit (bucket) | 101,344 | 0.42 ms | 1.08 ms |

The rate-limited route adds **~3.6 µs/req** over the no-filter baseline (~36 % of its throughput;
p99 stays in the same ~1.1–1.2 ms band — the µs/req is the inverse-throughput delta at 50 VUs).
That is the whole hot-path tax with no rejections — the filter dispatch floor (the same one the
[WASM ladder](#the-wasm-cost-ladder--isolating-each-cost) isolates) plus the host-native bucket
consult, including the per-call host-state quota check (ADR 000027) that keeps a multi-tenant
filter's bucket count bounded.

### Enforcement — does it actually hold the rate?

![Rate-limit enforcement](img/ratelimit_enforce.webp)

> R2 — a **tight** bucket (refill 1000 tok/s, burst 2000), offered **5000 req/s** open-loop at one
> key for 30 s (k6).

| offered | allowed (200) | shed (429) | accept p99 | 429 p99 |
| --- | --- | --- | --- | --- |
| 5,000/s | **1,033/s** | 79.3% | 1.66 ms | 0.62 ms |

Offered 5× over the limit, the **allowed throughput converges to the bucket's refill rate**
(≈ 1.0k/s — the configured 1000 tok/s plus the burst amortised over the run) — **the exact same
1,033/s and 79.3 % shed as the 07-05 snapshot**, since both fall out of the bucket's own math
(refill vs offered rate), not host timing. The excess is shed as 429 each decided at the edge in
**~0.6 ms** without touching the backend. Open-loop (`constant-arrival-rate`) keeps offering
regardless of the 429s, so the enforcement is measured honestly, not hidden by a self-throttling
client.

### Fairness — one key cannot starve another

![Rate-limit fairness](img/ratelimit_fairness.webp)

> R3 — same tight bucket; two keys concurrently: a **hot** key offered 4000/s and a **light** key
> offered 500/s (k6).

| key | offered | allowed (200) | shed |
| --- | --- | --- | --- |
| hot | 4,000/s | 1,033/s | 74% |
| light | 500/s | 500/s | **0%** |

State is **per key**, so the hot key is throttled to its own refill rate (1.0k/s, 74 % shed) while
the light key sharing the same filter **passes completely untouched** — no cross-key starvation. A
noisy tenant is contained to its own bucket.

## Request body handling

The request-side **body hook** (`on-request-body`, ADR 000025) follows a *buffer-then-decide* model:
for a filtered route carrying a body, the host buffers it (bounded — 16 MiB cap, fail-closed 413),
runs the filter's `on-request-body`, and forwards the possibly-transformed body — or short-circuits
before upstream. `filter-hello` uppercases the body (a real transform) or 403s on a `deny-body`
marker. A bodyless request, a filter-less route, and — since ADR 000038 — a route whose filters are
**all header-only** (none exports `on-request-body`) keep the zero-copy streaming path: the host
decides from the component's exports whether any filter reads the body, and buffers only then.

![Request body hook](img/body.webp)

> B — 50 VUs, POST a `SIZE`-byte body at 1 KB / 100 KB / 1 MB (k6), to `/body` (filter-hello buffers +
> transforms), `/body-headeronly` (a header-only filter — body streams through, ADR 000038), and
> `/baseline` (no filter). `MALLOC_ARENA_MAX=4`, the shipped allocator default (ADR 000038).

| size | route | req/s | throughput | p99 |
| --- | --- | --- | --- | --- |
| 1 KB   | /baseline        | 146,802 | 150 MB/s  | 1.12 ms |
| 1 KB   | /body            | 76,710  | 79 MB/s   | 1.26 ms |
| 1 KB   | /body-headeronly | 93,325  | 96 MB/s   | 1.16 ms |
| 100 KB | /baseline        | 47,469  | 4861 MB/s | 3.80 ms |
| 100 KB | /body            | 17,506  | 1793 MB/s | 5.79 ms |
| 100 KB | /body-headeronly | 45,585  | 4668 MB/s | 3.82 ms |
| 1 MB   | /baseline        | 6,569   | 6888 MB/s | 31.8 ms |
| 1 MB   | /body            | 2,050   | 2150 MB/s | 40.9 ms |
| 1 MB   | /body-headeronly | 6,305   | 6611 MB/s | 31.9 ms |

A filter that **reads** the body pays for it, growing with payload: **~48 % throughput at 1 KB** (the
buffer + WASM transform dominate the small request), **~63 % at 100 KB**, **~69 % at 1 MB** (a
full-body copy + uppercase per request) — a smoother monotonic climb this run than the previous
snapshot's near-plateau between 100 KB and 1 MB, within the usual host-noise band. A **header-only
filter takes the zero-copy bypass** — the body never enters guest memory: at 100 KB and 1 MB it
lands **within ~4 % of `/baseline`** (ADR 000038). At 1 KB it reads **−36 %** — but that gap is the
per-request **WASM dispatch floor** (the same ~3 µs the [ladder](#the-wasm-cost-ladder--isolating-each-cost)
isolates) showing against the native baseline on a tiny request, not a body cost. RSS at 1 MB × 50 VUs
(fresh proxy per route, `MALLOC_ARENA_MAX=4`): **~97 MB `/baseline` · ~178 MB `/body` · ~104 MB
`/body-headeronly`** (`data/body_rss.csv`). Two levers cut what an uncapped glibc once held (~317 MB): the arena cap
roughly halves the buffered path, and the export-presence bypass keeps a header-only route close to
baseline. The buffer stays bounded (16 MiB cap, fail-closed 413) for the
filters that do read the body. The remaining buffered-path copy is the target of a future `stream<u8>`
increment (ADR 000020); a per-request time-series / allocator-sweep decomposition lives in
`bench/perf/mem_matrix.py`.

## Footprint

Idle resident set and the marginal cost of an open connection (`bench/harnesses/bench-server`):

| Metric | Value |
| --- | --- |
| idle RSS | ~41 MB |
| RSS holding ~1,000 idle keep-alive connections | ~66 MB |
| marginal bytes / connection | ~25 KB |

---

# 3. Realistic & protocol coverage

## Weighted request mix — with its own baseline

> M1 — open-loop 20k req/s, a weighted blend across routes on one gateway (k6): read-heavy, partly
> edge-checked (per-tenant rate-limit keys, 200 tenants, never-deny bucket), occasional writes,
> rare large payloads. Paired with a **read-only control at the same arrival rate** — 100 % plain
> reads — so the per-class deltas are attributable to the traffic *blend*, not the offered load.

| Profile | Class (share) | route | p50 | p99 | p99.9 |
| --- | --- | --- | --- | --- | --- |
| read-only (control) | read 100 % | GET `/baseline` (1 KB) | 0.24 ms | 12.54 ms | 30.0 ms |
| mix | read 60 % | GET `/baseline` (1 KB) | 0.30 ms | 20.43 ms | 35.1 ms |
| mix | auth read 25 % | GET `/ratelimit` (tenant key) | 0.43 ms | 20.77 ms | — |
| mix | write 10 % | POST `/body` (1 KB) | 0.52 ms | 22.41 ms | — |
| mix | large 5 % | POST `/body` (100 KB) | 1.83 ms | 27.27 ms | — |

Both profiles achieve the full 20k/s (dropped iterations ≤ 0.25 %, zero 429s from the never-deny
bucket). The pairing is the point: at the same rate, **the blend costs the plain reads +7.9 ms at
p99** (12.5 → 20.4 ms) this run — head-of-line pressure from the body classes — and the classes order
exactly as their work predicts (read < auth read < write < large, all p50s sub-millisecond). Absolute
tails vary run to run (host noise); the ordering and the pairing methodology are the durable signal.
A single-endpoint test hides all of this; the control run keeps it honest.

## HTTP/3

The fast path terminates **HTTP/3 over QUIC** (ADR 000016; `tls-http` serves h1/h2/h3 on one port). A
functional check confirms it end-to-end:

```
curl --http3-only https://…/api/hello  ->  status=200 http_version=3
```

A **rigorous, coordinated-omission-safe H3 *load* benchmark is deferred**: oha and k6 have no native
HTTP/3, and a correct tail needs an H3-capable open-loop generator (**h2load** with
`--npn-list h3`, or an equivalent H3 load tool). Rather than publish process-spawn-bound `curl`-loop numbers, the H3 load figure
stays absent until that tooling is in place — server support is verified, not throughput.

## WebSocket Upgrade tunnel (ADR 000048)

Plecto Proxy's HTTP/1.1 Upgrade path (ADR 000048): a route declaring `[route.upgrade] protocols =
["websocket"]` forwards the client's handshake (controlled re-issue — hop-by-hop stripping stays
the default for every other route), and on the upstream's 101 the proxy splices the two connections
into an opaque bidirectional byte tunnel — the same post-upgrade relay shape used by typical
L7 `Upgrade` / TCP tunnel modes. This is a **different load shape**
than every other scenario in this report: a long-lived, stateful connection instead of a
short-lived request, so it exercises axes nothing else here does — connection-permit accounting
(the circuit breaker / least-request in-flight counters follow the tunnel for its whole lifetime,
not just the handshake) and an activity-based idle timeout, rather than throughput-per-request.

`bench-server`'s `/ws` route tunnels to a dedicated mock upstream that completes the RFC 6455
handshake and echoes every frame; `plecto-loadgen`'s `ws` subcommand drives three sub-scenarios
(`bench/perf/run-perf.sh`'s `ws` phase):

- **Handshake rate** — open-loop paced Upgrade attempts/sec (the 101 handshake runs through the full
  filter chain like any other request, so it pays the same per-request cost the rest of this report
  measures — only the post-101 tunnel is new).
- **Tunnel footprint** — RSS held by 1,000 concurrently open (idle) tunnels, the long-lived-connection
  analogue of [Footprint](#footprint)'s keep-alive connection measurement.
- **Echo throughput** — sustained request/response frames per held tunnel, at two payload sizes
  (1 KB / 64 KB), closed-loop per connection (the same concurrency model oha's `-c N` uses).

> Re-measured 2026-07-09: `bash bench/perf/run-perf.sh ws`.

| Scenario | Result |
| --- | --- |
| Handshake rate | 10,000/10,000 Upgrades succeeded at the paced 500/s target — **0 % failed** over 20 s |
| Tunnel footprint | idle RSS 45.2 MB → 63.7 MB with 1,000 held tunnels — **~18.9 KB/tunnel** |

![WebSocket echo throughput](img/ws_echo.webp)

| Payload | messages/s | throughput | p50 | p99 |
| --- | --- | --- | --- | --- |
| 1 KB  | 242,545 | 248 MB/s   | 0.17 ms | 0.82 ms |
| 64 KB | 72,713  | 4,765 MB/s | 0.65 ms | 1.48 ms |

The handshake rate holds at 100 % of target with zero rejections — the Upgrade path costs nothing
beyond the ordinary per-request floor. Tunnel footprint (~19 KB/tunnel) is in the same order as a
held keep-alive HTTP connection ([Footprint](#footprint): ~25 KB/conn) — a tunnel is not
meaningfully heavier to hold open than an ordinary idle connection, only longer-lived. Echo
throughput at 1 KB (243k msg/s) exceeds 64 KB (73k msg/s) as expected — the larger payload's
messages/s falls roughly in proportion to its size, while aggregate byte throughput rises (248 →
4,765 MB/s), consistent with a per-message dispatch floor that amortizes better over larger frames.
(Both the footprint and message-size rungs read close to the previous snapshot's — a stable signal
— though both echo rungs moved modestly faster this run, within this report's usual host-noise
band; the shape, not the absolute msg/s, is the signal.)

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
  layers is a bug in one of them, not noise.
- **CI regression gate (opt-in).** Per-PR runs only the light criterion micro-benchmarks
  (`cargo bench -- --baseline main`, seconds); the heavy k6/oha macro suite runs on manual dispatch /
  nightly. Hosted-runner numbers are treated as *relative* (regression direction), never absolute — CI
  VMs are noisy neighbours.
- **Prior art.** Disclosing open- vs closed-loop and corrected latency is standard in tools such as
  `wrk2` and k6. This report follows that spirit using only its own measurements.

## Reproducing

The tracked, in-repo subjects and the runbook that produces every CSV here:

```bash
# Build the release examples first (the runbook does not build). bench-server/swap-bench live
# outside plecto/ (bench/harnesses/), so they need --features bench-harnesses.
cargo build --release -p plecto-server --features bench-harnesses \
  --example load-balancing --example bench-server --example tls-http --example swap-bench

# One phase, or `all`. Pins the proxy to a core set and generators to a disjoint set; writes
# performance/data/*.csv. Phases:
#   quick ceiling sweep openloop rr ejection swap wasm tls h3 ws footprint ratelimit body mix all
bash bench/perf/run-perf.sh all

# Or just a fast local sanity check (~1 min, oha only, no k6/Docker, no tracked CSV):
bash bench/perf/run-perf.sh quick

# In-process micro-benchmarks (deterministic; the CI regression gate). Save a baseline, then compare:
cargo bench -p plecto-control -p plecto-host -- --save-baseline main   # on the base branch
cargo bench -p plecto-control -p plecto-host -- --baseline main        # on a change, to read the deltas

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
subcommands (`bench/loadgen/`, built lazily by the runbook). Charts are regenerated from the
measured CSVs:

```bash
python3 performance/plot.py     # reads performance/data/*.csv -> performance/img/*.webp
```

(`matplotlib` brings `numpy` + `Pillow`; Pillow supplies the WebP encoder. The benchmark *method* —
the runbook, scenarios, the Rust loadgen, plotting — is tracked, as are the rendered charts and this
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
- Open-loop HTTP/1–2–3 load generators suitable for an HTTP/3 *load* benchmark (deferred here).
- [h2load](https://nghttp2.org/documentation/h2load-howto.html) — nghttp2's load generator; supports HTTP/3 (`--npn-list h3`) with qlog output, and a candidate for the deferred H3 load run.
- [wrk2](https://github.com/giltene/wrk2) — constant throughput with corrected latency recording.
- [Wasmtime](https://docs.wasmtime.dev/) — the pooling allocator and epoch interruption behind pooled vs on-demand filter instances.
- [WebAssembly Component Model](https://component-model.bytecodealliance.org/) — the `plecto:filter` contract is a Component Model world.
- [RFC 6455](https://www.rfc-editor.org/rfc/rfc6455) — the WebSocket protocol `bench-server`'s `/ws` mock upstream and `plecto-loadgen`'s `ws` subcommand implement (handshake + frame codec) to drive the Upgrade tunnel scenario.
