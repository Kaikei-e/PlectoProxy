# Writing a filter

A **filter** is your request logic, running as a sandboxed WebAssembly component. Plecto Proxy's native
fast path handles connections, TLS, HTTP, routing, and load balancing; it hands each request to your
filter, which **inspects it and returns one typed decision**. This guide takes you from an empty
directory to a running filter.

New to the model? Read the [README](../README.md) first — the architecture and the three decisions.
This guide is the practical how-to.

## 1. The contract in one minute

A filter implements a `plecto:filter` world (the authoritative text is
[`plecto/wit/world.wit`](../plecto/wit/world.wit)): the header-only `filter` world, or `filter-body`
if it also needs a body. `filter-body` is `filter` plus the two body hooks — the **absence** of a
hook is itself the signal the host uses to skip buffering that direction and stream it straight
through, at no cost to a filter that does not read it (ADR 000038 / 000098). The host decides
buffering **per direction**, so a filter that reads only the response body leaves the request body
on the zero-copy path, and vice versa.

Those two worlds are the ENDS of what is accepted, not a list of the legal shapes: a component is
accepted when it exports all of `filter` plus **any subset** of the body hooks, and the host probes
each hook by name at load (ADR 000098 decision 2). If you need exactly one of the two, declare a
world of your own with just that hook — the published worlds exist to generate bindings from, and
`crates/host/fixtures/filter-respbody` is a worked example.

```wit
package plecto:filter@0.4.0;

interface types {
  // Header values are raw bytes (ADR 000071) — not lossy UTF-8 strings.
  record header { name: string, value: list<u8>, }

  // The request target carries the query, and since 0.4.0 the name says so (ADR 000104).
  record http-request { method: string, path-with-query: string, /* … */ }

  // The typed outcome of a request-side filter. Never a bare flag.
  variant request-decision {
    %continue,                       // pass unchanged to the next filter
    modified(request-edit),          // apply the edit, then continue
    short-circuit(http-response),    // stop the chain; synthesise a response now
  }

  // The body side has the SAME shape since 0.4.0 (ADR 000098): `%continue` carries no
  // payload, so a filter that only INSPECTS the body never pays to hand it back.
  variant request-body-decision {
    %continue,                       // forward the buffered body unchanged
    modified(request-body-edit),     // forward these bytes, with the header edits they force
    short-circuit(http-response),    // stop the chain; synthesise a response now
  }

  // The response side (ADR 000073): `replace` answers with a filter-authored response —
  // status, headers, AND body — in place of the upstream's (whose body is dropped unread,
  // so zero-copy stays intact).
  variant response-decision { %continue, modified(response-edit), replace(http-response) }
}

// deny-by-default: one capability per interface; a filter imports only what it is lent.
interface host-kv      { get: func(key: string) -> option<list<u8>>; set: func(key: string, value: list<u8>); /* … */ }
interface host-counter { increment: func(key: string, delta: s64) -> s64; /* atomic named counter */ }
interface host-log     { log: func(level: level, message: string); }
interface host-config  { get: func(key: string) -> option<string>; }  // manifest [filter.config]
// host-ratelimit keeps the token bucket host-native — the hot-path refill/counting never crosses
// the WASM boundary. The bucket spec (capacity/refill) is host-configured in the manifest; the
// filter passes only (key, cost), so an untrusted filter cannot widen its own limit
// (ADR 000005 / 000026).

world filter {
  import host-log;  import host-clock;  import host-kv;  import host-counter;
  import host-ratelimit;  import host-config;
  export init: func();                                                // heavy, once per instance
  export on-request:  func(req: http-request)  -> request-decision;   // hot path (headers)
  export on-response: func(req: http-request, resp: http-response) -> response-decision;
}

world filter-body {
  // …the same imports and exports as `filter`, plus the two hooks whose PRESENCE is what makes
  // the host buffer that direction at all (buffer-then-decide, ADR 000025 / 000098). Each is
  // independent — export only the one you read.
  export on-request-body: func(body: list<u8>) -> request-body-decision;
  // `resp` carries status + headers only; the bytes arrive as `body`. The host HOLDS the response
  // headers until this returns, which is why `replace` is still expressible here.
  export on-response-body: func(req: http-request, resp: http-response, body: list<u8>)
      -> response-body-decision;
}
```

