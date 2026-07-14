# Plecto Proxy Canon — Design Principles, Architectural Policy, and Working Guidelines

English · [日本語](design-principles.ja.md)

> **Established**: 2026-07-06 (adopted into `docs/` alongside [ADR 000056](ADR/000056.md))
> **Basis**: Derived from the primary sources of the Plecto repository at HEAD (2026-07-13), including the WIT contract (`plecto/wit/world.wit` @ 0.3.0), 86 ADRs (append-only graph with `amends` / `supersedes`), `CLAUDE.md`, `CONTEXT-MAP.md`, crate `CONTEXT.md` files, `docs/ROADMAP.md`, and operational docs.
> **Nature**: This document crystallises Plecto Proxy's design philosophy in three layers — **principles** (what does not change), **policy** (how structure is chosen), and **guidelines** (how day-to-day judgment is applied). The primary record of individual decisions lives in `docs/ADR/`; the contract's authoritative text lives in `wit/`. Where this document disagrees with an ADR or the WIT, the ADR/WIT wins and this document is revised (Chapter 7).

---

## Chapter 0 — Mission and the order of values

**Plecto Proxy is a self-hostable, programmable L7 reverse proxy / API gateway.** It joins two complementary halves with a typed WIT contract: the **fast path** (native Rust) — connection acceptance, TLS termination, HTTP/1.1/2/3, routing, load balancing, upstream management — and the **extension plane** (WASM Component Model filters) — the per-request *decisions*: authentication, rewriting, rate limiting, WAF, policy. The paths where speed is decisive stay native Rust; request logic runs as sandboxed WASM components that **can touch nothing beyond the capabilities the host has explicitly lent them — enforced by the sandbox, not by convention.**

Every design decision is subordinate to the order of values set by `CLAUDE.md`:

**Safety × portability × self-hostability × operational simplicity ＞ feature coverage × strong privileges × distributed-by-default.**

When the left side conflicts with the right, Plecto Proxy always chooses the left. This one line is the parent of every principle, policy, and non-goal below. The mission serves "teams that want to run their own infrastructure and keep both traffic and secrets on it" — **data sovereignty is the first principle.** The quality yardstick is "fit for production adoption by a SaaS company": able to answer a security questionnaire without blanks, with multi-replica semantics documented and a verifiable supply chain. Formal certification for finance/government procurement (FIPS 140-3 and the like) is a non-goal (ADR 000054).

---

## Chapter 1 — Design principles: what does not change

These principles constitute Plecto Proxy's identity. A proposal that changes them is not "an improvement to Plecto Proxy" but "a proposal for a different project"; revising a principle always requires an ADR.

### P1 — The simultaneous satisfaction of four conditions is the reason to exist

The traditional three options for placing custom gateway logic — config/DSL (a ceiling on expressiveness), recompiled-in native extensions (no untrusted code, one language, whole-process blast radius), and out-of-process callouts (a network round trip on every request) — **cannot simultaneously deliver "in-process speed", "sandboxed safety", "freedom of language", and "zero-downtime replacement".** WASM is effectively the only technology that satisfies all four at once, and Plecto Proxy stands on that single point (ADR 000001). A decade of module-ABI data-plane filters proved the in-process WASM shape; Plecto Proxy builds **natively** on the typed, polyglot, composable foundation of the Component Model / WIT that those earlier ABIs did not provide. Any design that permanently sacrifices one of the four conditions violates this principle.

### P2 — Two complementary halves, joined by a typed contract

The fast path and the extension plane are not in a hierarchy or a master–servant relation; they are **complementary halves**. Their boundary is not an implicit convention but a **typed contract** — the `plecto:filter` WIT world — and the contract is the entire boundary. Only the direction is fixed: the fast path drives the chain, the extension plane is driven. As a matter of terminology discipline, vague or overloaded words ("core", "engine", "data plane", "plugin layer", "middleware layer") are avoided in favour of the controlled vocabulary of `CONTEXT-MAP.md` (fast path / extension plane / two halves).

### P3 — Decisions travel as types; even the absence of a capability is a type

