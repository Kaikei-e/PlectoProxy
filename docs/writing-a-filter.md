# Writing a filter

A **filter** is your request logic, running as a sandboxed WebAssembly component. Plecto Proxy's native
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
- `host-config` — read-only manifest-declared business config (`[filter.config]`, ADR 000066); the
  host never interprets keys or values, only the filter does.

Nothing else — no network, no filesystem, no sockets — is reachable. That is enforced by the
Component Model sandbox, not by convention.

## 2. Scaffold

The fastest path, and the one this guide's other examples assume, is the Filter Dev Kit CLI
(ADR 000065) — it scaffolds the crate, fetches the WIT contract via `wkg` (ADR 000064), and
writes a ready-to-run dev manifest in one step:

```bash
plecto new-filter --lang rust my-filter
```

Rust is the only `--lang` implemented so far; `go`/`moonbit`/`c`/`js` scaffolds are tracked
follow-up work (see §7 for hand-written examples in those languages today). Without the CLI, or
to see exactly what it generates, start from the template in
[`plecto/examples/filters/filter-template/`](../plecto/examples/filters/filter-template/) instead:

```bash
# copy
cp -r plecto/examples/filters/filter-template my-filter

# …or with cargo-generate
cargo generate --git https://github.com/Kaikei-e/PlectoProxy.git \
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
Outside this repo — the normal case once your filter has its own home — refresh it with `wkg get`
instead of copying files by hand; see [§8](#8-contract-distribution-and-compatibility-policy).

Because the contract is WIT, **any language that compiles to a WASM component can write a filter** —
see [section 7](#7-other-languages).

## 3. Build and componentize

A filter is built for `wasm32-unknown-unknown` (no WASI: it imports only the granted Plecto Proxy
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
wasi = "minimal"                # optional (default "none"), ADR 000063: Tier B fat-guest WASI grant

[filter.config]                # optional, ADR 000066: arbitrary string→string business config
on_backend_error = "deny"      # read back via `host-config::get("on_backend_error")`
```

`isolation` is the biggest performance lever. `trusted` filters are built once and pooled (fast hot
path); `untrusted` filters run fresh per request with linear memory wiped each time (stronger
isolation, slower). The default is `untrusted` — fail-closed. `ratelimit`, when present, is the
**host-side** bucket spec for this filter's `host-ratelimit`; the operator owns it so an untrusted
filter cannot widen its own limit. `wasi = "minimal"` lends the fixed Tier B WASI slice a fat guest
(TinyGo/Go) needs to instantiate at all (§7) — requires the host's `fat-guest` build, otherwise
rejected at validate; the default `"none"` keeps a filter zero-WASI (Tier A). `[filter.config]` is
a read-only string map the filter reads back
via `host-config::get(key)` — the host never interprets it, so **the filter itself must validate any
key it requires** (typically in `init`, trapping on a missing/invalid value). Combined with
`isolation = "trusted"`, that trap surfaces as a load-time failure rather than a per-request one
(the host eager-builds one trusted instance at load, see [`filter-ratelimit-redis`](../plecto/examples/filters/filter-ratelimit-redis))
— a filter with a *required* config key should document that it needs `trusted` isolation.

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

Plecto Proxy loads filters from a local, digest-pinned OCI image-layout and **verifies a cosign signature**
plus an SBOM↔component binding before running them (ADR 000006 / 000007) — bad signature, refused,
fail-closed. The public key must be listed in `[trust]`.

For **local development**, `plecto dev <filter-dir>` (ADR 000065, Rust filters today) closes this
whole loop automatically — no manual signing, no separate reload command:

```bash
plecto dev my-filter
```

It watches `my-filter/src/`, and on every change: rebuilds (`cargo build --target
wasm32-unknown-unknown --release` + `wit-component`), runs the same generic conformance battery as
`plecto conformance` (§6), and — **only if conformant** — signs with your project's persistent dev
key (`.plecto/dev-key`, generated on first use, `.gitignore`d automatically), writes the OCI
layout, rewrites `my-filter/manifest.toml`'s pinned digest, and reloads the running gateway via the
same SIGHUP path `plecto serve` uses. A non-conformant rebuild is reported and discarded — the
gateway keeps serving the last good build. The verification code path is never weakened for dev:
only *which* key is in `[trust]` differs from production (P5, ADR 000006).

