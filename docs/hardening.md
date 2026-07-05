# Hardening guide

Operational guidance for running Plecto beyond a single instance. The first fact to internalize:
**all host-held state in Plecto is node-local.** There is no gossip, no shared store, no
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

The standard Plecto deployment shape for a SaaS-grade rollout ([ADR 000054](ADR/000054.md)) is a
**front load balancer fanning out to N replicas**. Because the rate limiter is node-local, the
`[route.rate_limit]` you configure is a **per-replica** bucket, not a fleet-wide one. Two concrete
consequences:

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

If the front LB (or Plecto's own weighted Maglev consistent hashing, see the [README](../README.md),
[ADR 35](ADR/000035.md)) pins a given key — typically client IP — to the same replica for the
lifetime of a hash ring, then that key's requests are counted by one bucket on one node. The
node-local limiter then behaves like a *de facto* global limiter for that key, with no coordination
needed. The trade-off: hash-ring churn on scale-up/down briefly reassigns some keys to a fresh
(full) bucket, and a skewed key distribution can still overload one replica while others sit idle —
Maglev minimizes (but does not eliminate) that disruption compared to naive modulo hashing.

## When you need a real global limit

Neither pattern above gives you an exact, coordination-free global limit — they give you an
engineering approximation. If your product requires a strict fleet-wide quota (e.g. a hard
per-tenant API quota that must hold regardless of which replica or how many), that is **shared
state**, and Plecto's placement rule keeps shared state out of the native fast path
([ADR 000029](ADR/000029.md), [ADR 000053](ADR/000053.md)). The supported path is a **filter** that
consults an external store (Redis or similar) over the lent `outbound-http` capability
([ADR 000036](ADR/000036.md)), the same shape Envoy uses for its external global rate limit service.
A reference filter for this is tracked as upcoming work in the [Roadmap](ROADMAP.md#m6--polyglot-sdks--reference-filters);
it does not ship yet.

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
- [ADR 000036](ADR/000036.md) — the `outbound-http` capability a filter would use to reach an
  external store for a real global limit.
- [ADR 000029](ADR/000029.md) — the role-driven placement rule (shared/global state stays out of
  native).