A filter's return value is never a bare flag or boolean but always a WIT `variant`: on the request side the three-valued `continue` / `modified(request-edit)` / `short-circuit(http-response)`; on the response side `continue` / `modified(response-edit)`. As the WIT source comments put it: **"Never a bare flag."** (Tenet 3.)

The principle covers **absence** as well as presence. The contract is split into a header-only `filter` world and a body-reading `filter-body` world, so the very **absence** of an `on-request-body` export is a machine-verifiable fact — "this filter does not read the body" — from which the host derives skipping body buffering entirely (zero-copy passthrough; ADR 000005 / 000025 / 000038). Deriving performance optimisation **from the shape of the contract** rather than from operator vigilance is the heart of Plecto Proxy's type design.

As of `plecto:filter@0.2.0` (ADR 000071), HTTP header **values** in the contract are `list<u8>` — the fast path and filters carry the wire bytes, not a lossy UTF-8 projection. Filters that need text decode UTF-8 explicitly; invalid UTF-8 is a first-class byte sequence, not an error at the boundary.

### P4 — Deny-by-default, and fail-closed

The only capabilities a filter can import by default are the interfaces the host explicitly lends: **host-log / host-clock / host-kv / host-counter / host-ratelimit / host-config — six base capabilities**; no other import exists in the Linker (ADR 000006 / 000066). Outbound HTTP and outbound TCP are **feature-gated** optional slices (ADR 000036 / 000060), never implied by the base contract. Zero WASI imports is the default (zero-WASI guests, Tier A) — the shape the deny-by-default Linker instantiates with no further declaration (ADR 000055). A fat guest whose runtime assumes some baseline WASI (TinyGo/Go, Tier B) may be lent a fixed, minimal, off-by-default slice — `wasi:io` / `clocks` / `random` / `cli`, plus an empty `wasi:filesystem` some runtimes' bootstrap unconditionally imports — never filesystem access, never sockets, and only when both the host build and the filter's manifest entry opt in (ADR 000063); absent either, it still fails to instantiate, deny-by-default.

Failure on the **decision path** (filter hooks that gate upstream) always falls to the closed side: trap, deadline, invalid guest header, pool exhaustion → synthetic 5xx, never pass-through. **Auxiliary paths** (e.g. OTLP export, host-log) are best-effort and must not weaken the decision path. Typed manifest `FailurePolicy` overrides remain a deferred increment; until then, classify by path (ADR adversarial review 2026-07-10).

### P5 — Verify before load; apply the same discipline to ourselves

Filters are pinned as OCI artifacts by content digest (sha256), with cosign signature + SBOM verified at load time. The SBOM is bound to the target component via in-toto subject digests, rejecting "a valid signature but an unrelated SBOM". `load` accepts only signed artifacts; no raw-bytes bypass path exists structurally. An empty trust policy means "load nothing"; there is no "allow unsigned" escape hatch in the production API. **Signed machine-readable filter requirements** (isolation, capabilities, config schema) shipped inside the artifact are the target form (ADR adversarial review 2026-07-10); until implemented, manifest + SBOM remain the operator-facing contract.

And **the supply-chain discipline imposed on filters applies to Plecto Proxy's own distribution** (ADR 000047): `cargo-auditable` builds, syft SBOMs, and cosign keyless signatures ship with every tagged release; CI toolchains are sha256-pinned; dependencies are governed by `deny.toml` (cargo-deny) as a CI-blocking gate. The promise "your code, trusted, runs on the gateway" holds only if our own supply chain survives the same verification.

### P6 — Filters are stateless; the host lends state, and state is node-local