The dev key is **not** a production signing key — `plecto validate` warns (`PLECTO-E0004`) if a
manifest's `[trust]` ever references one outside a dev context. For a **production** deploy, sign
with `cosign sign-blob` (or your CI's signer) using a key whose public half is in `[trust].keys`,
and follow the packaging pipeline in the [`wasm-auth` example](../plecto/examples/wasm-auth/main.rs)
— it signs the component, writes the offline OCI layout, computes the digest, and starts a real
proxy, all in one runnable file. **Read it as your production reference.**

## 6. Test it locally

The fastest contract-level check, no manifest or upstream needed, is `plecto conformance`
(ADR 000065) against a built component:

```bash
plecto conformance my-filter/dist/my_filter.component.wasm
```

It self-signs with a throwaway key (never your persistent `.plecto/dev-key`) and checks the
generic properties any `plecto:filter` must have: the component loads under the real signature/SBOM
gate, and it handles a generic request without trapping or exceeding its deadline. It does **not**
check your filter's specific policy (e.g. "does it block the right headers") — that is what §1's
world and your own test requests are for. `plecto dev` (§5) runs this same battery automatically
before every reload.

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
differs. The catch is not the language — it is **WASI**. Plecto Proxy recognizes two tiers:

- **Tier A (zero-WASI, the default)**: the host's default Linker lends only the plecto host-API
  and deliberately links no `wasi:*` interfaces, so a filter component must arrive with **zero
  WASI imports** or instantiation fails on the unresolved imports. Most languages can do this.
- **Tier B (minimal WASI, opt-in, ADR 000063)**: a "fat guest" — a language runtime that assumes
  some baseline WASI is present, TinyGo/Go being the reference case — is lent a fixed, minimal
  slice (`wasi:io` / `wasi:clocks` / `wasi:random` / `wasi:cli`, plus an empty `wasi:filesystem`
  some runtimes' bootstrap unconditionally imports even though it touches no file — zero preopens,
  so zero reachable paths). Never filesystem *access*, never sockets. Requires the host's
  off-by-default `fat-guest` cargo feature AND the filter's manifest entry to declare
  `wasi = "minimal"`; absent either, a fat guest fails to instantiate (deny-by-default, ADR 000063
  Decision 4) exactly like an unlisted `wasi:*` import does for Tier A.

Four bundled examples (the filter-hello conformance subset, ported) show which toolchains can do
this today:

| Tier | Language | Example | Toolchain | Component size | WASI surface |
|---|---|---|---|---|---|
| A | MoonBit | [`filter-hello-moonbit`](../plecto/examples/filters/filter-hello-moonbit) | `moon` + `wasm-tools` (`component embed --encoding utf16` + `component new`) | ~22 KB | none |
| A | JavaScript/TypeScript | [`filter-hello-js`](../plecto/examples/filters/filter-hello-js) | ComponentizeJS (`npm run build`) | ~12 MB (StarlingMonkey engine constant) | none (`disableFeatures: ['random','stdio','clocks','http','fetch-event']`) |
| A | C | [`filter-hello-c`](../plecto/examples/filters/filter-hello-c) | `wit-bindgen c` + wasi-sdk (`--target=wasm32-wasip2 -mexec-model=reactor`) | ~66 KB | none |
| B | Go | [`filter-hello-go`](../plecto/examples/filters/filter-hello-go) | TinyGo (`-target=wasip2`) + `wit-bindgen-go` (`go.bytecodealliance.org/cmd/wit-bindgen-go`) + `wkg` (WIT deps) | ~850 KB | `wasi:io`/`clocks`/`random`/`cli`/`filesystem` (preopens empty) |

Each Tier A example has a `build.sh` that builds the component and **fails the build if any
`wasi:*` import appears**; `filter-hello-go`'s `build.sh` instead asserts every `wasi:*` import is
within the Tier B allowlist (`io`/`clocks`/`random`/`cli`/`filesystem` — never `sockets`/`http`).
Run the relevant one, then verify against the host:

```bash
# Tier A — same assertion suite against all three languages:
cargo test -p plecto-host --features polyglot-conformance --test polyglot

# Tier B — the fat-guest grant, deny-by-default without it, and the conformance subset:
cargo test -p plecto-host --features polyglot-conformance,fat-guest --test polyglot_tier_b
```

To opt a Go/TinyGo (or other Tier B) filter in, build the host with the `fat-guest` cargo feature
and declare the grant in its manifest entry:

```toml
[[filter]]
id = "my-go-filter"
source = "artifacts/my-go-filter"
digest = "sha256:..."
wasi = "minimal"    # ADR 000063; requires the host's `fat-guest` build, else rejected at validate
```

stdout/stderr from a Tier B guest is bridged into that filter's `host-log` (stdout → `debug`,
stderr → `warn`; 4 KiB/line, 64 KiB/request combined, truncate-and-warn-once past the budget) — a
TinyGo panic message shows up in the same trace as the request that triggered it, without the
guest importing `host-log` itself.