| Export | World | When it runs | Returns |
| --- | --- | --- | --- |
| `init` | both | once per instance (heavy setup) | — |
| `on-request` | both | per request, on the headers | `continue` / `modified(edit)` / `short-circuit(response)` |
| `on-request-body` | optional | per request, on the buffered request body | `continue` / `modified(edit)` / `short-circuit(response)` |
| `on-response` | both | per response, on the headers, with the as-forwarded request snapshot (ADR 000073) | `continue` / `modified(edit)` / `replace(response)` |
| `on-response-body` | optional | per response, on the buffered response body, after `on-response` | `continue` / `modified(edit)` / `replace(response)` |

### Response-side decisions and chain order

The response chain runs the route's filters in **reverse** of the request order (request
`[auth, cors]` → response `cors → auth`). A `replace` is **terminal**: remaining filters in that
reverse walk are skipped and the synthesised response is sent to the client. So the filters most
at risk of being skipped are the ones **early in the request-order list** (they run last on the
response). A response filter that must always run — audit, security-header stamping — belongs
**last in the request-order list**, so it runs **first** on the response, before any `replace`
can end the walk. The trade-off is inherent to a terminal `replace`: a filter cannot both be
guaranteed to run and be guaranteed to see the final (possibly replaced) response.

`req` on `on-response` is the **as-forwarded** snapshot: the request as it left the request-side
chain (filter stamps such as `x-authenticated-user` are visible), **before** host egress
transforms (hop-by-hop strip, upstream path rewrite, `traceparent` injection). It is value-passed;
editing it does nothing.

### Reading the response body (`on-response-body`)

Export it and the host buffers the upstream's response body for this route and hands it to you,
**holding the response headers until your decision returns**. Nothing has been written to the
client yet, so the arms mean what they say: `%continue` forwards what the host already holds (the
bare arm — do not hand the bytes back to say "unchanged"), `modified` forwards your bytes plus the
header edits a transform forces, and `replace` discards the buffered body for a response of your
own. It runs **after** the whole `on-response` walk, and not at all if that walk produced a
`replace` — there is no upstream body left to read. `resp.body` is always empty here; the bytes are
the third parameter.

`Content-Length` is the host's. It re-derives it from the bytes it actually sends, and a decision
whose headers name `content-length` **fails closed** — a length that disagrees with the body is a
response-desync primitive, so it is refused rather than stripped. Trailers are out of scope for
v1 and are not part of what you inspect.

What the operator controls, per route, is when this runs at all — `[route.response_body]`:

| Key | Default | Meaning |
| --- | --- | --- |
| `content_types` | `text/plain`, `text/html`, `text/xml`, `application/json` | The allowlist that ARMS buffering, matched against the content type **as received from the upstream** — so an earlier filter cannot rewrite a header to steer a response past you. Outside it, the response streams through uninspected. |
| `max_bytes` | 1 MiB (ceiling 16 MiB) | The most the host will hold. It also bounds what YOU hand back: a larger `modified` / `replace` body fails closed. |
| `over_cap` | `reject` | `reject` answers 502; `process-partial` inspects the head and forwards the whole body (a rewrite of a head is refused — the host cannot frame it); `passthrough` forwards uninspected. |
| `uninspectable` | `reject` | What happens to a response the host cannot inspect at all. `passthrough` is the explicit opt-out. |

On such a route the host also strips `Accept-Encoding` / `Range` / `If-Range` from the request it
forwards, so what comes back is a whole identity representation — you always read plain bytes. A
response that is nonetheless uninspectable (a streaming media type, a surviving content coding, a
`206` fragment) is answered 502 by default and always leaves a reason in
`plecto_response_body_inspection_skipped_total{reason=…}` and on the access log line. Nothing is
skipped silently.

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

### Recipe: answer with a body of your own (`replace`)

