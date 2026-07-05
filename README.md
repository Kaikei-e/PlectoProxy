<div align="center">

# Plecto

**A self-hostable, programmable L7 reverse proxy & API gateway — in Rust, extended with WebAssembly.**

[![CI](https://github.com/Kaikei-e/Plecto/actions/workflows/ci.yml/badge.svg)](https://github.com/Kaikei-e/Plecto/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/rust-edition%202024-orange.svg)](https://doc.rust-lang.org/edition-guide/)
[![Status: early development](https://img.shields.io/badge/status-early%20development-yellow.svg)](#roadmap)

English · [日本語](README.ja.md)

</div>

---

Plecto pairs **two complementary halves** through a typed [WIT](https://component-model.bytecodealliance.org/) contract:

- a **fast path** in native Rust — connection handling, TLS termination, HTTP/1.1·2·3, routing, load balancing, and upstream management;
- an **extension plane** of **WebAssembly Component Model filters** — the per-request *decisions* (auth, header/body rewriting, rate limiting, WAF, policy) that you write in **any language**, plug in over the `plecto:filter` contract, and **hot-swap with zero downtime**.

The speed-critical path stays native Rust. Your request logic runs as a sandboxed WASM component that can touch **only** the capabilities the host explicitly lends it — enforced by the sandbox, not by convention.

> [!WARNING]
> **Status: early development.** The design is settled (54 ADRs, 53 accepted) and the foundation runs end to end: the `plecto:filter` contract, a wasmtime host that loads and runs filters, and a **fast path** that terminates **HTTP/1.1, HTTP/2 (ALPN), HTTP/3 (QUIC)** and **TLS**, **routes** by host · path-prefix · method · header · query in specificity order with weighted **traffic split (canary)**, runs the route's filter chain over headers **and** a request body, propagates the client IP in an edge model, and **load-balances across healthy upstream instances** — round-robin, **weighted least-request (power-of-two-choices)**, or **weighted Maglev consistent hashing** — backed by active/passive **health checks**, **outlier detection**, a per-upstream **circuit breaker**, two-tier (per-try + overall) **timeouts**, jittered **retry**, and a native L7 **rate-limit** floor. TLS terminates on a consolidated **aws-lc-rs** crypto provider with post-quantum X25519MLKEM768 key exchange preferred by default and **stateless TLS 1.3 session resumption** (rotated ticket keys, 0-RTT rejected). Upstream legs can be **re-encrypted with TLS+ALPN** (gRPC/HTTP-2 passthrough, custom CA, a pinned verification-name **`sni`** override for IP-literal or DNS-expanded endpoints) and **periodically re-resolved** from DNS so hostname upstreams track container churn; a per-route **HTTP/1.1 `Upgrade` token allowlist** splices WebSocket tunnels end to end. A security-hardening pass ([ADR 000027](docs/ADR/000027.md)) makes route selection a reliable auth boundary — the path is normalized at ingress and encoded escapes are rejected fail-closed — bounds host-held state with per-filter quotas, and enforces inbound resource limits. The shipped binary wires SIGHUP hot reload, graceful shutdown, OTLP trace export, and an operator CLI (`plecto validate` / `schema` / `--version`); `v0.1.1` is tagged with a signed-artifact release pipeline (cosign + SBOM) of its own. The full suite is green on CI — a foundation you can read, run, and build filters against. See the [Roadmap](#roadmap).

## Why Plecto?

Every gateway eventually faces the same question: **where does custom logic go?** The classic answers each involve trade-offs:

| Approach | In-process speed | Sandboxed | Any language | Hot-swap |
| --- | :---: | :---: | :---: | :---: |
| Config / DSL | ✅ | ✅ | ❌ | ✅ |
| Recompile into the binary | ✅ | ❌ | ❌ | ❌ |
| Out-of-process (`ext_proc`, sidecar) | ❌ | ✅ | ✅ | ✅ |
| **WASM filters — Plecto** | ✅ | ✅ | ✅ | ✅ |

WASM data-plane filters are an idea **Envoy and proxy-wasm pioneered** — proxy-wasm targets the earlier WASM ABI (v0.2.1); the **Component Model and WIT** have since matured into a typed, polyglot, composable foundation, and Plecto builds on them natively. High-performance Rust proxies like **Cloudflare's Pingora** show how fast a native data path can be; Plecto's focus is **pairing that speed with a Component-Model extension plane**, for teams who want to self-host and keep traffic and secrets on their own infrastructure — **data sovereignty** as a first principle.

See [ADR 000001](docs/ADR/000001.md) for the full rationale and rejected alternatives.

## Design tenets

> Safety × portability × self-hostability × operational simplicity **＞** feature breadth × broad privilege × distributed-by-default.

- **Deny-by-default capabilities** — a filter can reach nothing but the host-API explicitly lent to it (KV, counter, metrics, log, clock, random). No outbound network, filesystem, or sockets unless granted. Enforced by the Component Model sandbox.
- **Decisions are typed** — a filter returns a `decision` variant: `continue` / `modified` / `short-circuit`. Never an ambiguous flag or an implicit side effect.
- **Init vs per-request** — expensive setup (regex compile, schema build) goes in an `init` hook; the per-request hot path stays lean.
- **Filters are stateless** — rate-limit, session, and cache state live in host KV, so filters pool, scale, and hot-swap cleanly.
- **Fail-closed** — a filter trap or deadline overrun never silently passes traffic through.
- **Single-node first** — one node completes the job; distribution (membership, config consensus) is opt-in.
- **No panics in the data plane** — a single bad request must never take down a worker.

## Architecture

Plecto is a fast **native highway** plus a **checkpoint where your own code runs**: native Rust
accepts connections, terminates TLS, speaks HTTP, routes, and load-balances; the **extension plane**
hands each request to your *filter* — a small sandboxed WASM program — which inspects it and returns
one of three decisions. That decision is where the policy lives.

```mermaid
flowchart LR
    client(["Client"])
    upstream(["Upstream service"])

    subgraph fast["Fast path · native Rust"]
        direction TB
        edge["accept · TLS · HTTP/1·2·3"]
        route["route match · load balance"]
        edge --> route
    end

    subgraph ext["Extension plane · your filter, sandboxed WASM"]
        direction TB
        inspect["inspect each request<br/>headers, and the body if it asks"]
        decide{"decide"}
        inspect --> decide
    end

    state[("Host-held state and services<br/>rate-limit · KV · counter · log · clock")]

    client -->|"1 · request"| edge
    route -->|"2 · run the filter chain"| inspect
    decide -->|"3 · continue / modify, then forward"| upstream
    decide -.->|"3 · reject and answer now<br/>401 / 403 / 429 — upstream never reached"| client
    upstream -->|"4 · response — filters may edit on the way back"| client
    decide <-->|"borrows only the capabilities it was lent"| state
```

**Continue** (pass through), **modify** (rewrite a header/body, then pass), or **reject** (answer the
client *now* with `401/403/429` — the upstream is **never reached**) are the whole mental model. The
filter is **stateless**: anything it needs to remember lives in the host, reached only through
capabilities it was explicitly lent (deny-by-default).

A filter is a signed WASM component, and the **same** component runs two ways depending on how much
you trust it — the single biggest performance lever:

```mermaid
flowchart TB
    wasm["A filter is one signed WASM component<br/>(write it in any language)"]
    verify["verify the signature, then load<br/>bad signature → refused (fail-closed)"]
    profile{"how much is it trusted?"}

    pooled["trusted → pooled<br/>built once, instances reused<br/>fast hot path (~2 µs / request)"]
    fresh["untrusted → fresh per request<br/>rebuilt and wiped each time<br/>stronger isolation (~12× slower)"]

    guards["always on, every instance:<br/>time limit · memory limit<br/>fail-closed on trap or timeout"]

    wasm --> verify --> profile
    profile -->|trusted| pooled
    profile -->|untrusted| fresh
    pooled --> guards
    fresh --> guards
```

**Rule of thumb:** user-specific logic / policy / WAF / auth / rewrite → a WASM filter; TLS / routing / LB / connection pools / global counters → native Rust — a role-driven placement rule fixed in [ADR 000029](docs/ADR/000029.md): native grows only for cross-cutting concerns, never per-request policy. The WASM "tax" (data copy + host-call overhead) hits only request-decision logic, never the speed path — **~2 µs/request** for a pooled filter ([performance](performance/README.md)).

## What the gateway does today

The native fast path has matured well past "a proxy that works." A snapshot of what is implemented and CI-green (each row links the deciding ADR):

| Concern | Today |
| --- | --- |
| **Edge & HTTP** | HTTP/1.1, HTTP/2 (ALPN), HTTP/3 (QUIC, Alt-Svc advertised); TLS termination with SNI cert selection, manifest-declared, fail-closed, on a consolidated **aws-lc-rs** crypto provider with post-quantum X25519MLKEM768 preferred by default and **stateless TLS 1.3 session resumption** (rotated ticket keys, 0-RTT rejected) — [ADR 13–16](docs/ADR/000013.md) · [51](docs/ADR/000051.md) · [52](docs/ADR/000052.md) |
| **Routing & upgrades** | host / path-prefix / method / header / query matching in **specificity order**; weighted **traffic split / canary**; ingress path normalization as a fail-closed auth boundary; per-route **HTTP/1.1 `Upgrade`** tunnelling for WebSocket (`h2c` rejected) — [34](docs/ADR/000034.md) · [48](docs/ADR/000048.md) |
| **Load balancing & upstreams** | **round-robin** (default), **weighted least-request** (P2C), or **weighted Maglev** per upstream; active + passive health checks, outlier detection, circuit breaker, two-tier timeouts, jittered retry; per-upstream **TLS+ALPN re-encryption** (gRPC-ready, with a pinned verification-name **`sni`** override for IP-literal or DNS-expanded endpoints) and **periodic DNS re-resolution** — [17](docs/ADR/000017.md) · [35](docs/ADR/000035.md) · [42](docs/ADR/000042.md) · [44](docs/ADR/000044.md) · [50](docs/ADR/000050.md) |
| **Rate limiting** | native L7 token-bucket floor per **route** / **client-IP**, plus a per-filter `host-ratelimit` capability — **node-local** (see the [hardening guide](docs/hardening.md) for multi-replica scaling) — [33](docs/ADR/000033.md) · [53](docs/ADR/000053.md) |
| **Extension plane** | `plecto:filter` chain over headers and, for opted-in filters, the body (header-only filters skip buffering — zero-copy); typed `decision`; trusted **pooled** / untrusted **fresh** instances; deny-by-default host-API with per-filter + host-wide quotas; feature-gated **outbound HTTP** behind an SSRF guard — [1](docs/ADR/000001.md) · [25](docs/ADR/000025.md) · [38](docs/ADR/000038.md) |
| **Client IP** | edge-model propagation — re-issues `X-Forwarded-For` / `X-Real-IP` from the real peer before the chain runs — [18](docs/ADR/000018.md) |
| **Supply chain & ops** | cosign + SBOM-verified filter loading; zero-downtime SIGHUP reload + graceful shutdown wired into the shipped binary; W3C trace propagation, RED metrics, OTLP export; `plecto validate` / `schema` / `--version`; Plecto's own binary and container image carry the same signed-artifact discipline — [6](docs/ADR/000006.md) · [39](docs/ADR/000039.md) · [46](docs/ADR/000046.md) · [47](docs/ADR/000047.md) |

## The filter contract

The heart of Plecto is the `plecto:filter` WIT world — a custom world that defines Plecto's own vocabulary (the typed `decision`, init/per-request hooks, the deny-by-default host-API) while reusing standard types for polyglot compatibility.

```wit
package plecto:filter@0.1.0;

interface types {
  // The typed outcome of a request-side filter. Never a bare flag.
  variant request-decision {
    %continue,                       // pass unchanged to the next filter
    modified(request-edit),          // apply the edit, then continue
    short-circuit(http-response),    // stop the chain; synthesise a response now
  }
}

// deny-by-default: one capability per interface; a filter imports only what it is lent.
interface host-kv      { get: func(key: string) -> option<list<u8>>; set: func(key: string, value: list<u8>); /* … */ }
interface host-counter { increment: func(key: string, delta: s64) -> s64; /* atomic named counter */ }
interface host-log     { log: func(level: level, message: string); }
// host-ratelimit keeps the token bucket host-native — the hot-path refill/counting never crosses
// the WASM boundary. The bucket spec (capacity/refill) is host-configured in the manifest; the
// filter passes only (key, cost), so an untrusted filter cannot widen its own limit (ADR 000005 / 000026).

// Base contract: header-only filters (auth, rate-limit, WAF, rewrite) target this world. The host
// reads the ABSENCE of `on-request-body` as the signal to skip buffering the body entirely —
// zero-copy passthrough for filters that never touch it (ADR 000038).
world filter {
  import host-log;  import host-clock;  import host-kv;  import host-counter;  import host-ratelimit;
  export init: func();                                                // heavy, once per instance
  export on-request:  func(req: http-request)  -> request-decision;   // hot path (headers)
  export on-response: func(resp: http-response) -> response-decision; // hot path (headers)
}

// Body-reading contract: `filter` plus `on-request-body`. Its PRESENCE is what makes the host
// buffer the request body and run this hook (buffer-then-decide, ADR 000025).
world filter-body {
  import host-log;  import host-clock;  import host-kv;  import host-counter;  import host-ratelimit;
  export init: func();
  export on-request:      func(req: http-request)  -> request-decision;
  export on-request-body: func(body: list<u8>)     -> request-body-decision;  // buffered body hook
  export on-response:     func(resp: http-response) -> response-decision;
}
```

> v0.1.0 started **sync + header-only**; the request-side **body hook** (`on-request-body`, a buffered `list<u8>` in v1, [ADR 000025](docs/ADR/000025.md)) now runs end-to-end for filters targeting `filter-body`. An **experimental, feature-gated** `stream<u8>` body world ([ADR 000020](docs/ADR/000020.md)) and `wasi:http` type reuse are next, gated on the P3 guest toolchain.

## Writing a filter

A filter is just a component that implements the world. Here is the included example (`examples/filters/filter-quickstart`), in Rust:

```rust
wit_bindgen::generate!({ path: "../../../wit", world: "filter" });

struct FilterQuickstart;

impl Guest for FilterQuickstart {
    fn init() {}

    fn on_request(_req: HttpRequest) -> RequestDecision {
        RequestDecision::Continue
    }

    fn on_response(_resp: HttpResponse) -> ResponseDecision {
        // The one visible thing this filter does: stamp a header so `curl -i` shows a WASM filter
        // touched the response.
        ResponseDecision::Modified(ResponseEdit {
            set_status: None,
            set_headers: vec![Header { name: "x-plecto".into(), value: "hello-from-wasm".into() }],
            remove_headers: vec![],
        })
    }
}

export!(FilterQuickstart);
```

This targets the header-only `filter` world, so the host streams the body straight through untouched.
A filter that needs the body targets `filter-body` and adds one export — see
[`filter-apikey`](plecto/examples/filters/filter-apikey) (header-only) or
[`filter-hello`](plecto/examples/filters/filter-hello) (`filter-body`, the host's own conformance fixture).

Because the contract is WIT, **any language that compiles to a WASM component can write a filter** — Rust, Go (TinyGo), JavaScript/TypeScript (`jco`), or Python (`componentize-py`). Polyglot filter SDKs are on the [roadmap](#roadmap).

A complete how-to — scaffold, build, the manifest field reference, signing, and local testing — is in [**Writing a filter**](docs/writing-a-filter.md). A copy-ready starting point with the contract already vendored lives in [`examples/filters/filter-template`](plecto/examples/filters/filter-template).

## Try it

The repository pins its toolchain and WASM target in
[`plecto/rust-toolchain.toml`](plecto/rust-toolchain.toml) — [`rustup`](https://rustup.rs/) sets it
up automatically on your first build (outside that toolchain: `rustup target add wasm32-unknown-unknown`).

```bash
cd plecto
cargo test --all   # builds the example filter to a WASM component, loads it into the wasmtime host,
                    # and exercises the contract end to end
```

The example component imports only `plecto:filter/*` — zero WASI, network, or filesystem access — so
the suite proves a filter reaches **only** the capabilities it was lent, with the typed `decision`
round-tripping through a real component.

### Run the demos

Nine self-contained demos live under `examples/<name>/`, each wiring the **production load path** (sign + offline OCI layout + verify + load, fail-closed) and printing copy-paste `curl` commands on startup. [`examples/README.md`](plecto/examples/README.md) is the guided learning path with the full write-up of each; here's the quick map:

```bash
cd plecto
./examples/try.sh <name>                      # guided tour: runs it, curls it, cleans up (or `all`)
cargo run -p plecto-server --example <name>   # or drive it yourself, Ctrl-C to stop
```

| `<name>` | What it shows |
| --- | --- |
| `quickstart` | 5-minute hello: a signed WASM filter stamps a response header. Start here. |
| `wasm-auth` | A real filter doing real work — signed API-key auth, host KV, typed decisions. |
| `load-balancing` | Round-robin over 3 instances, active health checks, fail-closed ejection. |
| `filter-chain` | continue / modify / short-circuit / host-native rate limit, composed. |
| `tls-http` | TLS termination across HTTP/1.1, HTTP/2 (ALPN), and HTTP/3 on one port. |
| `hot-reload` | Zero-downtime config swap via SIGHUP; a broken edit stays fail-closed. |
| `canary` | 90/10 weighted traffic split, header-match routing, SIGHUP drain/promote. |
| `resilience` | Per-try timeout+retry, circuit breaker, outlier ejection — all visible from curl. |
| `production` | The real `plecto` binary serving a full deploy dir, two terminals. |

The benchmark harnesses (`bench-server`, `swap-bench`) are not demos — they live under [`bench/harnesses/`](bench/) and produce the numbers in [performance](performance/README.md).

## Roadmap

Plecto is built ADR-first, milestone by milestone. The full detail — landed items, what's next, and the deciding ADR for each — lives in [`docs/ROADMAP.md`](docs/ROADMAP.md); here's the snapshot:

| Milestone | Status | Covers |
| --- | --- | --- |
| **M0** — Foundation | ✅ done | `plecto:filter@0.1.0` contract, wasmtime host, capability boundary, CI |
| **M1** — Filter runtime hardening | ✅ landed | trusted pool / untrusted fresh-per-request, redb KV, host-native rate limiting, quotas |
| **M2** — The data path (fast path) | 🚧 maturing | HTTP/1–3 + TLS, routing / LB / resilience, upstream TLS + periodic DNS re-resolve, WebSocket tunnelling |
| **M3** — Async & bodies | 🚧 Stages 1–2 landed | wasmtime-46 async, header/body-world split, buffer-then-decide body hook; `stream<u8>` is experimental |
| **M4** — Provenance & zero-downtime reload | ✅ landed | OCI + cosign + SBOM filter loading, SIGHUP reload + graceful shutdown, signed releases of Plecto itself |
| **M5** — Observability & opt-in distribution | 🚧 mostly landed | W3C trace propagation, RED metrics, OTLP export landed; opt-in config consensus deferred |
| **M6** — Polyglot SDKs & reference filters | 🚧 outbound landed | SSRF-guarded outbound HTTP (feature-gated); Go/JS/Python SDKs and reference filters pending |

## Project layout

```
.
├── plecto/                    # Rust workspace (the native half)
│   ├── wit/world.wit          # the plecto:filter contract (contract-first)
│   ├── deny.toml              # cargo-deny supply-chain policy (CI-blocking)
│   ├── crates/
│   │   ├── host/              # wasmtime embedding: Linker, InstancePre, host-API (+ CONTEXT.md)
│   │   ├── control/           # control plane: manifest, OCI load, chain, reload, TLS/QUIC (+ CONTEXT.md)
│   │   └── server/            # fast path: HTTP/1.1·2 (hyper) + HTTP/3 (quinn), routing, LB, upstream (+ CONTEXT.md)
│   └── examples/              # runnable demos + example filter guests — see examples/README.md (the DX map)
│       ├── README.md          # the guided learning path (quickstart → real use cases)
│       ├── <use-case>/        # nine demos: cargo run -p plecto-server --example <name>
│       └── filters/           # example plecto:filter guests (own workspace, componentized by build.rs)
│           ├── filter-quickstart/ # minimal starter (stamps one header)
│           ├── filter-apikey/ # API-key auth gate (real-world example)
│           ├── filter-hello/  # the host's own conformance fixture
│           ├── filter-template/ # copy-ready starter (vendored WIT)
│           ├── filter-streaming/ # experimental stream<u8> filter (feature-gated)
│           └── filter-extauthz/ # ext_authz over outbound HTTP (feature-gated)
├── bench/                     # benchmark harnesses + runbook (k6/oha; harnesses/, filters/, perf/)
├── performance/              # the benchmark write-up + results (see performance/README.md)
├── docs/ADR/                  # Architecture Decision Records (000001–000054)
├── CHANGELOG.md               # Keep a Changelog + pre-1.0 versioning policy
├── CLAUDE.md                  # project conventions & design summary
├── CONTEXT-MAP.md             # domain glossary map (split per context)
└── Dockerfile                 # reference multi-stage build (distroless runtime)
```

## Design decisions

Plecto records every load-bearing decision as an ADR in the Fork form (*decision / rationale / re-examination condition*). All 54 (53 accepted, 1 proposed) live in [`docs/ADR/`](docs/ADR/) — start at [ADR 000001](docs/ADR/000001.md) (the two complementary halves); each cross-links the decisions it builds on.

## Contributing

Contributions are deliberate: please **agree an approach in an issue or [Discussion](https://github.com/Kaikei-e/Plecto/discussions) before opening a PR** (unsolicited PRs may be closed). Plecto follows outside-in TDD (E2E → WIT-conformance → unit) and records load-bearing decisions as ADRs. See [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide — including the areas that need extra care and DCO sign-off — and [CLAUDE.md](CLAUDE.md) for conventions. Local CI parity before a PR:

```bash
cd plecto
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

(or just `just check` from the repository root.)

## License

Licensed under the **Apache License, Version 2.0** — see [LICENSE](LICENSE). The Apache-2.0 patent grant suits an infrastructure project; it is the license used by Envoy, Linkerd, and containerd.

## Prior art & acknowledgements

Plecto stands on the shoulders of [Envoy](https://www.envoyproxy.io/) / [proxy-wasm](https://github.com/proxy-wasm), [Cloudflare Pingora](https://github.com/cloudflare/pingora), and the [Bytecode Alliance](https://bytecodealliance.org/) — [wasmtime](https://wasmtime.dev/), [WIT, and the Component Model](https://component-model.bytecodealliance.org/).