The `wasi:clocks` lent to a Tier B guest are the runtime's OWN real monotonic/wall clocks (needed
for the TinyGo runtime to boot at all) — reading them directly from guest code (e.g. Go's
`time.Now()`) is **non-deterministic** across a retry or a re-run. A filter's *decision* logic must
stay on the `host-clock` host-API (§1 above): the same per-request millisecond snapshot every
language gets, so policy stays reproducible regardless of tier. Treat `wasi:clocks` as a
Go-runtime implementation detail, not a contract-level time source.

One more deliberate exception to the zero-WASI rule, orthogonal to Tier B: a filter the manifest
lends an **outbound capability** to (`[filter.outbound_http]`, ADR 000036, or
`[filter.outbound_tcp]`, ADR 000060) also imports `wasi:*` interfaces (`wasi:http/outgoing-handler`,
or the `wasi:sockets` TCP-connect vocabulary, plus the `wasi:io` base they pull in), and the host
links exactly those slices — only for that filter, only behind the declared allowlist + SSRF guard,
and only on a build with the matching off-by-default cargo feature (`outbound-http` /
`outbound-tcp`). A filter can combine `wasi = "minimal"` with an outbound capability (e.g. a Go
filter that also calls out over HTTP) — the host composes both grants on the same `WasiCtx`. A
filter with none of these declared gets the default Linker and must arrive with zero WASI imports.
See `filter-extauthz` (HTTP) and `filter-tcp-gate` (TCP) under `plecto/examples/filters/` for the
outbound shapes.

One more language, for completeness:

- **Python** works the Tier A way (`componentize-py --stub-wasi` bundles CPython, ~17 MB). It
  passes the zero-WASI gate but is heavy for a per-request filter; no bundled example.