`replace` is how a filter authors a whole response — status, headers, **and body**. `modified`
retouches what upstream sent, but `response-edit` carries no body field, so a filter that must
return content of its own returns `replace`. Either way the upstream body is never read: on a
`replace` that stream is dropped unread, keeping the zero-copy passthrough intact (ADR 000038).
The body you send is synthesised, not a rewrite of the upstream's.

The canonical case is an error page keyed on what upstream actually returned:

```rust
use crate::plecto::filter::host_config;
use crate::plecto::filter::types::Header;

fn on_response(req: HttpRequest, resp: HttpResponse) -> ResponseDecision {
    // Which requests this policy covers is the FILTER's decision. A route's `path_prefix` is a
    // routing bound and is often wider than the condition a policy cares about, so match here.
    if resp.status < 500 || !req.path_with_query.starts_with("/api/") {
        return ResponseDecision::Continue;
    }

    // The page text is operator-owned config (`[filter.config]`, ADR 000066) rather than a
    // constant compiled into the component: editing it is a manifest change, not a rebuild,
    // re-sign, and re-pin of the digest.
    let page = host_config::get("upstream_error_page")
        .unwrap_or_else(|| "the service is temporarily unavailable".to_string());

    ResponseDecision::Replace(HttpResponse {
        status: 503,
        headers: vec![
            Header {
                name: "content-type".to_string(),
                value: b"text/plain; charset=utf-8".to_vec(),
            },
            Header {
                name: "retry-after".to_string(),
                value: b"5".to_vec(),
            },
        ],
        body: page.into_bytes(),
    })
}
```

```toml
[filter.config]                # on this filter's [[filter]] entry (§4)
upstream_error_page = "the service is temporarily unavailable"
```

`resp.status` is the upstream's own status, and `req` is the as-forwarded snapshot, so the same
hook can key on a request-chain stamp (`x-authenticated-user`) as readily as on the status. The
returned headers pass exactly the fail-closed validation a request-side `short-circuit` output
passes (ADR 000071), and a route's `[route.headers]` declaration (§4) is applied afterwards, to
this synthesised response as much as to a forwarded one. Keep the chain order above in mind: a
`replace` is terminal, so filters earlier in the route's `filters` list never see this response.

## 2. Scaffold

The fastest path, and the one this guide's other examples assume, is the Filter Dev Kit CLI
(ADR 000065) — it scaffolds the crate, writes out the WIT contract vendored into the `plecto`
binary itself (self-vendoring, ADR 000072 — no network, and always the exact version that binary's
own host runs), and writes a ready-to-run dev manifest in one step:

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

[filter.config_files]          # optional: same key space, but each value is a FILE PATH whose
hmac_key = "/run/secrets/hmac" # content (UTF-8, trimmed both ends, ≤ 1 MiB) becomes the value
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

`[filter.config_files]` is the secret-shaped sibling: container secrets arrive as **files**
(`/run/secrets/<name>` mounts, credential directories), and a checked-in manifest is exactly where
a secret must not be inlined. Each value is a path — absolute, or relative to the manifest's
directory — read at every load/reload (a SIGHUP picks up a rotated secret file) and served through
the **same** `host-config::get` keys; the filter cannot tell the two sections apart, and the WIT
contract is unchanged. Setting the same key in both sections is a validate error (the `X` /
`X_file` pairs convention: mutually exclusive), and a missing/unreadable/non-UTF-8/oversized file
fails the load — the same fail-closed posture as trust keys and TLS certs. Content is
whitespace-trimmed at both ends (the `echo`-appended trailing newline problem); a secret that
needs exact leading/trailing whitespace cannot ride this mechanism.

One filter binary, several rates: the `ratelimit` bucket spec belongs to the `[[filter]]` *entry*,
not the wasm — register the same layout under two ids (same `source`, same `digest`) to run, e.g.,
a lenient global limiter and a strict `/auth` limiter from one artifact. Each id gets its own
trusted pool and its own KV/counter namespace.

### `[[upstream]]`

