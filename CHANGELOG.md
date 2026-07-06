# Changelog

All notable changes to Plecto are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## Versioning policy (pre-1.0)

- **Binary / manifest**: while the version is `0.x`, a **minor** bump (`0.1 → 0.2`) may contain
  breaking changes to the manifest schema or CLI; they are always listed under **Changed** /
  **Removed** with a migration note. Patch bumps are always safe to take.
- **WIT contract**: the filter contract is versioned independently as `plecto:filter@<version>`.
  A manifest declares which contract its filters target; the host keeps loading every contract
  version it ships support for, so a proxy upgrade never silently breaks deployed filters.
- **Release artifacts**: binaries and images are cosign-signed (keyless) with SBOMs attached —
  the same supply-chain bar Plecto's own filter loading enforces. Verify commands are in the
  release notes of each release.

## [Unreleased]

## [0.1.4] - 2026-07-06

### Added

- PROXY protocol v2 reception (ADR 000057), opt-in per listener via `[listen.proxy_protocol]`
  with a required trusted-CIDR list: a v2 header arriving from a trusted load balancer restores
  the real client IP end to end (including before a TLS handshake), feeding the edge client-IP
  model, rate limiting and access logs. A missing, malformed or untrusted header cuts the
  connection fail-closed; traffic from peers outside the trusted CIDRs passes through unchanged.
- Polyglot filter examples proving the any-language claim: MoonBit, JavaScript (ComponentizeJS)
  and C (wasi-sdk) guests, each built to a zero-WASI header-only component and verified by the
  same conformance assertions as the Rust fixture (the `polyglot-conformance` test suite).
- Fuzzing scaffold: cargo-fuzz, with a first target on the PROXY protocol v2 parser.

### Changed

- Buffered request bodies now count as replayable for upstream retries (ADR 000058). On a
  `filter-body` route the body is already fully buffered, so a retry re-sends it instead of
  giving up: a connect failure (the upstream never received the request) retries for any
  method, a per-try timeout or gateway-class 5xx (502–504) retries idempotent methods only —
  the retry decision table itself is unchanged. Re-sends share one reference-counted buffer
  (no memory copy), stay inside the existing bounded-retry budget (max retries, jittered
  backoff, overall deadline), and the streaming (non-buffered) path behaves exactly as before.

## [0.1.3] - 2026-07-06

### Fixed

- Filter state quotas: `KvQuota`'s read-decide-write accounting is striped across 64
  hash-picked per-key locks (stripe seed per instance, so a tenant cannot precompute keys that
  pile onto another tenant's stripe) — one stalled `charge_and_apply` (e.g. a slow persistent
  write) no longer blocks unrelated keys. The namespace/global tallies moved to their own lock
  whose critical section is pure arithmetic: no backend I/O ever runs under a shared lock.
  Same-key atomicity (the accounting-race fix from 0.1.2) is preserved.

## [0.1.2] - 2026-07-06

### Added

- Stateless TLS 1.3 session resumption (ADR 000052): RFC 5077-style self-encrypted session
  tickets from one process-lifetime ticketer (6 h key rotation / 12 h acceptance window),
  shared by the TCP and QUIC server configs and across manifest reloads — a reload never
  invalidates outstanding tickets, per-session server memory is zero, and 0-RTT stays
  rejected.
- `plecto-loadgen tls`: full-handshake vs resumed-handshake benchmark rungs for the TLS
  termination path.

### Fixed

- server: a request-body buffer-permit acquisition error now fails closed (503) instead of
  silently proceeding without a permit (a latent bypass of the buffered-body concurrency cap);
  the admin (metrics/health) listener gained the same connection cap and header-read hardening
  the data-plane listener already had.
- control: closed a TOCTOU race in outlier detection where two instances crossing their
  failure threshold in the same instant could both eject and exceed `max_ejection_percent`;
  cut a per-request heap allocation and repeated per-request filter-list resolution on the
  routing hot path.
- host: per-filter quota accounting (`host-kv` / `host-counter` / `host-ratelimit`) is atomic
  under concurrency, closing a race where concurrent same-key calls could double-charge or
  double-release budget and drift the quota cap; the untrusted filter lifecycle gained a
  per-filter circuit breaker so a deterministically failing init stops re-paying its full init
  budget on every request; the in-memory trace sink's retained spans are bounded (FIFO
  eviction).

