# Writing a filter

A **filter** is your request logic, running as a sandboxed WebAssembly component. Plecto's native
fast path handles connections, TLS, HTTP, routing, and load balancing; it hands each request to your
filter, which **inspects it and returns one typed decision**. This guide takes you from an empty
directory to a running filter.

New to the model? Read the [README](../README.md) first — the architecture, the three decisions, and
the trusted/untrusted execution split. This guide is the practical how-to.

## 1. The contract in one minute

A filter implements one of two `plecto:filter` worlds (see
[`plecto/wit/world.wit`](../plecto/wit/world.wit)): the header-only `filter` world, or `filter-body`
if it also needs the request body. `filter-body` is `filter` plus one export — the **absence** of
that export in the base world is itself the signal the host uses to skip buffering the body and
stream it straight through, at no cost to a header-only filter (ADR 000038). Target `filter-body`
only when your filter actually reads the body.

| Export | World | When it runs | Returns |
| --- | --- | --- | --- |
| `init` | both | once per instance (heavy setup) | — |
| `on-request` | both | per request, on the headers | `continue` / `modified(edit)` / `short-circuit(response)` |
| `on-request-body` | `filter-body` only | per request, on the buffered body | `continue(body)` / `short-circuit(response)` |
| `on-response` | both | per response, on the headers | `continue` / `modified(edit)` |

A filter is **stateless**. Anything it must remember lives in host state, reached only through the
capabilities the host explicitly lends it — **deny-by-default**:

- `host-log` — structured logging.
- `host-clock` — a per-request millisecond clock snapshot (deterministic within a request).
- `host-kv` — per-filter key/value bytes (session, cache).
- `host-counter` — atomic named counters.
- `host-ratelimit` — a host-native token bucket; the filter passes only `(key, cost)`, never the spec.

Nothing else — no network, no filesystem, no sockets — is reachable. That is enforced by the
Component Model sandbox, not by convention.

## 2. Scaffold

Start from the template in [`plecto/examples/filters/filter-template/`](../plecto/examples/filters/filter-template/).
Either copy it or generate it:

```bash
# copy
cp -r plecto/examples/filters/filter-template my-filter

# …or with cargo-generate
cargo generate --git https://github.com/Kaikei-e/Plecto.git \
  examples/filters/filter-template --name my-filter
```

Then set the package `name` in `Cargo.toml` and edit `src/lib.rs`. The template implements the whole
world with a pass-through default; replace the body of `on_request` with your policy.

The contract is **vendored** in the template under `wit/`, and the binding macro references it
locally:

```rust
wit_bindgen::generate!({ path: "wit", world: "filter" });
```

That vendored copy is why a generated filter builds anywhere. Keeping it in sync with the canonical
`plecto/wit/world.wit` is your responsibility; inside this repo, `just sync-template-wit` refreshes it.

