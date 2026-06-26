# Plecto Performance

An honest performance snapshot of Plecto's two halves: the **native load-balancing fast
path** and the **WASM extension plane** (per-request filters). The goal is **transparency
about method**, not a leaderboard. Every number here is an internal **regression baseline** —
not a capacity guide, and not a comparison against other proxies.

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
  concurrency sweep (`constant-vus`), the open-loop tail (`constant-arrival-rate`) and the mixed
  short-circuit run; a small Python open-loop driver runs the fault-injection timeline; and
  [oha](https://github.com/hatoo/oha) drives the single-route overhead (WASM W1) and TLS runs.
  Different generators have different ceilings — **numbers are comparable within a section, and
  across same-generator sections, but not blindly across all of them** (a lighter generator
  reveals a higher proxy ceiling). Each section names its generator.
- **PMU not collected.** The runbook's optional micro-architectural attribution (cycles/req, IPC,
  LLC / branch misses via `perf`) needs a lowered `kernel.perf_event_paranoid` (privileged); it
  was not enabled on this run, so the WASM tax is reported as throughput / latency / **µs-per-req**,
  not a cycles breakdown.

## TL;DR

**Load-balancing fast path** (plaintext HTTP/1.1, 3 upstreams, trivial 0 ms backend; k6):

- Closed-loop throughput peaks at **~109k req/s** (50–100 VUs) with **p99 ≈ 1.7–3.6 ms** and zero
  failures; it holds **~104k at 200 VUs** (p99 6.3 ms) and degrades **gracefully** — still
  **~85k at 800 VUs** (p99 21 ms) with **0 failures and no latency cliff**.
- An open-loop arrival rate of **40k req/s** sustains with **0 failures** and 0.22 % dropped: bulk
  latency **p50 0.17 ms**, with the tail at **p99 110 ms / p99.9 155 ms** from open-loop queueing
  on the co-resident host. Pushed to ~76k/s the tail diverges (p99 275 ms, ~15 % dropped) — that
  divergence *is* the saturation signal, and is why the open-loop tail, not the closed-loop p99,
  is treated as authoritative.
- Round-robin across three upstreams is **even to within one request** (33.3 % each).
- **Resilience is as designed**: ejecting one upstream drops its share to zero in ~1 s and the
  survivors absorb the load with **no client-visible errors**; a *total* outage **fails closed
  with HTTP 503** (no hangs, the 503/s line tracks the full offered rate) and the pool **recovers
  within ~1 s** of health returning.
