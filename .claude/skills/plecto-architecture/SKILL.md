---
name: plecto-architecture
description: >-
  Plecto's core architecture — the two halves (native-Rust fast path / WASM extension
  plane), the WIT type contract between them, the deny-by-default capability boundary, the
  filter chain, typed decision/short-circuit, init vs per-request hooks, instance
  lifecycle, and host-held state.
when_to_use: >-
  Use when implementing or reviewing fast-path code, filter execution, the host-API
  surface, or filter chains; when deciding "does this belong in Rust or in a WASM
  filter?"; or when the user mentions fast path / extension plane / filter / host-API /
  capability / 「どっちに置く」.
---

# Plecto Architecture

Plecto は二つの半身を **WIT 型契約**で編み込む（braid）L7 リバースプロキシ。判断に迷ったら、設計の
source of truth（Tenets / Fork 1–10、`CLAUDE.md` に要約）に従う。本スキルはその要点をコード作業向けに
ナビゲートする。

```
            ┌────────────────────────── fast path (native Rust) ──────────────────────────┐
client ───▶ │ accept · TLS · HTTP/1.1/2/3 · routing · LB · upstream conn mgmt · hot-reload │ ───▶ upstream
            └───────────────┬───────────────────────────────────────────────┬─────────────┘
                            │  request chain                    response chain │
                            ▼  (WIT: plecto:filter)             (reverse)       ▲
            ┌──────────── extension plane (WASM Component Model filters) ──────────────┐
            │  per-filter: init hook (heavy, once) + per-request hook (hot)            │
            │  returns decision: continue | modified | short-circuit                   │
            │  touches ONLY host-API lent by the host (deny-by-default capability)      │
            └──────────────────────────────────────────────────────────────────────────┘
                                         │ host-API (KV/counter/metrics/log/clock/random)
                                         ▼
                              host-held state: redb (KV / rate-limit / cache)
```

## The two halves — what goes where

The single most common design question is **"Rust or WASM filter?"** Decide by Fork 6:

| Put in the **fast path (native Rust)** | Put in a **filter (WASM)** |
|---|---|
| TLS termination, HTTP framing, routing, LB, upstream pools | auth, header/body rewrite, WAF, policy, custom per-request logic |
| Global / hot counters (rate-limit state, count-min) | the *decision* of whether to rate-limit / who passes |
| Zero-copy body bypass for body-untouching filters | anything that must inspect/transform a body |
| Anything that must never be untrusted or hot-reloaded | anything a user supplies or swaps without a rebuild |

Rule of thumb (Fork 6): **user-specific logic / policy / WAF / auth / rewrite → WASM; TLS / routing
/ LB / connection pool / global counters → native.** The WASM "tax" (data-copy + ~3.5x host-call
overhead) is charged only to request-decision logic, not to the speed path.

## Layers and their dependency rules

| Layer | Responsibility | May depend on |
|---|---|---|
| **Fast path** | accept/TLS/HTTP/route/LB/upstream; drives the chain | host facades, config snapshot |
| **Filter host (runtime)** | embed wasmtime, instantiate/pool filters, run hooks, enforce epoch/memory limits | wasmtime, host-API impls, fast path types |
| **Host-API** | the capabilities lent to filters (KV/counter/metrics/log/clock/random) | redb, metrics sink — **never** the filter |
| **Filter (WASM)** | per-request decision; implements `plecto:filter` | only host-API it was *granted* (sandbox-enforced) |
| **Control** | declarative manifest, hot-reload, (opt-in) openraft/foca consensus | config types, fast path |

**Direction:** the fast path drives filters through the contract; filters depend *only* on lent
capabilities; the host-API depends on storage, never on a filter. A filter cannot reach the fast
path, the host's memory, the network, or the filesystem except through a granted import — this is
enforced by the Component Model sandbox, **not by convention** (Tenet 2, Fork 7).

## The contract: `plecto:filter`

