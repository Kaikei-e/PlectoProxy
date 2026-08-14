# Install

[日本語](install.ja.md)

Three ways to get Plecto Proxy, in descending order of provenance. For a first run, take the
container route and follow the [quick start](quickstart/README.md) — it walks the same
verification below, plus a manifest and a first proxied response, end to end.

## 1. Container image (recommended)

Verify the signature, then run the digest you verified — not the tag
([ADR 000084](ADR/000084.md) / [ADR 000087](ADR/000087.md)):

```bash
IMAGE=ghcr.io/kaikei-e/plecto
TAG=0.8.0   # pick the latest release: https://github.com/Kaikei-e/PlectoProxy/releases
DIGEST=$(docker buildx imagetools inspect "$IMAGE:$TAG" --format '{{json .Manifest.Digest}}' | tr -d '"')

docker run --rm ghcr.io/sigstore/cosign/cosign:v3.1.1 verify "$IMAGE@$DIGEST" \
  --certificate-identity-regexp 'https://github.com/Kaikei-e/PlectoProxy/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The image is distroless — no shell, no curl — so a Compose or Kubernetes healthcheck cannot shell
out. `plecto healthz` is the self-probe built for exactly that; see the
[operations guide](operations.md).

## 2. Signed release binary

Every [tagged release](https://github.com/Kaikei-e/PlectoProxy/releases) attaches
`plecto-<tag>-<target>.tar.gz` with a `cosign` bundle and a signed checksum beside it:

```bash
cosign verify-blob --bundle plecto-<tag>-<target>.tar.gz.sigstore.json \
  --certificate-identity-regexp 'https://github.com/Kaikei-e/PlectoProxy/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  plecto-<tag>-<target>.tar.gz
```

Binaries are built with `cargo-auditable` (the dependency graph is embedded in the binary itself)
and ship an SPDX SBOM ([ADR 000047](ADR/000047.md)). The exact commands for each release are in
that release's notes and in [`release.yml`](../.github/workflows/release.yml)'s header comment.

## 3. From crates.io

```bash
cargo install plecto
```

This builds the gateway and its operator CLI from source
([ADR 000090](ADR/000090.md) / [ADR 000091](ADR/000091.md)). It carries **no release
provenance** — you compiled it, so there is no signature to verify and no SBOM attached. Prefer
route 1 or 2 for a deployment; `cargo install` is for development and for embedding.

The three library crates publish alongside the binary, for building Plecto Proxy into your own
program rather than running it as one:

| Crate | What it is |
| --- | --- |
| [`plecto-host`](https://crates.io/crates/plecto-host) | The wasmtime embedding: `Linker`, `InstancePre`, the deny-by-default host-API, instance lifecycle. |
| [`plecto-control`](https://crates.io/crates/plecto-control) | The control plane: declarative manifest, OCI artifact load + provenance gate, filter-chain dispatch, atomic reload. |
| [`plecto-server`](https://crates.io/crates/plecto-server) | The fast path as a library: HTTP/1.1 · 2 · 3, TLS, routing, load balancing, upstream management. |

A CI job diffs each release's public API against the newest version already published, so an
unbumped breaking change fails before it ships rather than in a consumer's build — with one
honest bound: the tool compares type paths, so a break hiding behind an unchanged path (such as
a cross-crate re-export) still needs the manual judgement the CHANGELOG's versioning policy
reserves.

## Runtime capability profiles

Prebuilt binaries and images come in two **named runtime capability profiles**
([ADR 000079](ADR/000079.md)):

| Profile | Binary / image tag | What is compiled in |
| --- | --- | --- |
| **minimal** (unsuffixed, the default) | `plecto-<tag>-<target>.tar.gz` · `ghcr.io/kaikei-e/plecto:<version>` | Default features only — no outbound code is compiled in. The smallest attack surface; pick this for a plain reverse proxy / gateway. |
| **capabilities** | `plecto-<tag>-<target>-capabilities.tar.gz` · `ghcr.io/kaikei-e/plecto:<version>-capabilities` | Adds the `outbound-http`, `outbound-tcp`, and `fat-guest` capabilities — what the capability-backed reference filters (JWKS-refreshing JWT auth, ext-authz, the Redis-backed global rate limit) and Go/TinyGo guests need. |

**Compiling a capability in is not granting it.** A capabilities binary lends nothing to any filter
until the manifest declares that capability for that filter — the deny-by-default allowlist and the
SSRF floor apply unchanged ([ADR 000036](ADR/000036.md) / [ADR 000060](ADR/000060.md)).
`plecto --version` prints which profile a binary was compiled as.

## Reference filters

Filters ship separately from the runtime, as individually cosign-signed CNCF Wasm OCI Artifacts
with SPDX SBOM attestations under `ghcr.io/kaikei-e/plecto/filters/<name>`
([ADR 000080](ADR/000080.md)) — currently `jwt`, `cors`, `apikey`, and `extauthz`. Which filter
needs which runtime profile, and the verify-then-load recipe, are in
[reference-filters.md](reference-filters.md).

## Building from a clone

The repository pins its toolchain and WASM target in
[`plecto/rust-toolchain.toml`](../plecto/rust-toolchain.toml), so
[`rustup`](https://rustup.rs/) sets it up on the first build (outside that toolchain:
`rustup target add wasm32-unknown-unknown`).

```bash
cd plecto
cargo test --all   # builds the example filter to a WASM component, loads it into the wasmtime
                   # host, and exercises the contract end to end
cargo build --release -p plecto
```
