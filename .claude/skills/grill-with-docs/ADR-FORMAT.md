# ADR Format

Plecto records architecture decisions under `docs/ADR/` as zero-padded six-digit files
(`000001.md`, `000002.md`, …). This is the **single** ADR convention for the repo — the same
one the `plecto-adr-writer` skill uses. When a grilling session crystallises a load-bearing
decision, either hand off to `plecto-adr-writer` for the full record, or create the file here
lazily using the shared template at `docs/ADR/template.md`.

Create `docs/ADR/` (and `docs/ADR/template.md`) lazily — only when the first ADR is needed.

## Minimal body

An ADR can be short. The value is in recording *that* a decision was made and *why* — not in
filling out sections. At minimum:

```md
---
title: {動詞始まりの行動指向の一文}
date: {YYYY-MM-DD}
status: accepted
tags: [{許可タグから最大5}]
aliases: ["ADR-NNN", "ADR-000NNN"]
---

# {Short title of the decision}

## Context
{なぜこの決定が必要だったか。1–3 文でよい。}

## Decision
{採用した選択肢と、検討した代替案・却下理由。}

## Consequences
{Pros / Cons・トレードオフ。}
```

For Plecto-shaped decisions, the **Fork form** (判断 / 根拠 /
再検討条件) maps cleanly onto Decision / Context / Consequences — keep that structure when it fits.

## Numbering

Scan `docs/ADR/` for the highest existing six-digit number and increment by one
(`ls docs/ADR/ | grep -E '^[0-9]{6}\.md$' | sort | tail -1`). First ADR is `000001`.

## When to offer an ADR

All three of these must be true:

1. **Hard to reverse** — the cost of changing your mind later is meaningful
2. **Surprising without context** — a future reader will look at the code and wonder "why on earth did they do it this way?"
3. **The result of a real trade-off** — there were genuine alternatives and you picked one for specific reasons

If a decision is easy to reverse, skip it — you'll just reverse it. If it's not surprising, nobody will wonder why. If there was no real alternative, there's nothing to record beyond "we did the obvious thing."

### What qualifies (Plecto examples)

- **Architectural shape.** "Filters are stateless; state lives in host KV (redb)." "The filter contract is a custom `plecto:filter` world reusing `wasi:http` types, not raw `wasi:http/middleware`."
- **Instance lifecycle / security model.** "Per-worker-thread pooled instances for trusted filters; per-request new instances + pooling zeroization for untrusted ones."
- **Integration patterns between the two halves.** "The fast path bypasses the WASM tax for body-untouching filters via zero-copy." "Rate limiting runs host-native, not in WASM."
- **Technology choices that carry lock-in.** wasmtime, redb, quinn, openraft, foca — the load-bearing ones, not every crate.
- **Boundary and scope decisions.** "Host-API is deny-by-default; a filter gets KV/counter/metrics/log/clock only." The explicit no-s are as valuable as the yes-s.
- **Deliberate deviations from the obvious path.** "We target `wasm32-wasip2` and compile against P3 WIT instead of waiting for `wasm32-wasip3`." Anything where a reasonable reader would assume the opposite.
- **Constraints not visible in the code.** "stream splicing is deferred to WASI 0.3.x, so body transforms tolerate a hot-path intermediate copy for now."
- **Rejected alternatives when the rejection is non-obvious.** If you considered xDS-style dynamic push and picked a static manifest + reload, record it — otherwise someone will suggest xDS again in six months.
