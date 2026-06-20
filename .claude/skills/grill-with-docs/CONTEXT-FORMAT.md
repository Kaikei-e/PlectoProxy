# CONTEXT.md Format

## Structure

```md
# {Context Name}

{One or two sentence description of what this context is and why it exists.}

## Language

**Filter**:
A WASM component implementing the `plecto:filter` world; receives a request/response and returns a decision.
_Avoid_: plugin, middleware, extension (when you specifically mean a filter)

**Decision**:
A filter's typed return value — `continue`, `modified`, or `short-circuit`.
_Avoid_: result, verdict, response (those mean other things here)

**Fast path**:
The native-Rust half that handles connection, TLS, HTTP, routing, LB, upstream.
_Avoid_: core, engine (too vague), data plane (overloaded)
```

## Rules

- **Be opinionated.** When multiple words exist for the same concept, pick the best one and list the others under `_Avoid_`.
- **Keep definitions tight.** One or two sentences max. Define what it IS, not what it does.
- **Only include terms specific to this project's context.** General programming concepts (timeouts, error types, utility patterns) don't belong even if the project uses them extensively. Before adding a term, ask: is this a concept unique to this context, or a general programming concept? Only the former belongs.
- **Group terms under subheadings** when natural clusters emerge. If all terms belong to a single cohesive area, a flat list is fine.

## Single vs multi-context repos

**Single context (most repos, and Plecto today):** One `CONTEXT.md` at the repo root.

**Multiple contexts:** A `CONTEXT-MAP.md` at the repo root lists the contexts, where they live, and how they relate to each other:

```md
# Context Map

## Contexts

- [Fast path](./src/fastpath/CONTEXT.md) — connection, TLS, HTTP, routing, LB, upstream
- [Extension plane](./src/filters/CONTEXT.md) — WASM filter execution and the host-API surface
- [Control](./src/control/CONTEXT.md) — declarative manifest, hot reload, config consensus

## Relationships

- **Fast path → Extension plane**: the fast path drives each request through the filter chain via the `plecto:filter` contract
- **Control → Fast path / Extension plane**: the manifest pins routes, chains, and filter OCI digests; reload swaps instances atomically
```

The skill infers which structure applies:

- If `CONTEXT-MAP.md` exists, read it to find contexts
- If only a root `CONTEXT.md` exists, single context
- If neither exists, create a root `CONTEXT.md` lazily when the first term is resolved

When multiple contexts exist, infer which one the current topic relates to. If unclear, ask.
