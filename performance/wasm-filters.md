# Plecto Performance — the WASM filter plane

Plecto runs each request's *decision* — auth, rewriting, rate limiting, policy — as a sandboxed
**WebAssembly Component Model filter**, not as native proxy code. The obvious question is what
that costs. This report measures it on one server, one backend, changing only **how the decision
runs**. As with the [throughput report](README.md), the goal is an honest method, not a leaderboard.

## TL;DR

- A **pooled** WASM filter (init-once, instances reused) costs about **8% throughput** and **~1 ms
  at p99** versus a no-filter native baseline — the extension plane is cheap on the hot path.
- **Instance pooling is decisive.** The same filter run **fresh-per-request** (on-demand) managed
  ~3.1k req/s versus ~14.2k for the pooled path — roughly a **4.6× difference**, because every
  request re-pays initialization. This is the whole point of separating `init` from per-request work.
- A rejected request (missing/invalid key → **HTTP 401 short-circuit**) completes in **well under a
  millisecond and never touches the backend** — the edge sheds bad traffic ~30–50× faster than it
  forwards a good one.

## Scope and honesty notes

- **Machine specifications are intentionally omitted.** Load generator, Plecto, the upstream and any
  tooling shared a single commodity machine over loopback. Absolute numbers are a **relative /
  regression** signal, not a capacity claim.
- The only variable across the three routes is the decision path; everything else (backend, payload,
  client) is identical, so the differences are attributable to the filter mechanism.
- The backend sleeps a configurable amount (`BACKEND_LATENCY_MS`) to model a real service. Phase A
  uses **0 ms** to isolate raw filter cost; Phase B uses **15 ms** to show real-world proportions.
- For the cost measurement the filter's per-request deadline was set generously so neither isolation
  mode tripped a deadline (a 504) — this measures cost, not the fail-closed SLA.
- No comparison to other gateways or runtimes is made or implied.

## What is under test

The bundled `examples/wasm-bench` serves three routes that forward to the **same** backend:

| Route | Decision path |
| --- | --- |
| `/baseline/*` | no filter — native fast path only |
| `/trusted/*` | signed `filter-apikey` component, **pooled** (init-once, instances reused) |
| `/ondemand/*` | the **same** component, **fresh instance per request** |

`filter-apikey` is a real `plecto:filter` component: it reads `x-api-key`, and on a valid key
(`alice-secret`/`bob-secret`) stamps `x-authenticated-user` and forwards; on a missing/invalid key it
returns a typed `short-circuit` 401 and the upstream is never reached. It is cosign-signed and loaded
through the production verify-then-load path (fail-closed).

## Scenarios

- **W1 — overhead & pooling.** A fixed concurrency (50 VUs) for 30 s with a valid key against each
  route, with a **0 ms** backend so the filter cost isn't hidden by upstream time.
- **W2 — realistic mixed traffic.** A fixed **2000 req/s** arrival rate for 40 s against the pooled
  route over a **15 ms** backend, with a **~90% valid / ~10% invalid-or-missing** key mix — modelling
  a gateway that mostly serves authenticated callers but constantly fields expired tokens, scanners,
  and misconfigured clients.

## Results

### Per-request overhead and the value of pooling

![Throughput by decision path](img/wasm_throughput.webp)

![Per-request latency by decision path](img/wasm_latency.webp)

> Fixed 50 VUs, 0 ms backend. Single host, loopback — relative baseline.

| Route | Throughput | p50 | p95 | p99 |
| --- | --- | --- | --- | --- |
| baseline (no filter) | 15,452 req/s | 2.96 ms | 5.63 ms | 7.42 ms |
| pooled WASM filter | 14,238 req/s | 3.16 ms | 6.36 ms | 8.54 ms |
| on-demand WASM filter | 3,068 req/s | 16.05 ms | 29.95 ms | 35.03 ms |

The **pooled** filter — Plecto's default for trusted components — tracks the native baseline closely:
about **8% less throughput** and roughly **+0.2 ms median / +1.1 ms p99**. Running the *same* filter
**on-demand** (a fresh instance, re-initialized for every request) collapses to ~3.1k req/s with a
~16 ms median: about **4.6× less throughput** and **5× the latency**. The gap is the cost of
initialization, paid once and amortized when instances are pooled, and paid on every request when
they are not.

### Short-circuit: rejecting bad traffic at the edge

![Accept vs reject latency](img/wasm_shortcircuit.webp)

> Fixed 2000 req/s, 15 ms backend, ~90% valid / ~10% bad keys. 71,990 accepted, 8,010 rejected.

| Path | p50 | p95 | p99 |
| --- | --- | --- | --- |
| accept (200, forwarded to backend) | 16.50 ms | 17.36 ms | 17.80 ms |
| reject (401, short-circuited) | 0.31 ms | 0.50 ms | 0.76 ms |

Accepted requests cost the 15 ms backend plus roughly **1.5–2.8 ms** for the filter and proxy —
i.e. the pooled filter is a small fraction of a realistic request. Rejected requests are decided
**at the edge in well under a millisecond** and never reach the upstream, so bad traffic is both
cheap to refuse and harmless to the backend it would otherwise hit.

## Why pooling matters

Plecto's design splits a filter's lifecycle into an expensive, run-once `init` (compile regexes, seed
state into host KV, …) and a lightweight per-request `on-request`. Trusted filters keep warm
instances in a pool, so the hot path only re-pays `on-request`; an untrusted/on-demand filter rebuilds
its instance every request for stronger isolation, re-paying `init` each time. W1 puts a number on
that trade-off. (Filter faults or deadline overruns fail closed — 502/504 — rather than failing open;
that path is exercised by the test suite rather than this benchmark.)

## References

- [Wasmtime documentation](https://docs.wasmtime.dev/) — embedding, the pooling allocator, and epoch-based interruption used for per-request metering.
- [WebAssembly Component Model](https://component-model.bytecodealliance.org/) — the `plecto:filter` contract is a Component Model world.
- Coordinated omission and open-loop measurement, as summarized in ScyllaDB's [On Coordinated Omission](https://www.scylladb.com/2021/04/22/on-coordinated-omission/) — Phase B uses a fixed arrival rate for the same reason.
- [k6 executors](https://grafana.com/docs/k6/latest/using-k6/scenarios/executors/) — `constant-vus` (W1) and `constant-arrival-rate` (W2).

## Reproducing

```bash
# Start the harness (3 routes, one backend; BACKEND_LATENCY_MS models upstream time):
BACKEND_LATENCY_MS=0  cargo run --release -p plecto-server --example wasm-bench   # :8085

# W1 — drive one route at fixed concurrency with a valid key (repeat per route):
k6 run -e ROUTE_PATH=/trusted -e VUS=50 -e DUR=30s -e OUT=trusted.json bench/k6-wasm/route.js

# W2 — realistic 90/10 mix over a 15 ms backend (start the harness with BACKEND_LATENCY_MS=15):
k6 run -e RATE=2000 -e DUR=40s -e OUT=mixed.json bench/k6-wasm/mixed.js
```

Charts are regenerated from the measured CSVs under [`data/`](data/) with `python3 performance/plot.py`.

## Non-goals

- Not a capacity or sizing guide.
- Not a comparison against other gateways, proxies, or Wasm runtimes.
- Single host, trivial backend, HTTP/1.1 — representative of *relative* cost, not production absolutes.
