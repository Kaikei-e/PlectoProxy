# What the gateway does today

[日本語](features.ja.md)

A snapshot of what is **implemented and CI-green** in Plecto Proxy, concern by concern. Each row
links the deciding ADR, so a claim here is always one click from the decision that produced it and
the conditions under which it would be re-examined. The [README](../README.md) carries only the
one-paragraph summary; this page is the detail behind it, and the
[roadmap](ROADMAP.md) is what is *not* here yet.

| Concern | Today |
| --- | --- |
| **Edge & HTTP** | HTTP/1.1, HTTP/2 (ALPN), HTTP/3 (QUIC, Alt-Svc advertised); TLS termination with SNI cert selection, manifest-declared, fail-closed, on a consolidated **aws-lc-rs** crypto provider with post-quantum X25519MLKEM768 preferred by default and **stateless TLS 1.3 session resumption** (rotated ticket keys, 0-RTT rejected); **mutual TLS in both directions** — `[listen.client_auth]` requires a verified client certificate on every terminated handshake (h1/h2/h3 alike), `[upstream.tls]` presents a client identity on every upstream leg, health probes included; opt-in **PROXY protocol v2** reception (trusted-CIDR-required, fail-closed) behind a fronting L4 load balancer — [ADR 13–16](ADR/000013.md) · [51](ADR/000051.md) · [52](ADR/000052.md) · [57](ADR/000057.md) · [62](ADR/000062.md) · [78](ADR/000078.md) |
| **Inbound admission control** | A global connection cap **plus** a per-source-IP cap enforced in both accept loops (TCP after PROXY v2 resolution — so a fronting LB is not itself the capped source — QUIC on the peer address; IPv6 rounded to /64), backed by a fixed-size table so a spoofed-source flood cannot grow it; header-read timeout, body-read deadline, body-buffer budget; `RLIMIT_NOFILE` raised soft→hard at startup, warning when the hard ceiling is still below what the connection cap needs — [27](ADR/000027.md) · [92](ADR/000092.md) |
| **Routing & upgrades** | host / path-prefix / method / header / query matching in **specificity order**; weighted **traffic split / canary**; ingress path normalization as a fail-closed auth boundary (encoded separators and dot-escapes rejected, so a per-route filter is a reliable auth boundary); per-route **HTTP/1.1 `Upgrade`** tunnelling for WebSocket (`h2c` rejected at validation) — [27](ADR/000027.md) · [34](ADR/000034.md) · [48](ADR/000048.md) |
| **Response compression** | per-route **`[route.compression]`** opt-in (deny-by-default): RFC 9110 `Accept-Encoding` negotiation (gzip / br / zstd), content-type allowlist, `no-transform` / 206 / HEAD skips, `Vary` + weak `ETag`, applied after the response filter chain — [74](ADR/000074.md) · [75](ADR/000075.md). **Do not enable it on routes that reflect secrets into the response body** (CSRF tokens, session nonces echoed from the request): compression plus reflection enables [BREACH](https://breachattack.com/)-class attacks against TLS. Leave those routes without the block. |
| **Load balancing & upstreams** | **round-robin** (default), **weighted least-request** (P2C), or **weighted Maglev** consistent hashing per upstream; active + passive health checks (pessimistic start, fail-closed 503 when none are healthy), outlier detection, per-upstream circuit breaker, two-tier (per-try + overall) timeouts a route can override for its own traffic, bounded jittered retry; per-upstream **TLS+ALPN re-encryption** (gRPC-ready — `TE: trailers` passes through — with a pinned verification-name **`sni`** override for IP-literal or DNS-expanded endpoints) and **periodic DNS re-resolution**, so hostname upstreams track container churn — [17](ADR/000017.md) · [28](ADR/000028.md) · [30–32](ADR/000030.md) · [35](ADR/000035.md) · [42](ADR/000042.md) · [44](ADR/000044.md) · [50](ADR/000050.md) · [102](ADR/000102.md) |
| **Rate limiting** | a **two-tier model** ([61](ADR/000061.md)): a native L7 token-bucket **local floor** per **route** / **client-IP** (node-local, sheds bursts before they cost a round trip) plus [`filter-ratelimit-redis`](../plecto/examples/filters/filter-ratelimit-redis), a reference **global** filter that consults a RESP-compatible store over the outbound-TCP capability. Recommended together — the sizing formulas and the demo-only caveat on the reference filter are in the [hardening guide](hardening.md) — [33](ADR/000033.md) · [53](ADR/000053.md) · [60](ADR/000060.md) · [66](ADR/000066.md) · [81](ADR/000081.md) |
| **Extension plane** | the `plecto:filter` chain over headers and, for filters that opt in, the buffered request body (header-only filters skip buffering entirely — zero-copy); typed `decision`; trusted **pooled** / untrusted **fresh-per-request** instances; deny-by-default host-API bounded by per-filter and host-wide quotas; feature-gated **outbound HTTP** and **outbound TCP** (both SSRF-guarded, IP-pinned); a feature-gated **fat-guest** minimal-WASI grant (off by default) that unlocks Go/TinyGo filters without widening the zero-WASI default; a `host-config` capability lending manifest-declared business settings, with **`[filter.config_files]`** resolving a value from a file at load and reload so a secret arrives by mount rather than inlined in the manifest (fail-closed on key collision, missing file, non-UTF-8, or > 1 MiB) — [1](ADR/000001.md) · [25](ADR/000025.md) · [36](ADR/000036.md) · [38](ADR/000038.md) · [60](ADR/000060.md) · [63](ADR/000063.md) · [66](ADR/000066.md) · [95](ADR/000095.md) |
| **Client IP** | edge-model propagation — `X-Forwarded-For` / `X-Real-IP` re-issued from the real peer before the chain runs, so a filter cannot be fooled by a client-supplied header — [18](ADR/000018.md) · [22](ADR/000022.md) |
| **Observability** | host-propagated W3C trace context (an inbound `traceparent` is continued through the proxy), one span per filter execution over the OpenTelemetry data model, host-aggregated RED metrics on the admin `/metrics` endpoint, and OTLP network export via a host-side batch/retry pump that never touches the guest contract — [7](ADR/000007.md) · [9](ADR/000009.md) · [40](ADR/000040.md) |
| **Process lifecycle** | zero-downtime SIGHUP reload (content-hash reconciled, atomic, all-or-nothing, fail-closed on a broken edit) and a graceful shutdown contract a load balancer can rely on — `/readyz` flips first, a configurable readiness grace elapses, then the drain runs under one bounded window. The [operations guide](operations.md) is the full contract — [39](ADR/000039.md) · [59](ADR/000059.md) |
| **Supply chain** | cosign + SBOM-verified filter loading (digest-pinned offline OCI layout, SBOM bound to the component by in-toto subject digest, no unsigned-load escape hatch); an operator CLI covering the whole filter lifecycle — `conformance` → `package` (sign, write the layout, print the digest to pin) → `validate --resolve` (the same provenance gate the loader runs, as a CI pre-flight) — plus `new-filter` / `dev` / `healthz` / `schema` / `--version`; Plecto Proxy's own binaries, container images, and reference filters carry the same discipline — [6](ADR/000006.md) · [46](ADR/000046.md) · [47](ADR/000047.md) · [64](ADR/000064.md) · [65](ADR/000065.md) · [80](ADR/000080.md) · [94](ADR/000094.md) |

## Deliberately not here

Absence is a decision too, and each one is recorded rather than left as a silent gap:

- **Response caching** and a **native AI/LLM gateway** are declined for the native fast path
  ([ADR 000043](ADR/000043.md)) — they are per-request policy, which the role-driven placement rule
  ([ADR 000029](ADR/000029.md)) puts in the extension plane.
- **WAF** is placed in the extension plane on purpose, not built native ([ADR 000037](ADR/000037.md)).
- **Cross-replica shared state** is declined in the native path ([ADR 000053](ADR/000053.md)): all
  host-held state is node-local, and a fleet-wide limit is expressed as a filter consulting an
  external store. The [hardening guide](hardening.md) is the operational half of that decision.
- **Dynamic config push** (an xDS-style control-plane protocol) is not adopted; the manifest plus
  SIGHUP is the single source of truth ([ADR 000008](ADR/000008.md)).
- **EWMA / latency-aware load balancing**, **ring-hash**, and **header-presence / regex route
  matching** are not declined — just not built yet. See the [roadmap](ROADMAP.md).

## How these claims are verified

Every row above corresponds to tests in the repository, and the record that they hold is the
workflow being green rather than a ledger on this page. [verification.md](verification.md) maps
what runs where; [performance/](../performance/README.md) carries the measured numbers, with the
node-local bounds on any fairness or enforcement claim stated in the
[hardening guide](hardening.md).
