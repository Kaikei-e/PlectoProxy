# Hardening guide

Operational guidance for running Plecto Proxy beyond a single instance. The first fact to internalize:
**all host-held state in Plecto Proxy is node-local.** There is no gossip, no shared store, no
cross-instance consensus in the native fast path — each `plecto` process only knows about the
requests it personally handled. This is a deliberate design boundary ([ADR 000053](ADR/000053.md)),
not a gap: it keeps the core single-binary and self-hostable ([ADR 000008](ADR/000008.md)) instead
of quietly depending on an external coordination service.

## What "node-local" covers

| State | Where | ADR |
| --- | --- | --- |
| Native L7 rate limiter (per-route / per-client-IP token bucket) | `plecto-server` fast path | [33](ADR/000033.md) |
| `host-ratelimit` / `host-kv` / `host-counter` (per-filter capabilities) | `plecto-host` | [26](ADR/000026.md) |
| redb state backend | `plecto-host` (single-process by design) | [41](ADR/000041.md) |
| TLS 1.3 session ticket keys | `plecto-server` | [52](ADR/000052.md) |

None of these are shared across replicas. A counter, bucket, or cached ticket key on instance A is
invisible to instance B.

## Multi-replica rate limiting

**Recommended: run both layers together** ([ADR 000061](ADR/000061.md)). The native token bucket
below is a **local floor** — an immediate, external-call-free flood shed in front of every replica,
so a burst never spends WASM CPU or reaches a shared backend. On top of it,
[`filter-ratelimit-redis`](../plecto/examples/filters/filter-ratelimit-redis) is the **global
layer**: it consults a RESP-compatible store (Redis, Valkey, ...) over the lent `outbound-tcp`
capability ([ADR 000060](ADR/000060.md)) to hold the actual fleet-wide cap. This mirrors the
combined local + global pattern Envoy Gateway documents for the same problem — local absorbs
bursts before they reach the shared limiter; global holds the real number. Configure the
per-replica floor everywhere (cheap, always on) and add the filter on any route that needs an
exact fleet-wide quota rather than the engineering approximations below.

The standard Plecto Proxy deployment shape for a SaaS-grade rollout ([ADR 000054](ADR/000054.md)) is a
**front load balancer fanning out to N replicas**. Because the rate limiter is node-local, the
`[route.rate_limit]` you configure is a **per-replica** bucket, not a fleet-wide one. Two concrete
consequences if you rely on the local floor alone:

**1. Even load balancing (round-robin, least-request) — effective rate scales with N.**

If the front LB spreads requests roughly evenly across replicas, the fleet's effective allowed rate
for a given route is approximately:

```
effective_rate ≈ configured_rate × N
```

To hold a fleet-wide target rate `R_target` regardless of replica count, configure each replica with:

```
configured_rate = R_target / N
```

...and re-derive it whenever you scale `N` up or down. The same multiplier applies to `burst`.
Per-client-IP buckets are affected the same way *only* if a given client's requests actually land on
different replicas — see the next pattern for when they don't.

**2. Key-consistent routing (consistent hashing / Maglev) — node-local approximates global.**

If the front LB (or Plecto Proxy's own weighted Maglev consistent hashing, see the [README](../README.md),
[ADR 35](ADR/000035.md)) pins a given key — typically client IP — to the same replica for the
lifetime of a hash ring, then that key's requests are counted by one bucket on one node. The
node-local limiter then behaves like a *de facto* global limiter for that key, with no coordination
needed. The trade-off: hash-ring churn on scale-up/down briefly reassigns some keys to a fresh
(full) bucket, and a skewed key distribution can still overload one replica while others sit idle —
Maglev minimizes (but does not eliminate) that disruption compared to naive modulo hashing.

## When you need a real global limit

Neither approximation pattern above gives you an exact, coordination-free global limit — they
give you an engineering approximation. If your product requires a strict fleet-wide quota (e.g. a
hard per-tenant API quota that must hold regardless of which replica or how many), that is
**shared state**, and Plecto Proxy's placement rule keeps shared state out of the native fast path
([ADR 000029](ADR/000029.md), [ADR 000053](ADR/000053.md)). The supported path is a **filter** that
consults an external store over a lent outbound capability, the same shape Envoy uses for its
external global rate limit service — and Plecto Proxy's version of that service IS the filter itself
(no separate process, ADR 000061's single-binary win).

[`filter-ratelimit-redis`](../plecto/examples/filters/filter-ratelimit-redis) is the reference
implementation ([ADR 000061](ADR/000061.md)): a general, textbook fixed-window counter
(`INCRBY` + an unconditional `EXPIRE ... NX`, Redis >= 7.0 / Valkey) over the `outbound-tcp`
capability ([ADR 000060](ADR/000060.md)). Its manifest `[filter.config]` (via the `host-config`
capability, [ADR 000066](ADR/000066.md)) declares the backend host/port, window, limit, cost
source, and a **required** `on_backend_error = "deny" | "allow"` — there is no default, so an
operator must decide explicitly whether a Redis outage fails the route closed or falls back to
the local floor alone. The filter needs `isolation = "trusted"`: a pooled instance holds one
persistent backend connection across requests instead of reconnecting every time, and the same
eager load-time instantiation turns a missing/invalid required config value into a load failure
rather than a per-request 503 (see the filter's own doc comments and `docs/writing-a-filter.md`).

Deploying it alongside the local floor is the two-layer model this guide recommends above: the
local bucket sheds bursts before they cost a Redis round trip, and the filter enforces the actual
fleet-wide number on what gets through. A quantified local-vs-combined comparison across a real
N-replica fleet is tracked as follow-up measurement work
([ADR 000061](ADR/000061.md) Consequences, [ADR 000056](ADR/000056.md) R6) — this guide will link
the numbers here once that harness exists; until then, treat the combined shape as the
recommended architecture, not yet as a benchmarked claim.

## Fairness and enforcement claims are node-local

Any benchmark or README claim about rate-limit **fairness** (a noisy key cannot starve another) or
**enforcement** (allowed throughput converges to the configured rate) describes the behavior of a
**single node**. It says nothing about the aggregate behavior of a multi-replica fleet — apply the
formula above to reason about the fleet. See [performance/README.md](../performance/README.md#host-enforced-rate-limiting)
for the underlying single-node measurements.

## Related ADRs

- [ADR 000053](ADR/000053.md) — declares all host state node-local; this guide is the operational
  half of that decision.
- [ADR 000033](ADR/000033.md), [ADR 000026](ADR/000026.md), [ADR 000041](ADR/000041.md),
  [ADR 000052](ADR/000052.md) — the node-local state this guide covers.
- [ADR 000061](ADR/000061.md) — the local floor × global filter two-tier rate-limit model and the
  `filter-ratelimit-redis` reference filter this guide recommends.
- [ADR 000060](ADR/000060.md) — the `outbound-tcp` capability the reference filter uses to reach a
  RESP-compatible store.
- [ADR 000066](ADR/000066.md) — the `host-config` capability the reference filter reads its
  business config (backend, window, limit, `on_backend_error`, ...) from.
- [ADR 000029](ADR/000029.md) — the role-driven placement rule (shared/global state stays out of
  native).
