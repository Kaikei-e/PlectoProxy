# Quick start — verified image to first proxied response

[日本語](README.ja.md)

This is the operator quick start: pull the Plecto Proxy container image, **verify its
signature**, and get your first proxied response — with nothing installed but Docker.
Every command is copy-paste; nothing is hidden in a script. Signature verification is
part of the flow, not an optional extra ([ADR 000084](../ADR/000084.md) /
[ADR 000087](../ADR/000087.md)).

Target: from opening this page to the first proxied response in **under 5 minutes**.
That assumes the image layers are already cached locally, or a typical broadband
connection — on a cold pull, download time dominates the budget.
If it took you longer — or you got stuck — please
[tell us where](https://github.com/Kaikei-e/PlectoProxy/discussions): first-run friction
reports are how this page gets better.

**Prerequisite:** Docker (a current version; `docker buildx` ships with it).

## 1. Resolve the release tag to an immutable digest

Releases are cosign-signed **by image digest, never by tag** — a tag can move, a digest
cannot. So first pin the tag you want to the digest you will verify *and* run:

```bash
IMAGE=ghcr.io/kaikei-e/plecto
TAG=0.3.8   # pick the latest release: https://github.com/Kaikei-e/PlectoProxy/releases

DIGEST=$(docker buildx imagetools inspect "$IMAGE:$TAG" --format '{{json .Manifest.Digest}}' | tr -d '"')
echo "$DIGEST"
```

You can cross-check the printed digest against the one recorded in that release's notes.

## 2. Verify the signature

cosign runs from Sigstore's own published container, so there is nothing to install.
The identity flags pin the signer to this repository's release workflow — issuer alone
would match any GitHub Actions workflow:

```bash
docker run --rm ghcr.io/sigstore/cosign/cosign:v3.1.1 verify "$IMAGE@$DIGEST" \
  --certificate-identity-regexp 'https://github.com/Kaikei-e/PlectoProxy/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

A successful run prints the verified claims (a JSON array). If verification fails,
**stop** — do not run the image.

<details>
<summary>Prefer a locally installed cosign?</summary>

Install cosign with your package manager (`brew install cosign`, `apk add cosign`,
`pacman -S cosign`, or a signed release binary from
[sigstore/cosign](https://github.com/sigstore/cosign/releases)), then run the same
`cosign verify` command without the `docker run --rm ghcr.io/sigstore/cosign/cosign:v3.1.1`
prefix.

</details>

## 3. Run the verified digest

Plecto is configured by one TOML manifest. Write a minimal one — listen on 8080,
forward everything to a backend — and start a stand-in backend next to it:

```bash
mkdir -p plecto-quickstart && cd plecto-quickstart

cat > plecto.toml <<'EOF'
[listen]
addr = "0.0.0.0:8080"

[[upstream]]
name = "backend"
addresses = ["backend:80"]
[upstream.health]
path = "/"
interval_ms = 1000

[[route]]
upstream = "backend"
[route.match]
path_prefix = "/"
EOF

docker network create plecto-quickstart
docker run -d --name backend --network plecto-quickstart traefik/whoami
docker run -d --name plecto --network plecto-quickstart -p 18080:8080 \
  -v "$PWD:/etc/plecto:ro" "$IMAGE@$DIGEST"
```

Host port `18080` avoids colliding with whatever else is already bound to `8080` on
your machine. If you'd rather use `8080` (matching the container's internal listen
port), change the mapping to `-p 8080:8080` and update the `curl` command in the next
step to match.

Note that the proxy runs **the digest you verified** — not the tag. The backend
(`traefik/whoami`, a tiny echo server) is a stand-in for your own service; it is *not*
covered by the verification above. Plecto's supply-chain claims are about what **Plecto**
loads and runs, never about your upstreams.

## 4. First proxied response

```bash
curl -s http://localhost:18080/
```

You should see the whoami response — proxied through a signature-verified Plecto. That's
the whole loop: **resolve → verify → run → respond**.

## Troubleshooting

**`docker run` fails, or a retry says a name is already in use.** If step 3 failed
partway (for example the host port was already taken), Docker can leave a container
behind in the `Created` state still holding the `plecto` or `backend` name — a common
trap with `docker run`. Remove the stale containers before retrying:

```bash
docker rm -f plecto backend
```

Then re-run the `docker run` commands from step 3.

## Clean up

```bash
docker rm -f plecto backend
docker network rm plecto-quickstart
cd .. && rm -r plecto-quickstart
```

## Where to next

- **Multiple replicas behind a load balancer** — the runnable multi-replica reference
  (graceful drain, PROXY protocol v2, TLS scenarios):
  [`plecto/examples/multi-replica/`](../../plecto/examples/multi-replica/README.md).
  Grab just that directory without a full clone:
  ```bash
  git clone --depth 1 --filter=blob:none --sparse https://github.com/Kaikei-e/PlectoProxy
  cd PlectoProxy && git sparse-checkout set plecto/examples/multi-replica
  ```
- **Write a filter** — the extension plane is the point:
  [docs/writing-a-filter.md](../writing-a-filter.md)
- **Signed reference filters** (JWT, CORS, API-key, ext-authz) and their verify-then-load
  recipe: [docs/reference-filters.md](../reference-filters.md)
- **Operations** (readiness, drain, hot reload): [docs/operations.md](../operations.md)
- **Runtime capability profiles** — this page used the `minimal` profile; the
  `-capabilities` image adds outbound capabilities for the filters that need them
  ([ADR 000079](../ADR/000079.md))