First-class polyglot SDKs and reference filters (auth, rate limit, WAF) remain on the
[roadmap](../README.md#roadmap) (M6).

## 8. Contract distribution and compatibility policy

Everything above assumes you have `plecto/wit/`. If your filter lives outside this repository —
which is the normal case for a real filter — fetch the contract with the standard WIT toolchain
instead of copying files by hand (ADR 000064). `plecto new-filter --lang rust` (§2) runs a `wkg get`
for you today; read on if you want the manual steps, are scaffolding another language by hand, or
just want to know what the CLI is doing under the hood. (ADR 000072 accepts embedding the same
`wit/world.wit` the host bindgen reads so the scaffold no longer needs the network — not landed yet.)

The contract is published on every tagged release as a [CNCF Wasm OCI
Artifact](https://tag-runtime.cncf.io/wgs/wasm/deliverables/wasm-oci-artifact/) to `ghcr.io`, the
same way WASI's own WIT packages are distributed under `ghcr.io/webassembly`. There is no
`/.well-known/wasm-pkg/registry.json` under a Plecto-controlled domain yet (that comes once a docs
domain exists), so point [`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools) at the
registry explicitly:

```bash
cat > wkg-registry.toml <<'EOF'
[namespace_registries.plecto]
registry = "ghcr.io"
metadata = { oci = { registry = "ghcr.io", namespacePrefix = "kaikei-e/wit/" } }
EOF

wkg get plecto:filter@0.2.0 --config wkg-registry.toml -o wit/ --format wit
```

That writes the plain-text WIT to `wit/`, ready for `wit_bindgen::generate!` (or any other
language's binding generator) — no `git clone` of this repository required. Pin the version, and
verify the pulled contract against the digest recorded in that tag's [GitHub
Release](https://github.com/Kaikei-e/PlectoProxy/releases) notes before you build against it, the
same fail-closed instinct §5's digest-pinned filter loading already asks of you.

The experimental streaming contract publishes the same way, one package over: `plecto:filter-streaming@0.1.0`.
It carries **no compatibility guarantee** — it is the off-by-default `streaming-body` feature's
contract (§3's `filter`/`filter-body` split has no third `streaming` world yet) and may change or
disappear without a major bump. Do not depend on it outside an explicit opt-in build.

### Compatibility policy

The contract's version is **independent of Plecto's own release version** — CHANGELOG.md's
versioning policy already says so. `plecto:filter@0.2.0` and a `plecto` binary at `0.2.6` is the
normal, expected state.

- **SemVer, additive = minor, breaking = major.** A new capability interface, a new optional
  field, a new export on `filter-body` — minor. Removing or changing the signature of an existing
  export or host-API function — major.
- **The host keeps loading every contract version it ships support for.** A `plecto` upgrade does
  not silently break a filter built against an older `plecto:filter` version; the host branches on
  the component's own world version at load time. On a major bump, the previous major stays
  accepted for **at least two release series** before its removal is declared — via ADR, the same
  way any other deprecation in this project is declared (never silently).
- **`filter` vs. `filter-body` compatibility is part of this policy** (ADR 000038): the base
  `filter` world exporting nothing new stays minor-compatible forever by construction (the
  *absence* of `on-request-body` is itself contractual, not an oversight). Adding an export to
  `filter-body` is minor; changing an existing export's signature (on either world) is major.

This is the filter-author-facing analogue of the supply-chain discipline Plecto applies to its own
release binaries and images (ADR 000047): a digest-pinned artifact, a declared stability contract,
and a fail-closed way to tell when either one is violated.

## 9. Error codes (PLECTO-E)

Some rejections carry a **stable code** alongside the human-readable message (ADR 000065). Where
you see it depends on the wall you hit: on an HTTP response it is the `x-plecto-error-code`
header next to `x-plecto-fault` — deliberately the code alone, so the remediation text below
never leaks to an arbitrary client; at startup, in the reload log, and in `plecto validate`
output, the full four-part diagnostic (code, cause, suggestion, docs link) is printed.

| Code | Where it appears | Meaning | What to do |
|------|------------------|---------|------------|
| `PLECTO-E0001` | startup error / reload log | the component or SBOM signature does not verify against any key in the manifest's `[trust]` | sign with a key listed under `[trust]` (cosign sign-blob or your CI's signer); for local dev, `plecto dev` signs with `.plecto/dev-key` automatically ([ADR 000006](ADR/000006.md)) |
| `PLECTO-E0002` | `429` response header | the request exceeded the filter's host-native rate-limit bucket | raise `[filter.ratelimit]` capacity / `refill_per_sec`, or have the client back off per the 429's `retry-after` header ([ADR 000026](ADR/000026.md)) |
| `PLECTO-E0003` | `400` response header | the request path failed normalization (`..` traversal, invalid percent-encoding, or a raw control byte) | this rejects the client's request, not your manifest — check what the client sends as the path ([ADR 000013](ADR/000013.md)) |
| `PLECTO-E0004` | `plecto validate` warning | a `[trust]` key file carries the dev-key marker (generated by `plecto dev` / `plecto new-filter`) | expected for a dev manifest; for production, replace it with a key from your real signing pipeline ([ADR 000065](ADR/000065.md)) |