```toml
[[upstream]]
name = "app"                          # required
addresses = ["127.0.0.1:9000", "127.0.0.1:9001"]  # required: host:port instances, round-robined
resolve_interval_ms = 0               # optional (default 0 = off): re-resolve hostname addresses
                                       # on this interval, each A/AAAA record its own LB endpoint
request_timeout_ms = 30000            # optional (default 30000; 0 disables — long-poll/streaming)
overall_timeout_ms = 0                # optional (default 0 = none): bounds every attempt + backoff
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
sni = "internal.example"              # optional: verification-name override (IP-literal / DNS-expanded endpoints)
client_cert_path = "certs/client.pem" # optional: present a client identity (upstream mTLS, ADR 000078)
client_key_path = "certs/client.key"  # required when client_cert_path is set
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
upstream = "app"         # required: the [[upstream]] name to forward a passing request to
filters = ["my-filter"]  # optional: filter ids run in order (empty = pure pass-through)
strip_prefix = "/api"    # optional: strip this prefix before forwarding (the chain saw the original)
[route.match]            # required: the match dimensions, ANDed
path_prefix = "/api"     # required: match requests whose path starts here (longest prefix wins)
host = "example.com"     # optional: match only this authority (case-insensitive); omit = any host
[route.timeouts]         # optional: absent = the upstream's own timeouts apply, unchanged
request_timeout_ms = 30000 # optional: overrides the upstream's per-try bound; 0 disables it here
overall_timeout_ms = 45000 # optional: overrides the upstream's overall bound; 0 = no overall bound
[route.upgrade]          # optional: absent = deny-by-default, no HTTP/1.1 Upgrade tunnelled
protocols = ["websocket"] # required if the section is present; token allowlist, `h2c` rejected
idle_timeout_ms = 300000  # optional (default 300000 = 5 min); 0 disables the idle timer
[route.headers]          # optional: literal response headers, no filter needed
set = { "X-Content-Type-Options" = "nosniff" }  # replaces any same-named header on the way out
remove = ["server"]                             # dropped from every response this route answers
```

`[route.upgrade]` opts a route into tunnelling `HTTP/1.1 Upgrade` (e.g. WebSocket): a listed token
re-issues the handshake upstream and, on a verified `101`, splices a bidirectional byte tunnel with
an activity-reset idle timeout (ADR 000048).

