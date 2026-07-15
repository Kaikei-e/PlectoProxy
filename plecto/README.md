# Plecto Proxy

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

This installs the `plecto` binary — the operator CLI (`plecto new-filter`, `plecto dev`,
`plecto validate`, `plecto conformance`, `plecto schema`) plus the gateway itself
(`plecto <manifest.toml> <listen-addr>`).

## This workspace

This crate is one member of the Plecto Proxy Cargo workspace:

- [`plecto`](https://docs.rs/plecto) — the `plecto` binary and operator CLI. `cargo install plecto` is the primary entry point.
- [`plecto-host`](https://docs.rs/plecto-host) — the wasmtime embedding host that loads, sandboxes, and runs `plecto:filter` WASM components.
- [`plecto-control`](https://docs.rs/plecto-control) — the control plane: declarative manifest, OCI artifact loading, filter-chain dispatch, atomic hot reload.
- [`plecto-server`](https://docs.rs/plecto-server) — the fast path data plane library (HTTP/1.1, HTTP/2, HTTP/3, TLS, routing, load balancing).

## Links

- Repository & full documentation: <https://github.com/Kaikei-e/PlectoProxy>
- Quickstart: <https://github.com/Kaikei-e/PlectoProxy/tree/main/docs/quickstart>
- Design principles & ADRs: <https://github.com/Kaikei-e/PlectoProxy/tree/main/docs>

## License

Apache-2.0. See [LICENSE](https://github.com/Kaikei-e/PlectoProxy/blob/main/LICENSE).
