# Plecto Performance

An honest performance snapshot of Plecto's two halves: the **native load-balancing fast
path** and the **WASM extension plane** (per-request filters, host-enforced rate limiting, the
request-body hook). The goal is **transparency about method**, not a leaderboard. Every number
here is an internal **regression baseline** — not a capacity guide, and not a comparison against
other proxies.

All components — load generator, Plecto, the upstream instances, and any tooling — run
**co-resident on a single commodity developer host over loopback**, so absolute figures are
bounded by that host and by the generator, not by Plecto in isolation. Read them as **relative**
signals — ratios, curve shapes and time-constants, not headline throughput.

## Measurement setup

- **Core isolation by pinning.** Plecto (and its in-process backends) is pinned to one dedicated
  set of CPU cores; **every** load generator is pinned to a separate, disjoint set. The generator
  therefore never steals a core from the proxy — the run measures Plecto, not the generator
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

> **Snapshot context (2026-07-02, harness rebuild).** Re-measured with the rebuilt harness: the
> Python drivers replaced by `plecto-loadgen` (Rust), **warm-up excluded from every measured
> window**, k6 tuned for generator headroom (`discardResponseBodies`, Little's-law VU allocation),
> a new **fixed-rate CO-safe tail** run for the WASM ladder, and a **paired same-rate baseline**
> for the weighted mix. Where numbers moved vs the previous (same-day, post-hot-path-audit)
> snapshot, the cause is harness honesty — cold-start seconds no longer pollute percentiles and
> the generator no longer melts first — not proxy changes. The µs/req deltas remain the figures to
> compare across snapshots.
>
> **Harness consolidation (2026-07-04, post ADR 000041–000048).** `wasm-bench` and `edge-bench`
> merged into one `bench-server` harness, so the plain-HTTP/1.1 ceiling — previously measured three
> times independently (once each by the WASM ladder's `/baseline` rung, the TLS run's `plain (h1)`
> row, and the standalone connection-churn run, at 240–243k and 228k req/s respectively — a spread
> that was always host noise across three server processes measuring the *same* route, not a real
> difference) — is now measured **exactly once** (the `ceiling` phase, below) and every other
> section reads that number instead of re-measuring it. Two new scenarios land alongside recent
> ADRs with no prior baseline: **endpoint-set swap under load** (ADR 000044 — a reload changes the
> upstream's resolved address SET, not just instance health) and the **WebSocket Upgrade tunnel**
> (ADR 000048). `ceiling`, `swap`, and `ws` were freshly re-measured for this consolidation; the
> WASM ladder and TLS decomposition tables keep their pre-consolidation figures (a fresh functional
> run confirmed the merge preserves their behaviour, but this session's host was noisier than the
> isolated run behind the published numbers — see each section's own note). See the
> [ceiling](#plain-http11-ceiling), [endpoint-set swap](#endpoint-set-swap-under-load-adr-000044),
> and [WebSocket](#websocket-upgrade-tunnel-adr-000048) sections.
>
> **Re-measured 2026-07-04 (post ADR 000050 / 000051).** Full suite re-run after adding an
> `[upstream.tls]` `sni` verification-name override (ADR 000050, no perf-relevant path) and
> consolidating the TLS crypto provider onto **aws-lc-rs**, replacing `ring` (ADR 000051). The
> [TLS section](#tls-termination)'s absolute numbers now reflect that different provider — read
> them as a new baseline, not a delta against the pre-000051 `ring` figures.

**Load-balancing fast path** (plaintext HTTP/1.1, 3 upstreams, trivial 0 ms backend; k6):

- Closed-loop throughput peaks at **~152k req/s** (50 VUs) with **p99 ≈ 1.2 ms** and zero
  failures; it degrades **gracefully** — still **~117k at 800 VUs** (p99 15.8 ms) with **0 failures
  and no latency cliff**.
- Open-loop at the pinned **60k/s** **achieves 59.3k/s (99 %)** with **p50 0.09 ms, p99 25 ms,
  1.6 % dropped, 0 % failed** — an honest queueing tail, not generator noise. The runbook's
  automatic target (70 % of the closed-loop peak, ~107k/s) still exceeds the co-resident
  generator's ceiling, which is why the pinned rate stays the published figure.
- Round-robin across three upstreams is **even to within one request** (33.3 % each).
- **Resilience is as designed**: ejecting one upstream drops its share to zero in ~1 s and the
  survivors absorb the load with **no client-visible errors**; a *total* outage **fails closed
  with HTTP 503** and the pool **recovers within ~1 s** of health returning.
- TLS termination (now on **aws-lc-rs**, ADR 000051) reads as **~48 % throughput vs plaintext**
  (h1 keep-alive ~127k vs plain ~261k): the TLS path is **crypto-bound**, so the native-path
  optimisations don't reach it (see [TLS](#tls-termination)).
- A **kept-alive** connection serves **~261k req/s**; forcing a **TCP handshake per request** costs
  **~46 % throughput and +0.45 ms p99** — connection reuse is load-bearing (see
  [the plain HTTP/1.1 ceiling](#plain-http11-ceiling)).

**WASM extension plane** (the cost of running a decision as a sandboxed component; oha / k6):

- A **cost ladder** isolates each cost by adjacent delta. The **irreducible dispatch floor** — a pure
  no-op WASM filter, pooled — is **≈ 3.9 µs/req (−51 % throughput)** over the native baseline; a
  **real filter's own work** (`filter-apikey`: header + host-KV + counter) adds only **another
  ~0.4 µs (−4 %)**; and running that filter **fresh-per-request** instead of pooled costs **~16×**
  throughput — the price of re-paying `init` every request, and the value of pooling. The **µs/req
  is the portable figure**. A **fixed-rate tail run** (all rungs at the same below-knee 4.7k/s,
  CO-corrected) puts honest latency on the same ladder: the pooled no-op adds **+0.33 ms p99**
  over native, the real pooled filter +0.27 ms — while the fresh rungs live at **p99 371–440 ms**.
- These macro deltas **reconcile with the criterion [micro-benchmarks](#0-micro-benchmarks-in-process-criterion)**:
  the pooled guest call is ~2.1 µs, and the remainder of the floor is the blocking-pool handoff the
  no-filter path skips entirely — the two layers agree.
- A rejected request (**HTTP 401 short-circuit**) is decided in **~0.25 ms and never reaches the
  backend** — bad traffic is shed **~69× faster** than good traffic is forwarded through a 15 ms backend.

**Host-enforced rate limiting** (token bucket, spec host-configured in the manifest; k6):

- The rate-limited route costs **~3.0 µs/req** (~32 % throughput, p99 unchanged) over a no-filter
  baseline when the bucket never denies — the filter dispatch floor plus the host-native bucket
  consult (and its multi-tenant quota check) on the hot path.
- Offered **5× over the configured rate**, the **allowed throughput converges to the bucket's refill
  rate** (≈ 1.0k/s for a 1000-token/s bucket) and **79 % is shed as 429** — decided at the edge in
  **~0.6 ms**, never reaching the backend.
- Buckets are **per key**: a hot key offered 4× its limit is throttled to its refill rate while a
  light key on the **same filter passes untouched (0 % shed)** — no cross-key starvation.

**Request-body hook** (buffer-then-decide, ADR 000025; export-presence zero-copy bypass, ADR 000038; k6):

- A filter that **reads** the body (`/body`, filter-hello) costs **~46 % throughput at 1 KB** and
  scales with payload: **~59 % at 100 KB**, **~68 % at 1 MB**, versus the streaming passthrough. A
  **header-only filter** (`/body-headeronly` — no `on-request-body` export) **streams the body
  through**: at 100 KB and 1 MB it lands **within ~2–7 % of `/baseline`** (no body tax, ADR 000038);
  at 1 KB it shows **−34 %** — that gap is the ordinary **WASM dispatch floor** dominating a tiny
  request, not a body cost.
- RSS at 1 MB × 50 VUs (`MALLOC_ARENA_MAX=4`, the shipped default): **~97 MB `/baseline` · ~191 MB
  `/body` · ~94 MB `/body-headeronly`**. The arena cap roughly halves the buffered path (an uncapped
  glibc held ~317 MB); the header-only bypass keeps it at baseline. The buffer stays bounded (16 MiB
  cap, fail-closed 413).

## Scope & honesty notes

- **Machine specs intentionally omitted.** Single commodity host, loopback, everything
  co-resident. Absolute throughput is contended and clock-variable; treat figures as relative /
  regression signals.
- **Generator-bound where noted.** The closed-loop sweep tops out near the *generator's* ceiling on
  its cores, not the proxy's: the same fast path serves a single route at ~261k req/s under the
  lighter oha (see the [plain HTTP/1.1 ceiling](#plain-http11-ceiling)), well above the k6 sweep's
  ~152k. The sweep curve's *shape* is the signal, not its absolute peak.
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
| LB pick — round-robin | 21 → 27 ns (3 → 32 instances) | ~O(1) over the eligible set |
| LB pick — P2C weighted-least-request | 31 → 62 ns | two eligibility passes + the sampled compare |
| LB pick — weighted Maglev | ~17 ns | + one table lookup |
| LB pick under swap churn (`pick_under_swap_churn`) | 111 → 82 → 88 ns (3 → 8 → 32 instances) | round-robin pick while a background thread continuously calls `update_endpoints` (ADR 000044) — the per-pick `ArcSwap<Endpoints>` load cost under worst-case concurrent churn |
| route match (`find_route`) | 35 ns → 216 ns (1 → 64 routes) | scans by specificity, allocation-free |
| ingress path normalization | ~48–65 ns clean / ~176 ns dot-segments | ADR 000027; a clean path is borrowed, no allocation |

All three LB algorithms are covered here; the macro suite only load-tests round-robin.
(An earlier revision under-reported the LB picks at ~7–17 ns: the bench never promoted its
instances to healthy, so it was timing the eligible==0 fail-fast path, not a real pick — the
kind of methodological bug this report exists to disclose. `pick_under_swap_churn` learned from
that: its rotating instance is never promoted either, but n−1 *other* instances are pre-promoted
and stay eligible throughout, so the bench still times a real pick, not the same fail-fast trap.)
The `n=3` cell reads slower than `n=8`/`n=32`, which is counter-intuitive — under continuous churn
the eligible set is tiny (2 instances) relative to the fixed cost of the concurrent
`update_endpoints` allocation contending for the same cache lines every tick; reported as measured
rather than smoothed, since a real (if surprising) noise floor is more useful than a tidier-looking
number.

**Extension plane** (`crates/host/benches/wasm.rs`):

| bench | cost | isolates |
| --- | --- | --- |
| `on_request` — pooled instance | ~2.1 µs/req | dispatch + call (init amortized) |
| `on_request` — fresh instance / request | ~28 µs/req | + per-request instantiation (the pool's value) |
| cold `load` (verify + instantiate + init) | ~15 ms | cosign signature + SBOM verification dominates |

The ~13× pooled→fresh gap here is the same one the [macro ladder](#the-wasm-cost-ladder--isolating-each-cost)
shows end-to-end (~18× there, with the HTTP layer around it) — the two layers agree, so a divergence
between them is a real bug. (This run is an A/B against a pre-ADR-000040 baseline on the same host:
the pooled and fresh calls are statistically unchanged by the OTLP exporter change — the span's new
`sampled` field and the sink gate cost nothing measurable.)

---

# 1. Load-balancing fast path

Subject: one Plecto route forwarding to an upstream pool of **3 instances**, round-robin pick
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
| keep-alive       | 261,155 | 0.18 ms | 0.50 ms |
| cold (TCP/req)   | 140,732 | 0.31 ms | 0.94 ms |

*(Re-measured 2026-07-04, post ADR 000050 / 000051 — `bash bench/perf/run-perf.sh ceiling`. Earlier
same-day figures (228,319 / 115,830, then 237,269 / 111,451 post harness-consolidation) sit within
this report's usual host-noise spread of the same measurement, not a real change.)*

A TCP handshake per request costs **~46 % throughput and +0.45 ms p99** even on loopback (where the
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
| 50  | **152,473** | 0.21 ms | 0.71 ms | 1.17 ms | 2.52 ms | 0% |
| 100 | 148,562 | 0.45 ms | 1.43 ms | 2.42 ms | 4.58 ms | 0% |
| 200 | 142,840 | 0.83 ms | 2.43 ms | 4.06 ms | 8.01 ms | 0% |
| 400 | 133,416 | 1.31 ms | 4.35 ms | 6.98 ms | 13.37 ms | 0% |
| 800 | 116,649 | 3.40 ms | 9.11 ms | 15.81 ms | 25.85 ms | 0% |

Throughput peaks at **~152k at 50 VUs** (the k6 generator's ceiling on its cores) and declines
**gracefully** as concurrency climbs — latency rises in proportion with **no failures and no cliff
even at 800 VUs**. The useful reading is the shape: a flat-then-declining ceiling with an orderly
latency climb, the pinned proxy never collapsing under the generator.

## Tail latency under open-loop load

Open-loop sends at a **constant arrival rate** regardless of how fast responses come back, so
queueing surfaces in the tail instead of being hidden — the *coordinated-omission-safe* model.

| Model | target | achieved | p50 | p95 | p99 | p99.9 | dropped | failed |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| open-loop, 0 ms backend (pinned) | 60,000/s | 59,276/s | 0.09 ms | 8.51 ms | 24.7 ms | 39.3 ms | 1.6% | 0% |

The pinned `OPENLOOP_RATE=60000` is **achieved to 99 %** with a sub-ms p50 and a ~25 ms p99 queueing
tail — a real queueing tail, not generator noise (Little's-law VU allocation with a capped
`maxVUs`). The runbook's automatic target (70 % of the closed-loop peak, ~107k/s) exceeds what the
co-resident generator can sustain, so the pinned rate stays the published figure; overload past the
generator's own ceiling degrades into `dropped_iterations`, not VU explosion.

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

- **Even baseline.** ~4k req/s split three ways while healthy.
- **Graceful ejection.** When **b** is driven unhealthy its share falls to zero within ~1 s and the
  survivors (a + c) absorb the full load **with zero failed requests**. The survivors' split is
  *not* even — the ejected instance's round-robin slot is taken by its neighbour — so traffic
  shifts but isn't re-balanced across the survivors (the all-healthy split *is* exactly even).
- **Fail-closed, not fail-open.** With **every** instance unhealthy, Plecto returns **HTTP 503**
  promptly (no hang, no blind forward); the 503/s line jumps to the full offered rate.
- **Fast recovery.** Restoring health returns instances to rotation within ~1 s.

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

> Re-measured 2026-07-04: a steady ~4k req/s open-loop while, at t=15 s (post-warmup),
> the manifest is rewritten `[a, b, c]` → `[a, b, d]` and SIGHUP-reloaded.

- **Zero client-visible failures.** All 240,000 responses over the 60 s run succeeded — **0 %
  failed** — even through the swap itself. Unlike a health-based ejection, nothing here ever needs
  to fail closed: `a` and `b` are unchanged addresses, so `reconcile` reuses their `Arc`s and
  health outright (ADR 000017's reuse rule), and only `d` starts pessimistic.
- **The swap completes within one second.** The transition second (t=15) shows a brief mixed
  bucket (`a=1614, b=1613, c=13, d=760`) as in-flight requests to `c` finish and the reconciled
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
| plain (h1)               | 261,155 | 0.18 ms | 0.50 ms | [ceiling](#plain-http11-ceiling) keep-alive |
| TLS h1, keep-alive       | 126,585 | 0.38 ms | 0.79 ms | record layer + TLS I/O path = Δ vs plain |
| TLS h1, handshake/req    | 26,351  | 1.71 ms | 4.97 ms | full handshake (ECDHE + signature) per request |
| TLS (h2)                 | 112,569 | 0.43 ms | 0.81 ms | h2 multiplexing over TLS |

The decomposition is the point. The kept-alive TLS delta reads **−51 % / +0.29 ms p99** vs the
plaintext ceiling: the TLS-terminated path is the **crypto-/TLS-I/O-bound** path that the
native-path optimisations don't reach — the next optimisation target the ratio exposes. **The
handshake still dominates** — forcing a fresh ECDHE handshake on *every* request collapses
throughput to ~26k/s (~4.8× below kept-alive TLS) and adds ~1.3 ms median. And **h2 is clean**
(113k/s, p99 0.81 ms). A client that funnels many VUs over a handful of multiplexed connections can
make h2 *look* far worse (head-of-line queueing, not server work); measuring with a
connection-per-concurrency client removes that artifact.

*(Re-measured 2026-07-04 on **aws-lc-rs** (ADR 000051, replacing `ring`) — `bash bench/perf/run-perf.sh tls`.
This is a provider change, not a re-measurement of the same thing: the absolute numbers here are a
new baseline for the `aws-lc-rs` path, not directly comparable to earlier `ring`-based snapshots.
The **ratios hold**, though — kept-alive TLS costs ~51 % of plaintext under either provider, within
this report's usual host-noise spread of the same measurement.)*

---

# 2. WASM extension plane

Plecto runs each request's *decision* — auth, rewriting, rate limiting, policy — as a sandboxed
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
| `/baseline` | native fast path (no filter) | 261,155 | 0.18 ms | 0.50 ms |
| `/noop-pooled` | a **pure no-op** WASM filter, pooled | 129,163 | 0.37 ms | 0.74 ms |
| `/noop-fresh` | the same no-op, **fresh instance / request** | 7,896 | 4.28 ms | 24.4 ms |
| `/trusted` | the real `filter-apikey`, pooled | 123,404 | 0.39 ms | 0.75 ms |
| `/ondemand` | `filter-apikey`, fresh instance / request | 10,911 | 3.85 ms | 20.0 ms |

*(Re-measured 2026-07-04. The `/baseline` row is sourced from [ceiling.csv](#plain-http11-ceiling)
per the harness-consolidation convention (see the [TL;DR](#tldr)); the other four rungs are
measured together in one run for internal consistency, and the deltas below are computed against
that same run's own baseline.)*

- **baseline → noop-pooled** = the **irreducible extension-plane dispatch cost** (chain dispatch +
  the blocking-pool hop + instance acquisition + one empty host↔guest crossing), with *no* filter
  work: **−51 % throughput, ≈ 3.9 µs/req**. Every WASM filter pays this floor.
- **noop-pooled → noop-fresh** = the **per-request instantiation cost**, now cleanly isolated from any
  host work: throughput collapses **~16×** (129k → 7.9k). This is what pooling buys.
- **noop-pooled → trusted** = a **real filter's own work** on top of the no-op (header parse +
  host-KV lookup + counter): only **−4 % (~0.4 µs)**. The apikey filter is cheap; the dispatch floor
  dominates it.
- **noop-fresh and ondemand are the same order of magnitude** (7.9k vs 10.9k req/s), confirming
  instantiation dominates the fresh path — the filter's per-request work is noise next to re-paying
  `init` (~28 µs) every request.

### The same ladder at a fixed below-knee rate — honest tails

> W1b — every rung offered the **same** fixed 4,737 req/s (60 % of the slowest rung's ceiling), 50
> connections, oha `-q` + `--latency-correction` (coordinated-omission-safe). Identical offered
> load, so the latency columns are directly comparable — and none of them is queueing-at-max-load.

| Route | achieved | p50 | p90 | p99 |
| --- | --- | --- | --- | --- |
| `/baseline` | 4,737/s | 0.23 ms | 0.37 ms | 0.66 ms |
| `/noop-pooled` | 4,737/s | 0.38 ms | 0.55 ms | 0.99 ms |
| `/trusted` | 4,737/s | 0.42 ms | 0.58 ms | 0.92 ms |
| `/noop-fresh` | 4,703/s | 73.7 ms | 300 ms | 440 ms |
| `/ondemand` | 4,712/s | 53.7 ms | 342 ms | 820 ms |

At a rate every rung sustains, the pooled dispatch floor costs **+0.15 ms p50 / +0.33 ms p99** over
native and the real pooled filter **+0.19 ms p50 / +0.27 ms p99** — sub-millisecond even at p99.
The fresh rungs, which *survive* at this rate (they cannot at their ceilings), still live at
**p99 440–820 ms**: per-request instantiation is not a tail you can operate behind, which is the
pooling decision stated as a latency, not a throughput.

**The µs/req deltas are the invariants to track for regressions, not the percentages** (which widen or
shrink whenever the *baseline* moves, as it just did). These macro deltas **reconcile with the
in-process [micro-benchmarks](#0-micro-benchmarks-in-process-criterion)**: criterion clocks the pooled
per-request call at ~2.1 µs; the remaining ~2 µs of the macro floor is the `spawn_blocking` handoff
(sync wasmtime, `!Send` store) that a route with no filters skips entirely — and the fresh
(instantiate + init + call) at ~28 µs matches the ladder's collapse.

## Short-circuit: rejecting bad traffic at the edge

![Accept vs reject latency](img/wasm_shortcircuit.webp)

> W2 — fixed 2000 req/s, 15 ms backend, ~90 % valid / ~10 % bad keys (k6). 107,908 accepted, 12,122 rejected.

| Path | p50 | p95 | p99 |
| --- | --- | --- | --- |
| accept (200, forwarded) | 16.27 ms | 17.08 ms | 17.47 ms |
| reject (401, short-circuited) | 0.24 ms | 0.39 ms | 0.56 ms |

Accepted requests cost the 15 ms backend plus the small pooled-filter + proxy overhead. Rejected
requests are decided **at the edge in ~0.24 ms** and never reach the upstream: bad traffic is shed
**~69× faster** than good traffic is forwarded, and is harmless to the backend it would otherwise
hit. (Filter faults or deadline overruns **fail closed** — 502/504 — exercised by the test suite,
not this benchmark.)

## Outbound ext_authz (ADR 000036)

A filter can call an external authorization service per request over the lent, SSRF-guarded outbound
capability (`filter-extauthz`). Its per-request cost decomposes into three parts, only the first two of
which are Plecto's:

- **WASM tax** — the same dispatch floor and (for untrusted) instantiation the
  [cost ladder](#the-wasm-cost-ladder--isolating-each-cost) measures.
- **The outbound gate** — the operator allowlist (an exact scheme/host/port match) plus the SSRF
  classification of every resolved address. Structurally this is a small scan + a handful of octet
  checks — nanoseconds, the same order as an LB pick (see [# 0](#0-micro-benchmarks-in-process-criterion)) —
  and negligible next to the two costs around it.
- **The network round-trip** to the authz endpoint — which is the *operator's* authz-service latency,
  not a Plecto overhead, and dominates the total (as proxy-wasm's own guidance notes for ext_authz).

Two facts keep this out of the headline load numbers for now, honestly rather than faked: the SSRF
guard **blocks loopback by design**, so a hermetic mock authz needs a non-loopback endpoint
(environment-specific), and the current connector opens **a new connection per call** — outbound
connection pooling is a follow-up. A through-the-guest ext_authz *load* benchmark is therefore
deferred (like [HTTP/3](#http3)) rather than published with an environment-dependent,
connect-per-request number. The capability itself is verified end-to-end by the host's `outbound-http`
test suite (allowlist deny + the DNS-rebinding SSRF block).

## Host-enforced rate limiting

Plecto's rate limiter is a **host-native token bucket** (ADR 000026): the bucket spec
(`capacity` / `refill_tokens` / `refill_interval_ms`) is configured **in the operator's manifest**,
not by the filter — an untrusted filter passes only `(key, cost)` and so cannot widen its own limit.
The refill + counting stay host-side (the WASM boundary is not crossed on the hot path); the filter
only decides *whether* to consult the limiter and *on what key*. Driven through `bench/harnesses/bench-server`
(`filter-hello`, pooled); a `429` carries `retry-after-ms`.

### Overhead — the cost of consulting the bucket

> R1 — 50 VUs, 0 ms backend, a **never-deny** bucket spread across 1000 keys (k6). `/baseline` vs
> `/ratelimit`.

| Route | req/s | p50 | p99 |
| --- | --- | --- | --- |
| /baseline (no filter) | 154,680 | 0.21 ms | 1.26 ms |
| /ratelimit (bucket) | 105,353 | 0.39 ms | 1.17 ms |

The rate-limited route adds **~3.0 µs/req** over the no-filter baseline (~32 % of its throughput;
p99 unchanged — the µs/req is the inverse-throughput delta at 50 VUs). That is the whole hot-path
tax with no rejections — the
filter dispatch floor (the same one the [WASM ladder](#the-wasm-cost-ladder--isolating-each-cost)
isolates) plus the host-native bucket consult, including the per-call host-state quota check
(ADR 000027) that keeps a multi-tenant filter's bucket count bounded.

### Enforcement — does it actually hold the rate?

![Rate-limit enforcement](img/ratelimit_enforce.webp)

> R2 — a **tight** bucket (refill 1000 tok/s, burst 2000), offered **5000 req/s** open-loop at one
> key for 30 s (k6).

| offered | allowed (200) | shed (429) | accept p99 | 429 p99 |
| --- | --- | --- | --- | --- |
| 5,000/s | **1,033/s** | 79.3% | 3.21 ms | 0.55 ms |

Offered 5× over the limit, the **allowed throughput converges to the bucket's refill rate**
(≈ 1.0k/s — the configured 1000 tok/s plus the burst amortised over the run). The excess **79 % is
shed as 429**, each decided at the edge in **~0.6 ms** without touching the backend. Open-loop
(`constant-arrival-rate`) keeps offering regardless of the 429s, so the enforcement is measured
honestly, not hidden by a self-throttling client.

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
| 1 KB   | /baseline        | 148,524 | 152 MB/s  | 1.16 ms |
| 1 KB   | /body            | 80,991  | 83 MB/s   | 1.38 ms |
| 1 KB   | /body-headeronly | 97,476  | 100 MB/s  | 1.25 ms |
| 100 KB | /baseline        | 47,074  | 4820 MB/s | 3.92 ms |
| 100 KB | /body            | 19,263  | 1973 MB/s | 5.42 ms |
| 100 KB | /body-headeronly | 43,777  | 4483 MB/s | 4.17 ms |
| 1 MB   | /baseline        | 6,308   | 6615 MB/s | 31.1 ms |
| 1 MB   | /body            | 2,011   | 2108 MB/s | 42.2 ms |
| 1 MB   | /body-headeronly | 6,186   | 6487 MB/s | 31.0 ms |

A filter that **reads** the body pays for it, growing with payload: **~46 % throughput at 1 KB** (the
buffer + WASM transform dominate the small request), **~59 % at 100 KB**, **~68 % at 1 MB** (a
full-body copy + uppercase per request). A **header-only filter takes the zero-copy bypass** — the
body never enters guest memory: at 100 KB and 1 MB it lands **within ~2–7 % of `/baseline`**
(ADR 000038). At 1 KB it reads **−34 %** — but that gap is the per-request **WASM dispatch floor**
(the same ~4 µs the [ladder](#the-wasm-cost-ladder--isolating-each-cost) isolates) showing against
the native baseline on a tiny request, not a body cost. RSS at 1 MB × 50 VUs (fresh proxy per
route, `MALLOC_ARENA_MAX=4`): **~97 MB `/baseline` · ~191 MB `/body` · ~94 MB
`/body-headeronly`** (`data/body_rss.csv`). Two levers cut what an uncapped glibc once held (~317 MB): the arena cap
roughly halves the buffered path, and the export-presence bypass keeps a header-only route at
baseline. The buffer stays bounded (16 MiB cap, fail-closed 413) for the
filters that do read the body. The remaining buffered-path copy is the target of a future `stream<u8>`
increment (ADR 000020); a per-request time-series / allocator-sweep decomposition lives in
`bench/perf/mem_matrix.py`.

## Footprint

Idle resident set and the marginal cost of an open connection (`bench/harnesses/bench-server`):

| Metric | Value |
| --- | --- |
| idle RSS | ~41 MB |
| RSS holding ~1,000 idle keep-alive connections | ~65 MB |
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
| read-only (control) | read 100 % | GET `/baseline` (1 KB) | 0.15 ms | 9.72 ms | 27.3 ms |
| mix | read 60 % | GET `/baseline` (1 KB) | 0.12 ms | 17.66 ms | 34.1 ms |
| mix | auth read 25 % | GET `/ratelimit` (tenant key) | 0.18 ms | 17.92 ms | — |
| mix | write 10 % | POST `/body` (1 KB) | 0.22 ms | 19.30 ms | — |
| mix | large 5 % | POST `/body` (100 KB) | 0.74 ms | 24.42 ms | — |

Both profiles achieve the full 20k/s (dropped iterations ≤ 0.26 %, zero 429s from the never-deny
bucket). The pairing is the point: at the same rate, **the blend costs the plain reads +7.9 ms at
p99** (9.7 → 17.7 ms) this run — head-of-line pressure from the body classes — and the classes order
exactly as their work predicts (read < auth read < write < large, all p50s sub-millisecond). This
session's absolute tails run noisier than earlier snapshots (a shared, non-dedicated session host);
the ordering and the pairing methodology are the durable signal, not the absolute ms this run. A
single-endpoint test hides all of this; the control run keeps it honest.

## HTTP/3

The fast path terminates **HTTP/3 over QUIC** (ADR 000016; `tls-http` serves h1/h2/h3 on one port). A
functional check confirms it end-to-end:

```
curl --http3-only https://…/api/hello  ->  status=200 http_version=3
```

A **rigorous, coordinated-omission-safe H3 *load* benchmark is deferred**: the load generators here
(oha, k6) have no native HTTP/3, and a correct H3 tail needs an H3-capable open-loop generator such as
**Nighthawk** or **h2load** (nghttp2's generator, `--npn-list h3`, with qlog output for QUIC-level
diagnostics — the more readily available option since it ships as a plain distro package rather than
a source build). Rather than publish process-spawn-bound `curl`-loop numbers, the H3 load figure is
honestly left absent until that tooling is in place — the server support is verified, not the throughput.

## WebSocket Upgrade tunnel (ADR 000048)

Plecto's HTTP/1.1 Upgrade path (ADR 000048): a route declaring `[route.upgrade] protocols =
["websocket"]` forwards the client's handshake (controlled re-issue — hop-by-hop stripping stays
the default for every other route), and on the upstream's 101 the proxy splices the two connections
into an opaque bidirectional byte tunnel — the same relay technique nginx `proxy_pass`, Envoy's
generic TCP proxy, and HAProxy's `mode tcp` all use post-upgrade. This is a **different load shape**
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

> Re-measured 2026-07-04: `bash bench/perf/run-perf.sh ws`.

| Scenario | Result |
| --- | --- |
| Handshake rate | 10,000/10,000 Upgrades succeeded at the paced 500/s target — **0 % failed** over 20 s |
| Tunnel footprint | idle RSS 45.5 MB → 63.8 MB with 1,000 held tunnels — **~18.8 KB/tunnel** |

![WebSocket echo throughput](img/ws_echo.webp)

| Payload | messages/s | throughput | p50 | p99 |
| --- | --- | --- | --- | --- |
| 1 KB  | 173,574 | 178 MB/s   | 0.18 ms | 1.12 ms |
| 64 KB | 83,160  | 5,450 MB/s | 0.54 ms | 1.57 ms |

The handshake rate holds at 100 % of target with zero rejections — the Upgrade path costs nothing
beyond the ordinary per-request floor. Tunnel footprint (~19 KB/tunnel) is in the same order as a
held keep-alive HTTP connection ([Footprint](#footprint): ~25 KB/conn) — a tunnel is not
meaningfully heavier to hold open than an ordinary idle connection, only longer-lived. Echo
throughput at 1 KB (174k msg/s) exceeds 64 KB (83k msg/s) as expected — the larger payload's
messages/s falls roughly in proportion to its size, while aggregate byte throughput rises (178 →
5,450 MB/s), consistent with a per-message dispatch floor that amortizes better over larger frames.

*(A per-request small-frame delayed-ACK stall — the exact Nagle signature the
[connection-churn history](#plain-http11-ceiling) already found once — appeared during this
scenario's development on the mock upstream's accept-side socket; disabling Nagle there fixed it.
The numbers above are post-fix. The idle timeout (default 5 min, ADR 000048) and its interaction
with the breaker permit / least-request in-flight counters are exercised by the host's own test
suite, not this benchmark; a transfer-bytes / tunnel-duration metric is a documented observability
gap (ADR 000048's re-examine condition (d)) this report will pick up once it lands.)*

---

## Methodology — why the numbers look the way they do

- **Open- vs closed-loop matters.** A closed-loop generator throttles itself whenever the server
  slows, quietly hiding queueing and under-reporting the tail (Gil Tene's *coordinated omission*).
  An open-loop, fixed-rate generator keeps offering load and surfaces the real tail. We treat
  open-loop figures as authoritative for latency tails and closed-loop figures as a throughput ceiling.
- **Pin the proxy, pin the generator, separately.** Co-residency means the generator competes with
  Plecto for CPU; pinning each to a disjoint core set removes that contention from the proxy's
  numbers. Absolute figures still shift on dedicated hardware and a real network — they exist to
  catch regressions between changes.
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
- **Tooling by job.** criterion for the deterministic in-process layer and the CI gate; k6
  `constant-arrival-rate` (open-loop) for the macro tails; oha for single-route capacity ceilings
  and — with `-q` + `--latency-correction` — for the fixed-rate CO-safe ladder tails;
  `plecto-loadgen` (Rust, `bench/loadgen/`) for the fault timeline, the endpoint-set swap timeline,
  the round-robin count, the footprint connection-holder, and the WebSocket handshake/hold/echo
  scenarios. Neither oha nor k6 has native HTTP/3, so H3 *load* is deferred to an H3-capable
  generator (Nighthawk or h2load) rather than faked (see [HTTP/3](#http3)).
- **CI regression gate (opt-in).** Per-PR runs only the light criterion micro-benchmarks
  (`cargo bench -- --baseline main`, seconds); the heavy k6/oha macro suite runs on manual dispatch /
  nightly. Hosted-runner numbers are treated as *relative* (regression direction), never absolute — CI
  VMs are noisy neighbours. Running the project's own benchmarks in GitHub Actions is squarely within
  GitHub's Acceptable Use ("testing … the software project associated with the repository"); keeping
  heavy load off per-PR respects the "no disproportionate burden" clause.
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
- [Nighthawk](https://github.com/envoyproxy/nighthawk) — Envoy's open-loop, HTTP/1–2–3 load generator; one tool an HTTP/3 *load* benchmark would use (deferred here).
- [h2load](https://nghttp2.org/documentation/h2load-howto.html) — nghttp2's load generator; supports HTTP/3 (`--npn-list h3`) with qlog output, and — like Nighthawk — a candidate for the deferred H3 load run.
- [wrk2](https://github.com/giltene/wrk2) — constant throughput with corrected latency recording.
- [Wasmtime](https://docs.wasmtime.dev/) — the pooling allocator and epoch interruption behind pooled vs on-demand filter instances.
- [WebAssembly Component Model](https://component-model.bytecodealliance.org/) — the `plecto:filter` contract is a Component Model world.
- [RFC 6455](https://www.rfc-editor.org/rfc/rfc6455) — the WebSocket protocol `bench-server`'s `/ws` mock upstream and `plecto-loadgen`'s `ws` subcommand implement (handshake + frame codec) to drive the Upgrade tunnel scenario.