Because the contract is WIT, **any language that compiles to a WASM component can write a filter** —
see [section 7](#7-other-languages).

## 3. Build and componentize

A filter is built for `wasm32-unknown-unknown` (no WASI: it imports only the granted Plecto
capabilities, ADR 000010), then wrapped into a **component**:

```bash
cargo build --target wasm32-unknown-unknown --release
# core module → component (no WASI adapter needed):
wasm-tools component new \
  target/wasm32-unknown-unknown/release/my_filter.wasm -o my-filter.component.wasm
```

For the example filters **inside this repo**, this two-step is automatic:
[`plecto/crates/host/build.rs`](../plecto/crates/host/build.rs) builds each guest and runs the
`wit-component` encoder, so `cargo test --all` and the demos just work. For your own out-of-tree
filter, run the `wasm-tools component new` step yourself (or wire an equivalent into your build).

Verify what it imports and exports:

```bash
wasm-tools component wit my-filter.component.wasm
```

A correct filter imports only `plecto:filter/*` — no `wasi:*`, no network, no filesystem.

## 4. The manifest

The manifest is the static source of truth for which filters load, with which trust roots, in what
order, and how requests route to upstreams (ADR 000007 / 000008). It is TOML. A ready-to-edit copy
ships with the template ([`manifest.toml`](../plecto/examples/filters/filter-template/manifest.toml)).
The authoritative schema is [`plecto/crates/control/src/manifest/mod.rs`](../plecto/crates/control/src/manifest/mod.rs);
the field reference below mirrors it.

### `[trust]`

```toml
[trust]
keys = ["keys/signer.pub"]    # PEM public keys (manifest-relative) trusted to sign filters
```

Trust is fixed at construction — a reload that changes `[trust]` is rejected.

### `[[filter]]`

```toml
[[filter]]
id = "my-filter"               # required: host identity; namespaces this filter's KV (survives reload)
source = "artifacts/my-filter" # required: manifest-relative path to the local OCI image-layout
digest = "sha256:..."          # required: pinned OCI image-manifest digest
isolation = "untrusted"        # "untrusted" (default, fresh per request) | "trusted" (pooled, fast)
init_deadline_ms = 200         # optional: metering overrides; unset = host default
request_deadline_ms = 25       # optional
max_memory_bytes = 16777216    # optional
ratelimit = { capacity = 100, refill_tokens = 10, refill_interval_ms = 1000 }  # optional, ADR 000026
```

`isolation` is the biggest performance lever. `trusted` filters are built once and pooled (fast hot
path); `untrusted` filters run fresh per request with linear memory wiped each time (stronger
isolation, slower). The default is `untrusted` — fail-closed. `ratelimit`, when present, is the
**host-side** bucket spec for this filter's `host-ratelimit`; the operator owns it so an untrusted
filter cannot widen its own limit.

### `[[upstream]]`

```toml
[[upstream]]
name = "app"                          # required
addresses = ["127.0.0.1:9000", "127.0.0.1:9001"]  # required: host:port instances, round-robined
resolve_interval_ms = 0               # optional (default 0 = off): re-resolve hostname addresses
                                       # on this interval, each A/AAAA record its own LB endpoint
request_timeout_ms = 30000            # optional (default 30000; 0 disables — long-poll/streaming)
max_retries = 1                       # optional (default 1; 0 disables retry onto another instance)
[upstream.health]                     # required: instances start unhealthy, a probe admits them
path = "/healthz"                     # required
interval_ms = 2000                    # optional (default 2000)
timeout_ms = 1000                     # optional (default 1000)
healthy_threshold = 2                 # optional (default 2)
unhealthy_threshold = 3               # optional (default 3)
port = 9100                           # optional (default: probe the instance's own traffic port)
[upstream.tls]                        # optional (absent = plain HTTP/1.1 to every instance)
ca_path = "certs/internal-ca.pem"     # optional: replaces the webpki roots (self-signed / internal CA)
```

Every upstream **requires** a `[upstream.health]` block with at least `path`, because instances start
pessimistic and only a passing probe puts one into rotation (ADR 000017). `[upstream.tls]`
re-encrypts the forward leg to every instance with ALPN-negotiated HTTP/2 (falling back to
HTTP/1.1) — verification is always on, with no insecure escape hatch (ADR 000042); `TE: trailers`
and response trailers pass through, so gRPC upstreams work end to end. `resolve_interval_ms`
re-resolves a hostname address on an interval (Compose service names, k8s headless Services);
`0` (the default) resolves once, the pre-000044 behaviour.

### `[[route]]`

```toml
[[route]]
path_prefix = "/api"     # required: match requests whose path starts here (longest prefix wins)
upstream = "app"         # required: the [[upstream]] name to forward a passing request to
filters = ["my-filter"]  # optional: filter ids run in order (empty = pure pass-through)
host = "example.com"     # optional: match only this authority (case-insensitive); omit = any host
strip_prefix = "/api"    # optional: strip this prefix before forwarding (the chain saw the original)
[route.upgrade]          # optional: absent = deny-by-default, no HTTP/1.1 Upgrade tunnelled
protocols = ["websocket"] # required if the section is present; token allowlist, `h2c` rejected
idle_timeout_ms = 300000  # optional (default 300000 = 5 min); 0 disables the idle timer
```

`[route.upgrade]` opts a route into tunnelling `HTTP/1.1 Upgrade` (e.g. WebSocket): a listed token
re-issues the handshake upstream and, on a verified `101`, splices a bidirectional byte tunnel with
an activity-reset idle timeout (ADR 000048).

### `[[tls]]`

```toml
[[tls]]
cert_path = "certs/app.pem"   # required: PEM cert chain (manifest-relative)
key_path = "certs/app.key"    # required: PEM private key
host = "example.com"          # optional SNI host; omit = the default cert
```

With no `[[tls]]`, the fast path serves plain HTTP/1.1; one or more certs enable TLS termination
(rustls, ADR 000014). `[chain]` exists for the single-chain convenience API, but the fast-path server
uses `[[route]]`.

### `[listen]`

```toml
[listen]
addr = "0.0.0.0:8443"    # optional: data-plane bind (default 127.0.0.1:8080; the CLI arg overrides)
advertised_port = 443    # optional: the port Alt-Svc advertises for h3 when the published port differs

[listen.proxy_protocol]  # optional: PROXY protocol v2 reception (ADR 000057); absent = off
trusted = ["10.0.0.0/8"] # required when present: CIDRs of the L4 LBs allowed to speak PROXY v2
```

`[listen]` is captured at startup — a reload does not re-bind or change it; restart to apply.
With `[listen.proxy_protocol]`, a peer inside `trusted` MUST open every TCP connection with a
PROXY v2 header (its `LOCAL` command — LB health checks — keeps the real endpoints), and the
restored client address feeds the per-client-IP rate limit, `X-Forwarded-For`/`X-Real-IP`
re-issuing, Maglev `source_ip` hashing, and the access log. Everything else is fail-closed cut:
a missing/malformed header from a trusted peer, a PROXY v2 signature from an untrusted peer, or
any non-TCP/IPv4/IPv6 `PROXY` command. `trusted` takes CIDR notation only (a single host is
`"192.0.2.1/32"`), and the h3 (QUIC/UDP) listener is out of scope — front it with a QUIC-aware
LB only if that LB can pass the client address another way (e.g. Kubernetes
`externalTrafficPolicy: Local`).

## 5. Package, sign, and run

Plecto loads filters from a local, digest-pinned OCI image-layout and **verifies a cosign signature**
plus an SBOM↔component binding before running them (ADR 000006 / 000007) — bad signature, refused,
fail-closed. The public key must be listed in `[trust]`.

The complete, worked pipeline is the [`wasm-auth` example](../plecto/examples/wasm-auth/main.rs): it
signs the component, writes the offline OCI layout, computes the digest, and starts a real proxy — all
in one runnable file. **Read it as your reference.** For production, sign with `cosign sign-blob`
using a key whose public half is in `[trust].keys`.

Streamlined packaging/signing tooling (`plecto`-side helpers) is on the roadmap (M6); today the
example is the canonical recipe.

## 6. Test it locally

The fastest way to see *your* filter run end to end is to adapt an example:

```bash
# run the bundled examples to see the shape of a working setup
just demo wasm-auth      # a signed auth filter, signed + packaged + served, with curl recipes
just demo filter-chain   # continue / modify / short-circuit / host-native rate limit
```

Then copy the example whose shape matches yours (e.g. `plecto/examples/wasm-auth/`), point it at your
component, and run it with `cargo run -p plecto-server --example ...`. The examples use
`plecto_host::test_support::TestSigner` to sign on the fly — that is a **test/example convenience**;
real deployments sign out of band with cosign and pin the digest in the manifest.

To exercise the contract without a full proxy, load the component into the host directly the way the
host's conformance tests do (see `plecto/crates/host/`), asserting on the typed decision your filter
returns.

## 7. Other languages

Because the contract is WIT, a filter can be written in any language that targets a WASM component.
The contract and the manifest are the same regardless of language; only the binding toolchain
differs. The catch is not the language — it is **WASI**: the host's default Linker lends only the
plecto host-API and deliberately links no `wasi:*` interfaces, so a filter component must arrive
with **zero WASI imports** or instantiation fails on the unresolved imports. Three bundled examples
(the filter-hello conformance subset, ported) show which toolchains can do that today, and CI runs
the **same assertion suite** against all of them (`plecto/crates/host/tests/polyglot.rs`, job
`polyglot-guests`):

| Language | Example | Toolchain | Component size | Zero-WASI how |
|---|---|---|---|---|
| MoonBit | [`filter-hello-moonbit`](../plecto/examples/filters/filter-hello-moonbit) | `moon` + `wasm-tools` (`component embed --encoding utf16` + `component new`) | ~22 KB | pure core module, no adapter needed |
| JavaScript/TypeScript | [`filter-hello-js`](../plecto/examples/filters/filter-hello-js) | ComponentizeJS (`npm run build`) | ~12 MB (StarlingMonkey engine constant) | `disableFeatures: ['random','stdio','clocks','http','fetch-event']` |
| C | [`filter-hello-c`](../plecto/examples/filters/filter-hello-c) | `wit-bindgen c` + wasi-sdk (`--target=wasm32-wasip2 -mexec-model=reactor`) | ~66 KB | call no WASI API and no adapter is linked |

Each example has a `build.sh` that builds the component and **fails the build if any `wasi:*`
import appears** — run it, then verify against the host with:

```bash
cargo test -p plecto-host --features polyglot-conformance --test polyglot
```

Two more languages, for completeness:

- **Python** works the same way (`componentize-py --stub-wasi` bundles CPython, ~17 MB). It passes
  the same gate but is heavy for a per-request filter; no bundled example.
- **Go is the one that cannot** (yet): TinyGo's `wasip2` target is built around the
  `wasi:cli/command` world, so its components always import `wasi:cli`/`wasi:io`/`wasi:clocks` —
  which the default Linker will not provide. Lending a minimal `WasiCtx` to such "fat" guests is a
  deliberate, separate host decision (a new security surface flagged in ADR 000010); until it is
  taken, Go filters cannot load. Big Go (the `gc` toolchain) has no Component Model support at all.

First-class polyglot SDKs and reference filters (auth, rate limit, WAF) remain on the
[roadmap](../README.md#roadmap) (M6).