"Stateless" is defined precisely: what is forbidden is holding **mutable business state** inside an instance; keeping **immutable init derivatives** resident (compiled regexes, built schemas) is allowed and encouraged (ADR 000011; Tenet 4: heavy initialisation goes into the `init` hook, the hot path stays light). Mutable state goes to the host through host-kv (redb-backed, namespaced by filter identity — no filter can forge its way into another's keyspace), host-counter, and host-ratelimit.

All host-held state is **node-local**. That is not an immaturity of the implementation but a **declared semantics** (ADR 000053), documented in the hardening guide down to its consequence ("effective rate = configured value × N replicas"). When truly global shared state is needed, the receptacle is not native but the extension plane (Fork 6: user-policy goes to filters) — matching the industry's settled local-floor + external-store shape for distributed rate limiting.

### P7 — Single-node first; configuration is declarative and static, change is zero-downtime reload

Distribution is an opt-in layer, not the default (ADR 000008). Configuration is a single declarative manifest — pinning filters by OCI digest and statically declaring trust roots, chain order, routes, and upstreams; the source of truth for "what is loaded" — and xDS-style dynamic config push is not adopted. Change happens via SIGHUP-driven zero-downtime reload: content-hash reconciled, atomic `ArcSwap`, all-or-nothing. Trust roots are fixed at construction and do not change on reload. `plecto validate` provides the same fail-closed validation without reading artifacts, as a CI / pre-reload gate (ADR 000046).

### P8 — Maturity is role-driven; what we won't do is spelled out as "declined"

The axis for adding features is not "a competitor has it" but "the **role** of an L7/API gateway demands it" (ADR 000029). That ADR fixed the native/WASM placement criteria, and subsequent decisions (native rate-limit floor to the fast path, WAF to the extension plane, distributed state pushed out) derive from it.

What we decide not to do is not silently shelved but **recorded as declined in an ADR**. Already declined explicitly: native response caching and AI/LLM gateway (ADR 000043), native WAF (ADR 000037), native distributed state (ADR 000053), h2c (ADR 000015), 0-RTT (ADR 000052). Declined is distinguished from deferred (waiting on timing), and deferred items carry a managed ordering (per ADR 000054, revised by ADR 000056; its former head, mTLS, landed via ADR 000078).

### P9 — Measure honestly: methodology, not a leaderboard

The opening declaration of `performance/README.md` is canon: the goal is **"transparency about method, not a leaderboard."** Every number is an internal **regression baseline**, not a capacity guide and not a comparison with other proxies. Absolute values are bound to the host and the generator; what should be read are the **relative signals — ratios, curve shapes, time constants**. Generators and the proxy are separated by core pinning, warm-up is excluded, closed-loop and open-loop are distinguished, and per-generator ceilings are stated (numbers are comparable only within a section and a single generator). Filter-plane cost is expressed in µs/req, avoiding host-dependent percentages.

### P10 — ADR-first; but when the evidence changes, retract even the same day

Big decisions are written to `docs/ADR/NNNNNN.md` before implementation (six-digit zero-padded, frontmatter, wikilink cross-references). **ADR bodies are append-only history**: current truth is derived from the `amends` / `supersedes` graph (`scripts/check_adr_graph.py` in CI), not from rewriting past Decision text. **Tenets and this document are an independent normative layer** — they are updated directly when principles change; ADRs cite them via `amends_tenets` when needed, but do not generate tenet text. ADRs are an operated discipline, not decoration — the proof is ADR 000051: after deciding "aws-lc-rs declined for its cmake dependency; default to ring", a hands-on `cargo tree` check revealed aws-lc-rs was already linked via sigstore, the premise collapsed, and the decision was **retracted and re-made the same day, consolidating on aws-lc-rs**. The essence of the discipline is not "we decided, so we comply" but "**follow the evidence**"; retraction is not shame, it is the discipline working. TDD is outside-in (E2E → WIT-conformance → Unit); RED and GREEN are separate commits. Security properties are fixed by **falsifiable tests**, not claims.

### P11 — No panics in the data plane

Untrusted input is never allowed to take a worker down. Filter traps are isolated by a circuit breaker (consecutive-trap threshold → 503 cooldown → half-open); pool exhaustion is a bounded wait then fail-closed (ADR 000012); resources are bounded by epoch metering, memory/table limits, and per-filter quotas. "Doesn't crash" and "doesn't fail open" (P4) are held simultaneously — don't fall over, and don't let it through.

### P12 — Control the language

`CONTEXT-MAP.md` and each crate's `CONTEXT.md` define, per context, the controlled vocabulary and the **_Avoid_ vocabulary** (paraphrases not to use). A route is not called a rule, a filter is not called a plugin, an upstream is not called an origin. The glossaries carry no implementation details, specs, or decisions — decisions go to ADRs, contracts to WIT, conventions to CLAUDE.md; this **separation of document roles** is itself a principle. The writing is bilingual: documentation prose in Japanese (with English counterparts for public documents), code / commands / library names / WIT / identifiers in English.

---

## Chapter 2 — Architectural policy: how structure is chosen

### 2.1 Three bounded contexts

Plecto Proxy's Rust workspace consists of three bounded contexts, each with its own `CONTEXT.md`. A
fourth crate, `plecto` (ADR 000091), is a thin operator-CLI entry point over Fast path + Control
(`cargo install plecto`) — it introduces no context of its own, and only exists as the landing spot
for the CLI once it was split out of `plecto-server`.

| Context | crate | Responsibility |
|---|---|---|
| **Fast path** | `plecto-server` | Connection acceptance, TLS termination, HTTP/1.1/2/3, route matching, chain driving, upstream forwarding (ADR 000013) |
| **Extension plane / host runtime** | `plecto-host` | The embedded wasmtime host. Enforcement of the `plecto:filter` contract, the filter execution model, the capability boundary (host-API) |
| **Control** | `plecto-control` | Declarative manifest, loading through the provenance gate, zero-downtime reload, config versions. "What is loaded, and when it swaps" |

Three relations: **Fast path → Extension plane** (drives the chain per request), **Control → Extension plane** (the manifest digest-pins filters and declares chain order and trust roots; reload swaps atomically), **Control → Fast path** (the manifest declares routes and targets; the fast path takes a per-request `ConfigSnapshot` to select routes). The contract `wit/` sits at the workspace root, belonging to no crate — the contract is shared property between contexts, owned by none of them.

### 2.2 Contract architecture (`plecto:filter@0.3.0`)

The contract is defined as its own world, with type convergence toward `wasi:http` (proxy / middleware) fixed as the M3 direction (ADR 000002 / 000020). Deny-by-default is maintained independently of the type vocabulary. Header values are `list<u8>` (ADR 000071); `on-response` receives the as-forwarded request snapshot and `response-decision` carries a `replace` arm, so P3 (decisions as types) holds symmetrically on the response side (ADR 000073). `0.1.0` / `0.2.0` remain loadable via frozen trees + host adapters. The current contract's structure:

- **types**: `http-request` / `http-response` (header values are raw bytes), `request-edit` / `response-edit` (rewrites expressed as diffs), and the typed decision variants — request-side `continue` / `modified` / `short-circuit`, response-side `continue` / `modified` / `replace` (ADR 000073).
- **host-API (six capabilities; 1 interface = 1 capability)**: `host-log` / `host-clock` (wall-clock snapshot captured once at request start) / `host-kv` / `host-counter` / `host-ratelimit` (token bucket stays **host-native**) / `host-config` (read-only manifest `[filter.config]`, ADR 000066).
- **Two worlds**: `filter` (header-only) and `filter-body` (+ `on-request-body`, buffer-then-decide, `list<u8>` in v1). The duplication instead of `include` is a deliberate workaround for WIT's `use`-propagation semantics, with the reason recorded in comments — **the contract file carrying its own design annotations is this repository's house style.**
- **Experimental**: `plecto:filter-streaming` (`stream<u8>`, async) is quarantined behind the off-by-default `streaming-body` feature and stays out of the default build until `wasm32-wasip3` reaches Tier 2.

Contract-evolution policy: changes are additive by default; true streaming of bodies has a reserved seat in the contract as the `list<u8>` → `stream<u8>` swap. Hot-path work (rate-limit refill and the like) drops out of the contract into native — "the WASM tax is paid only on decision logic." `plecto new-filter` fetches the published contract via `wkg` today (ADR 000064 / 000065); ADR 000072 accepts offline self-vendoring of the same `wit/world.wit` the host bindgen reads as the follow-on.

The compatibility promise is **staged** (ADR 000085). Through 0.x, the shipped policy stands: the host keeps loading every contract version it has shipped support for (0.1 / 0.2 run today via frozen trees + load-time adapters), and a superseded major stays accepted for at least two release series before an ADR-declared removal (ADR 000064). From contract 1.0 onward, **every shipped world stays loadable permanently** — the sole exception, "keeping this world loadable is itself unsafe to maintain", requires a dedicated ADR, at least 24 months' notice, and a migration document. Cutting 1.0 is the act that brings this pledge into force, so 1.0 is a milestone of promise, not of feature count.

### 2.3 Execution model: a lifecycle that branches on trust

The precise definition of "stateless" (P6) makes the two-way branch of instance lifecycles a **necessity** (ADR 000011 / 000012):

- **trusted**: checked out per request from a fixed-capacity, lazily-filled instance pool and reused (init runs once; init derivatives stay resident). Exhaustion means a bounded wait then fail-closed. A pool-wide circuit breaker and recycle-after-N bound state accumulation and failure.
- **untrusted**: fresh-per-request instantiation. Linear memory is **fresh by construction** (not an active zeroize operation). The lesson of CVE-2022-39393 (slot-reuse leakage under pooling + memory-init-cow; fixed in wasmtime 2.0.2) is carved into the design as defence-in-depth even though it is long fixed.

Runtime bounding is layered: epoch interruption (a CPU budget; chosen over fuel per wasmtime's own report that it is lighter. It is not a wall-clock SLA, so blocking host calls carry a separate host-timeout — a two-layer design), `StoreLimits` memory caps, table caps, per-filter + host-wide state quotas (fail-closed on excess), and a tightened init deadline for untrusted filters (ADR 000027). The sync chain is bridged to the tokio fast path via `spawn_blocking` (ADR 000013); from wasmtime 46 the host runs guest hooks with `call_async` (fibers) (M3 Stage 1).

### 2.4 Fast-path policy: deterministic matching, layered resilience

**Routing** is multi-axis AND matching over host, path-prefix, method, header (exact), and query (exact); among multiple matches, exactly one route is chosen **deterministically** by specificity (host > longest path prefix > method > header-match count > query-match count > manifest order). No match is 404. A route carries its inline chain, strip_prefix, and rate limit; the target is a single upstream or weighted backends (the canonical primitive for traffic split / canary; `weight 0` = drain). **The path is normalised exactly once at ingress, and encoded separators / dot-escapes that cannot be interpreted are rejected fail-closed — which is what makes a per-route filter a trustworthy authentication boundary** (ADR 000027). This is the fast path's most important safety claim.

**LB and resilience** are layered with separated concerns: instance selection (round-robin / weighted least-request P2C / weighted Maglev consistent hashing, ADR 000035; the RR cursor survives reloads, ADR 000024) → active/passive health checks (pessimistic start; all-unhealthy is a fail-closed 503) → outlier detection (a third axis independent of the health state machine, ADR 000032) → a per-upstream circuit breaker (a concurrency cap, a concern separate from health, ADR 000028) → two-tier timeouts (per-try + overall; overrun is a fail-closed 504, ADR 000031) → bounded retry (jittered exponential backoff, from idempotent/bodyless up to retriable-5xx, always to a different healthy instance, ADR 000023 / 000030) → the native L7 rate-limit floor (coarse-grained per route / client IP; distinct from the host-ratelimit lent to filters, ADR 000033). **"Health", "outlier", and "breaker" are never blended** — each has its own signal and its own recovery path; that is the design principle of the layering.

**Protocol policy**: HTTP/2 terminates over TLS+ALPN; h2c is not adopted (ADR 000015). HTTP/3 terminates on a separate quinn+h3 UDP listener, advertised via Alt-Svc; 0-RTT is rejected (ADR 000016 / 000052). Upstream re-encryption is TLS+ALPN (HTTP/2 preferred, `TE: trailers` passthrough for end-to-end gRPC, custom CA, an `sni` verification-name override for IP endpoints, ADR 000042 / 000050). Hostname upstreams are periodically re-resolved, expanding each A/AAAA record into its own LB endpoint that tracks container re-creation (ADR 000044). WebSocket is a tunnel via a per-route Upgrade-token allowlist (`h2c` rejected at validation) with an activity-based idle timeout (ADR 000048). Client IP follows the edge model — inbound `X-Forwarded-*` is stripped and re-issued from the real peer (ADR 000018 / 000022). Header bytes a filter did not touch pass through byte-for-byte (header byte-equivalence).

### 2.5 TLS and cryptography policy

The crypto provider is **consolidated on aws-lc-rs** (ADR 000051 — recorded together with the retraction of the earlier cmake-declined decision after hands-on verification). Post-quantum X25519MLKEM768 key exchange is preferred by default. TLS 1.3 stateless session resumption ships with ticket-key rotation, holding **0-RTT rejection and node-locality of ticket keys as invariants** (ADR 000052 — a line drawn from the 2025 wave of shared-ticket-key vulnerabilities). Certificates are static files managed by the declarative manifest (ADR 000014). Mutual TLS ships in both directions (ADR 000078): a listener can require verified client certificates (`[listen.client_auth]`, required mode only; combining it with shared STEK is refused outright per ADR 000062 (b)), and an upstream leg can present a client identity (`[upstream.tls] client_cert_path` / `client_key_path` — health probes included). Per-node resumption stays on under client auth (the ticket restores the verified chain; CertificateRequest is not re-sent), so **certificate expiry and revocation are not re-checked for the ticket lifetime** — an Honest gap that shared-STEK refusal does not close. Revocation (CRL/OCSP) and propagation of the verified identity to filters stay declared deferred.

### 2.6 Observability policy: the host propagates; the guest contract stays clean

W3C trace context **continues** the inbound `traceparent` through the proxy (no new root), with one span per filter execution in the OpenTelemetry data model. Span state is the **host's** responsibility (ADR 000009), and OTLP network export is carried by a host-side export pump (batch/retry/flush) — **the no-tokio filter boundary is not broken for observability's sake** (ADR 000040). Making `wasi-otel` part of the guest contract is deferred to M3+. RED metrics are host-aggregated.

### 2.7 State-backend policy

The host state backend is bundled under a single `[state]` setting; the production path is redb (an embedded KV matching the single-process design, ADR 000041). Durability follows purpose: durable KV writes use `Immediate`; ephemeral hot state (counters / buckets) uses `Durability::None`, skipping fsync while keeping atomicity within a single write transaction. The policy is **"durability strength is not uniform — it follows the meaning of the state"**, and corruption behaviour is always deny + self-heal (P4).

---

## Chapter 3 — Working guidelines: how day-to-day judgment is applied

### 3.1 Where does a new feature go? (placement decision tree)

From the role-driven criteria of ADR 000029 and Fork 6, placement is asked in this order:

1. **Is it a common floor every request passes through?** (tenant-independent defence like the rate-limit floor, path normalisation, inbound limits) → **native / fast path**. Keep it coarse-grained; do not seek policy expressiveness there.
2. **Is it a per-user decision (user-policy)?** (authentication, WAF rules, PII masking, custom logic) → **extension plane (WASM filter)**. Do not pull it into native (native WAF is declined, ADR 000037).
3. **Does it need shared state?** → Not in native. Express it as a filter delegating to an external store via the `outbound-http` capability (deny-by-default + per-filter allowlist + IP-pinned SSRF guard, ADR 000036) (ADR 000053).
4. **Is it hot-path counting/refill?** → Drop it out of the contract into host-native, leaving the filter only the "consult the decision" part (the same shape as host-ratelimit).
5. **None of the above?** → Check against the non-goals (Chapter 4). If it matches, write a declined ADR.

### 3.2 Discipline for adding capabilities

A new host-API is cut as "1 interface = 1 capability", preserving deny-by-default. Dangerous capabilities are **quarantined behind off-by-default feature gates before** they land — current instances: `outbound-http` (outside the default build until the wasi:http convergence gate), `streaming-body` (until wasip3 Tier 2), `polyglot-conformance` (no effect on default `cargo test`), `fat-guest` (the minimal-WASI grant for Tier B guests, ADR 000063 — off by default, and even on, inert unless a filter's manifest entry declares `wasi = "minimal"`). Shapes that let a filter relax its own constraints by self-declaration (guest-specified bucket capacity, etc.) are forbidden at design time.

### 3.3 Discipline for adding dependencies

A new dependency must pass cargo-deny (`deny.toml`, CI-blocking); must not drag a cmake-class external toolchain into the build (settled in the ADR 000051 affair — which also teaches "check what is actually linked with `cargo tree` before deciding"); CI toolchains are sha256-pinned. High-risk dependencies like crypto get trimmed default-features (precedent: sigstore restricted to offline keyed verification only).

### 3.4 Make claims falsifiable before publishing them

Strengths, performance, and language support written in README or docs are claimed only after being made falsifiable by tests or measurement. The canonical example is ADR 000055: admitting that the "write filters in any language (polyglot)" banner was an aspirational claim backed only by a Rust example, it was replaced by MoonBit / JS / C zero-WASI example filters verified in CI through a **single shared assertion suite** (`tests/polyglot.rs`) — the commit message itself records "replace the aspirational polyglot claim with the verified per-language status". Likewise, security properties (signature-gate non-bypassability, fail-closed, quotas) are fixed by E2E tests, and performance claims are published as regression baselines together with their measurement method (P9). **A claim that cannot be verified is either cut, or its verification is built first.**

Outward messaging follows a **fixed banner order** (ADR 000083): supply-chain-verified extensibility is the first banner; the typed WIT contract and contract-derived performance are spoken as its means and evidence, never as banners of their own; mesh-less mutual TLS is the complementary second banner, scoped to environments that do not bring a mesh; the implementation language stays in the background as substrate. Regulatory context (EU CRA) may explain why verifiable components matter, but compliance is never claimed and regulatory dates never justify a publishing deadline (ADR 000076). Longevity claims are held to the same falsifiability bar (ADR 000086): no year-number support pledge — an intent declaration plus a retirement protocol (≥12 months' EOL notice with continued security fixes for a deliberate wind-down; reproducible, signed final releases as the honest answer to the involuntary case) and a visible map of the verification culture whose record of truth is default-branch CI green, never a separate ledger.

### 3.5 Process conventions (essentials)

ADRs live at `docs/ADR/NNNNNN.md` with frontmatter + wikilinks; the template is `template.md`. TDD is outside-in (E2E → WIT-conformance → Unit) with RED and GREEN as separate commits. Finish with the local CI-parity sweep: fmt / clippy (`-D warnings`) / type / test. Respect the separation of document roles: terms in CONTEXT.md, decisions in ADRs, contracts in WIT, conventions in CLAUDE.md, operational guidance in the hardening guide, measurement in performance/README.

---

## Chapter 4 — Non-goals: what we deliberately do not build

These are **decisions**, not neglect; each row has a grounding ADR. Lifting one requires a new ADR.

| Non-goal | Grounds | Note |
|---|---|---|
| Becoming a general-purpose compute platform (long-lived stateful execution) | Founding decision | Filters are stateless (P6). Learn from scope-bloat cautionary tales |
| Cloning a full L7 feature catalogue | ADR 000029 | Maturity is role-driven; we don't compete on feature count |
| Native distributed state (gossip / central counters / shared-store dependency) | ADR 000053 | Shared state goes to the extension plane per Fork 6 |
| Native WAF | ADR 000037 | User-policy goes to the extension plane |
| Native response caching / AI·LLM gateway | ADR 000043 | Outside the declared role |
| Legacy module-ABI filter compatibility | ADR 000001 | Being Component Model-native is the reason to exist |
| xDS-style dynamic config push | ADR 000008 | Declarative static config + zero-downtime reload (P7) |
| h2c (plaintext HTTP/2) | ADR 000015 | Rejected even via the Upgrade allowlist, at validation |
| TLS 0-RTT | ADR 000052 | No replay surface. An invariant |
| FIPS 140-3 and similar formal compliance certification | ADR 000054 | The quality target is SaaS-adoption grade |
| Designing around a managed SaaS | Founding decision | Self-hosting / data sovereignty is the first principle (Chapter 0) |

---

## Chapter 5 — Conditions for evolution: what re-opens what

The principles do not change, but the policies carry explicit external triggers for reconsideration.

| Trigger | What is reconsidered |
|---|---|
| `wasm32-wasip3` reaches Rust Tier 2, wit-bindgen async matures | Promotion of the `streaming-body` feature toward default (real `stream<u8>` implementation, M3's true-streaming increment) |
| The `wasi:http` (proxy / middleware) convergence gate is met | Executing the `wasi:http` type convergence (ADR 000020) and the default-build decision for `outbound-http` |
| Go (`gc`) reaches Tier-equivalent wasip2/p3 Component Model support | Revisit the TinyGo-only assumption behind Tier B (ADR 000063, decided 2026-07-06 and implemented 2026-07-08) |
| Revocation or identity-propagation demand appears on the landed mTLS floor | The declared deferreds of the mTLS slice (ADR 000078, landed 2026-07-12): CRL/OCSP wiring, and propagation of the verified client identity to filters |
| Demand materialises for remote filter-registry fetch (the wkg boundary) | The M4 remainder — offline image-layout is the intended default today |
| A credible alternative crypto provider matures (e.g. an audited pure-Rust implementation) | Revisiting ADR 000051, using the criteria that ADR established: actual-link verification + build DX + maintenance status |
| Real demand for opt-in distributed consensus (foca / openraft) | Whether to start the deferred M5 portion — as an opt-in layer, keeping single-node first (P7) |
| The technical-preview floor (ADR 000077 + 000084) lands | The reachability-first priority rule expires and ordering returns to role-driven (ADR 000029 / 000084). Until then, reachability items — the signature-verified one-command quick start (operator TTFV ≤ 5 min yardstick), the compose reference, English first-run docs — outrank new feature slices |
| Contract 1.0 is cut (only after the wasi:http convergence major has settled) | The permanent world-loading pledge comes into force (ADR 000085): security-only exception, via a dedicated ADR + ≥24 months' notice + a migration document |
| Primary evidence of demand for typed-contract migration appears (migration cases, operator/author asks) | The subordinate place of the WIT contract in outward messaging is re-evaluated for promotion (ADR 000083); likewise, mesh-grade mTLS reaching non-orchestrated environments demotes the complementary second banner |

Reconsideration always runs "trigger → dedicated ADR → implementation"; feature-gate defaulting and deferred-item promotion never happen without an ADR.

---

## Chapter 6 — One-page summary: when in doubt

1. **Order of values**: safety × portability × self-hostability × operational simplicity beat features, privileges, and distribution.
2. **The boundary is the contract**: between the fast path and the extension plane there are only WIT types. Decisions ride variants; absence is a type too.
3. **Default is deny**: imports, failures, unsigned artifacts, unconfigured buckets, all-unhealthy upstreams — everything falls closed.
4. **State to the host, on the node**: filters hold no mutable state; host state is node-local. If sharing is needed, it goes out through a filter.
5. **The floor is native, policy is WASM**: coarse defence everyone passes through lives in the fast path; per-user decisions live in the extension plane.
6. **Claims must be falsifiable**: a strength without a test or a measurement is not yet a strength.
7. **Write first, then build; overturn on evidence**: ADR-first. Retraction is the discipline working, not failing.

---

## Chapter 7 — Authority and revision procedure

This document is founding-level, but **it ranks below the ADRs and the WIT** in the order of primary sources: (1) `wit/` (the contract's authoritative text), (2) `docs/ADR/` (the primary record of decisions), (3) this document (the crystallisation of principles and policy), (4) CLAUDE.md / CONTEXT-MAP / hardening / performance (the operating texts of their areas). When a conflict with a higher source is found, this document is the one revised.

Revision procedure: changes to Chapter 1 (principles) must be preceded by an ADR. Chapters 2 (policy), 4 (non-goals), and 5 (conditions for evolution) track the acceptance/decline of the corresponding ADRs. Chapter 3 (guidelines) may be updated as process improvement in sync with CLAUDE.md. Any revision re-records the grounding HEAD commit hash at the top of this document — **this document is itself subject to P10 (follow the evidence) and §3.4 (falsifiability).**
