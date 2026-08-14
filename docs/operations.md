# Operations guide

How to run Plecto Proxy behind a fleet: the shutdown/readiness contract a front load balancer can
rely on, and the knobs that tune it. Companion to the [hardening guide](hardening.md) (which
covers multi-instance state semantics); this page covers process lifecycle.

## Graceful shutdown: the contract

On `SIGTERM` / `SIGINT`, a `plecto` process runs this sequence, in this order
([ADR 000039](ADR/000039.md), [ADR 000059](ADR/000059.md)):

1. **`/readyz` flips to `503 draining`** — immediately, before anything else changes. New
   connections are still accepted and served normally.
2. **The readiness grace elapses** (`[listen.drain] readiness_grace_ms`, default `0`). This is
   the time your load balancer needs to observe the 503 and take the replica out of rotation.
   With the default `0`, this step collapses and the drain starts at once.
3. **The drain starts.** The listeners stop accepting. Every open connection is told to finish
   its in-flight work and close: HTTP/1.1 keep-alive stops, HTTP/2 and HTTP/3 send GOAWAY
   (h3 clients can safely retry rejected requests elsewhere — they are refused with
   `H3_REQUEST_REJECTED`). Upgrade tunnels (WebSocket) are closed — a long-lived tunnel does
   not get to hold the drain open.
4. **The drain window bounds step 3** (`[listen.drain] window_ms`, default `30000`). One
   window, shared by every path — TCP requests, h3 requests, tunnels. Whatever is still open
   when it expires is cut (fail-closed).
5. The process exits `0`.

`/healthz` (liveness) stays `200` through all of it: a draining process is exiting on purpose,
and a liveness probe that restarted it would defeat the drain. Point your LB's rotation checks
at `/readyz`, and any restart-supervisor checks at `/healthz`.

```toml
[listen.drain]
readiness_grace_ms = 5000   # ≥ your LB's health-check interval × unhealthy threshold
window_ms = 30000           # how long in-flight work may finish
```

