# Multi-replica reference — L4 LB → Plecto ×2 → backend ×2

The runnable form of Plecto's multi-replica story ([ADR 000082](../../../docs/ADR/000082.md) /
[ADR 000088](../../../docs/ADR/000088.md)): a front **L4 load balancer** sends
**PROXY protocol v2** to two Plecto replicas, each health-checked on `/readyz`, each
load-balancing over two backends. One `docker compose up`, then a handful of scripts
prove the properties instead of asserting them:

- **drain one replica → zero dropped requests** (readiness grace + graceful drain)
- **TLS resumption survives replica hops** (scenario A: shared STEK)
- **no client certificate → no handshake** (scenario B: downstream mTLS)

This is a *reference*, not "the one correct production topology": compose-local DNS,
a fixed demo subnet, and self-signed certificates are demo conveniences, called out
inline where they appear.

> **Prerequisites:** Docker with Compose v2.24+ (the scenario overrides use the
> `!override` YAML tag), `curl`, and `openssl` for the TLS scenarios. The Plecto image
> is the released, cosign-signed one — to verify it first, follow the
> [quick start](../../../docs/quickstart/README.md); pin your verified digest with
> `export PLECTO_IMAGE=ghcr.io/kaikei-e/plecto@sha256:...`.

## The L4 slot

The front slot stands in for whatever L4 tier you already have (a cloud NLB, keepalived
pair, or anycast edge). The skeleton needs two capabilities from it:

1. **send PROXY protocol v2** to the proxied connections, so replicas restore the real
   client address ([ADR 000057](../../../docs/ADR/000057.md)), and
2. **actively health-check a different port** than the traffic port — Plecto's `/readyz`
   lives on the admin listener, and flips to 503 *before* a drain starts
   ([ADR 000059](../../../docs/ADR/000059.md)).

Any L4 load balancer with those two capabilities fills the slot the same way. This
reference uses **HAProxy** as its example implementation (`lb/haproxy.cfg`, pinned to
the 3.4 LTS line); its role here is the L4 stand-in, nothing more. The PROXY protocol
itself is the public specification maintained by HAProxy Technologies, which Plecto's
receive side is implemented from ([ADR 000057](../../../docs/ADR/000057.md)).

*HAProxy is a trademark of HAProxy Technologies. This project is not affiliated with or
endorsed by HAProxy Technologies.*

## Base skeleton — drain without drops

```bash
docker compose up -d
curl -s http://localhost:8080/        # a whoami response, via LB → plecto → backend
scripts/verify-drain.sh               # stops plecto-1 mid-traffic; asserts 0 failures
docker compose down
```

What the drain proof exercises: `docker compose stop plecto-1` sends SIGTERM →
`/readyz` flips to 503 while the replica **keeps accepting** for `readiness_grace_ms`
(3s) → the LB's checks (`inter 1s fall 2`) take it out of rotation → the drain window
finishes in-flight work → exit. The constant curl loop through the LB never sees a
failure.

## Scenario A — TLS termination + shared STEK

TLS terminates at each replica, and the session-ticket key is **shared**
([ADR 000062](../../../docs/ADR/000062.md) opt-in): a ticket issued by one replica
resumes on the other, so clients keep their resumption when the LB re-balances them.

```bash
scripts/gen-certs.sh                  # demo-only credentials into ./manifests/secrets
export PLECTO_UID=$(id -u)            # replicas run as you, to read your 0600 key files
docker compose -f compose.yaml -f compose.scenario-a.yaml up -d
curl -sk https://localhost:8443/      # via the LB (TLS passthrough)
scripts/verify-resumption.sh          # ticket from plecto-1 reused on plecto-2
docker compose -f compose.yaml -f compose.scenario-a.yaml down
```

## Scenario B — downstream mTLS, per-node resumption

Every handshake requires a client certificate chaining to the demo CA
([ADR 000078](../../../docs/ADR/000078.md)). Shared STEK is **not** loaded — resumption
would skip client-certificate re-verification, so the combination fails the build
closed ([ADR 000062](../../../docs/ADR/000062.md)); add `[resumption]` to
`manifests/scenario-b.toml` if you want to watch it refuse.

```bash
scripts/gen-certs.sh                  # (once; also generates the client CA + cert)
export PLECTO_UID=$(id -u)
docker compose -f compose.yaml -f compose.scenario-b.yaml up -d
scripts/verify-mtls.sh                # no cert → refused; demo cert → proxied 200
docker compose -f compose.yaml -f compose.scenario-b.yaml down
```

## Scenario C — upstream mTLS (future leg)

Re-encrypting the upstream leg with a client identity (`[[upstream]]`
`client_cert_path`, independent of the downstream mode —
[ADR 000078](../../../docs/ADR/000078.md)) is not wired into this reference yet; it is
the declared next leg, kept out of the initial scope to hold the scenario count at
three ([ADR 000082](../../../docs/ADR/000082.md)).

## What is deliberately NOT here

- **Global rate limiting.** The Redis-backed global reference filter stays out of this
  skeleton until its secure path lands ([ADR 000081](../../../docs/ADR/000081.md)) — a
  demo-only leg would muddy what this reference claims.
- **Kubernetes manifests.** Compose comes first ([ADR 000082](../../../docs/ADR/000082.md));
  k8s artifacts wait for operator-trial demand.

## Layout

```
multi-replica/
├── compose.yaml                # base skeleton (plaintext :8080)
├── compose.scenario-a.yaml     # override: TLS + shared STEK (:8443)
├── compose.scenario-b.yaml     # override: downstream mTLS (:8443)
├── lb/haproxy.cfg              # the L4 slot's example implementation
├── manifests/                  # one Plecto manifest per scenario
│   └── secrets/                # generated demo credentials (git-ignored)
└── scripts/                    # gen-certs / verify-drain / verify-resumption / verify-mtls
```
