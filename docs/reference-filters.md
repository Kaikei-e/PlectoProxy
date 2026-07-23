# Reference filters — the signed OCI shelf

Plecto ships its reference filters as **individually cosign-signed CNCF Wasm OCI Artifacts**,
separate from the runtime binaries and images (ADR 000080). "On the shelf" is a falsifiable claim
here: a filter counts as shipped only when it has a published digest, a keyless cosign signature,
an SPDX SBOM attestation of the **component bytes**, and a row in the compatibility matrix below
— a directory in this repository, or a test-fixture build in CI, does not count
(ADR 000080 Decision 3).

Each artifact is one component under:

```
ghcr.io/kaikei-e/plecto/filters/<name>:<version>
```

`<name>` drops the crate's `filter-` prefix — the `filters/` namespace already says it (the crate
and directory keep the prefix; the matrix maps the two). `<version>` is the filter's **own**
SemVer, taken from its `Cargo.toml` — independent of the Plecto release version and of the
`plecto:filter` contract version, exactly like the WIT contract's own versioning
(`docs/writing-a-filter.md` §8). Per-release digests are recorded in each [GitHub
Release](https://github.com/Kaikei-e/PlectoProxy/releases)'s notes; sign/attest happen against the
**digest**, never a tag.

**Version tags are immutable.** Changing a filter's source without bumping its `Cargo.toml`
version fails the release job closed (stripped-component content hash ≠ published). Bump the
filter version to ship a new digest. Hashes ignore custom sections (`wasm-tools strip`) so
benign toolchain metadata noise does not force a bump.

**First push of each `filters/<name>` package on GHCR lands private** — flip it Public once in
the package settings (same quirk as the WIT packages, ADR 000064).

## Compatibility matrix

| Artifact | Crate | Version | Contract (world) | Guest target | Imports beyond `plecto:filter` | Required runtime profile | Manifest requirements |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `filters/jwt` | `filter-jwt` | 0.1.4 | `plecto:filter@0.3.0` (`filter`) | `wasm32-wasip2` | `wasi:http` (outgoing-handler, types) + the `wasi:io` / `wasi:cli` slices the wasip2 target bootstraps — outgoing calls happen only on the JWKS-at-init path; the static PEM/JWK path never calls out | **capabilities** | `isolation = "trusted"` (ADR 000070); `outbound-http` allowlist for the JWKS path, `allow = []` for static keys |
| `filters/cors` | `filter-cors` | 0.1.3 | `plecto:filter@0.3.0` (`filter`) | `wasm32-unknown-unknown` | none (zero-WASI) | any (minimal or capabilities) | — |
| `filters/apikey` | `filter-apikey` | 0.1.3 | `plecto:filter@0.3.0` (`filter`) | `wasm32-unknown-unknown` | none (zero-WASI) | any (minimal or capabilities) | — |
| `filters/extauthz` | `filter-extauthz` | 0.1.3 | `plecto:filter@0.3.0` (`filter`) | `wasm32-wasip2` | `wasi:http` (outgoing-handler, types) + the `wasi:io` / `wasi:cli` slices the wasip2 target bootstraps | **capabilities** | `outbound-http` allowlist naming the authorization endpoint |

CI asserts the import floor for every PR (`scripts/build-reference-filters.sh`): zero-WASI
entries must not import `wasi:http`; capabilities entries must. `wkg` embeds those imports into
the OCI wasm config on push (CNCF Wasm OCI Artifact layout).

"Required runtime profile" is the **compile-time floor** (ADR 000079): a component that imports
`wasi:http/outgoing-handler` cannot even instantiate on a minimal-profile binary, because minimal
never compiles the host side of that interface in. Compiling it in is still not granting it — at
runtime every outbound call is gated by the manifest's per-filter deny-by-default allowlist and
SSRF floor (ADR 000036 / 000060), on either profile.

`filter-jwt` keeps its static-key and JWKS paths deliberately distinct (ADR 000080 Decision 5):
the same artifact serves both, but a static-key deployment declares `allow = []` and lends
nothing. A future least-privilege build that drops the `wasi:http` import entirely for the static
path is tracked separately; until then even static-key deployments need the capabilities profile
to satisfy the import at instantiation.

## Not on the shelf (and why)

- **`filter-ratelimit-redis`** — the RESP path speaks to the store without AUTH, ACL, or TLS;
  ADR 000081 defines the production-promotion conditions and until they are met this filter stays
  a source-tree demo. Shipping a signed artifact would misread as a production endorsement.
- **`plecto:filter-streaming` guests** — the streaming contract is experimental and excluded from
  every prebuilt profile (ADR 000079).
- **`filter-hello`, `filter-quickstart`, the polyglot `filter-hello-*` family** — conformance and
  teaching fixtures, not references (ADR 000080 Decision 3).

## Verify, then load

The published artifacts are signed **keyless** (Fulcio OIDC) by the release workflow — there is no
long-lived signing key to leak, and the identity you verify is the workflow itself (ADR 000047).
The SBOM attestation is an SPDX document produced by scanning the **shipped `.wasm` component**
and bound to the OCI manifest digest via `cosign attest` (not `cosign attach sbom`). That is
OCI-digest-bound attestation of the component bytes; it is **not** the host's keyed
SBOM↔component-byte binding (ADR 000006) — that binding is re-established when you re-sign under
your own `[trust]` key.

Verify both the signature and the SBOM attestation against the digest from the release notes:

```bash
DIGEST=sha256:...   # from the release notes, never from the tag
cosign verify "ghcr.io/kaikei-e/plecto/filters/<name>@${DIGEST}" \
  --certificate-identity-regexp 'https://github.com/Kaikei-e/PlectoProxy/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
cosign verify-attestation --type spdxjson "ghcr.io/kaikei-e/plecto/filters/<name>@${DIGEST}" \
  --certificate-identity-regexp 'https://github.com/Kaikei-e/PlectoProxy/\.github/workflows/release\.yml@.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Plecto's own load path (`docs/writing-a-filter.md` §5) then takes over, and it is **your** trust
root, not ours: the proxy loads filters from a local, digest-pinned OCI image-layout and verifies
a signature by a key listed in the manifest's `[trust].keys` — fail-closed. The hand-off is
deliberate: Plecto's release identity vouches for what was shipped; your `[trust]` key vouches for
what you deploy. Pull the verified component and package it under your own key:

```bash
wkg oci pull "ghcr.io/kaikei-e/plecto/filters/<name>@${DIGEST}" -o <name>.component.wasm
# then sign + write the offline OCI layout + pin the digest in manifest.toml —
# docs/writing-a-filter.md §5 and examples/wasm-auth/ are the production reference
```

Verifying the registry signature **inside** the proxy (keyless identity as a `[trust]` root) is a
deliberately separate slice — it changes the host's trust model and gets its own decision record
before any code.

## Compatibility and deprecation

- **Contract**: every artifact row above names the `plecto:filter` version its world targets. The
  host keeps loading every contract version it ships support for, and a contract major bump keeps
  the previous major accepted for at least two release series (`docs/writing-a-filter.md` §8) —
  a shelf artifact built against an older contract keeps loading across proxy upgrades.
- **Filter**: each filter follows its own SemVer. A breaking change to a filter's config schema
  or observable behavior is a major bump of that filter, declared in CHANGELOG.md; the old
  version's artifact and digest stay published and verifiable.
- **Conformance / release-parity record**: a release tag can only point at a commit whose
  default-branch CI run is green (release.yml's gate). That run (1) builds the shelf with the
  same script release uses (`scripts/build-reference-filters.sh`, including the import floor),
  (2) runs host behaviour tests for cors / apikey via fixture builds and jwt / extauthz via the
  `outbound-http` feature suites. There is no separate per-filter suite ledger — the gate is the
  record.