## [0.1.1] - 2026-07-04

### Added

- `[upstream.tls] sni` (ADR 000050): pins the TLS verification name for a forwarded upstream leg
  independently of the connected address — closes the gap where an IP-literal or DNS-expanded
  (`resolve_interval_ms`, ADR 000044) upstream address sends no SNI and is verified against the
  bare IP, which fails unless the certificate carries an IP SAN. `plecto validate` warns (never
  rejects) when `sni` is absent on an upstream that may resolve to a bare IP.

### Changed

- TLS crypto provider consolidated on `aws-lc-rs` (ADR 000051), replacing `ring`, across
  downstream TLS termination, upstream re-encryption, and QUIC/HTTP-3. `sigstore` (cosign
  signature verification, ADR 000006 / 000047) already links aws-lc-rs unconditionally, so this
  removes a second crypto backend rather than adding a new dependency, and gets X25519MLKEM768
  preferred by default (rustls `prefer-post-quantum`) on both the TCP and QUIC paths.

## [0.1.0] - 2026-07-03

The first tagged release. Everything below ships in `v0.1.0`; the highlights of the
pre-release history are summarised first, the final pre-tag additions follow.

### Highlights (initial release)

- **Fast path**: HTTP/1.1, HTTP/2 (TLS+ALPN), HTTP/3 (QUIC, same port, Alt-Svc advertised);
  rustls TLS termination with SNI selection and certificate hot reload.
- **Routing**: host / path-prefix / method / header / query matching (most-specific wins),
  weighted traffic splits (canary), prefix strip, fail-closed ingress path normalization.
- **Resilience**: round-robin / weighted least-request (P2C) / weighted Maglev load balancing,
  active + passive health checks (pessimistic start), outlier detection, per-upstream circuit
  breaker, two-tier timeouts (per-try + overall), jittered bounded retries, native per-route
  rate limiting.
- **Extension plane**: `plecto:filter` WASM Component Model filters (any language), pooled
  instances, deny-by-default capabilities (log / clock / KV / counter / rate-limit /
  outbound-HTTP with SSRF guard), per-filter quotas and deadlines, cosign + SBOM
  verify-then-load, fail-closed trap handling.
- **Operations**: declarative TOML manifest (strict parse), SIGHUP hot reload + graceful
  shutdown, Prometheus metrics + health/readiness admin endpoint, structured JSON logs,
  opt-in access log, OTLP trace export, redb persistent filter state.

### Added

- HTTP/1.1 Upgrade / WebSocket tunnelling (`[route.upgrade]`, ADR 000048): a per-route token
  allowlist (the h2c-smuggling mitigation shape; `h2c` is rejected at validation) re-issues the
  handshake upstream and splices a bidirectional tunnel on a verified 101, with an
  activity-reset idle timeout (default 5 min, `0` disables) and drain-aware shutdown.
- `plecto schema`: the manifest's JSON Schema (draft-07) on stdout, derived from the parsing
  structs themselves — editor completion (taplo / Even Better TOML) and CI validation from one
  generated artifact (ADR 000049).
- Upstream TLS re-encryption (`[upstream.tls]`, ADR 000042): per-upstream rustls client with
  ALPN-negotiated HTTP/2 / HTTP/1.1, optional custom CA (`ca_path`), `TE: trailers` pass-through
  and response-trailer forwarding — gRPC now works end-to-end through the proxy. Health probes
  follow the upstream scheme; certificate verification has no off switch (fail-closed).
- `plecto validate <manifest>` (the `nginx -t` shape): static manifest validation for CI and
  pre-reload checks — strict parse plus every fail-closed startup check that needs no artifact —
  and `plecto --version`.
- `[listen]` manifest section: the data-plane bind address (`addr`) and the Alt-Svc h3
  advertisement port (`advertised_port`) are declared in the manifest, fixing container
  deployments (`0.0.0.0` binds; internal-vs-published port mismatch).
- Periodic DNS re-resolution (`resolve_interval_ms` on `[[upstream]]`): each A/AAAA record a
  hostname resolves to becomes a load-balancing endpoint with its own health, refreshed on an
  interval — Compose service names and k8s headless Services now track container re-creation.
- Release engineering: reference `Dockerfile` (distroless runtime), tag-triggered release
  workflow producing signed binaries (cosign keyless + SBOM) and a signed multi-arch GHCR image.