- A custom `plecto:filter` world (Fork 2). Current contract: `plecto:filter@0.3.0`, zero-WASI and
  header-only (ADR 000010); 0.1 / 0.2 are frozen with load-time adapters (ADR 000071 / 000073).
  It defines Plecto's own `decision`, init/per-request hooks, and host-API. Details and evolution
  live in the `wit-contract-design` skill.
- **decision (a WIT variant, Tenet 3):** request side `continue` · `modified` · `short-circuit`
  (stop, synthesize a response now, don't reach upstream); response side `continue` · `modified` ·
  `replace` (ADR 000073). Header values are raw bytes (`list<u8>`, ADR 000071) and `on-response`
  receives the as-forwarded request snapshot. Auth failure and rate-limit exceed are
  `short-circuit`. Never express intent with ambiguous flags.
- Bodies as `stream<u8>` (Fork 1, async-first) are projected, not current — they land with the
  wasm32-wasip2 move, and body transforms will tolerate a hot-path intermediate copy at first.

## Init vs per-request (Tenet 4)

Every filter has an **init hook** (config load, regex compile, schema build — runs once) and a
**per-request hook** (the hot path). Push heavy work to init; keep per-request lean. Mixing them is
the canonical performance bug (Envoy proxy-wasm issue #450). Request and response sides are symmetric.

## Instance lifecycle & state (Fork 3 & 4)

- **Trusted (first-party) filters:** per-worker-thread pre-instantiated (`InstancePre`) + pooling
  allocator reuse. Fast and the default.
- **Untrusted (third-party) filters:** opt into per-request new instances + pooling-zeroization
  (CVE-2022-39393 lesson). See `wasmtime-host` / `security-auditor`.
- **Filters are stateless.** No filter-local persistent state. Rate-limit / session / cache state
  lives in **host KV (redb)**, lent via the host-API. Filter-local state collides with pool reuse
  and hot-reload, and risks state leakage.

## Single-node first (Fork 5 & 10)

One node completes the job. Distribution is opt-in: `foca` (SWIM) for membership / filter+config
distribution, `openraft` (Raft) for strong-consistency config/route replication. State stays
node-local (redb); distribution is limited to *config consensus*. No xDS-style dynamic push —
static declarative manifest + hot-reload is first (Fork 10).

## File / module patterns (as `src/` grows)

These are the intended seams — match new code to them (and to `CONTEXT.md` once it exists):

- `**/fastpath/**` or `**/proxy/**` — listener, TLS, HTTP, router, LB, upstream
- `**/host/**` or `**/runtime/**` — wasmtime embedding, instance pool, hook dispatch, metering
- `**/hostapi/**` or `**/capabilities/**` — KV/counter/metrics/log/clock host functions (deny-by-default)
- `**/filter/**` — filter chain orchestration + (separately) example filters
- `**/control/**` — manifest, hot-reload, consensus (foca/openraft)
- `**/wit/**` or `wit/` — the `plecto:filter` world definitions

## Common violations (call these out in review)

- **Business logic in the fast path** that should be a filter (or vice-versa: a hot global counter
  done in WASM instead of native — Fork 6).
- **A filter granted more than it needs** — host-API must be deny-by-default; lend only the minimum.
- **Filter-local mutable state** across requests (breaks Fork 4; state belongs in host KV).
- **Heavy work in the per-request hook** that belongs in init (Tenet 4).
- **The host depending on a specific filter**, or a filter reaching past its lent capabilities.
- **Panicking on untrusted input in the fast path** (a single bad request must not down the worker).
- **A contract change without a conformance test** (see `tdd-workflow` Phase 1).
- **Treating a read-recomputable thing as a source of truth** — read models / projections (if any)
  stay disposable; the manifest + content hashes are the authority for what's loaded (Fork 8).

## Related skills

- `wit-contract-design` — designing/evolving the `plecto:filter` world and host-API surface.
- `wasmtime-host` — the host-side embedding (InstancePre, pooling, epoch, Linker deny-by-default).
- `design-an-interface` — explore radically different shapes for a contract or host-API.
- `security-auditor` — capability/sandbox + proxy/gateway threat review.
