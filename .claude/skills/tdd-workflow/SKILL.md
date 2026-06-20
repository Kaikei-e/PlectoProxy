---
name: tdd-workflow
description: Test-Driven Development workflow for Plecto (Rust-first, plus JS/TS filters). Use when implementing features, fixing bugs, or refactoring, or when the user says "TDDで". Enforces outside-in order E2E (request through the filter chain) → WIT-conformance (when a filter/host boundary is crossed) → Unit (RED-GREEN-REFACTOR) with concrete stubs, plus a local CI-parity sweep (fmt/clippy/type/test) before handoff.
allowed-tools: Bash, Read, Glob, Grep, Edit, Write
argument-hint: <feature-description> [--component=<dir>]
---

# TDD Workflow (Plecto)

Test-Driven Development workflow. Use in both Plan mode and implementation mode whenever the
task may change code or tests.

**Outside-in order for feature work: E2E → WIT-conformance → Unit.** This governs the *order of
writing tests*. The test pyramid still governs *quantity*: few E2E, more conformance, many unit
tests ([Fowler — Practical Test Pyramid](https://martinfowler.com/articles/practical-test-pyramid.html)).
Two different axes — order vs. quantity — both apply.

**Finish every task with Phase 5 — local CI parity.** After Phases 0–4 are green, run the same
formatters / linters / type checkers / tests CI would run, locally, before declaring done.
"Tests pass" ≠ "CI will pass".

Three layers, mapped to Plecto:

- **E2E (outermost)** — *"does a real request flow through the proxy and its filter chain produce
  the right outcome?"* → drive a request through the running proxy (an HTTP-level test with
  `reqwest`/`hurl`, or an in-process harness that wires a listener + a filter chain) and assert
  the client-visible result (status/headers/body, or the `decision` taken).
- **WIT-conformance (only when a filter/host boundary is crossed)** — *"does a filter component
  satisfy the `plecto:filter` contract, and does the host honour the host-API contract it lends?"*
  → load the component into wasmtime and assert it implements the WIT world and behaves per the
  contract (this is Plecto's analogue of a consumer-driven contract test).
- **Unit** — *"does each module work?"* → per-module tests (router, filter-chain dispatch,
  host-API impl, config parse).

For a pure refactor inside one module (no external behaviour change, no contract change), skip
Phase 0 and Phase 1 and jump to Phase 2.

## Phase 0: E2E FIRST

**Goal:** Write the outermost failing test expressing the client-visible / cross-component
behaviour the change must deliver.

### Decision tree

- Touches a request/response outcome through the chain (auth, rewrite, rate-limit, routing, WAF) → **HTTP-level / in-process proxy E2E**
- Touches only a single inner module with no external behaviour change → skip Phase 0 (go to Phase 2)

### HTTP-level / in-process E2E best practices

- **Parameterize** hosts / ports / tokens — never hardcode `http://127.0.0.1:...`.
- **Health-gate** before exercising business behaviour (retry until the listener is ready).
- **Assertions**: status/headers first, then explicit body assertions. For decision-level tests,
  assert the taken `decision` (continue / modified / short-circuit) and the resulting response.
- Seed config/manifest explicitly (routes, chain, filter digests) — don't rely on ambient state.
- Mock upstreams (a local fake server) rather than hitting real external endpoints.

### Steps

1. **Detect scope** with the decision tree.
2. **Write the failing E2E first** (new test, following a neighbouring file as template).
3. **Run it** — confirm RED for the *right reason* (missing behaviour), not the wrong reason
   (listener not up, connection refused, compile error).
4. **Commit the failing E2E on its own**:
   ```bash
   git commit -m "test(e2e): add failing <feature> scenario"
   ```
5. **Proceed**: filter/host boundary crossed → Phase 1 (WIT-conformance); otherwise → Phase 2.

## Phase 1: WIT-CONFORMANCE CHECK (only if a filter/host boundary is crossed)

**Goal:** If the change touches the `plecto:filter` contract or a host-API surface, add/update a
conformance test so every crossed boundary has one. Run after Phase 0's E2E is RED.

### Detect if a boundary is crossed

- Modifies the `plecto:filter` WIT world (a function, a record/variant field, a resource)?
- Adds/modifies a host-API capability the host lends to filters (KV, counter, metrics, log, clock)?
- Changes the request/response types or the `decision` variant?
- Tightens what a filter must provide or what the host requires (a new required import/field)?

### If a boundary is touched

1. **Filter side (consumer) first** — write/update a conformance test that loads a component into
   wasmtime against the *current* WIT and asserts the behaviour the host depends on
   (see [templates/wit_filter_conformance.rs.tmpl](templates/wit_filter_conformance.rs.tmpl)).
2. **Host side (provider)** — verify the host's host-API implementation satisfies what the
   contract lends (deny-by-default still holds; capabilities not granted are absent).
3. **Provider-tightens rule** — if the host tightens what filters must provide (a new required
   export/field, stricter type), every existing filter's conformance test must pin the new
   requirement and pass before shipping. Enumerate all in-tree filters and confirm each conforms.
4. **Version discipline** — host and components must target the **same WIT version**. If you bump
   the world's version, the conformance suite is the gate (see `wit-contract-design`).

If no boundary is crossed, skip to Phase 2.

## Phase 2: RED (Write Failing Unit Test)

**Goal:** Define expected behaviour through unit tests BEFORE implementation.

### Steps

1. **Detect language & module** — `Cargo.toml` (Rust, the default), `package.json` (Node/JS filter
   or tooling). Identify the architecture layer (see `plecto-architecture`).
2. **Write the test** — define expected behaviour (success / error / edge), not symbol existence.
3. **Create the implementation stub first** so the test fails for the right reason:
   - Rust: `unimplemented!()` / `todo!()`
   - TypeScript/JS: `throw new Error("not implemented")`
4. **Verify the test fails for the RIGHT reason** (not a missing symbol or a compile error). If it
   passes without implementation, rewrite it.
5. **Commit the failing test on its own** (RED and GREEN are separate commits):
   ```bash
   git commit -m "test(<module>): add failing tests for <feature>"
   ```

## Phase 3: GREEN (Minimal Implementation)

**Goal:** Write ONLY enough code to pass the tests.

- Write minimal code; do **not** modify tests to pass them; do **not** add untested features.
- All tests must pass before proceeding.
- Check layer/seam discipline (fast path ↔ extension plane; host-API stays deny-by-default).
- Hot-path code obeys `bp-rust` data-plane discipline (no panics on untrusted input).

## Phase 4: REFACTOR (Clean Up)

**Goal:** Improve quality while keeping tests green.

- Remove duplication, improve naming, simplify — run tests after each change.
- If Phase 1 detected a boundary change, re-run the conformance suite (filter + host).
- **Final commit** (separate from RED):
  ```bash
  git commit -m "feat(<module>): implement <feature>"
  ```

## Phase 5: LOCAL CI PARITY (MANDATORY before handoff)

**Goal:** Reproduce locally the gates CI would run, as the last step before reporting complete.

### Steps

1. **Enumerate touched areas** (`git diff --name-only` against the branch point).
2. **Run the language gate below.** All must pass before handoff.
3. **Never suppress a failing gate** (no `#[allow]` to dodge a lint, no loosening config, no
   skipping a test) — fix the underlying issue or escalate.

### Per-language CI parity commands

**Rust** (the proxy / host)
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
cargo test --all
# 任意（あれば）: cargo audit
```

**TypeScript / JS** (tooling, integration tests, JS filters)
```bash
npm ci                         # or pnpm install --frozen-lockfile
npx tsc --noEmit               # type check
npx biome check .              # or eslint, whichever is configured
node --test                    # or npx vitest run
```

**WASM filter build** (when a filter changed)
```bash
# Rust filter:
cargo build --target wasm32-wasip2 --release        # then componentize per build setup
# JS filter:
npx jco componentize <entry>.js --wit <world>.wit -o <out>.wasm
# then run the conformance suite against the built component
cargo test --test conformance
```

### Reporting

At handoff, state explicitly which gates you ran and their exit status. If a gate is skipped, say
so explicitly (e.g. "skipped cargo audit — not installed, rely on CI"). Do **not** silently skip.

## Test File Conventions

| Language | Unit Test | Contract Test (WIT-conformance) |
|----------|-----------|---------------------------------|
| Rust | `#[cfg(test)] mod tests` or `tests/*.rs` | `tests/conformance/*.rs` (load component into wasmtime) |
| TypeScript / JS | `*.test.ts` / `*.spec.ts` (Vitest / node:test) | `tests/conformance/*.test.ts` |

Per-language stub templates live in `templates/` (rust unit, typescript unit, WIT filter conformance).

## Architecture Integration

1. Identify the target layer (fast path / extension plane / host-API / control) — see `plecto-architecture`.
2. Mock dependencies from outer layers (in-memory redb / fake upstream / hand-written conformance filter).
3. Test only the module's responsibility.

## Anti-Patterns (AVOID)

1. Writing implementation before tests
2. Modifying tests to make them pass
3. Adding features not covered by tests
4. Skipping error / edge case tests
5. Testing implementation details instead of behaviour
6. Changing the `plecto:filter` contract without updating its conformance tests
7. Unit tests that only fail because a symbol doesn't exist yet (use a concrete stub instead)
8. Tightening the host's requirements without updating every filter's conformance test (Phase 1)
9. Treating a host-API capability change as "infra" and skipping Phase 0 — it changes the contract
10. Hardcoding `127.0.0.1:PORT` in E2E — parameterize host/port
11. Writing unit tests first and backfilling E2E at the end — violates outside-in order
12. Declaring work complete without running Phase 5 (local CI parity)
13. Suppressing a Phase 5 failure (disabling a lint, loosening config, skipping a test) to green the gate
14. Allowing a panic on untrusted input to pass review because "the test happened to pass"

## References

- Martin Fowler: The Practical Test Pyramid — https://martinfowler.com/articles/practical-test-pyramid.html
- Component Model — WIT & worlds — https://component-model.bytecodealliance.org/
- wasmtime — embedding & testing components — https://docs.wasmtime.dev/