Both endpoints live on the admin listener (`[observability] admin_addr`), which is off by
default — zero-downtime rollouts behind an LB require it to be set. Note that `[listen.drain]`
and `[observability]` are captured at startup: changing them is a restart, not a reload
(see [Reload vs restart](#reload-vs-restart)).

## Probing from inside the container: `plecto healthz`

The official image is distroless — no shell, no curl — so a Docker/Compose `healthcheck:` cannot
shell out to anything. `plecto healthz` is the self-probe: it reads `[observability] admin_addr`
from the manifest (no second copy of the address in the Compose file), performs one bounded
HTTP/1.1 GET, and exits `0` on a 2xx response, `1` on anything else — never `2`, which the
Docker healthcheck contract reserves. It probes `/readyz` by default, because a Compose
`depends_on: condition: service_healthy` is a start-ordering gate — readiness semantics; pass
`--live` to probe `/healthz` for a restart supervisor, or `--admin-addr <host:port>` to skip the
manifest lookup.

```yaml
# distroless: exec-array form only — a string test: would need the /bin/sh the image lacks
healthcheck:
  test: ["CMD", "/usr/local/bin/plecto", "healthz", "/etc/plecto/manifest.toml"]
  interval: 30s
  timeout: 5s
  retries: 3
```

In Kubernetes, prefer `httpGet` probes straight at the admin endpoints (the kubelet probes from
outside the container; nothing needs to run in the image) — the table below already points them
at `/readyz` / `/healthz`.

## Choosing `readiness_grace_ms`

The rule: **the grace must cover the time between the first failed readiness check and the LB
actually removing the replica.** If the LB is still routing to the replica when the drain
starts, those clients see refused connections — the exact blip the contract exists to prevent.

| Front | What to set |
| --- | --- |
| No LB (direct clients, single instance) | `0` (the default). Nothing watches `/readyz`; a grace only delays shutdown. |
| Kubernetes | ≥ readiness probe `periodSeconds × failureThreshold` of the Pod. Point the readinessProbe at `/readyz`, the livenessProbe at `/healthz`. |
| Active health checks (interval × consecutive failures) | ≥ that product (plus any post-fail hold-down your front LB applies). |
| Passive health checks (interval × unhealthy threshold) | ≥ that product. |
| DNS-based routing | ≥ record TTL. If the TTL is minutes, prefer removing the record first and only then signalling. |

Orchestrators that remove the replica from rotation *before* delivering `SIGTERM` (Kubernetes
does, once the endpoint leaves the EndpointSlice) shrink the needed grace — but the readiness
probe is still what triggers that removal, so the probe-derived bound above stays the safe
choice.

`window_ms` is a separate concern: it bounds how long **accepted** work may finish. Size it to
your slowest legitimate request (the default 30 s matches the default per-try upstream timeout
and the common 30 s supervisor kill grace — e.g. Kubernetes `terminationGracePeriodSeconds`,
which must exceed `readiness_grace_ms + window_ms`).

## Watching a drain (and tunnels)

The admin `/metrics` endpoint exposes, alongside the RED signals:

- `plecto_requests_in_flight` — requests currently being served; a drain waits for these.
- `plecto_tunnels_active` — upgrade tunnels currently open ([ADR 000048](ADR/000048.md)).
  Each holds a circuit-breaker permit and a load-balancer pick for its whole life, so this
  gauge is the first thing to check when a breaker opens or least-request skews without
  matching request volume. It is also what a drain will cut: tunnels do not outlive shutdown.
- `plecto_tunnel_bytes_down_total` / `plecto_tunnel_bytes_up_total` — bytes relayed
  downstream (upstream → client) and upstream (client → upstream) by tunnels, recorded as
  each tunnel closes.

## The access log: field contract

The access log is opt-in and **off by default**. Turn it on with `[observability] access_log`
(captured at startup, like the rest of `[observability]` — turning it on is a restart):

```toml
[observability]
access_log = true
```

Plecto then emits one `tracing` event per request on the `plecto::access` target, and the binary's
JSON subscriber renders it as one line. The event's fields sit at the **top level** of that line,
beside `timestamp` / `level` / `target` — an ingestion layer can map them straight into typed
slots without unwrapping a nested object first.

```json
{"timestamp":"...","level":"INFO","client":"203.0.113.7","scheme":"https","method":"GET","authority":"api.example.com","path":"/v1/items","status":200,"duration_ms":12,"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","span_id":"00f067aa0ba902b7","message":"access","target":"plecto::access"}
```

> **Migrating from a release before this line was flattened:** the same fields used to sit inside
> a `fields` object (`fields.method`, `fields.status`, …). The names are unchanged; only the
> nesting is gone. Point your ingestion mapping at the line root.

| Field | Type | Meaning |
| --- | --- | --- |
| `client` | string | The address the transaction is attributed to: the connection peer, or — behind a declared `[listen.trusted_proxy]` ([ADR 000103](ADR/000103.md)) — the client that proxy named. The same address the re-issued `X-Forwarded-For` and the per-client rate limit use, so the log agrees with enforcement. |
| `scheme` | string | `http` or `https` — taken from the wire, never from an inbound `X-Forwarded-Proto`. |
| `method` | string | The request method as received. |
| `authority` | string | The request's host authority. |
| `path` | string | The request path **without its query string**. |
| `status` | number | The status Plecto returned to the client. A transport error the proxy could not answer is recorded as `502`. |
| `duration_ms` | number | Whole milliseconds from the start of the transaction to the response head. |
| `trace_id` | string | W3C trace id (32 lowercase hex chars) — the caller's, when it sent a `traceparent`, otherwise one Plecto minted. |
| `span_id` | string | W3C span id (16 lowercase hex chars) of Plecto's own request span — the id it propagates upstream. |
| `response_body_inspection_skipped` | string (absent otherwise) | Why a route that declared an `on-response-body` filter served a response that filter never saw ([ADR 000098](ADR/000098.md)): `streaming-content-type`, `content-encoding`, `partial-content`, or `over-cap`. On every other transaction — including every route with no such filter — the key is **absent**, not `null`; don't pin an ingestion mapping on a null value. |

Two properties this line is meant to keep:

- **It carries no secrets.** No `Authorization`, no `Cookie`, no header values at all, and the path
  is logged without its query string. Sending an access log to a lower-trust destination than the
  traffic itself is therefore not, on its own, a disclosure.
- **`trace_id` / `span_id` are always present, sampled or not.** They join the line to whatever
  sampling kept downstream, and for an unsampled transaction the line is the only handle on it.
  Correlating a slow request with its trace is `trace_id` on both sides.

**The field set is a contract, on the same footing as the manifest schema.** Adding, renaming, or
removing a field is a change to a published interface: it is listed under **Changed** in
[`CHANGELOG.md`](../CHANGELOG.md) with a migration note, under the pre-1.0 versioning policy stated
there. Pin your ingestion mapping to the field names above rather than to field order or to the
line's overall shape.

## Declared response headers: which responses they land on

A route can declare the response headers it always wants, without a filter:

```toml
[[route]]
upstream = "app"
[route.match]
path_prefix = "/"
[route.headers]
set = { "X-Content-Type-Options" = "nosniff", "Referrer-Policy" = "no-referrer" }
remove = ["server"]
```

Both keys are optional, but the block must declare at least one of them. **Values are literals** —
no conditionals, no interpolation from the request, no patterns. A header whose value depends on
the request is a per-request decision, which is a filter's job
([ADR 000029](ADR/000029.md) · [ADR 000100](ADR/000100.md)).

**The declaration is a floor, not a suggestion.** It is applied after the response filter chain and
before compression, so:

- `set` **replaces** any same-named header the upstream or a filter produced (every copy of it, if
  there were several), and `remove` drops the name entirely. `remove` runs first, so a name in both
  lists ends up set.
- Header names are matched case-insensitively — `X-Frame-Options` and `x-frame-options` are the same
  declaration, and declaring both in one `set` is rejected as ambiguous.
- It lands on **every response the route answers**, including the ones you cannot see coming: a
  filter's `replace`, a filter's short-circuit, a chain's fail-closed 5xx, the native rate limit's
  429, and the forward-side 502 / 503 / 504. A security header whose whole value is that it does
  not disappear when things break therefore does not disappear when things break.

**One gap, deliberate.** A response returned **before** a route is chosen carries no declaration,
because there is no route to take it from. That is the no-route **404** and the path-normalization
**400** (an ambiguous or root-escaping request target, rejected at ingress). If you need a header on
those too, terminate elsewhere or accept the gap — a listener-wide declaration is not offered.

**Validation is fail-closed.** An invalid header name or value fails `plecto validate`, startup, and
reload rather than being dropped at request time. So does naming a hop-by-hop header
(`connection`, `transfer-encoding`, `upgrade`, `te`, `trailer`, `keep-alive`, `proxy-connection`,
`proxy-authorization`, `proxy-authenticate`) or `content-length`: connection management belongs to
the transport, and the length belongs to the body Plecto actually sends — a declared one would be a
response-desync primitive.

## Request timeouts: which value actually applies

Two bounds govern a forwarded request, and an upstream declares the default for both:

| Knob | What it bounds | Default | `0` means |
| --- | --- | --- | --- |
| `request_timeout_ms` (per-try) | one attempt's time to the response **headers**; the body then streams without a deadline | `30000` | no per-try bound — the long-poll / streaming opt-out |
| `overall_timeout_ms` | the whole transaction: every attempt **plus** the backoff between them | `0` | no overall bound |

A route can override either of them for its own traffic, so routes with genuinely different latency
budgets share one upstream instead of forcing a duplicate `[[upstream]]` (which would duplicate its
health prober and split its circuit-breaker state):

```toml
[[upstream]]
name = "app"
addresses = ["127.0.0.1:9000"]
request_timeout_ms = 5000     # the default every route to this upstream inherits
[upstream.health]
path = "/healthz"

[[route]]                     # inherits 5000
upstream = "app"
[route.match]
path_prefix = "/api"

[[route]]                     # same upstream, a longer budget
upstream = "app"
[route.match]
path_prefix = "/images/resize"
[route.timeouts]
request_timeout_ms = 30000
overall_timeout_ms = 45000

[[route]]                     # same upstream, no per-try bound at all
upstream = "app"
[route.match]
path_prefix = "/events"
[route.timeouts]
request_timeout_ms = 0
```

**The resolution order is one rule: the route's value if it declares one, otherwise the upstream's**
— per knob, independently. A route that sets only `overall_timeout_ms` still runs on the upstream's
per-try value.

**Omitting a field and writing `0` are different things.** Omitted takes the upstream's value; `0`
disables that bound for this route. `[route.timeouts] request_timeout_ms = 0` is therefore how a
streaming route opts out of a short upstream default — it is not the same as leaving the block out.

**Both bounds apply together, and the tighter one wins.** Each attempt is bounded by the per-try
value *and* by whatever is left of the overall budget, whichever is smaller; the overall budget keeps
shrinking as attempts and backoff spend it. An `overall_timeout_ms` smaller than the per-try value is
not rejected — the runtime simply applies the smaller one, so a single attempt may be cut short.

Exceeding either fails closed with **504**, and the fault marker says which one:
`x-plecto-fault: upstream-timeout` for a per-try expiry, `request-timeout` for the overall deadline.

Two things this deliberately does not do. Nothing renders the *resolved* value: to know what a route
runs under, read its `[route.timeouts]` and then its upstream's fields. And `max_retries`,
`[upstream.circuit_breaker]`, and `[upstream.outlier_detection]` stay per-upstream — they describe
the backend (how much load it may take, whether it is broken), not this route's time budget
([ADR 000102](ADR/000102.md)).

## Upgrading: two independent version series

Plecto ships **two version series, and they move independently**:

| Series | What it versions | Where you see it |
| --- | --- | --- |
| Binary / image / library crates | The proxy itself: manifest schema, CLI, data plane, host | `plecto --version`, the image tag, the crate versions |
| `plecto:filter@<version>` | The WIT contract between the proxy and your filters | `plecto --version`'s `filter contracts:` line, and one startup log line per filter |

**Bumping the proxy never requires rebuilding a filter.** The host keeps loading every contract
version it ships support for, so a filter built against an older contract keeps running across
proxy upgrades — including patch upgrades that carry security fixes. Take them.

The two places that answer the two different questions:

```console
$ plecto --version
plecto 0.8.0 (profile: minimal)
filter contracts: plecto:filter@0.1.0, plecto:filter@0.2.0, plecto:filter@0.3.0, plecto:filter@0.4.0
```

That is what **this binary accepts**. What each of **your** filters actually bound is a separate
question, answered at startup (and on every reload) by one line per filter:

```json
{"timestamp":"...","level":"INFO","filter":"hello","contract":"plecto:filter@0.3.0","isolation":"trusted","message":"filter loaded","target":"plecto_control"}
```

Before an upgrade, check that every `contract` you see there is still on the new binary's
`filter contracts:` list. If it is — and it will be, unless the release notes declare a contract
version retired — the upgrade needs no filter work at all. A major contract version stays loadable
for at least two release series and its retirement is declared in its own ADR, per the
compatibility policy in [ADR 000085](ADR/000085.md); it is never dropped silently.

## CI pre-flight: `plecto validate --resolve`

A manifest edit or a filter digest bump should fail in CI, not at reload time. `plecto validate
<manifest.toml>` runs every fail-closed startup check that needs no artifact — strict parse,
route/upstream/TLS checks — and mutates nothing (no state file is created), so it is safe to run
against the production manifest anywhere. `--resolve` extends it to the artifact layer: each
`[[filter]]`'s OCI layout is resolved, the pinned digest is compared, and the loader's provenance
gate runs — trusted component/SBOM signatures plus SBOM↔component binding — still with no serving,
no wasmtime, no state ([ADR 000094](ADR/000094.md)).

The gate is the same function the loader calls at startup and on `SIGHUP`, not a re-implementation,
so a green pre-flight and a green load cannot drift apart at the artifact layer. Exit code is the
contract: `0` when everything would load, non-zero otherwise (one `filter <id> OK: artifact
verified (<digest>)` line per filter on success).

```bash
plecto validate manifest.toml            # static: config alone
plecto validate --resolve manifest.toml  # + digest pins, signatures, SBOM binding
```

Two things stay load-time-only, by design: contract-version support and trusted `init()` behaviour
need compile/instantiate, which would break validate's "mutates nothing" contract — both still fail
closed at the actual load. The authoring-side pipeline that feeds this check (`plecto conformance` →
`plecto package` → pin the printed digest) is in [writing a filter §5](writing-a-filter.md); since
0.8.0 the underlying gate scores each case with a five-way verdict, and everything except `pass`
and `na` — including `environment`, a run that could not lend a capability the component imports —
keeps the exit code non-zero.

## Reload vs restart

Configuration changes do not need this machinery at all: `SIGHUP` re-reads the manifest and
swaps it atomically, fail-closed, with zero connection impact ([ADR 000039](ADR/000039.md)).
Reach for the shutdown sequence only when the *binary or host* must go away — deploys, node
drains — and let rolling replicas + the readiness contract make that invisible to clients.

**Rotating a file the manifest points at counts as a config change.** The reload gate digests
the referenced files' bytes, not just their paths, so overwriting a `[[tls]]` certificate and
key, an `[upstream.tls]` CA or client identity, the `[resumption]` STEK, or a
`[filter.config_files]` secret **in place** and sending `SIGHUP` rebuilds and swaps — no manifest
edit needed. A certbot deploy hook is therefore just `cp` (or the renewal itself) followed by
`kill -HUP`. Two halves back that: public material (certificates, CA bundles) rides the logged
config version, while secret material (private keys, the STEK, config-file values) rides a
separate fingerprint that is deliberately never logged — a logged digest over a low-entropy
secret would be an offline brute-force oracle. Two things still need a restart rather than a
reload: `[trust]` and `[state]`, both rejected fail-closed by `SIGHUP`. A third group is
**startup-fixed rather than rejected**: the listener half of `[listen]` (`addr`,
`advertised_port`, `proxy_protocol`, `trusted_proxy`, `drain` — everything except
`[listen.client_auth]`, which a reload does consume) and all of `[observability]` are captured
when the process starts. Editing only those sections leaves the config version unchanged, so
`SIGHUP` logs "unchanged" and swaps nothing — plan a restart for them.
