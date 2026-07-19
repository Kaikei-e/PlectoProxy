<div align="center">

# Plecto Proxy

**A self-hostable, programmable L7 reverse proxy & API gateway — in Rust, extended with WebAssembly.**

[![CI](https://github.com/Kaikei-e/PlectoProxy/actions/workflows/ci.yml/badge.svg)](https://github.com/Kaikei-e/PlectoProxy/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust edition 2024](https://img.shields.io/badge/rust-edition%202024-orange.svg)](https://doc.rust-lang.org/edition-guide/)
[![Status: early development](https://img.shields.io/badge/status-early%20development-yellow.svg)](#roadmap)

English · [日本語](README.ja.md)

</div>

---

Plecto Proxy pairs **two complementary halves** through a typed [WIT](https://component-model.bytecodealliance.org/) contract:

- a **fast path** in native Rust — connection handling, TLS termination, HTTP/1.1·2·3, routing, load balancing, and upstream management;
- an **extension plane** of **WebAssembly Component Model filters** — the per-request *decisions* (auth, header/body rewriting, rate limiting, WAF, policy) that you write in **any language**, plug in over the `plecto:filter` contract, and **hot-swap with zero downtime**.

The speed-critical path stays native Rust. Your request logic runs as a sandboxed WASM component that can touch **only** the capabilities the host explicitly lends it — enforced by the sandbox, not by convention.

> [!WARNING]
> **Status: early development.** The design is settled (see [`docs/ADR/`](docs/ADR/) for the accepted decisions) and the foundation runs end to end: the `plecto:filter@0.3.0` contract (byte-valued headers; response hooks see the as-forwarded request and can `replace` the response; `0.1.0` / `0.2.0` still loadable), a wasmtime host that loads and runs filters, and a **fast path** that terminates **HTTP/1.1, HTTP/2 (ALPN), HTTP/3 (QUIC)** and **TLS**, **routes** by host · path-prefix · method · header · query in specificity order with weighted **traffic split (canary)**, runs the route's filter chain over headers **and** a request body, propagates the client IP in an edge model, and **load-balances across healthy upstream instances** — round-robin, **weighted least-request (power-of-two-choices)**, or **weighted Maglev consistent hashing** — backed by active/passive **health checks**, **outlier detection**, a per-upstream **circuit breaker**, two-tier (per-try + overall) **timeouts**, jittered **retry**, and a two-tier **rate-limit** model (a native per-replica local floor plus a Redis-backed global reference filter). TLS terminates on a consolidated **aws-lc-rs** crypto provider with post-quantum X25519MLKEM768 key exchange preferred by default and **stateless TLS 1.3 session resumption** (rotated ticket keys, 0-RTT rejected). Upstream legs can be **re-encrypted with TLS+ALPN** (gRPC/HTTP-2 passthrough, custom CA, a pinned verification-name **`sni`** override for IP-literal or DNS-expanded endpoints) and **periodically re-resolved** from DNS so hostname upstreams track container churn; a per-route **HTTP/1.1 `Upgrade` token allowlist** splices WebSocket tunnels end to end. A security-hardening pass ([ADR 000027](docs/ADR/000027.md)) makes route selection a reliable auth boundary — the path is normalized at ingress and encoded escapes are rejected fail-closed — bounds host-held state with per-filter quotas, and enforces inbound resource limits. The shipped binary wires SIGHUP hot reload, graceful shutdown, OTLP trace export, and an operator CLI (`plecto validate` / `schema` / `new-filter` / `dev` / `conformance` / `--version`); every [tagged release](https://github.com/Kaikei-e/PlectoProxy/releases) ships its own signed-artifact pipeline (cosign + SBOM). The full suite is green on CI — a foundation you can read, run, and build filters against. See the [Roadmap](#roadmap).

## Quick start

Verify the signed container image, then run it — Docker is the only prerequisite:

```bash
IMAGE=ghcr.io/kaikei-e/plecto
TAG=0.5.0   # pick the latest release: https://github.com/Kaikei-e/PlectoProxy/releases
DIGEST=$(docker buildx imagetools inspect "$IMAGE:$TAG" --format '{{json .Manifest.Digest}}' | tr -d '"')

docker run --rm ghcr.io/sigstore/cosign/cosign:v3.1.1 verify "$IMAGE@$DIGEST" \
  --certificate-identity-regexp 'https://github.com/Kaikei-e/PlectoProxy/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Then run the digest you just verified. The full copy-paste flow — minimal manifest,
stand-in backend, first proxied response, in under 5 minutes — is
**[docs/quickstart/](docs/quickstart/README.md)**. Signature verification is part of the
flow, not a footnote ([ADR 000084](docs/ADR/000084.md) / [ADR 000087](docs/ADR/000087.md)).

## Why Plecto Proxy?

Every gateway eventually faces the same question: **where does custom logic go?** The classic answers each involve trade-offs:

| Approach | In-process speed | Sandboxed | Any language | Hot-swap |
| --- | :---: | :---: | :---: | :---: |
| Config / DSL | ✅ | ✅ | ❌ | ✅ |
| Recompile into the binary | ✅ | ❌ | ❌ | ❌ |
| Out-of-process (`ext_proc`, sidecar) | ❌ | ✅ | ✅ | ✅ |
| **WASM filters — Plecto Proxy** | ✅ | ✅ | ✅ | ✅ |

Earlier data-plane filter work proved that **in-process WASM** can carry gateway policy; it typically sat on the older **module ABI**. The **Component Model and WIT** have since matured into a typed, polyglot, composable foundation, and Plecto Proxy builds on that natively — pairing a fast native data path with a sandboxed extension plane — for teams who want to self-host and keep traffic and secrets on their own infrastructure (**data sovereignty** as a first principle). Positioning is by extension-model type, not by product catalogue ([ADR 000067](docs/ADR/000067.md)); the outward message order is fixed ([ADR 000083](docs/ADR/000083.md)) — **supply-chain-verified extensibility first** (signature · SBOM · capability contract as a mandatory gate on what you load), the typed WIT contract spoken as its means, and **mesh-less mutual TLS** as the complementary second banner for environments that do not bring a mesh.

See [ADR 000001](docs/ADR/000001.md) for the full rationale and rejected alternatives.

## Design tenets

> Safety × portability × self-hostability × operational simplicity **＞** feature breadth × broad privilege × distributed-by-default.

- **Deny-by-default capabilities** — a filter can reach nothing but the host-API explicitly lent to it (log, clock, KV, counter, rate-limit, config). No outbound network, filesystem, or sockets unless granted. Enforced by the Component Model sandbox.
- **Decisions are typed** — a filter returns a `decision` variant: `continue` / `modified` / `short-circuit`. Never an ambiguous flag or an implicit side effect.
- **Init vs per-request** — expensive setup (regex compile, schema build) goes in an `init` hook; the per-request hot path stays lean.
- **Filters are stateless** — rate-limit, session, and cache state live in host KV, so filters pool, scale, and hot-swap cleanly.
- **Fail-closed** — a filter trap or deadline overrun never silently passes traffic through.
- **Single-node first** — one node completes the job; distribution (membership, config consensus) is opt-in.
- **No panics in the data plane** — a single bad request must never take down a worker.

## Architecture

Plecto Proxy is a fast **native highway** plus a **checkpoint where your own code runs**: native Rust
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

**Rule of thumb:** user-specific logic / policy / WAF / auth / rewrite → a WASM filter; TLS / routing / LB / connection pools / global counters → native Rust — a role-driven placement rule fixed in [ADR 000029](docs/ADR/000029.md): native grows only for cross-cutting concerns, never per-request policy. The WASM "tax" (data copy + host-call overhead) hits only request-decision logic, never the speed path — **≈ 1 µs/request** for a pooled filter ([performance](performance/README.md)).

## What the gateway does today

The native fast path has matured well past "a proxy that works." A snapshot of what is implemented and CI-green (each row links the deciding ADR):

| Concern | Today |
| --- | --- |
| **Edge & HTTP** | HTTP/1.1, HTTP/2 (ALPN), HTTP/3 (QUIC, Alt-Svc advertised); TLS termination with SNI cert selection, manifest-declared, fail-closed, on a consolidated **aws-lc-rs** crypto provider with post-quantum X25519MLKEM768 preferred by default and **stateless TLS 1.3 session resumption** (rotated ticket keys, 0-RTT rejected) — [ADR 13–16](docs/ADR/000013.md) · [51](docs/ADR/000051.md) · [52](docs/ADR/000052.md) |
| **Routing & upgrades** | host / path-prefix / method / header / query matching in **specificity order**; weighted **traffic split / canary**; ingress path normalization as a fail-closed auth boundary; per-route **HTTP/1.1 `Upgrade`** tunnelling for WebSocket (`h2c` rejected) — [34](docs/ADR/000034.md) · [48](docs/ADR/000048.md) |
| **Response compression** | per-route **`[route.compression]`** opt-in (deny-by-default): RFC 9110 `Accept-Encoding` negotiation (gzip / br / zstd), content-type allowlist, `no-transform` / 206 / HEAD skips, `Vary` + weak `ETag`, after the response filter chain — [74](docs/ADR/000074.md) · [75](docs/ADR/000075.md). **Do not enable on routes that reflect secrets into the response body** (CSRF tokens, session nonces echoed from the request): compression + reflection enables [BREACH](https://breachattack.com/)-class attacks against TLS. Leave those routes without the block. |
| **Load balancing & upstreams** | **round-robin** (default), **weighted least-request** (P2C), or **weighted Maglev** per upstream; active + passive health checks, outlier detection, circuit breaker, two-tier timeouts, jittered retry; per-upstream **TLS+ALPN re-encryption** (gRPC-ready, with a pinned verification-name **`sni`** override for IP-literal or DNS-expanded endpoints) and **periodic DNS re-resolution** — [17](docs/ADR/000017.md) · [35](docs/ADR/000035.md) · [42](docs/ADR/000042.md) · [44](docs/ADR/000044.md) · [50](docs/ADR/000050.md) |
| **Rate limiting** | **two-tier model** ([ADR 61](docs/ADR/000061.md)): a native L7 token-bucket **local floor** per **route** / **client-IP** (node-local, sheds bursts before they cost a round trip) plus [`filter-ratelimit-redis`](plecto/examples/filters/filter-ratelimit-redis), a reference **global** filter that consults a RESP-compatible store (Redis/Valkey) over the outbound-TCP capability — recommended together, see the [hardening guide](docs/hardening.md) — [33](docs/ADR/000033.md) · [53](docs/ADR/000053.md) · [60](docs/ADR/000060.md) · [66](docs/ADR/000066.md) |
| **Extension plane** | `plecto:filter` chain over headers and, for opted-in filters, the body (header-only filters skip buffering — zero-copy); typed `decision`; trusted **pooled** / untrusted **fresh** instances; deny-by-default host-API with per-filter + host-wide quotas; feature-gated **outbound HTTP** and **outbound TCP** (both SSRF-guarded); a feature-gated **fat-guest** minimal-WASI grant (off by default) unlocks Go/TinyGo filters without widening the zero-WASI default; a `host-config` capability lends filter business settings declared in the manifest — [1](docs/ADR/000001.md) · [25](docs/ADR/000025.md) · [38](docs/ADR/000038.md) · [60](docs/ADR/000060.md) · [63](docs/ADR/000063.md) · [66](docs/ADR/000066.md) |
| **Client IP** | edge-model propagation — re-issues `X-Forwarded-For` / `X-Real-IP` from the real peer before the chain runs — [18](docs/ADR/000018.md) |
| **Supply chain & ops** | cosign + SBOM-verified filter loading; zero-downtime SIGHUP reload + graceful shutdown wired into the shipped binary; W3C trace propagation, RED metrics, OTLP export; `plecto validate` / `schema` / `new-filter` / `dev` / `conformance` / `--version`; Plecto Proxy's own binary and container image carry the same signed-artifact discipline — [6](docs/ADR/000006.md) · [39](docs/ADR/000039.md) · [46](docs/ADR/000046.md) · [47](docs/ADR/000047.md) · [64](docs/ADR/000064.md) · [65](docs/ADR/000065.md) |

## The filter contract

The heart of Plecto Proxy is the `plecto:filter` WIT world — a custom world that defines Plecto Proxy's own vocabulary (the typed `decision`, init/per-request hooks, the deny-by-default host-API) while reusing standard types for polyglot compatibility.

```wit
package plecto:filter@0.3.0;

interface types {
  // Header values are raw bytes (ADR 000071) — not lossy UTF-8 strings.
  record header { name: string, value: list<u8>, }

  // The typed outcome of a request-side filter. Never a bare flag.
  variant request-decision {
    %continue,                       // pass unchanged to the next filter
    modified(request-edit),          // apply the edit, then continue
    short-circuit(http-response),    // stop the chain; synthesise a response now
  }

  // The response side (ADR 000073): `replace` supplants the upstream response with a
  // synthesised one (the upstream body is dropped unread — zero-copy stays intact).
  variant response-decision {
    %continue,
    modified(response-edit),
    replace(http-response),
  }
}

// deny-by-default: one capability per interface; a filter imports only what it is lent.
interface host-kv      { get: func(key: string) -> option<list<u8>>; set: func(key: string, value: list<u8>); /* … */ }
interface host-counter { increment: func(key: string, delta: s64) -> s64; /* atomic named counter */ }
interface host-log     { log: func(level: level, message: string); }
interface host-config  { get: func(key: string) -> option<string>; }  // manifest [filter.config]
// host-ratelimit keeps the token bucket host-native — the hot-path refill/counting never crosses
// the WASM boundary. The bucket spec (capacity/refill) is host-configured in the manifest; the
// filter passes only (key, cost), so an untrusted filter cannot widen its own limit (ADR 000005 / 000026).

// Base contract: header-only filters (auth, rate-limit, WAF, rewrite) target this world. The host
// reads the ABSENCE of `on-request-body` as the signal to skip buffering the body entirely —
// zero-copy passthrough for filters that never touch it (ADR 000038).
world filter {
  import host-log;  import host-clock;  import host-kv;  import host-counter;
  import host-ratelimit;  import host-config;
  export init: func();                                                // heavy, once per instance
  export on-request:  func(req: http-request)  -> request-decision;   // hot path (headers)
  // `req` is the AS-FORWARDED request snapshot (ADR 000073): the request as it left the
  // request-side chain — an auth filter's stamp and the untouched `Origin` both ride it.
  export on-response: func(req: http-request, resp: http-response) -> response-decision;
}

// Body-reading contract: `filter` plus `on-request-body`. Its PRESENCE is what makes the host
// buffer the request body and run this hook (buffer-then-decide, ADR 000025).
world filter-body {
  import host-log;  import host-clock;  import host-kv;  import host-counter;
  import host-ratelimit;  import host-config;
  export init: func();
  export on-request:      func(req: http-request)  -> request-decision;
  export on-request-body: func(body: list<u8>)     -> request-body-decision;  // buffered body hook
  export on-response:     func(req: http-request, resp: http-response) -> response-decision;
}
```

> Current contract is **`plecto:filter@0.3.0`** (byte-valued headers, [ADR 000071](docs/ADR/000071.md); response-side request context + the `replace` decision, [ADR 000073](docs/ADR/000073.md) — the pair that makes a CORS dynamic-origin-echo filter expressible, see `examples/filters/filter-cors`); `0.1.0` / `0.2.0` remain loadable with a deprecation warning. The request-side **body hook** (`on-request-body`, buffered `list<u8>`, [ADR 000025](docs/ADR/000025.md)) runs end-to-end for filters targeting `filter-body`. An **experimental, feature-gated** `stream<u8>` body world ([ADR 000020](docs/ADR/000020.md)) and `wasi:http` type reuse are next, gated on the P3 guest toolchain.
>
> **Contract stability** ([ADR 000064](docs/ADR/000064.md) / [000085](docs/ADR/000085.md)): the host keeps loading every contract version it has shipped support for — `0.1.0` / `0.2.0` load today via frozen trees and load-time adapters, and a superseded major stays accepted for at least two release series before an ADR-declared removal. From contract **1.0** onward the promise hardens: every shipped world stays loadable permanently, with a security-only exception that itself requires a dedicated ADR, ≥ 24 months' notice, and a migration document.

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

    fn on_response(_req: HttpRequest, _resp: HttpResponse) -> ResponseDecision {
        // The one visible thing this filter does: stamp a header so `curl -i` shows a WASM filter
        // touched the response.
        ResponseDecision::Modified(ResponseEdit {
            set_status: None,
            set_headers: vec![Header {
                name: "x-plecto".into(),
                value: b"hello-from-wasm".to_vec(), // list<u8> header values (@0.3.0)
            }],
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

Because the contract is WIT, **any language that compiles to a WASM component can write a filter** — proven two ways. **Tier A (zero-WASI, the default)**: the same conformance subset ported to **MoonBit** (~22 KB), **JavaScript/TypeScript** (ComponentizeJS, ~12 MB engine constant), and **C** (wasi-sdk), each building to a component with **zero WASI imports** that the unchanged deny-by-default host loads and runs through the same assertion suite as the Rust fixture (`plecto/crates/host/tests/polyglot.rs`, CI job `polyglot-guests`). Python fits the same shape (`componentize-py --stub-wasi`, ~17 MB — works, but heavy for a filter). **Tier B (fat guest, feature-gated `fat-guest`, off by default, [ADR 000063](docs/ADR/000063.md))**: languages whose runtime hard-wires a WASI baseline get a fixed, minimal slice — `wasi:io` / `wasi:clocks` / `wasi:random` / `wasi:cli`, still zero filesystem and zero sockets — opt-in per filter via manifest `wasi = "minimal"`. **Go/TinyGo** is the first Tier B guest (`filter-hello-go`); its stdout/stderr bridges into `host-log` so a panic's own diagnostic still reaches the request's trace span, and a dedicated suite (`polyglot_tier_b.rs`, CI job `polyglot-guest-go`) verifies the grant stays fail-closed. Per-language recipes: [Writing a filter §7](docs/writing-a-filter.md#7-other-languages); polyglot filter **SDKs** (scaffolding beyond one fixture per language) remain on the [roadmap](#roadmap).

Fastest start: `plecto new-filter --lang rust my-filter` — scaffolds the crate, fetches the WIT contract via `wkg` ([ADR 000064](docs/ADR/000064.md)), and writes a ready-to-run dev manifest ([ADR 000065](docs/ADR/000065.md); [ADR 000072](docs/ADR/000072.md) accepts offline self-vendoring as the follow-on). Full how-to — scaffold, build, manifest fields, signing, local testing — is in [**Writing a filter**](docs/writing-a-filter.md). A copy-ready in-tree starter also lives in [`examples/filters/filter-template`](plecto/examples/filters/filter-template).

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

### Prebuilt binaries & container images

Every tagged release ships prebuilt artifacts in two **named runtime capability profiles**
([ADR 000079](docs/ADR/000079.md)):

| Profile | Binary / image tag | What is compiled in |
| --- | --- | --- |
| **minimal** (unsuffixed, the default) | `plecto-<tag>-<target>.tar.gz` · `ghcr.io/kaikei-e/plecto:<version>` | Default features only — no outbound code is compiled in. The smallest attack surface; pick this for a plain reverse proxy / gateway. |
| **capabilities** | `plecto-<tag>-<target>-capabilities.tar.gz` · `ghcr.io/kaikei-e/plecto:<version>-capabilities` | Adds the `outbound-http`, `outbound-tcp`, and `fat-guest` capabilities — what the capability-backed reference filters (JWKS-refreshing JWT auth, ext-authz, the Redis-backed global rate limit) and TinyGo/Go guests need. |

**Compiling a capability in is not granting it.** A capabilities binary lends nothing to any
filter until the manifest declares that capability for that filter — the deny-by-default
allowlist and SSRF floor apply unchanged ([ADR 000036](docs/ADR/000036.md) /
[ADR 000060](docs/ADR/000060.md)). `plecto --version` prints which profile a binary was
compiled as.

Both profiles ride the same supply-chain discipline — cargo-auditable build, SPDX SBOM, cosign
keyless signature, and per-profile image digests recorded in the release notes
([ADR 000047](docs/ADR/000047.md)); the verification commands are in each release's notes and
in [`release.yml`](.github/workflows/release.yml)'s header comment.

The **reference filters** ship the same way, as separate artifacts: each release publishes
`filters/jwt`, `filters/cors`, `filters/apikey`, and `filters/extauthz` as individually
cosign-signed CNCF Wasm OCI Artifacts with SPDX SBOM attestations under
`ghcr.io/kaikei-e/plecto/filters/<name>` ([ADR 000080](docs/ADR/000080.md)). Which filter needs
which runtime profile — and the verify-then-load recipe — is in
[docs/reference-filters.md](docs/reference-filters.md).

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

Beyond the single-process demos, [`examples/multi-replica/`](plecto/examples/multi-replica/README.md) is a **docker-compose reference** — an L4 load balancer speaking PROXY protocol v2 to two Plecto replicas — whose scripts *prove* the operational properties: drain one replica with zero dropped requests, TLS resumption surviving replica hops (shared STEK), and downstream mTLS ([ADR 000088](docs/ADR/000088.md)).

The benchmark harnesses (`bench-server`, `swap-bench`) are not demos — they live under [`bench/harnesses/`](bench/) and produce the numbers in [performance](performance/README.md).

## Roadmap

Plecto Proxy is built ADR-first, milestone by milestone. The full detail — landed items, what's next, and the deciding ADR for each — lives in [`docs/ROADMAP.md`](docs/ROADMAP.md); here's the snapshot:

| Milestone | Status | Covers |
| --- | --- | --- |
| **M0** — Foundation | ✅ done | `plecto:filter@0.3.0` contract (0.1 / 0.2 frozen + load adapters), wasmtime host, capability boundary, CI |
| **M1** — Filter runtime hardening | ✅ landed | trusted pool / untrusted fresh-per-request, redb KV, host-native rate limiting, quotas |
| **M2** — The data path (fast path) | 🚧 maturing | HTTP/1–3 + TLS, routing / LB / resilience, upstream TLS + periodic DNS re-resolve, WebSocket tunnelling |
| **M3** — Async & bodies | 🚧 Stages 1–2 landed | wasmtime-46 async, header/body-world split, buffer-then-decide body hook; `stream<u8>` is experimental |
| **M4** — Provenance & zero-downtime reload | ✅ landed | OCI + cosign + SBOM filter loading, SIGHUP reload + graceful shutdown, signed releases of Plecto Proxy itself |
| **M5** — Observability & opt-in distribution | 🚧 mostly landed | W3C trace propagation, RED metrics, OTLP export landed; opt-in config consensus deferred |
| **M6** — Polyglot SDKs & reference filters | 🚧 examples landed | Tier A zero-WASI example filters (MoonBit/JS/C) + Tier B fat-guest Go/TinyGo ([63](docs/ADR/000063.md)), each verified by its own CI-gated conformance suite; SSRF-guarded outbound HTTP and outbound TCP (both feature-gated), with `filter-ratelimit-redis` as a real-world reference filter; polyglot SDKs still pending |

## Project layout

```
.
├── plecto/                    # Rust workspace (the native half)
│   ├── wit/world.wit          # the plecto:filter contract (contract-first)
│   ├── deny.toml              # cargo-deny supply-chain policy (CI-blocking)
│   ├── crates/
│   │   ├── host/              # wasmtime embedding: Linker, InstancePre, host-API (+ CONTEXT.md)
│   │   ├── control/           # control plane: manifest, OCI load, chain, reload, TLS/QUIC (+ CONTEXT.md)
│   │   ├── server/            # fast path: HTTP/1.1·2 (hyper) + HTTP/3 (quinn), routing, LB, upstream (+ CONTEXT.md)
│   │   └── plecto/            # the `plecto` binary + operator CLI (validate/conformance/new-filter/dev/schema, + CONTEXT.md)
│   └── examples/              # runnable demos + example filter guests — see examples/README.md (the DX map)
│       ├── README.md          # the guided learning path + the full filter guest catalog (canonical list)
│       ├── <use-case>/        # nine demos: cargo run -p plecto-server --example <name>
│       └── filters/           # example plecto:filter guests (own workspace, componentized by build.rs) —
│                               # see examples/README.md for the current, full list (a starter, a
│                               # real-world example, conformance fixtures across languages, and
│                               # feature-gated references)
├── bench/                     # benchmark harnesses + runbook (k6/oha; harnesses/, filters/, perf/)
├── performance/              # the benchmark write-up + results (see performance/README.md)
├── docs/ADR/                  # Architecture Decision Records
├── CHANGELOG.md               # Keep a Changelog + pre-1.0 versioning policy
├── CLAUDE.md                  # project conventions & design summary
├── CONTEXT-MAP.md             # domain glossary map (split per context)
└── Dockerfile                 # reference multi-stage build (distroless runtime)
```

## Design decisions

Plecto Proxy records every load-bearing decision as an ADR in the Fork form (*decision / rationale / re-examination condition*). All accepted ADRs live in [`docs/ADR/`](docs/ADR/) — start at [ADR 000001](docs/ADR/000001.md) (the two complementary halves); each cross-links the decisions it builds on.

What gets verified, and where, is mapped in one page: [docs/verification.md](docs/verification.md) — a map to the CI/release machinery whose record is the workflows themselves being green, not a separate ledger ([ADR 000086](docs/ADR/000086.md)).

Two of those decisions are commitments to you rather than to the code. The **contract compatibility promise** is staged: the host keeps loading every `plecto:filter` version it has shipped, and from contract 1.0 every shipped world stays loadable permanently — security-only exception, via a dedicated ADR plus ≥ 24 months' notice ([ADR 000085](docs/ADR/000085.md)). And the **longevity discipline** ([ADR 000086](docs/ADR/000086.md)): no year-number support pledge — instead a declared intent to maintain long-term, a retirement protocol (≥ 12 months' EOL notice with continued security fixes, should development ever be deliberately wound down), and reproducible signed releases (source, signed artifacts, SBOM attestation, pinned dependencies) so every tagged release stays verifiable and forkable even without its maintainer.

## Contributing

Contributions are deliberate: please **agree an approach in an issue or [Discussion](https://github.com/Kaikei-e/PlectoProxy/discussions) before opening a PR** (unsolicited PRs may be closed). Plecto Proxy follows outside-in TDD (E2E → WIT-conformance → unit) and records load-bearing decisions as ADRs. See [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide — including the areas that need extra care and DCO sign-off — and [CLAUDE.md](CLAUDE.md) for conventions. Local CI parity before a PR:

```bash
cd plecto
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

(or just `just check` from the repository root.)

## License

Licensed under the **Apache License, Version 2.0** — see [LICENSE](LICENSE). The Apache-2.0 patent grant suits an infrastructure project and is widely used across the cloud-native and container ecosystems.

## Prior art & acknowledgements

Plecto Proxy builds on the [Bytecode Alliance](https://bytecodealliance.org/) stack — [wasmtime](https://wasmtime.dev/), [WIT, and the Component Model](https://component-model.bytecodealliance.org/) — and on a decade of industry work that showed in-process WASM can carry data-plane policy. Positioning relative to other extension models is recorded in [ADR 000067](docs/ADR/000067.md) (by model type, not by product name).

The PROXY protocol implemented by the listener ([ADR 000057](docs/ADR/000057.md)) is the public specification maintained by HAProxy Technologies; the multi-replica reference uses HAProxy as its example L4 load balancer. HAProxy is a trademark of HAProxy Technologies — this project is not affiliated with or endorsed by them.
