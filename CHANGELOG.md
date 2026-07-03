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

### Added

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