- TLS termination (ALPN **h2**) costs about **28 % throughput and +0.2 ms p99** here — a realistic
  termination cost, not the inflated figure a multiplexing-bound client would report (see
  [TLS](#tls-termination)).

**WASM extension plane** (the cost of running a decision as a sandboxed component; oha / k6):

- A **pooled** filter (init-once, instances reused) adds **~1.5 µs of CPU per request** on the hot
  path and **+0.16 ms p99** versus a no-filter native baseline. As a *fraction* that is **~21 % of
  throughput at this run's ~168k req/s** — but the **~1.5 µs/req is the portable figure**; the
  percentage shrinks as the rest of the request gets heavier (a slower generator reads the same
  cost as single-digit %).
- The **same** filter run **fresh-per-request** falls to ~4.8k req/s vs ~133k pooled — a **~35×
  difference**, the cost of re-paying `init` on every request. Separating `init` from per-request
  work is what makes the plane cheap.
- A rejected request (**HTTP 401 short-circuit**) is decided in **~0.3 ms and never reaches the
  backend** — bad traffic is shed **~55× faster** than good traffic is forwarded through a 15 ms
  backend.

## Scope & honesty notes

- **Machine specs intentionally omitted.** Single commodity host, loopback, everything
  co-resident. Absolute throughput is contended and clock-variable; treat figures as relative /
  regression signals.
- **Generator-bound where noted.** The closed-loop sweep tops out near the *generator's* ceiling
  on its cores, not the proxy's: the same fast path serves a single route at **~168k req/s** under
  the lighter oha (see WASM baseline / TLS plain), well above the k6 sweep's ~109k. The sweep
  curve's *shape* is the signal, not its absolute peak.
- **Trivial upstreams** (tiny static responses, 0 ms latency by default) deliberately isolate
  **proxy + LB + filter overhead** rather than backend work. A 15 ms synthetic backend is used
  where realistic proportions matter (WASM short-circuit).
- The LB figures are **plaintext HTTP/1.1**, except the dedicated [TLS run](#tls-termination)
  which exercises rustls termination + ALPN.
- **No comparative claims.** Mature proxies are cited only for shared methodology, never ranking.
- Charts rendered with matplotlib → WebP; an optional InfluxDB + Grafana stack provides live
  dashboards during k6 runs.

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
| 50  | **108,941** | 0.32 ms | 1.02 ms | 1.70 ms | 4.29 ms | 0% |
| 100 | 108,316 | 0.60 ms | 2.20 ms | 3.60 ms | 7.96 ms | 0% |
| 200 | 103,555 | 0.89 ms | 3.69 ms | 6.29 ms | 12.54 ms | 0% |
| 400 | 96,188 | 1.70 ms | 6.07 ms | 10.31 ms | 19.40 ms | 0% |
| 800 | 84,788 | 4.77 ms | 10.78 ms | 20.63 ms | 28.58 ms | 0% |

Throughput plateaus near **50–100 VUs** (the k6 generator's ceiling on its cores) and declines
**gracefully** as concurrency climbs — latency rises in proportion with **no failures and no cliff
even at 800 VUs**. The useful reading is the shape: a flat-then-declining ceiling with an orderly
latency climb, the pinned proxy never collapsing under the generator.

## Tail latency under open-loop load

Open-loop sends at a **constant arrival rate** regardless of how fast responses come back, so
queueing surfaces in the tail instead of being hidden — the *coordinated-omission-safe* model.

| Model | target | achieved | p50 | p95 | p99 | p99.9 | dropped | failed |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| open-loop, 0 ms backend | 40,000/s | 39,907/s | 0.17 ms | 64.2 ms | 110.5 ms | 155.2 ms | 0.22% | 0% |

At 40k/s the host sustains the rate with **zero failures**; the bulk is **sub-0.2 ms** while the
tail (p99 110 ms) reflects open-loop queueing and co-resident scheduling on a single box. Pushed
toward the generator's ceiling (~76k/s) the tail diverges sharply (achieved 65k/s, **~15 %
dropped**, p99 ≈ 275 ms) — that divergence is the saturation signal, and is why we treat the
open-loop tail, not the optimistic closed-loop p99, as authoritative.

## Round-robin distribution

![Round-robin distribution](img/rr_distribution.webp)

Over a steady window with all three upstreams healthy, **120,000** requests split **40,000 /
40,000 / 40,000** — even to a single request (33.3 % each). Round-robin holds under load.

## Resilience: ejection & fail-closed

A steady open-loop rate (~4k req/s) while a controller drives a fault timeline (`eject b` →
`rejoin b` → `eject all` → `restore all`) and the driver buckets each upstream's served-count and
the 503/s every second:

![Load balancing under fault injection](img/ejection_timeline.webp)

- **Even baseline.** ~4k req/s split three ways while healthy.
- **Graceful ejection.** When **b** is driven unhealthy its share falls to zero within ~1 s and the
  survivors (a + c) absorb the full load **with zero failed requests**. The survivors' split is
  *not* even, though: the ejected instance's round-robin slot is taken by its neighbour (here ~1:2
  a:c), so traffic shifts but isn't re-balanced across the survivors — worth noting against the
  "round-robin over the healthy set" description (the all-healthy split *is* exactly even).
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
| plain (h1)               | 164,859 | 0.28 ms | 0.65 ms | baseline |
| TLS h1, keep-alive       | 132,194 | 0.35 ms | 0.77 ms | record-layer AES-GCM = Δ vs plain |
| TLS h1, handshake/req    | 17,354  | 1.66 ms | 8.52 ms | full handshake (ECDHE + signature) per request |
| TLS (h2)                 | 118,621 | 0.39 ms | 0.88 ms | h2 multiplexing over TLS |

The decomposition is the point. **Record-layer crypto is cheap** — amortised over a kept-alive
connection, TLS h1 costs ~20 % throughput and only **+0.12 ms p99** vs plaintext, because AES-GCM
runs on AES-NI hardware. **The handshake dominates** — forcing a fresh ECDHE handshake on *every*
request collapses throughput to ~17k/s (~8× lower) and adds ~1.3 ms median, which is where TLS
cost actually lives. And **h2 is clean** (118k/s, p99 0.88 ms): ALPN-negotiated HTTP/2 over TLS
costs ~28 % throughput and +0.23 ms p99 vs plaintext — a realistic termination cost. A client that
funnels many VUs over a handful of multiplexed connections can make h2 *look* far worse (head-of-line
queueing, not server work); measuring with a connection-per-concurrency client removes that artifact.

---

# 2. WASM extension plane

Plecto runs each request's *decision* — auth, rewriting, rate limiting, policy — as a sandboxed
**WebAssembly Component Model filter**, not native proxy code. This measures what that costs,
changing only **how the decision runs**. The bundled `examples/wasm-bench` serves three routes
that forward to the **same** backend:

| Route | Decision path |
| --- | --- |
| `/baseline/*` | no filter — native fast path only |
| `/trusted/*` | signed `filter-apikey` component, **pooled** (init-once, instances reused) |
| `/ondemand/*` | the **same** component, **fresh instance per request** |

`filter-apikey` is a real `plecto:filter` component: it reads `x-api-key`, stamps
`x-authenticated-user` on a valid key and forwards, or returns a typed `short-circuit` **401** on a
missing/invalid key (the upstream is never reached). It is cosign-signed and loaded through the
production verify-then-load path (fail-closed).

## Overhead & the value of pooling

![Throughput by decision path](img/wasm_throughput.webp)
![Per-request latency by decision path](img/wasm_latency.webp)

> W1 — fixed 50 connections, 0 ms backend, valid key (oha). Isolates filter cost from upstream time.

| Route | req/s | p50 | p95 | p99 | CPU/req |
| --- | --- | --- | --- | --- | --- |
| baseline (no filter) | 167,827 | 0.28 ms | 0.44 ms | 0.59 ms | 5.96 µs |
| pooled WASM filter | 132,945 | 0.35 ms | 0.56 ms | 0.75 ms | 7.52 µs |
| on-demand WASM filter | 4,760 | 7.22 ms | 23.51 ms | 27.59 ms | ~210 µs |

The **pooled** filter — Plecto's default for trusted components — adds **~1.56 µs of CPU per
request** (5.96 → 7.52 µs/req) and **+0.16 ms p99** over the native baseline. At this run's ~168k
req/s that ~1.5 µs is **~21 % of throughput**; at a heavier per-request cost it is a far smaller
fraction (a k6-bound ~15k/s run reads the same overhead as single-digit %). **The µs/req is the
invariant to track for regressions, not the percentage.** The *same* filter run **on-demand**
(fresh, re-initialised every request) collapses to ~4.8k req/s — **~35× less throughput** — because
it re-pays `init` (~210 µs) on every request. That gap *is* the value of pooling.

## Short-circuit: rejecting bad traffic at the edge

![Accept vs reject latency](img/wasm_shortcircuit.webp)

> W2 — fixed 2000 req/s, 15 ms backend, ~90 % valid / ~10 % bad keys (k6). 108,159 accepted, 11,840 rejected.

| Path | p50 | p95 | p99 |
| --- | --- | --- | --- |
| accept (200, forwarded) | 16.32 ms | 17.14 ms | 17.53 ms |
| reject (401, short-circuited) | 0.29 ms | 0.48 ms | 0.79 ms |

Accepted requests cost the 15 ms backend plus the small pooled-filter + proxy overhead. Rejected
requests are decided **at the edge in ~0.3 ms** and never reach the upstream: bad traffic is shed
**~55× faster** than good traffic is forwarded, and is harmless to the backend it would otherwise
hit.

**Why pooling matters.** A filter's lifecycle splits into an expensive, run-once `init` (compile
regexes, seed host KV, …) and a lightweight per-request `on-request`. Trusted filters keep warm
instances in a pool so the hot path re-pays only `on-request`; untrusted/on-demand filters rebuild
per request for stronger isolation, re-paying `init` each time. (Filter faults or deadline
overruns **fail closed** — 502/504 — exercised by the test suite, not this benchmark.)

## Footprint

Idle resident set and the marginal cost of an open connection (`examples/wasm-bench`):

| Metric | Value |
| --- | --- |
| idle RSS | ~35 MB |
| RSS holding ~1,000 idle keep-alive connections | ~57 MB |
| marginal bytes / connection | ~21 KB |

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
- **Track the invariant, not the headline.** The WASM tax is ~µs/req (not a %), resilience is
  ~time-constants (not a host's req/s), and round-robin is exact — these hold across hosts and
  generators, so a change in them is a real regression. A change in absolute peak throughput is
  usually just the host or the generator.
- **Prior art.** Disclosing *how* a number was produced — open- vs closed-loop, corrected latency —
  is standard in tools such as `wrk2` and k6. This report follows that spirit using only its own
  measurements.

## Reproducing

The tracked, in-repo subjects and the runbook that produces every CSV here:

```bash
# One phase, or `all`. Pins the proxy to a core set and generators to a disjoint set; writes
# performance/data/*.csv. Phases: sweep openloop rr ejection wasm tls footprint all.
bash bench/perf/run-perf.sh all

# The underlying examples (default ports overridable with PLECTO_PROXY_ADDR):
cargo run --release -p plecto-server --example load-balancing   # LB fast path
BACKEND_LATENCY_MS=0 cargo run --release -p plecto-server --example wasm-bench   # WASM plane
cargo run --release -p plecto-server --example tls-http          # TLS termination
```

The k6 scenarios live in `bench/k6/` and `bench/k6-wasm/`; the round-robin counter and the
open-loop fault driver in `bench/perf/`. Charts are regenerated from the measured CSVs:

```bash
python3 performance/plot.py     # reads performance/data/*.csv -> performance/img/*.webp
```

(`matplotlib` brings `numpy` + `Pillow`; Pillow supplies the WebP encoder. The measured CSVs and
the local heavy-load harness are git-untracked working data, like `bench/`.)

## Non-goals

- Not a sizing or capacity guide.
- Not a comparison against other proxies, gateways, or Wasm runtimes.
- Not representative of production hardware, real networks, or non-trivial upstream work.

## References

- Gil Tene, *coordinated omission* — summarized in ScyllaDB's [On Coordinated Omission](https://www.scylladb.com/2021/04/22/on-coordinated-omission/).
- [k6 executors](https://grafana.com/docs/k6/latest/using-k6/scenarios/executors/) — closed-loop (`constant-vus`) vs open-loop (`constant-arrival-rate`) models.
- [oha](https://github.com/hatoo/oha) — the single-connection-pool HTTP load generator used for the overhead and TLS runs.
- [wrk2](https://github.com/giltene/wrk2) — constant throughput with corrected latency recording.
- [Wasmtime](https://docs.wasmtime.dev/) — the pooling allocator and epoch interruption behind pooled vs on-demand filter instances.
- [WebAssembly Component Model](https://component-model.bytecodealliance.org/) — the `plecto:filter` contract is a Component Model world.
