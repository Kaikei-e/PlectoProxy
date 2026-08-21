# Plecto Proxy

[![crates.io](https://img.shields.io/crates/v/plecto.svg)](https://crates.io/crates/plecto)
[![docs.rs](https://img.shields.io/docsrs/plecto)](https://docs.rs/plecto)
[![CI](https://github.com/Kaikei-e/PlectoProxy/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Kaikei-e/PlectoProxy/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/plecto.svg)](https://github.com/Kaikei-e/PlectoProxy/blob/main/LICENSE)

A self-hostable, programmable L7 reverse proxy & API gateway — in Rust, extended with WebAssembly.

Plecto Proxy pairs two complementary halves through a typed [WIT](https://component-model.bytecodealliance.org/) contract:

- a **fast path** in native Rust — connection handling, TLS termination, HTTP/1.1·2·3, routing, load balancing, and upstream management;
- an **extension plane** of **WebAssembly Component Model filters** — the per-request decisions (auth, header/body rewriting, rate limiting, WAF, policy) written in any language, plugged in over the `plecto:filter` contract, hot-swapped with zero downtime.

The speed-critical path stays native Rust. Filter logic runs as a sandboxed WASM component that can touch only the capabilities the host explicitly lends it.

> **Status: early development.** APIs and the `plecto:filter` contract may still change between releases.

## Install

```bash
cargo install plecto
```

This installs the `plecto` binary — the gateway itself (`plecto <manifest.toml> [listen-addr]`)
plus the operator CLI: `new-filter` and `dev` to author a filter, `conformance` → `package` →
`validate --resolve` to gate, sign, and pre-flight it, and `healthz` / `schema` / `--version` for
operations.

A `cargo install` build carries no release provenance. For a deployment, prefer the signed
container image or release binary — see
[docs/install.md](https://github.com/Kaikei-e/PlectoProxy/blob/main/docs/install.md).

## Quick start

Plecto is configured by one declarative TOML manifest. Listen on a port, forward everything to
a backend:

```toml
# plecto.toml
[listen]
addr = "127.0.0.1:8080"

[[upstream]]
name = "backend"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/"
interval_ms = 1000

[[route]]
upstream = "backend"
[route.match]
path_prefix = "/"
```

```bash
plecto plecto.toml         # serve; SIGHUP hot-reloads the manifest without downtime
curl -i http://127.0.0.1:8080/
```

Adding a WASM filter is one more `[[filter]]` block pinning a signed OCI artifact by digest, and
a `filters = [...]` list on the route — the
[quickstart](https://github.com/Kaikei-e/PlectoProxy/tree/main/docs/quickstart) walks the whole
path, filter included.

## This workspace

This crate is one member of the Plecto Proxy Cargo workspace (all members version in lockstep):

- [`plecto`](https://docs.rs/plecto) — the `plecto` binary and operator CLI. `cargo install plecto` is the primary entry point.
- [`plecto-host`](https://docs.rs/plecto-host) — the wasmtime embedding host that loads, sandboxes, and runs `plecto:filter` WASM components; also home of the versioned conformance battery (`run_conformance` / `run_conformance_with`, five-way per-case verdicts).
- [`plecto-control`](https://docs.rs/plecto-control) — the control plane: declarative manifest, OCI artifact loading, filter-chain dispatch, atomic hot reload.
- [`plecto-server`](https://docs.rs/plecto-server) — the fast path data plane library (HTTP/1.1, HTTP/2, HTTP/3, TLS, routing, load balancing).

## Versioning and upgrades

- The proxy and the `plecto:filter` WIT contract are **two independent version series**:
  **upgrading the proxy never requires rebuilding a deployed filter** — the host keeps loading
  every contract version it has shipped support for. Details:
  [upgrading — two independent version series](https://github.com/Kaikei-e/PlectoProxy/blob/main/README.md#upgrading-two-independent-version-series).
- Release notes live in the
  [CHANGELOG](https://github.com/Kaikei-e/PlectoProxy/blob/main/CHANGELOG.md); git tags mark
  every published release.
- **MSRV**: Rust 1.97 (declared as `rust-version` in `Cargo.toml`).

## Links

- Repository & full documentation: <https://github.com/Kaikei-e/PlectoProxy>
- Quickstart: <https://github.com/Kaikei-e/PlectoProxy/tree/main/docs/quickstart>
- Design principles & ADRs: <https://github.com/Kaikei-e/PlectoProxy/tree/main/docs>

## License

Apache-2.0. See [LICENSE](https://github.com/Kaikei-e/PlectoProxy/blob/main/LICENSE).
