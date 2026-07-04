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
