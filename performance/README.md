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
  GIL-bound workers melted before the proxy did) runs the fault-injection timeline and the
  round-robin count; and [oha](https://github.com/hatoo/oha) drives the single-route overhead
  (WASM W1), TLS and connection-churn runs. Different generators have different ceilings — **numbers
  are comparable within a section, and across same-generator sections, but not blindly across all of
  them** (a lighter generator reveals a higher proxy ceiling). Each section names its generator.
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
  the optional dashboard's phone-home are disabled. Nothing leaves the host during a load run.
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

**Load-balancing fast path** (plaintext HTTP/1.1, 3 upstreams, trivial 0 ms backend; k6):

- Closed-loop throughput peaks at **~147k req/s** (50 VUs) with **p99 ≈ 1.2 ms** and zero
  failures; it degrades **gracefully** — still **~112k at 800 VUs** (p99 16.9 ms) with **0 failures
  and no latency cliff**.
- Open-loop at the pinned **60k/s** now **achieves 59.3k/s (99 %)** with **p50 0.09 ms, p99 25 ms,
  1.4 % dropped, 0 % failed** — the previous snapshot managed only 46.5k/s at the same target
  because the *generator* saturated first; the k6 headroom fixes (see snapshot context) moved
  that, and the tail is now a real queueing tail, not generator noise. The runbook's automatic
  target (70 % of the closed-loop peak, ~103k/s) still exceeds the co-resident generator's
  ceiling (36k/s achieved), which is why the pinned rate stays the published figure.
- Round-robin across three upstreams is **even to within one request** (33.3 % each).
- **Resilience is as designed**: ejecting one upstream drops its share to zero in ~1 s and the
  survivors absorb the load with **no client-visible errors**; a *total* outage **fails closed
  with HTTP 503** and the pool **recovers within ~1 s** of health returning.
- TLS termination reads as **~49 % throughput vs plaintext** (h1 keep-alive ~118k vs plain ~243k):
  the TLS path is **crypto-bound**, so the native-path optimisations don't reach it (see
  [TLS](#tls-termination)).
- A **kept-alive** connection serves **~228k req/s**; forcing a **TCP handshake per request** costs
  **~49 % throughput and +0.7 ms p99** — connection reuse is load-bearing (see
  [churn](#connection-churn)).

**WASM extension plane** (the cost of running a decision as a sandboxed component; oha / k6):

- A **cost ladder** isolates each cost by adjacent delta. The **irreducible dispatch floor** — a pure
  no-op WASM filter, pooled — is **≈ 4.0 µs/req (−49 % throughput)** over the native baseline; a
  **real filter's own work** (`filter-apikey`: header + host-KV + counter) adds only **another
  ~0.4 µs (−5 %)**; and running that filter **fresh-per-request** instead of pooled costs **~18×**
  throughput — the price of re-paying `init` every request, and the value of pooling. The **µs/req
  is the portable figure**. A new **fixed-rate tail run** (all rungs at the same below-knee 4k/s,
  CO-corrected) puts honest latency on the same ladder: the pooled no-op adds **+0.25 ms p99**
  over native, the real pooled filter +0.32 ms — while the fresh rungs live at **p99 194–391 ms**.
- These macro deltas **reconcile with the criterion [micro-benchmarks](#0-micro-benchmarks-in-process-criterion)**:
  the pooled guest call is ~2.1 µs, and the remainder of the floor is the blocking-pool handoff the
  no-filter path skips entirely — the two layers agree.
- A rejected request (**HTTP 401 short-circuit**) is decided in **~0.25 ms and never reaches the
  backend** — bad traffic is shed **~65× faster** than good traffic is forwarded through a 15 ms backend.

**Host-enforced rate limiting** (token bucket, spec host-configured in the manifest; k6):

- The rate-limited route costs **~2.8 µs/req** (~29 % throughput, p99 unchanged) over a no-filter
  baseline when the bucket never denies — the filter dispatch floor plus the host-native bucket
  consult (and its multi-tenant quota check) on the hot path.
- Offered **5× over the configured rate**, the **allowed throughput converges to the bucket's refill
  rate** (≈ 1.0k/s for a 1000-token/s bucket) and **79 % is shed as 429** — decided at the edge in
  **~0.6 ms**, never reaching the backend.
- Buckets are **per key**: a hot key offered 4× its limit is throttled to its refill rate while a
  light key on the **same filter passes untouched (0 % shed)** — no cross-key starvation.

**Request-body hook** (buffer-then-decide, ADR 000025; export-presence zero-copy bypass, ADR 000038; k6):

- A filter that **reads** the body (`/body`, filter-hello) costs **~48 % throughput at 1 KB** and
  scales with payload: **~59 % at 100 KB**, **~67 % at 1 MB**, versus the streaming passthrough. A
  **header-only filter** (`/body-headeronly` — no `on-request-body` export) **streams the body
  through**: at 100 KB and 1 MB it lands **within ~3–8 % of `/baseline`** (no body tax, ADR 000038);
  at 1 KB it shows **−35 %** — that gap is the ordinary **WASM dispatch floor** dominating a tiny
  request, not a body cost.
- RSS at 1 MB × 50 VUs (`MALLOC_ARENA_MAX=4`, the shipped default): **~102 MB `/baseline` · ~181 MB
  `/body` · ~104 MB `/body-headeronly`**. The arena cap roughly halves the buffered path (an uncapped
  glibc held ~317 MB); the header-only bypass keeps it at baseline. The buffer stays bounded (16 MiB
  cap, fail-closed 413).

## Scope & honesty notes

- **Machine specs intentionally omitted.** Single commodity host, loopback, everything
  co-resident. Absolute throughput is contended and clock-variable; treat figures as relative /
  regression signals.
- **Generator-bound where noted.** The closed-loop sweep tops out near the *generator's* ceiling on
  its cores, not the proxy's: the same fast path serves a single route at ~228k–243k req/s under the
  lighter oha (see WASM baseline / TLS plain / churn), well above the k6 sweep's ~147k. The sweep
  curve's *shape* is the signal, not its absolute peak.
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
| route match (`find_route`) | 35 ns → 216 ns (1 → 64 routes) | scans by specificity, allocation-free |
| ingress path normalization | ~48–65 ns clean / ~176 ns dot-segments | ADR 000027; a clean path is borrowed, no allocation |

All three LB algorithms are covered here; the macro suite only load-tests round-robin.
(An earlier revision under-reported the LB picks at ~7–17 ns: the bench never promoted its
instances to healthy, so it was timing the eligible==0 fail-fast path, not a real pick — the
kind of methodological bug this report exists to disclose.)

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

## Throughput & latency vs concurrency

Closed-loop sweep (k6 `constant-vus`) — a fixed number of virtual users, each issuing its next
request only after the previous response. Rising concurrency walks the load curve.

![Throughput vs concurrency](img/throughput_vs_concurrency.webp)
![Latency percentiles vs concurrency](img/latency_vs_concurrency.webp)

| VUs | req/s | p50 | p95 | p99 | p99.9 | failed |
| --- | --- | --- | --- | --- | --- | --- |
| 50  | **147,155** | 0.22 ms | 0.73 ms | 1.22 ms | 2.52 ms | 0% |
| 100 | 145,628 | 0.44 ms | 1.51 ms | 2.67 ms | 4.87 ms | 0% |
| 200 | 139,670 | 0.82 ms | 2.58 ms | 4.23 ms | 8.40 ms | 0% |
| 400 | 126,313 | 1.45 ms | 4.73 ms | 7.73 ms | 14.77 ms | 0% |
| 800 | 112,104 | 3.66 ms | 9.84 ms | 16.85 ms | 26.19 ms | 0% |

Throughput peaks at **~147k at 50–100 VUs** (the k6 generator's ceiling on its cores) and declines
**gracefully** as concurrency climbs — latency rises in proportion with **no failures and no cliff
even at 800 VUs**. The useful reading is the shape: a flat-then-declining ceiling with an orderly
latency climb, the pinned proxy never collapsing under the generator. (With warm-up excluded the
whole curve sits slightly cleaner than the previous snapshot — same ceiling, marginally lower
tails at every level.)

## Tail latency under open-loop load

Open-loop sends at a **constant arrival rate** regardless of how fast responses come back, so
queueing surfaces in the tail instead of being hidden — the *coordinated-omission-safe* model.

| Model | target | achieved | p50 | p95 | p99 | p99.9 | dropped | failed |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| open-loop, 0 ms backend (pinned) | 60,000/s | 59,335/s | 0.09 ms | 9.2 ms | 25 ms | 41 ms | 1.4% | 0% |

The pinned `OPENLOOP_RATE=60000` is now **achieved to 99 %** with a sub-ms p50 and a 25 ms p99
queueing tail — the previous snapshot managed only 46.5k/s at the same target because the
*generator* saturated first (its Python-era sibling melted outright). The harness rebuild
(`discardResponseBodies`, Little's-law VU allocation with a capped `maxVUs`) moved the generator's
ceiling, so this tail is finally the proxy's queueing, not the generator's. The runbook's automatic
target (70 % of the closed-loop peak, ~103k/s) still exceeds what the co-resident generator can
offer (36k/s achieved, dropped iterations counting the shortfall honestly) — the pinned rate stays
the published figure, and overload now degrades into `dropped_iterations` instead of VU explosion.

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

## TLS termination

The same single-backend pass-through, re-run with rustls TLS termination, decomposed so the cost
of each layer is separable (oha; h1 client isolates the record/handshake split from h2
multiplexing). `plain (h1)` is the plaintext baseline.

![TLS vs plain](img/tls_vs_plain.webp)

| Variant | req/s | p50 | p99 | isolates |
| --- | --- | --- | --- | --- |
| plain (h1)               | 242,705 | 0.19 ms | 0.52 ms | baseline |
| TLS h1, keep-alive       | 118,336 | 0.40 ms | 0.86 ms | record layer + TLS I/O path = Δ vs plain |
| TLS h1, handshake/req    | 29,761  | 1.49 ms | 4.32 ms | full handshake (ECDHE + signature) per request |
| TLS (h2)                 | 105,521 | 0.45 ms | 0.91 ms | h2 multiplexing over TLS |

The decomposition is the point. The **absolute TLS numbers are stable across snapshots** (h1
keep-alive ~117–118k, h2 ~105k, handshake ~30k) while the plaintext baseline sits at ~243k, so the
kept-alive TLS delta reads **−51 % / +0.34 ms p99**: the TLS-terminated path is the
**crypto-/TLS-I/O-bound** path that the native-path optimisations don't reach — the next
optimisation target the ratio exposes. **The handshake still dominates** — forcing a fresh ECDHE
handshake on *every* request collapses throughput to ~30k/s (~4× below kept-alive TLS) and adds
~1 ms median. And **h2 is clean** (105k/s, p99 0.91 ms). A client that funnels many VUs over a
handful of multiplexed connections can make h2 *look* far worse (head-of-line queueing, not server
work); measuring with a connection-per-concurrency client removes that artifact.

## Connection churn

The cost of *establishing* a connection vs reusing one, on the same plaintext single-backend path
(oha; keep-alive vs `--disable-keepalive` = a fresh TCP handshake per request).

![Connection churn](img/churn.webp)

| Variant | req/s | p50 | p99 |
| --- | --- | --- | --- |
| keep-alive       | 228,319 | 0.20 ms | 0.58 ms |
| cold (TCP/req)   | 115,830 | 0.37 ms | 1.30 ms |

A TCP handshake per request costs **~49 % throughput and +0.72 ms p99** even on loopback (where the
handshake is nearly free) — over a real network the gap widens with RTT. Connection reuse is
load-bearing; this is the plaintext analogue of the TLS handshake-per-request row above.

> **A note on a latency bug this scenario caught.** An early body run showed a ~40 ms p99 cliff on
> medium streamed bodies — the signature of a delayed-ACK stall. The upstream client had Nagle's
> algorithm on (no `TCP_NODELAY`), so a streamed request body sent in several writes stalled on the
> peer's delayed-ACK timer. Disabling Nagle on the upstream sockets — standard practice for L7
> proxies — removed it (100 KB streamed p99 42.9 ms → 4.2 ms). The numbers here are post-fix.

---

# 2. WASM extension plane

Plecto runs each request's *decision* — auth, rewriting, rate limiting, policy — as a sandboxed
**WebAssembly Component Model filter**, not native proxy code. This measures what that costs,
changing only **how the decision runs**. The bundled `bench/harnesses/wasm-bench` serves a **ladder** of
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
| `/baseline` | native fast path (no filter) | 240,262 | 0.19 ms | 0.47 ms |
| `/noop-pooled` | a **pure no-op** WASM filter, pooled | 122,596 | 0.39 ms | 0.77 ms |
| `/noop-fresh` | the same no-op, **fresh instance / request** | 6,692 | 4.76 ms | 25.5 ms |
| `/trusted` | the real `filter-apikey`, pooled | 116,394 | 0.41 ms | 0.78 ms |
| `/ondemand` | `filter-apikey`, fresh instance / request | 7,714 | 4.68 ms | 23.4 ms |

- **baseline → noop-pooled** = the **irreducible extension-plane dispatch cost** (chain dispatch +
  the blocking-pool hop + instance acquisition + one empty host↔guest crossing), with *no* filter
  work: **−49 % throughput, ≈ 4.0 µs/req**. Every WASM filter pays this floor.
- **noop-pooled → noop-fresh** = the **per-request instantiation cost**, now cleanly isolated from any
  host work: throughput collapses **~18×** (123k → 6.7k). This is what pooling buys.
- **noop-pooled → trusted** = a **real filter's own work** on top of the no-op (header parse +
  host-KV lookup + counter): only **−5 % (~0.4 µs)**. The apikey filter is cheap; the dispatch floor
  dominates it.
- **noop-fresh ≈ ondemand** confirms instantiation dominates the fresh path — the filter's per-request
  work is noise next to re-paying `init` (~28 µs) every request.

### The same ladder at a fixed below-knee rate — honest tails

> W1b — every rung offered the **same** fixed 4,014 req/s (60 % of the slowest rung's ceiling), 50
> connections, oha `-q` + `--latency-correction` (coordinated-omission-safe). Identical offered
> load, so the latency columns are directly comparable — and none of them is queueing-at-max-load.

| Route | achieved | p50 | p90 | p99 |
| --- | --- | --- | --- | --- |
| `/baseline` | 4,014/s | 0.27 ms | 0.42 ms | 0.68 ms |
| `/noop-pooled` | 4,014/s | 0.39 ms | 0.55 ms | 0.93 ms |
| `/trusted` | 4,014/s | 0.42 ms | 0.59 ms | 0.99 ms |
| `/noop-fresh` | 4,014/s | 48.8 ms | 215 ms | 391 ms |
| `/ondemand` | 4,013/s | 32.2 ms | 114 ms | 194 ms |

At a rate every rung sustains, the pooled dispatch floor costs **+0.12 ms p50 / +0.25 ms p99** over
native and the real pooled filter **+0.15 ms p50 / +0.32 ms p99** — sub-millisecond even at p99.
The fresh rungs, which *survive* at this rate (they cannot at their ceilings), still live at
**p99 194–391 ms**: per-request instantiation is not a tail you can operate behind, which is the
pooling decision stated as a latency, not a throughput.

**The µs/req deltas are the invariants to track for regressions, not the percentages** (which widen or
shrink whenever the *baseline* moves, as it just did). These macro deltas **reconcile with the
in-process [micro-benchmarks](#0-micro-benchmarks-in-process-criterion)**: criterion clocks the pooled
per-request call at ~2.1 µs; the remaining ~2 µs of the macro floor is the `spawn_blocking` handoff
(sync wasmtime, `!Send` store) that a route with no filters skips entirely — and the fresh
(instantiate + init + call) at ~28 µs matches the ladder's collapse.

## Short-circuit: rejecting bad traffic at the edge

![Accept vs reject latency](img/wasm_shortcircuit.webp)

> W2 — fixed 2000 req/s, 15 ms backend, ~90 % valid / ~10 % bad keys (k6). 108,216 accepted, 11,815 rejected.

| Path | p50 | p95 | p99 |
| --- | --- | --- | --- |
| accept (200, forwarded) | 16.26 ms | 17.08 ms | 17.43 ms |
| reject (401, short-circuited) | 0.25 ms | 0.43 ms | 0.61 ms |

Accepted requests cost the 15 ms backend plus the small pooled-filter + proxy overhead. Rejected
requests are decided **at the edge in ~0.25 ms** and never reach the upstream: bad traffic is shed
**~65× faster** than good traffic is forwarded, and is harmless to the backend it would otherwise
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
only decides *whether* to consult the limiter and *on what key*. Driven through `bench/harnesses/edge-bench`
(`filter-hello`, pooled); a `429` carries `retry-after-ms`.

### Overhead — the cost of consulting the bucket

> R1 — 50 VUs, 0 ms backend, a **never-deny** bucket spread across 1000 keys (k6). `/baseline` vs
> `/ratelimit`.

| Route | req/s | p50 | p99 |
| --- | --- | --- | --- |
| /baseline (no filter) | 148,245 | 0.22 ms | 1.33 ms |
| /ratelimit (bucket) | 104,924 | 0.39 ms | 1.17 ms |

The rate-limited route adds **~2.8 µs/req** over the no-filter baseline (~29 % of its throughput;
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
| 5,000/s | **1,033/s** | 79.3% | 3.02 ms | 0.60 ms |

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
| 1 KB   | /baseline        | 142,325 | 146 MB/s  | 1.18 ms |
| 1 KB   | /body            | 74,261  | 76 MB/s   | 1.44 ms |
| 1 KB   | /body-headeronly | 92,589  | 95 MB/s   | 1.27 ms |
| 100 KB | /baseline        | 44,910  | 4599 MB/s | 4.07 ms |
| 100 KB | /body            | 18,513  | 1896 MB/s | 5.47 ms |
| 100 KB | /body-headeronly | 41,443  | 4244 MB/s | 4.33 ms |
| 1 MB   | /baseline        | 6,131   | 6428 MB/s | 32.2 ms |
| 1 MB   | /body            | 2,005   | 2103 MB/s | 40.8 ms |
| 1 MB   | /body-headeronly | 5,958   | 6247 MB/s | 32.3 ms |

A filter that **reads** the body pays for it, growing with payload: **~48 % throughput at 1 KB** (the
buffer + WASM transform dominate the small request), **~59 % at 100 KB**, **~67 % at 1 MB** (a
full-body copy + uppercase per request). A **header-only filter takes the zero-copy bypass** — the
body never enters guest memory: at 100 KB and 1 MB it lands **within ~3–8 % of `/baseline`**
(ADR 000038). At 1 KB it reads **−35 %** — but that gap is the per-request **WASM dispatch floor**
(the same ~4 µs the [ladder](#the-wasm-cost-ladder--isolating-each-cost) isolates) showing against
the native baseline on a tiny request, not a body cost. RSS at 1 MB × 50 VUs (fresh proxy per
route, `MALLOC_ARENA_MAX=4`): **~102 MB `/baseline` · ~181 MB `/body` · ~104 MB
`/body-headeronly`** (`data/body_rss.csv`). Two levers cut what an uncapped glibc once held (~317 MB): the arena cap
roughly halves the buffered path, and the export-presence bypass keeps a header-only route at
baseline. The buffer stays bounded (16 MiB cap, fail-closed 413) for the
filters that do read the body. The remaining buffered-path copy is the target of a future `stream<u8>`
increment (ADR 000020); a per-request time-series / allocator-sweep decomposition lives in
`bench/perf/mem_matrix.py`.

## Footprint

Idle resident set and the marginal cost of an open connection (`bench/harnesses/wasm-bench`):

| Metric | Value |
| --- | --- |
| idle RSS | ~35 MB |
| RSS holding ~1,000 idle keep-alive connections | ~57 MB |
| marginal bytes / connection | ~23 KB |

---

# 3. Realistic & protocol coverage

## Weighted request mix — with its own baseline

> M1 — open-loop 20k req/s, a weighted blend across routes on one gateway (k6): read-heavy, partly
> edge-checked (per-tenant rate-limit keys, 200 tenants, never-deny bucket), occasional writes,
> rare large payloads. Paired with a **read-only control at the same arrival rate** — 100 % plain
> reads — so the per-class deltas are attributable to the traffic *blend*, not the offered load.

| Profile | Class (share) | route | p50 | p99 | p99.9 |
| --- | --- | --- | --- | --- | --- |
| read-only (control) | read 100 % | GET `/baseline` (1 KB) | 0.21 ms | 5.70 ms | 18.7 ms |
| mix | read 60 % | GET `/baseline` (1 KB) | 0.19 ms | 6.89 ms | 17.5 ms |
| mix | auth read 25 % | GET `/ratelimit` (tenant key) | 0.28 ms | 7.11 ms | — |
| mix | write 10 % | POST `/body` (1 KB) | 0.33 ms | 7.45 ms | — |
| mix | large 5 % | POST `/body` (100 KB) | 0.96 ms | 8.31 ms | — |

Both profiles achieve the full 20k/s (dropped iterations ≤ 0.15 %, zero 429s from the never-deny
bucket). The pairing is the point: at the same rate, **the blend costs the plain reads +1.2 ms at
p99** (5.7 → 6.9 ms) — head-of-line pressure from the body classes — and the classes order exactly
as their work predicts (read < auth read < write < large, all p99 ≤ 8.3 ms, all p50s
sub-millisecond). A single-endpoint test hides all of this; the control run keeps it honest.

## HTTP/3

The fast path terminates **HTTP/3 over QUIC** (ADR 000016; `tls-http` serves h1/h2/h3 on one port). A
functional check confirms it end-to-end:

```
curl --http3-only https://…/api/hello  ->  status=200 http_version=3
```

A **rigorous, coordinated-omission-safe H3 *load* benchmark is deferred**: the load generators here
(oha, k6) have no native HTTP/3, and a correct H3 tail needs an H3-capable open-loop generator such as
**Nighthawk**. Rather than publish process-spawn-bound `curl`-loop numbers, the H3 load figure is
honestly left absent until that tooling is in place — the server support is verified, not the throughput.

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
  `plecto-loadgen` (Rust, `bench/loadgen/`) for the fault timeline, the round-robin count, and the
  footprint connection-holder. Neither oha nor k6 has native HTTP/3, so H3 *load* is deferred to an
  H3-capable generator (Nighthawk) rather than faked (see [HTTP/3](#http3)).
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
# Build the release examples first (the runbook does not build). wasm-bench/edge-bench live
# outside plecto/ (bench/harnesses/), so they need --features bench-harnesses.
cargo build --release -p plecto-server --features bench-harnesses \
  --example load-balancing --example wasm-bench --example tls-http --example edge-bench

# One phase, or `all`. Pins the proxy to a core set and generators to a disjoint set; writes
# performance/data/*.csv. Phases:
#   sweep openloop rr ejection wasm tls h3 ratelimit body churn mix footprint all
bash bench/perf/run-perf.sh all

# In-process micro-benchmarks (deterministic; the CI regression gate). Save a baseline, then compare:
cargo bench -p plecto-control -p plecto-host -- --save-baseline main   # on the base branch
cargo bench -p plecto-control -p plecto-host -- --baseline main        # on a change, to read the deltas

# Optional live dashboard (images are a one-time setup pull; the load stays on loopback):
INFLUX=1 bash bench/perf/run-perf.sh all     # http://localhost:3000/d/plecto-lb-k6

# The underlying examples (default ports overridable with PLECTO_PROXY_ADDR):
cargo run --release -p plecto-server --example load-balancing   # LB fast path
BACKEND_LATENCY_MS=0 cargo run --release -p plecto-server --features bench-harnesses --example wasm-bench   # WASM plane
cargo run --release -p plecto-server --example tls-http          # TLS termination
cargo run --release -p plecto-server --features bench-harnesses --example edge-bench        # rate-limit + body hook
```

The k6 scenarios live in `bench/k6/` and `bench/k6-wasm/`; the round-robin counter and the
open-loop fault driver are `plecto-loadgen` subcommands (`bench/loadgen/`, built lazily by the
runbook). Charts are regenerated from the measured CSVs:

```bash
python3 performance/plot.py     # reads performance/data/*.csv -> performance/img/*.webp
```

(`matplotlib` brings `numpy` + `Pillow`; Pillow supplies the WebP encoder. The benchmark *method* —
the runbook, scenarios, the Rust loadgen, plotting — is tracked, as are the curated CSVs and charts
under `performance/`; raw run artifacts under `bench/` stay untracked. See `bench/plan.md`.)

## Non-goals

- Not a sizing or capacity guide.
- Not a comparison against other proxies, gateways, or Wasm runtimes.
- Not representative of production hardware, real networks, or non-trivial upstream work.

## References

- Gil Tene, *coordinated omission* — summarized in ScyllaDB's [On Coordinated Omission](https://www.scylladb.com/2021/04/22/on-coordinated-omission/).
- [k6 executors](https://grafana.com/docs/k6/latest/using-k6/scenarios/executors/) — closed-loop (`constant-vus`) vs open-loop (`constant-arrival-rate`) models.
- [oha](https://github.com/hatoo/oha) — the single-connection-pool HTTP load generator used for the overhead, TLS and churn runs.
- [criterion.rs](https://bheisler.github.io/criterion.rs/book/) — the in-process micro-benchmark harness (LB pick, route match, WASM per-request cost) and its baseline-comparison regression gate.
- [Nighthawk](https://github.com/envoyproxy/nighthawk) — Envoy's open-loop, HTTP/1–2–3 load generator; the tool an HTTP/3 *load* benchmark would use (deferred here).
- [wrk2](https://github.com/giltene/wrk2) — constant throughput with corrected latency recording.
- [Wasmtime](https://docs.wasmtime.dev/) — the pooling allocator and epoch interruption behind pooled vs on-demand filter instances.
- [WebAssembly Component Model](https://component-model.bytecodealliance.org/) — the `plecto:filter` contract is a Component Model world.
