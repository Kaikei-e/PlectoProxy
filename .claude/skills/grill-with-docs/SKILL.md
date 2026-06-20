---
name: grill-with-docs
description: Grilling session that challenges your plan against Plecto's documented design decisions and domain model, sharpens terminology, and updates documentation (CONTEXT.md, ADRs) inline as decisions crystallise. Use when the user wants to stress-test a plan against the project's language and documented decisions, or says 「設計を詰めて」「用語を固めて」「ドキュメントと突き合わせて」.
---

<what-to-do>

Interview me relentlessly about every aspect of this plan until we reach a shared understanding. Walk down each branch of the design tree, resolving dependencies between decisions one-by-one. For each question, provide your recommended answer.

Ask the questions one at a time, waiting for feedback on each question before continuing.

If a question can be answered by exploring the codebase, explore the codebase instead.

</what-to-do>

<supporting-info>

## Domain awareness

During codebase exploration, also look for existing documentation. For Plecto the canonical
sources are the project's founding design — its Tenets, Fork decisions (1–10), and open
questions, summarised in `CLAUDE.md` — plus `CONTEXT.md` and `docs/ADR/` once they exist.

### File structure

Most repos have a single context:

```
/
├── CLAUDE.md                        ← project conventions + design summary (source of truth)
├── CONTEXT.md
├── docs/
│   └── ADR/
│       ├── template.md
│       ├── 000001.md                ← e.g. "plecto:filter ワールドを独自定義する"
│       └── 000002.md                ← e.g. "フィルタはステートレス、状態はホスト KV に置く"
└── src/
```

If a `CONTEXT-MAP.md` exists at the root, the repo has multiple contexts. The map points to where each one lives (e.g. `src/fastpath/CONTEXT.md`, `src/filters/CONTEXT.md`).

Create files lazily — only when you have something to write. If no `CONTEXT.md` exists, create one when the first term is resolved. If no `docs/ADR/` exists, create it when the first ADR is needed.

## During the session

### Challenge against the glossary

When the user uses a term that conflicts with the existing language in `CONTEXT.md`, call it out immediately. "Your glossary defines 'filter' as a `plecto:filter` component, but you seem to mean a native fast-path hook — which is it?"

### Sharpen fuzzy language

When the user uses vague or overloaded terms, propose a precise canonical term. "You're saying 'plugin' — do you mean a Filter (WASM component) or a host-native fast-path extension? Those are different things with different trust models."

### Discuss concrete scenarios

When domain relationships are being discussed, stress-test them with specific scenarios. Invent scenarios that probe edge cases and force the user to be precise about the boundaries between concepts (e.g. "a body-transform filter wants to short-circuit after reading half the stream — is that expressible in the contract?").

### Cross-reference with code and the documented design

When the user states how something works, check whether the code and the documented design decisions (see `CLAUDE.md`) agree. If you find a contradiction, surface it: "You just said filters can hold per-request state, but Fork 4 says filters are stateless and state lives in host KV — which is right?"

### Update CONTEXT.md inline

When a term is resolved, update `CONTEXT.md` right there. Don't batch these up — capture them as they happen. Use the format in [CONTEXT-FORMAT.md](./CONTEXT-FORMAT.md).

`CONTEXT.md` should be totally devoid of implementation details. Do not treat `CONTEXT.md` as a spec, a scratch pad, or a repository for implementation decisions. It is a glossary and nothing else.

### Offer ADRs sparingly

Only offer to create an ADR when all three are true:

1. **Hard to reverse** — the cost of changing your mind later is meaningful
2. **Surprising without context** — a future reader will wonder "why did they do it this way?"
3. **The result of a real trade-off** — there were genuine alternatives and you picked one for specific reasons

If any of the three is missing, skip the ADR. Use the format in [ADR-FORMAT.md](./ADR-FORMAT.md). For a full, frontmatter-complete ADR, hand off to the `plecto-adr-writer` skill.

</supporting-info>
