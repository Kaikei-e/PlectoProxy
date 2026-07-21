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
default — zero-downtime rollouts behind an LB require it to be set.

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
| Passive-plane health checks (interval × unhealthy threshold) | ≥ that product. |
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
`plecto package` → pin the printed digest) is in [writing a filter §5](writing-a-filter.md).

## Reload vs restart

Configuration changes do not need this machinery at all: `SIGHUP` re-reads the manifest and
swaps it atomically, fail-closed, with zero connection impact ([ADR 000039](ADR/000039.md)).
Reach for the shutdown sequence only when the *binary or host* must go away — deploys, node
drains — and let rolling replicas + the readiness contract make that invisible to clients.
