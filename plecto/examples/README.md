# Plecto examples

Runnable, use-case-focused demos — a guided path from a 5-minute hello up through the real things a
gateway does. Each is one self-contained file that spins up the proxy **plus** its in-process
upstreams, prints exactly what to `curl`, and cleans up on exit. Every demo loads its filter through
the **production path** (cosign signature + SBOM verification, fail-closed), so nothing here is a toy
shortcut.

Run any of them with:

```bash
cargo run -p plecto-server --example <name>
```

Each example directory has its own README with the exact `curl`s and their expected
output; `./examples/try.sh <name>` (or `just demo <name>`) runs the whole scenario for you.

## Learning path

Start at the top and work down — each step adds one concept. 1–3 are the **extension
plane** (what a WASM filter is), 4–8 are the **native fast path** (what the gateway does
around it), 9 puts it all in one real deployment.

| # | Example | What you learn |
|---|---------|----------------|
| 1 | **`quickstart`** | The 5-minute hello: a sandboxed WASM filter stamps a header on your response. |
| 2 | **`wasm-auth`** | A *real* filter doing real work — API-key authentication, host-held state (KV), and the typed `decision` (`continue` / `modified` / `short-circuit` 401). Plecto's thesis in one file. |
| 3 | **`filter-chain`** | Compose filters: how a request flows through the chain, each hook's typed decision, and host-native rate limiting. |
| 4 | **`load-balancing`** | The native fast path: one upstream over three instances, round-robin + active health checks, and **fail-closed** ejection/recovery (a total outage → 503, no client errors). |
| 5 | **`tls-http`** | TLS termination (rustls): HTTP/1.1, HTTP/2 (ALPN), and **HTTP/3 over QUIC** on one port, with `/api/*` routing. |
| 6 | **`hot-reload`** | Zero-downtime config swap: edit the manifest, `kill -HUP`, and watch it take effect atomically — a broken edit stays fail-closed (the proxy never drops). |
| 7 | **`canary`** | A rollout you can operate: a 90/10 weighted split, a header-match route for internal testers, and a zero-downtime drain/promote via SIGHUP (ADR 000034). |
| 8 | **`resilience`** | The failure axes, each visible from curl: per-try timeout + retry to another instance, the overall deadline (504), the circuit breaker (503 `circuit-open`), and silent outlier ejection (ADR 000023/000028/000031/000032). |
| 9 | **`production`** | The shape you operate: the **real `plecto` binary** serving a deploy dir (manifest + trust root + signed OCI layout), with `least_request` LB, a native rate-limit floor, and the `/metrics` admin endpoint (two terminals). |

**Advanced (feature-gated).** The outbound **ext_authz** capability (ADR 000036, `--features
outbound-http`) and the **streaming body** filter (`--features streaming-body`) are exercised today by
the host test suite and their guest crates (`filters/filter-extauthz`, `filters/filter-streaming`);
dedicated server demos are a follow-up.

## Write your own filter

[`filters/filter-template`](filters/filter-template/) is a self-contained starting point (the WIT
contract is vendored, so it builds anywhere). Copy it, or `cargo generate` it — see its README.

## The filter guests (`filters/`)

The WASM components the demos load. Each is its own workspace, built for `wasm32-unknown-unknown` and
componentized by `crates/host/build.rs` (ADR 000010).

| Guest | Role |
|-------|------|
| `filter-quickstart` | Minimal starter — stamps one response header (behind `quickstart`). |
| `filter-apikey` | Real-world API-key auth gate (behind `wasm-auth`). |
| `filter-hello` | The **conformance fixture** the host tests load — exercises every host-API. Not a starter (it does everything on purpose). |
| `filter-template` | Scaffold for your own filter. |
| `filter-streaming` | Streaming `stream<u8>` body filter (feature-gated). |
| `filter-extauthz` | ext_authz over the SSRF-guarded outbound capability (feature-gated, ADR 000036). |

## Not here: benchmarks

The performance harnesses are **not** demos — they live under [`bench/`](../../bench/) (the runbook
and load scenarios) and produce the numbers in [`performance/`](../../performance/README.md). Keep
`examples/` for learning; go to `bench/` to measure.