`[route.timeouts]` overrides the two timeouts the upstream declares as defaults (ADR 000102), per
knob and independently: the route's value wins where it declares one, the upstream's stands
everywhere else. Omitting a field is **not** the same as writing `0` — `0` disables that bound for
this route, which is how a streaming route opts out of a short upstream default. Details and the
per-try/overall interaction are in the
[operations guide](operations.md#request-timeouts-which-value-actually-applies).

`[route.headers]` is the constant-header case that does **not** need a filter (ADR 000100): literal
values only, applied after the response chain to every response the route answers — a filter's
`replace` and the fail-closed 5xx included. A header whose value depends on the request is a
per-request decision and stays a filter's job. Details and the one gap (a response returned before
a route is chosen) are in the [operations guide](operations.md#declared-response-headers-which-responses-they-land-on).

### `[[tls]]`

```toml
[[tls]]
cert_path = "certs/app.pem"   # required: PEM cert chain (manifest-relative)
key_path = "certs/app.key"    # required: PEM private key
host = "example.com"          # optional SNI host; omit = the default cert
```

With no `[[tls]]`, the fast path serves plain HTTP/1.1; one or more certs enable TLS termination
(rustls, ADR 000014). Filters run only as part of a `[[route]]`'s `filters`; a manifest that still
declares the older global `[chain]` is rejected at validation, with a diagnostic naming that move.

### `[listen]`

```toml
[listen]
addr = "0.0.0.0:8443"    # optional: data-plane bind (default 127.0.0.1:8080; the CLI arg overrides)
advertised_port = 443    # optional: the port Alt-Svc advertises for h3 when the published port differs

[listen.proxy_protocol]  # optional: PROXY protocol v2 reception (ADR 000057); absent = off
trusted = ["10.0.0.0/8"] # required when present: CIDRs of the L4 LBs allowed to speak PROXY v2

[listen.trusted_proxy]   # optional: client identity from X-Forwarded-For (ADR 000103); absent = off
trusted = ["10.0.0.0/8"] # required when present: CIDRs of the L7 front proxies allowed to name it
```

`[listen]` is captured at startup — a reload does not re-bind or change it; restart to apply.
(The one exception is `[listen.client_auth]`, inbound-mTLS client verification, which a reload
*does* consume — a rotated client-CA bundle is picked up by `SIGHUP`. That block and
`[listen.drain]`, the graceful-shutdown knobs, are covered in the
[hardening](hardening.md) and [operations](operations.md) guides.)
With `[listen.proxy_protocol]`, a peer inside `trusted` MUST open every TCP connection with a
PROXY v2 header (its `LOCAL` command — LB health checks — keeps the real endpoints), and the
restored client address feeds the per-client-IP rate limit, `X-Forwarded-For`/`X-Real-IP`
re-issuing, Maglev `source_ip` hashing, and the access log. Everything else is fail-closed cut:
a missing/malformed header from a trusted peer, a PROXY v2 signature from an untrusted peer, or
any non-TCP/IPv4/IPv6 `PROXY` command. `trusted` takes CIDR notation only (a single host is
`"192.0.2.1/32"`), and the h3 (QUIC/UDP) listener is out of scope — front it with a QUIC-aware
LB only if that LB can pass the client address another way (e.g. Kubernetes
`externalTrafficPolicy: Local`).

`[listen.trusted_proxy]` answers the same question one layer up, for an L7 front proxy that cannot
speak PROXY v2. When the address already resolved for a request falls inside `trusted`, its inbound
`X-Forwarded-For` is read right to left, declared hops are dropped, and the first address no
declared proxy vouched for becomes the client — feeding the same consumers as above. Anything else
falls back to that resolved address: an absent, malformed, or entirely-declared list, and every
request from outside the CIDRs. Only `X-Forwarded-For` is a restoration source, and the scheme
stays the wire truth (an inbound `X-Forwarded-Proto` is never honored). Prefer PROXY v2 when the
front tier can speak it — it restores the client below HTTP, where the request cannot reach.
See the [hardening guide](hardening.md#client-identity-behind-a-front-proxy).

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
manifest's `[trust]` ever references one outside a dev context. For a **production / CI** deploy,
`plecto package` is the one-shot pipeline: it conformance-gates the built component, signs it and
its SBOM with *your* key (ECDSA P-256 PKCS8 PEM, the cosign `sign-blob` scheme), writes the offline
OCI layout, and prints **only** the pinned image-manifest digest to stdout:

```bash
DIGEST=$(plecto package my_filter.component.wasm --key signer.pem --out artifacts/my-filter)
# pin $DIGEST in the manifest's [[filter]].digest, then prove the pair loads — without serving:
plecto validate --resolve manifest.toml
```

`validate --resolve` runs the loader's provenance gate (digest pin + trusted signatures + SBOM
binding) against the real layout, plus a static contract check that targets the component at every
`plecto:filter` world the binary ships ([ADR 000114](ADR/000114.md)) — so CI knows *before* a
deploy that the manifest + artifact pair would pass, and that the filter was not built against a
WIT version this binary no longer carries. Both are what plain `validate` (static, artifact-free)
deliberately does not check. The contract check adds a rejection and nothing else: a pass is not a
load guarantee, and a filter that imports outside the contract (a Go guest's `wasi:*`, an outbound
capability) is reported `contract UNCHECKED` — the manifest decides what is lent to it. `--sbom
<statement.json>` replaces the default minimal in-toto statement; the statement's subject digest
must still be this component's sha256, or the loader refuses the pair. The
[`wasm-auth` example](../plecto/examples/wasm-auth/main.rs) remains a runnable end-to-end
reference of the same pipeline as embedder code.

## 6. Test it locally

The fastest contract-level check, no manifest or upstream needed, is `plecto conformance`
(ADR 000065) against a built component:

```bash
plecto conformance my-filter.component.wasm   # §3's output (JS builds emit dist/*.component.wasm)
```

It self-signs with a throwaway key (never your persistent `.plecto/dev-key`) and checks the
generic properties any `plecto:filter` must have: the component loads under the real signature/SBOM
gate, and it handles a generic request without trapping or exceeding its deadline. It does **not**
check your filter's specific policy (e.g. "does it block the right headers") — that is what §1's
world and your own test requests are for. `plecto dev` (§5) runs this same battery automatically
before every reload.

Since 0.8.0 the battery underneath is **versioned** (`battery@1.0.0`, cases `v1.0.0-S1` /
`v1.0.0-D1`) and scores each case with a five-way verdict: `pass` / `fail` / `na` /
`inconclusive` / `environment`. `environment` means the run could not lend a capability your
component imports — a bare `plecto conformance` lends nothing, so a filter built with
`outbound-http` or `wasi = "minimal"` lands here. That is a diagnosis ("nothing lent it"), not a
defect ("it doesn't satisfy the world"), but it still exits non-zero. The CLI reports both halves
per case: `--json` carries `checks[].id` and `checks[].verdict` next to the `passed` bool, and the
text output prints the verdict as its marker (`[pass]` / `[environment]` / …). What is still
pending is ADR 000108's PIXIT flags (`--battery`, `--manifest --filter`) — until they land,
running the battery with your manifest's grants is a `plecto-host` library call
(`run_conformance_with`).

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

### Start from a practical example

The `filter-hello-*` guests above are *conformance fixtures* — they exercise every host-API on
purpose and are a poor thing to copy. For a filter that does real work, start from
**`filter-tokenlimit-*`** instead: one policy (an LLM token-cost rate limiter — price each request
from its JSON body, spend that price against the host bucket, report what it cost) written three
times, and the **first `plecto:filter@0.4.0` example guests** in the repository. They show the
0.4.0-only shapes a new filter should be written against: `path-with-query`, the bare `%continue`
body arm, and a guest-declared subset world that exports `on-request-body` but deliberately not
`on-response-body`.

| Tier | Language | Example | Start here because |
|---|---|---|---|
| A | JavaScript/TypeScript | [`filter-tokenlimit-js`](../plecto/examples/filters/filter-tokenlimit-js) | The canonical copy-target: the complete build → sign → `validate --resolve` → serve → curl walkthrough lives in its README. |
| A | MoonBit | [`filter-tokenlimit-moonbit`](../plecto/examples/filters/filter-tokenlimit-moonbit) | The smallest build of the three; shows committed wit-bindgen bindings and the UTF-16 → UTF-8 boundary a MoonBit guest has to get right. |
| B | Go | [`filter-tokenlimit-go`](../plecto/examples/filters/filter-tokenlimit-go) | The fat-guest exemplar: `wasi = "minimal"` in the manifest, vendored WIT deps with a drift check, and the Tier B import allowlist asserted at build time. |

All three are held to **one shared assertion battery** in the host test suite, so "same body → same
cost, status, and headers" is a test rather than a claim:

```bash
cargo test -p plecto-host --features polyglot-conformance --test tokenlimit
cargo test -p plecto-host --features polyglot-conformance,fat-guest --test tokenlimit_tier_b
```

They are starters, not shelf artifacts (ADR 000080): copy one, change the cost formula to whatever
your upstream actually bills, and read its README's *Expectations* section first — an
estimate-and-admit limiter never reconciles against actual usage, and that boundary is a design
choice, not a gap.

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
[roadmap](ROADMAP.md) (M6).

## 8. Contract distribution and compatibility policy

Everything above assumes you have `plecto/wit/`. If your filter lives outside this repository —
which is the normal case for a real filter — fetch the contract with the standard WIT toolchain
instead of copying files by hand (ADR 000064). `plecto new-filter --lang rust` (§2) does NOT need
this — it vendors the contract into the binary itself (ADR 000072) — but you do, if you are
scaffolding another language by hand, pinning a specific past contract version, or working with
the experimental streaming contract below.

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

wkg get plecto:filter@0.4.0 --config wkg-registry.toml -o wit/ --format wit
```

That writes the plain-text WIT to `wit/`, ready for `wit_bindgen::generate!` (or any other
language's binding generator) — no `git clone` of this repository required. Pin the version, and
verify the pulled contract against the digest recorded in that tag's [GitHub
Release](https://github.com/Kaikei-e/PlectoProxy/releases) notes before you build against it, the
same fail-closed instinct §5's digest-pinned filter loading already asks of you.

The reference filter **components** are distributed the same way, as individually cosign-signed
CNCF Wasm OCI Artifacts under `ghcr.io/kaikei-e/plecto/filters/<name>` (ADR 000080) —
[reference-filters.md](reference-filters.md) has the shelf, the filter × runtime-profile
compatibility matrix, and the verify-then-package recipe that feeds §5's load path.

The experimental streaming contract publishes the same way, one package over: `plecto:filter-streaming@0.1.0`.
It carries **no compatibility guarantee** — it is the off-by-default `streaming-body` feature's
contract (§3's `filter`/`filter-body` split has no third `streaming` world yet) and may change or
disappear without a major bump. Do not depend on it outside an explicit opt-in build.

### Compatibility policy

The contract's version is **independent of Plecto's own release version** — CHANGELOG.md's
versioning policy already says so. `plecto:filter@0.4.0` and a `plecto` binary at `0.8.x` is the
normal, expected state.

- **SemVer, additive = minor, breaking = major.** A new capability interface, a new optional
  field, a new export on `filter-body` — minor. Removing or changing the signature of an existing
  export or host-API function — major.
- **The host keeps loading every contract version it ships support for.** A `plecto` upgrade does
  not silently break a filter built against an older `plecto:filter` version; the host branches on
  the component's own world version at load time. On a major bump, the previous major stays
  accepted for **at least two release series** before its removal is declared — via ADR, the same
  way any other deprecation in this project is declared (never silently). This is not aspiration:
  `0.1.0` / `0.2.0` components load on today's host via frozen contract trees + load-time adapters.
- **The promise is staged, and gets stronger at contract 1.0** (ADR 000085). Everything above is
  the 0.x policy. From `plecto:filter` 1.0 onward, **every shipped world stays loadable
  permanently** — the sole exception is "keeping this world loadable is itself unsafe to
  maintain", and even that requires a dedicated ADR, **at least 24 months' notice**, and a
  migration document. Cutting 1.0 is the act that brings this pledge into force (which is why 1.0
  waits for the `wasi:http` convergence major to settle first).
- **`filter` vs. `filter-body` compatibility is part of this policy** (ADR 000038 / 000098): the base
  `filter` world exporting nothing new stays minor-compatible forever by construction (the
  *absence* of a body hook is itself contractual, not an oversight). Adding an export to
  `filter-body` is minor; changing an existing export's signature (on either world) is major.

### Rebuilding a 0.3.0 filter against 0.4.0

An already-deployed 0.3.0 component needs **no** rebuild — it keeps loading. Rebuilding its
*source* against 0.4.0 takes two mechanical edits:

- `req.path` → `req.path_with_query` (`http-request.path` → `path-with-query`, ADR 000104). Same
  value, query included; only the name changed, so the compiler finds every site.
- `RequestBodyDecision::Continue(bytes)` → either `Continue` (you did not change the body) or
  `Modified(RequestBodyEdit { body: bytes, set_headers: vec![], remove_headers: vec![] })` (you
  did). The split is the point: `%continue` no longer carries a body, so an inspecting filter
  stops paying to hand the bytes back (ADR 000098). When in doubt, `Modified` is always
  behaviour-preserving — it is exactly what the host's 0.3 adapter does with your old `continue`.

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
| `PLECTO-E0002` | `429` response header | the request exceeded the route's native rate-limit floor (`[route.rate_limit]`, checked before the chain runs) | raise the route's `rate` / `burst`, or have the client back off per the 429's `retry-after` header ([ADR 000033](ADR/000033.md)) |
| `PLECTO-E0003` | `400` response header | the request path failed normalization (`..` traversal, invalid percent-encoding, or a raw control byte) | this rejects the client's request, not your manifest — check what the client sends as the path ([ADR 000013](ADR/000013.md)) |
| `PLECTO-E0004` | `plecto validate` warning | a `[trust]` key file carries the dev-key marker (generated by `plecto dev` / `plecto new-filter`) | expected for a dev manifest; for production, replace it with a key from your real signing pipeline ([ADR 000065](ADR/000065.md)) |
