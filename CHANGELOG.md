# Changelog

All notable changes to Plecto are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## Versioning policy (pre-1.0)

- **Binary / manifest**: while the version is `0.x`, a **minor** bump (`0.1 → 0.2`) may contain
  breaking changes to the manifest schema or CLI; they are always listed under **Changed** /
  **Removed** with a migration note. Patch bumps are always safe to take.
- **Library crates**: `plecto-host` / `plecto-control` / `plecto-server` are published from the
  same version as the binary. A change a downstream crate cannot compile against takes a
  **minor** bump too — including a change to the *type* a public signature exposes, such as the
  error type of a dependency re-surfaced through a public enum. `cargo semver-checks` runs on
  every release, but it compares type paths, so a dependency's major bump behind an unchanged
  path is a gap it cannot see; that judgement stays manual.
- **WIT contract**: the filter contract is versioned independently as `plecto:filter@<version>`,
  and **bumping the proxy never requires rebuilding a filter**. The upgrade rule now lives where
  the decision is made — [README](README.md#upgrading-two-independent-version-series) and
  [docs/operations.md](docs/operations.md#upgrading-two-independent-version-series) — rather than
  only here. The contract is published as a CNCF Wasm OCI Artifact to `ghcr.io` on every tagged
  release (`wkg publish`, ADR 000064); the published digest is recorded in that tag's release
  notes, the contract-side counterpart of the binary/image supply-chain record below.
- **Access log**: the field set of the `plecto::access` line is a contract on the same footing as
  the manifest schema. Adding, renaming, or removing a field is listed under **Changed** with a
  migration note; the typed field list is in
  [docs/operations.md](docs/operations.md#the-access-log-field-contract).
- **Release artifacts**: binaries and images are cosign-signed (keyless) with SBOMs attached —
  the same supply-chain bar Plecto's own filter loading enforces. Verify commands are in the
  release notes of each release.

## [Unreleased]

### Added

- **Declarative per-route response headers** (`[route.headers]`, ADR 000100). A route can now
  declare `set = { … }` / `remove = [ … ]` and get constant response headers without writing a
  filter — the case that previously cost an operator a Rust toolchain, a vendored WIT copy, a
  signing key, an OCI layout, and a `sha256:` digest to re-pin on every build. Values are literals:
  no conditionals, no interpolation from the request, no patterns; a header whose value depends on
  the request is a per-request decision and stays a filter's job. The declaration is a **floor** —
  applied after the response filter chain and before compression, `set` replaces every same-named
  header the upstream or a filter produced and `remove` drops the name, on **every** response the
  route answers: a filter's `replace`, a filter's short-circuit, a chain's fail-closed 5xx, the
  native rate limit's 429, and the forward-side 502 / 503 / 504. One gap is deliberate: a response
  returned before a route is chosen (the no-route 404, the path-normalization 400) carries no
  declaration, because there is no route to take it from. Header names are matched
  case-insensitively, and validation is fail-closed — an invalid name or value, a duplicate name, an
  empty block, or a hop-by-hop / `content-length` name fails `plecto validate`, startup, and reload
  rather than being dropped at request time. Absent section, unchanged behavior. See the
  [operations guide](docs/operations.md#declared-response-headers-which-responses-they-land-on).
- **Client identity behind a trusted L7 proxy** (`[listen.trusted_proxy]`, ADR 000103). Declaring
  `trusted = ["<CIDR>", ...]` lets a request whose already-resolved address falls inside those
  CIDRs have its client read out of the inbound `X-Forwarded-For`: the list is walked right to
  left, declared hops are dropped, and the first address no declared proxy vouched for becomes the
  client — feeding the per-client-IP rate limit, `source_ip` Maglev hashing, the access log's
  `client` field, and the re-issued `X-Forwarded-For` / `X-Real-IP`. This closes the gap for a
  fronting L7 tier that cannot speak PROXY protocol v2; where v2 is available it remains the first
  choice, since it restores the address below HTTP. Absent section, unchanged behavior — the
  restoration path does not exist unless declared, and a request from outside the CIDRs always
  keeps the edge default (inbound forwarding headers dropped, the peer re-issued). Every
  unresolvable case falls back to that peer: an absent, malformed, or entirely-declared list, and
  any list longer than the scan bound. `X-Forwarded-For` is the only restoration source — the rest
  of the client-IP header family stays dropped — and the scheme still comes from the wire, so an
  inbound `X-Forwarded-Proto` is never honored. `trusted` takes CIDR notation only (a single host
  is `"10.1.2.3/32"`) and must list at least one entry; an empty or unparsable list fails
  `plecto validate` and startup. See the
  [hardening guide](docs/hardening.md#client-identity-behind-a-front-proxy).
- **`plecto_upstream_instances{upstream,state}`** (ADR 000099): a gauge counting each upstream's
  instances by state — `healthy`, `unhealthy`, `ejected` — so a fail-closed 503 from an empty
  rotation (ADR 000017) has an externally visible explanation. Rendered by walking the live
  upstream groups at scrape time, with no persistent counter behind it, so a reload can never
  leave it stale. Probe health (ADR 000017) and outlier ejection (ADR 000032) are independent
  axes, and the label folds them by severity (`ejected` > `unhealthy` > `healthy`) so each
  instance is counted in exactly one series: the per-upstream series sum back to its instance
  count, and cardinality is bounded by declared upstreams × 3. The combination "probe-healthy but
  ejected" is therefore not recoverable from this metric — `plecto_outlier_ejections_total`
  remains the cumulative ejection signal.
- **Filter contract versions in `plecto --version`** (ADR 000099): a `filter contracts:` line
  listing every `plecto:filter` version this binary can load. Alongside it, each filter now logs
  one line at load (startup and every reload) naming the contract version it actually bound and
  its isolation — the version set the binary accepts and the version a given filter uses are
  different questions, and an upgrade decision needs both.

### Changed

- **Log lines are flattened JSON** (ADR 000099). The binary's JSON subscriber now writes each
  event's fields at the top level of the line — beside `timestamp` / `level` / `target` — instead
  of nesting them under a `fields` object. The nesting was the JSON layer's default, never a
  chosen shape, and it left ingestion layers unable to map `method` / `status` / `duration_ms`
  into typed slots without unwrapping first. This changes every log line the binary emits, not
  only the access log. **Migration**: read the access log's fields from the line root rather than
  from `fields.*`; the field names themselves are unchanged. The full typed field list — now a
  contract, per the versioning policy above — is in
  [docs/operations.md](docs/operations.md#the-access-log-field-contract).
- **The access log carries `trace_id` and `span_id`** (ADR 000099), unconditionally — not only for
  sampled transactions. The proxy is the one hop that sees every request, so its log line is where
  "give me the trace for this slow request" gets answered; for an unsampled transaction the ids
  are the only handle on it at all. Both are the W3C lowercase-hex forms, and `trace_id` is the
  caller's when the request arrived with a `traceparent`.
- **Manifest: a declared `[chain]` is now rejected** (breaking, ADR 000101). The section was
  validated (its filter ids were checked against the declared set) and resolved into the active
  config, but no serving path ever ran it — the fast path runs the matched `[[route]]`'s inline
  `filters` and nothing else, and a manifest with no routes answers 404. A manifest could
  therefore validate green, start clean, and apply zero filters. A non-empty `[chain] filters`
  now fails closed in `plecto validate`, at startup, and on reload, with a diagnostic that names
  where the filters belong. The rejection is unconditional — it does not depend on whether
  `[[route]]` is also declared — so there is no "sometimes live" case to remember. An empty or
  absent `[chain]` keeps validating, and config versions (manifest content hashes) are unchanged.
  **Migration**: move each filter id from `[chain] filters` into the `filters` of the `[[route]]`
  that needs it.

### Removed

- **The chain-only convenience API** (breaking, ADR 000101): `Control::on_request` /
  `Control::on_response`, `ConfigSnapshot::on_request` / `ConfigSnapshot::on_response`, and the
  `ControlError::UnknownChainFilter` variant that reported against the section above. The
  manifest `[chain]` was their only input — the config they read is crate-private and cannot be
  built from outside — so rejecting the section leaves them unreachable rather than merely
  unused. **Migration**: `ConfigSnapshot::find_route` plus `dispatch_request` /
  `dispatch_request_body` / `dispatch_response` — the calls the fast path itself drives — run a
  route's chain against one snapshot pinned for the whole transaction. `plecto-control` is
  published to crates.io, so this is a public-API removal and rides a pre-1.0 **minor** bump per
  the versioning policy above; `cargo semver-checks` detects it.

## [0.6.4] - 2026-08-06

Patch release: routine dependency maintenance. `wit-component` moves 0.254 → 0.255 (the only
out-of-date dependency crossing a semver boundary), which unifies the wasm-tools family
(`wasm-encoder` / `wasmparser` / `wasm-metadata` / `wit-parser`) onto a single 0.255 line instead
of straddling 0.254/0.255 — the duplication `cargo-deny`'s `multiple-versions = "warn"` flagged
since 0.6.3 is gone. No source change. `cargo semver-checks` reports no semver update required
against 0.6.3 (196 checks on each of `plecto-host` / `plecto-control` / `plecto-server`, default
features).

### Changed

- **Workspace lockfile refresh**: `wit-component` 0.254 → 0.255 (declared bump in `crates/host`
  and `crates/plecto`), pulling `wasm-encoder` / `wasmparser` / `wasm-metadata` / `wit-parser`
  along to 0.255. `filter-jwt`'s example lockfile picks up `base64` 0.23.1 and `zerocopy` 0.8.56
  patch bumps, which change its compiled component bytes. Every other guest lockfile is
  untouched, so `filters/cors` / `filters/apikey` / `filters/extauthz` are byte-identical and do
  not republish.
- **Reference-filter shelf republished**: `filters/jwt` 0.1.5 → 0.1.6 (ADR 000080 — filter tags
  are immutable, and the `base64`/`zerocopy` bump above changed the stripped-component hash, so
  the existing `0.1.5` tag can't carry the new bytes). No filter source change. The compatibility
  matrix (`docs/reference-filters.md`) is updated to match.
- **CI hardening**: `dtolnay/rust-toolchain`'s pinned commit was orphaned by an upstream history
  rewrite (zizmor's `impostor-commit` audit caught it) — repinned to the permanent
  `2c7215f132e9` ("Add 1.97.1 patch release") merge commit across `ci.yml` / `bench.yml` /
  `release.yml`, with the now-required `toolchain` input restored. `Swatinem/rust-cache` moved
  off a since-advanced `v2` branch-tip SHA to the tagged `v2.9.2` commit. Separately,
  `.github/zizmor-requirements.txt` pinned zizmor 1.27.0, since yanked from PyPI for
  GHSA-f42p-wjw5-97qh (a debug-logging defect that prints configured GitHub credentials) —
  bumped to 1.29.0.

## [0.6.3] - 2026-08-01

Patch release: the SIGHUP reload gate now notices an in-place rotation of every file the
manifest references, closing the window where an ACME deploy-hook rotated a certificate on
disk and the proxy silently kept serving the expiring one (ADR 000097), plus a routine
workspace lockfile refresh. The source change is confined to the control plane; there is no
WIT contract, manifest schema, or CLI change, and no public API change — `cargo semver-checks`
reports no semver update required against 0.6.2 (196 checks on each of `plecto-host` /
`plecto-control` / `plecto-server`, default features). No guest lockfile moves, so the built
filter components are byte-identical and the reference-filter shelf does not republish.

Two decisions are recorded alongside the fix: ADR 000097 promotes ACME (automatic HTTPS) from
outside the deferred order to its head, fixing this two-tier content hash as the floor any
future ACME implementation builds on; ADR 000098 fixes the contract shape of
`on-response-body` (buffer-then-decide) for `plecto:filter@0.4.0` — contract decision only,
no implementation in this release.

### Fixed

- **Control: the reload gate detects an in-place rotation of any referenced file.** The gate
  compared only `content_hash_at`, which digested `[listen.client_auth].ca_path` alone — so
  overwriting a `[[tls]]` cert/key, an `[upstream.tls]` CA or client identity, the
  `[resumption]` STEK, or a `[filter.config_files]` secret in place left the config version
  untouched and the reload reported `Unchanged`; the rotated material never loaded. The gate
  now compares a pair split by sensitivity: the public config version additionally digests
  `[[tls]]` cert bytes and `[upstream.tls]` CA / client-cert bytes (each domain-separated by a
  label plus a length-prefixed entry identity), and a new **never-logged reload fingerprint**
  digests the secret bytes — `[[tls]]` private keys, `[upstream.tls]` client keys, the STEK,
  every resolved `[filter.config_files]` value — taking the public version as a prefix input.
  Secrets stay off the logged version deliberately: a logged digest over a low-entropy secret
  would hand a manifest-plus-log holder an offline brute-force oracle, so the fingerprint
  never reaches a tracing event, an error message, or a public accessor. Error semantics are
  unchanged — either half failing to compute at the gate logs a warning and falls through to
  the full rebuild, which re-reads and fails closed with the precise error — and
  `config_version()` keeps its exact previous meaning. (ADR 000097)

### Changed

- **Workspace lockfile refresh**: clap 4.6.5, http 1.5.0, hybrid-array 0.4.14, rustls 0.23.43,
  wast 255.0.0 / wat 1.255.0 (the wast family adds a second `wasm-encoder` / `wasmparser` at
  0.255 next to the wit-component 0.254 encoder `crates/host/build.rs` pins; cargo-deny's
  `multiple-versions = "warn"` records the duplication). Every move stays inside the major its
  requirement already declares. Guest, bench, and spike lockfiles are untouched.

## [0.6.2] - 2026-08-01

Security patch release: wasmtime 47.0.2 → 47.0.3, taken the day upstream shipped it. There is
**no source change** to the fast path, the host, or the control plane, and no WIT contract,
manifest schema, or CLI change. The bump stays inside the major the workspace already declares,
`cargo semver-checks` is clean against 0.6.1 (196 checks on each of `plecto-host` /
`plecto-control` / `plecto-server`, default features), and — unlike 0.6.1 — no guest lockfile
moves, so
the built filter components are byte-identical and the reference-filter shelf does not
republish. The patch is "always safe to take" under the versioning policy above, and given what
it fixes, it should be.

### Security

- **Runtime: wasmtime 47.0.2 → 47.0.3** (`wasmtime-wasi` / `wasmtime-wasi-http` in lockstep,
  per the workspace's single-declaration rule). 47.0.3 fixes two advisories:
  - [GHSA-2hw9-mc66-jc2q](https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-2hw9-mc66-jc2q)
    — *preemption and traps during bulk operations enable breaking internal VM state*. This is
    the one Plecto is directly exposed to: every engine runs with epoch interruption on (the
    ADR 000006 metering path — 1 ms tick, per-request deadlines), untrusted isolation is the
    manifest default, and the bulk-memory proposal is default-on in wasmtime, so any ordinary
    compiled guest emits `memory.copy` / `memory.fill` that a deadline can preempt mid-operation.
    On the trusted pooling engine the same window exists across reused slots. An
    attacker-supplied filter needs nothing beyond a large copy loop to sit in it.
  - [GHSA-hgjw-h833-99q9](https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-hgjw-h833-99q9)
    — *stores can mix up type indices between engines*. Latent here rather than reachable: the
    host does hold two engines in one process (trusted pooling / untrusted on-demand), but every
    `Component` / `InstancePre` / `Store` pairing is built from the one engine chosen at load
    time, there is no component cache, and hot reload recompiles from bytes — no code path hands
    an object from one engine to the other's store. The fix turns that convention into an
    upstream-enforced check.
- **Streaming spike host follows in step** (its own workspace): wasmtime / wasmtime-wasi
  47.0.3 in `spike/streaming-async/host`.

## [0.6.1] - 2026-07-29

Patch release: a dependency refresh with **no source change** to the fast path, the host, or the
control plane, and no WIT contract, manifest schema, or CLI change. `cargo semver-checks` is
clean against 0.6.0 (196 checks on each of `plecto-host` / `plecto-control` / `plecto-server`,
default features). Unlike 0.6.0, every bump here stays inside the major its `Cargo.toml` already
declares, so no public signature changes the type it exposes and the patch stays "always safe to
take" under the versioning policy above.

The reference-filter shelf republishes: the guest lockfile refresh changes the built component
bytes, and filter tags are immutable (ADR 000080), so each entry takes a new version with this
release rather than tripping the publish guard the way 0.4.1 and 0.5.1 did.

### Changed

- **Workspace lockfile refresh**: `schemars` 1.2.2 (`serde_derive_internals` 0.30 — the manifest
  JSON Schema derive), `toml` 1.1.4 / `toml_parser` 1.1.3, `tokio-macros` 2.7.2, `displaydoc`
  0.2.7, `clang-sys` 1.9.1. No dependency *requirement* moves; these are the resolved versions.
- **Guest lockfile refresh** across the Rust example filters, the filter template, the compat
  fixture, and the streaming spike guest: `serde` / `serde_derive` 1.0.229 (which moves the
  derive onto `syn` 3), `serde_json` 1.0.151, `anyhow` 1.0.104, `proc-macro2` 1.0.107, `quote`
  1.0.47.
- **`filter-jwt` guest: `base64` 0.22 → 0.23**, the version `plecto-host` and `plecto-server`
  already declare — the guest was the last place in the tree still a major behind.
- **Reference-filter shelf republished**: `filters/jwt` 0.1.4 → 0.1.5; `filters/cors`,
  `filters/apikey`, `filters/extauthz` 0.1.3 → 0.1.4. All four stripped-component hashes change
  under the refreshed guest lockfiles (verified against a baseline build of the pre-refresh
  ones). No filter source changes beyond the jwt `base64` bump; the compatibility matrix
  (`docs/reference-filters.md`) is updated to match.
- **CI / release toolchain: `wasm-tools` CLI 1.252.0 → 1.254.0.** The shelf encode path
  (`wasm-tools component new`) is meant to be the CLI face of the `wit-component` encoder
  `crates/host/build.rs` uses, and that reached 0.254 with wasmtime 47 while the CLI pin stayed a
  release behind. Shelf output is byte-identical across the two CLI versions, so this restores
  the intended parity without moving any published component.
- **Benchmark harnesses**: the load generator picks up `hyper` 1.11.0, `tokio` 1.53.1,
  `hdrhistogram` 7.6.0 and `cc` 1.4.0; the streaming spike host follows the wasm-tools family
  253 → 254, in step with the production wasmtime pin.

### Deferred

- **`wkg` stays at 0.15.1** (0.16.0 is released). `wkg publish` and `wkg oci push` are the
  release workflow's only path to ghcr.io, so the bump does not ride along inside a release-prep
  change — it wants its own change, with a dry run behind it.
- **`generic-array` stays at 0.14.7**: `crypto-common` 0.1.7 requires it *exactly*, and that
  arrives through `sigstore` → `crypto_secretbox`. 0.14.9 is unreachable until that chain moves.
- The `filter-jwt` guest stays on `p256` 0.13 / `signature` 2 / `sha2` 0.10, unchanged from
  0.6.0: `rsa` 0.10 is still pre-release, and taking `p256` 0.14 without it would put two
  `signature` majors inside one filter.
- `rsa` 0.10, `libc` 1.0 and `rustls` 0.24 are still pre-release and are not taken.

## [0.6.0] - 2026-07-27

Minor release: a dependency refresh across the workspace, the guest-adjacent tooling, and the
benchmark harnesses. There is **no WIT contract change, no manifest schema change, and no CLI
change**, and no behavioural change to the fast path or the capability boundary — for anyone
running the binary or the container image this upgrade carries no migration.

**Why this is a minor and not a patch.** The `toml` 0.8 → 1.1 bump changes the type behind a
public signature: `plecto_control::ControlError::ManifestParse` wraps `toml::de::Error`, so a
crate that builds that error from its own `toml` value has to move majors together with us.
`cargo semver-checks` reports 196 checks passing against 0.5.4, but it compares type *paths* and
`toml::de::Error` is spelled identically in both majors, so this break is outside what it can
detect. The versioning policy above holds patch releases to "always safe to take", and this one
would not have been; the minor absorbs it rather than quietly widening what a patch may contain.

### Changed

- **Runtime: wasmtime 47.0.1 → 47.0.2** (`wasmtime-wasi` / `wasmtime-wasi-http` in lockstep).
  A patch within the 47 line; the engine still turns the GC and exception-handling proposals
  explicitly off (deny-by-default, ADR 000096).
- **Manifest parsing: `toml` 0.8 → 1.1** (and `toml_edit` 0.22 → 0.25 for `plecto dev`'s
  format-preserving digest rewrite). Manifests are whole TOML *documents*, which is the path
  1.x keeps on `from_str`, so parsing and every fail-closed validation behave identically — a
  manifest that loaded on 0.5.4 loads here with the same diagnostics. See **Migration** for the
  one library-level consequence.
- **Hashing: `sha2` 0.10 → 0.11** (`digest` 0.11) for OCI content-digest pinning, the SPKI
  fingerprints, and the STEK key schedule. This is the line sigstore's own key material already
  resolves to, so digest computation and the provenance path now share one hash stack.
- **Reload / guest-call plumbing**: `signal-hook` 0.3 → 0.4 (the no-tokio SIGHUP receiver behind
  the `ReloadSource` seam) and `pollster` 0.4 → 1.0 (the no-reactor executor that drives a guest
  call to completion). Both are drop-in at the surface Plecto uses.
- **Dev, test, and bench dependencies**: `criterion` 0.5 → 0.8, `rcgen` 0.13 → 0.14,
  `jsonwebtoken` 10 → 11, `base64` 0.22 → 0.23, `sha1` 0.10 → 0.11, `tikv-jemallocator`
  0.6 → 0.7. rcgen 0.14 renames `CertifiedKey::key_pair` to `signing_key`, which the
  self-signed-certificate helpers in the TLS/H2/H3 tests and the `tls-http` example follow.
- **Benchmark harnesses**: the bench filters move to `wit-bindgen` 0.60, matching the example
  filters and the compat fixture; the load generator moves to `rand` 0.10 (`thread_rng` is gone,
  so the WebSocket nonce and frame masks use the top-level `rand::fill`); the streaming spike
  host tracks the production wasmtime pin at 47.
- **Lockfile refresh** for the rest of the tree: `tokio` 1.53.1, `hyper` 1.11.0, `cranelift`
  0.134.2, `serde` 1.0.229, `serde_json` 1.0.151, `thiserror` 2.0.19, `anyhow` 1.0.104.

### Migration

- **Operators**: nothing to do. The manifest schema, the CLI, the WIT contract, and the container
  image interface are unchanged; upgrade the binary or the image tag and reload as usual.
- **Crates depending on `plecto-control`**: only code that *produces* a
  `ControlError::ManifestParse` needs a change — move your own `toml` dependency to 1.x so the
  `#[from]` conversion still applies (a `?` on a `toml` 0.8 `Result` inside a function returning
  `ControlError` is the case that stops compiling). Matching the variant, or reading the error
  through `Display` / `source()`, is unaffected.

### Deferred

- `rsa` 0.10, `libc` 1.0 and `rustls` 0.24 are still pre-release and are not taken.
- The `filters/jwt` guest stays on `p256` 0.13 / `signature` 2. Moving to `p256` 0.14 requires
  `signature` 3, which its RSA verification path cannot reach until `rsa` 0.10 is released —
  taking one without the other would put two `signature` majors inside a single filter.

## [0.5.4] - 2026-07-23

Patch release: the guest toolchain moves to **wit-bindgen 0.60**. No WIT or manifest schema
changes; the runtime stays on wasmtime 47.0.1; library APIs are unchanged (`cargo
semver-checks` clean against 0.5.3).

### Changed

- **Guest toolchain: wit-bindgen 0.59 → 0.60** across the Rust example filters, the filter
  template, the compat fixture, and the streaming spike; guest lockfiles move the wasm-tools
  family 253 → 254 in lockstep, and CI installs the sha256-pinned 0.60.0 CLI. 0.60's MoonBit
  generator renames generated package directories from camelCase to kebab-case, so the
  MoonBit guest's committed bindings are regenerated and its hand-written filter moves with
  them; polyglot conformance is unchanged.
- **Reference-filter shelf republished**: `filters/jwt` 0.1.3 → 0.1.4; `filters/cors`,
  `filters/apikey`, `filters/extauthz` 0.1.2 → 0.1.3. The toolchain refresh changes the
  built component bytes of every shelf entry and filter tags are immutable (ADR 000080), so
  each entry takes a new version with this release instead of tripping the publish guard the
  way 0.4.1 and 0.5.1 did. No filter source changes; the compatibility matrix
  (`docs/reference-filters.md`) is updated to match.
- **Dev/test profiles compile the wasm-compilation pipeline at `opt-level = 3`**: per-package
  overrides in the workspace profiles speed up test runs that JIT-compile filters while
  keeping the workspace itself debuggable with debug assertions active.
- **Docs: the operations guide gains a CI pre-flight section** for `plecto validate
  --resolve` — validating manifest edits and filter digest updates in CI before they can
  fail closed at reload time.

## [0.5.3] - 2026-07-21

Patch release: the wasmtime line moves to **47.0.1**. No WIT or manifest schema changes;
library APIs are unchanged (`cargo semver-checks` clean against 0.5.2).

### Changed

- **Runtime: wasmtime 46.0.1 → 47.0.1** (`wasmtime-wasi` / `wasmtime-wasi-http` in lockstep,
  `wit-component` 0.252 → 0.254). wasmtime 47 enables the GC and exception-handling
  proposals by default; both are now explicitly disabled in the engine config — filters
  gain no wasm feature the host never decided to lend (deny-by-default, ADR 000096).
  Upstream's `wasi-common` / wasi-threads removal does not affect Plecto.
- **Reference filter `filters/jwt` 0.1.2 → 0.1.3**: republished so the shelf entry picks up
  the guest dependency refresh that landed after the v0.5.2 tag; filter tags are immutable
  (ADR 000080), so the entry takes a new version. No jwt source changes.
- **CI: `actions/checkout` pin comments corrected to `v7.0.0`**: upstream moved the `v7`
  major tag off the pinned commit, tripping zizmor's online `ref-version-mismatch` audit;
  the SHA pin itself is unchanged.

## [0.5.2] - 2026-07-20

Patch release that completes what 0.5.1 started. The 0.5.1 tag's release run pushed
`filters/jwt:0.1.2`, then failed at the immutable-tag guard for `filters/cors:0.1.1`:
rebuilding the shelf under Rust 1.97.1 changed component bytes while those tags stayed
at 0.1.1 (same class of failure as 0.4.1 → 0.4.2). Use 0.5.2.

### Changed

- **Reference-filter shelf republished as 0.1.2**: `filters/cors`, `filters/apikey`, and
  `filters/extauthz` each bump 0.1.1 → 0.1.2 (`filters/jwt` already at 0.1.2 from the
  0.5.1 attempt). No filter source changes beyond the jwt clippy fix already in 0.5.1;
  the compatibility matrix (`docs/reference-filters.md`) is updated to match.

## [0.5.1] - 2026-07-20

Patch release: MSRV / CI / Docker toolchain pin follows Rust **1.97.1** (was 1.96.0).
No WIT or manifest schema changes. The tagged release run did not finish the reference-
filter shelf publish; use **0.5.2**.

### Changed

- **Toolchain / MSRV**: `rust-toolchain.toml`, `Cargo.toml` `rust-version`, CI
  (`dtolnay/rust-toolchain` SHA pin), release bookworm image, and `Dockerfile` all move
  to Rust 1.97.1.
- **Reference filter `filters/jwt` 0.1.1 → 0.1.2**: clippy `question_mark` cleanup in
  `parse_url` under rustc 1.97; filter tags are immutable (ADR 000080), so the shelf
  entry takes a new version.

## [0.5.0] - 2026-07-20

Feature release closing the packaging/CI and container-operations gaps surfaced by
dogfooding field reports: filters were easy to write, but everything *around* them —
producing the signed artifact in CI, delivering file-based secrets, health-checking a
shell-less container — needed out-of-tree workarounds. All three now have first-class,
documented paths (ADR 000094 / 000095).

The minor bump (not a patch) follows the pre-1.0 policy above: one Rust-API breaking
change, listed under **Changed** with its migration note. TOML manifests from 0.4.x are
unaffected — every new manifest section is optional.

### Changed

- **Breaking (Rust embedder API)**: `plecto_control::FilterEntry` gained the
  `config_files` field. Code constructing `FilterEntry` with a struct literal must add
  `config_files: None`; manifests and the CLI are unaffected. (Flagged by
  cargo-semver-checks: adding a field to an externally-constructible struct requires a
  major-position bump.)

### Added

- **`plecto package`** — one-shot CI packaging: conformance-gate a built component, sign it
  and its SBOM with an operator key (ECDSA P-256 PKCS8 PEM, the `sign-blob` scheme), write
  the signed offline OCI image-layout the loader requires, and print only the pinned
  image-manifest digest to stdout (`DIGEST=$(plecto package …)` composes). `--sbom` swaps in
  a supplier-provided in-toto statement. Unlike `plecto dev` it touches no manifest, watches
  nothing, and never generates a key (ADR 000094).
- **`plecto validate --resolve`** — extends static validation with the loader's own
  provenance gate, run without serving: digest-pin resolution plus trusted-signature and
  SBOM-binding verification through the very same code path as a real load
  (`TrustPolicy::verify_artifact`). CI can now prove a manifest + layout pair would load
  before a deploy (ADR 000094).
- **`plecto healthz`** — self-probe for shell-less (distroless) containers: reads
  `[observability] admin_addr` from the manifest (or `--admin-addr`), performs one bounded
  HTTP/1.1 GET, exits 0 on 2xx and 1 otherwise (never the reserved exit code 2). Probes
  `/readyz` by default — what a Compose `service_healthy` start gate means — and `/healthz`
  with `--live`. The multi-replica example now ships a working `healthcheck:` block, and the
  operations guide documents the pattern.
- **`[filter.config_files]`** — file-based secret indirection for filter config: same key
  space as `[filter.config]`, but each value is a path (absolute or manifest-relative) whose
  content — UTF-8, whitespace-trimmed, ≤ 1 MiB — is served through the existing
  `host-config::get` keys. Resolved at every load/reload, so a SIGHUP picks up rotated
  secret files; a key set in both sections, or a missing/unreadable file, fails closed at
  validate and at load. The WIT contract is unchanged (ADR 000095).

## [0.4.2] - 2026-07-19

Patch release that completes what 0.4.1 started. The 0.4.1 tag's release run failed at the
reference-filter publish step: the dependency refresh changed the built component bytes of
the shelf entries while their own versions stayed 0.1.0, and filter tags are immutable
(ADR 000080), so the guard failed closed. 0.4.1 therefore has a container image on GHCR but
no GitHub release; use 0.4.2.

### Changed

- **Reference-filter shelf republished as 0.1.1**: `filters/jwt`, `filters/cors`,
  `filters/apikey`, and `filters/extauthz` each bump 0.1.0 → 0.1.1 to carry the refreshed
  dependencies under a new immutable tag. No filter source changes; the compatibility matrix
  (`docs/reference-filters.md`) is updated to match.

## [0.4.1] - 2026-07-19

Patch release: a routine dependency refresh (`cargo update`) across every workspace lockfile —
the release workspace plus the bench, fuzz, example-filter guest, and spike workspaces. No
source changes; the WIT contract is untouched and `plecto:filter@0.3.0` remains current.

### Changed

- **Dependencies**: notable bumps on the release path include `tokio` 1.53.0, `rustls` 0.23.42,
  `aws-lc-rs` 1.17.3, `bytes` 1.12.1, and `regex` 1.13.1. No API, manifest-schema, or CLI
  changes ride along.

## [0.4.0] - 2026-07-17

Minor (not patch) per the pre-1.0 policy above: `plecto-control`'s public `FilterEntry`
gained fields, which is breaking for external struct-literal constructors (see **Changed**).
The WIT contract is untouched — `plecto:filter@0.3.0` remains current and every deployed
filter keeps loading unmodified.

### Added

- **Trusted-pool lifecycle knobs in the manifest**: a `[[filter]]` entry can now set
  `pool_size` (max concurrent reusable instances), `checkout_timeout_ms` (bounded wait under
  saturation before failing closed), and `max_requests_per_instance` (recycle bound) — the
  ADR 000012 knobs that previously existed only as host `LoadOptions` builders with no
  manifest path to reach them. Trusted-only: declaring any of them under
  `isolation = "untrusted"` (fresh-per-request, no pool) is rejected at validate, as are the
  zero-value typos. The JSON schema (`plecto schema`) picks the fields up automatically.
- **Pooling-allocator residency on `/metrics`**: three new admin gauges —
  `plecto_pool_component_instances`, `plecto_pool_memories`, and
  `plecto_pool_unused_memory_resident_bytes` — surface the trusted engine's wasmtime
  `PoolingAllocatorMetrics`, lowered through the new `plecto_control::PoolResidency` /
  `Control::pool_residency()` so the fast path names no wasmtime types.
- **Maglev demo**: the `load-balancing` example now also runs a `pool-sticky` upstream
  (`lb_algorithm = "maglev"` keyed on the `x-session` header, ADR 000035) behind a `/sticky`
  route — per-key stickiness, the round-robin fallback without the header, and the
  eject-remaps-only-that-key behavior are all observable by hand.
- **Hop-by-hop parity test**: the RFC 9110 §7.6.1 strip list is carried independently by the
  fast path (`plecto-server::headers`) and the guest-output mappers
  (`plecto-host::contract`); a new cross-crate test pins the two lists identical so they can
  no longer drift apart silently (CWE-444 adjacent).

### Changed

- **Library (`plecto-control`) — `FilterEntry` gained the three pool fields above.** The
  struct's fields are public, so external code constructing it with a struct literal must add
  the new fields (`None` keeps host defaults); deserializing from TOML/JSON is unaffected.
  Per the pre-1.0 policy above this rides the next **minor** bump.
- **Graceful shutdown now joins its background supervisors**: `serve_with_shutdown` kept
  handles for the h3 endpoint and the OTLP pump but spawned the admin, health-check, and DNS
  supervisors fire-and-forget, so an embedder could see serve return while those tasks still
  held `Control`. All three are now awaited (bounded, warn on expiry) in the drain sequence —
  the guarantee their doc comments had been promising.
- **Internal simplifications** (no behavior change, all suites green): the transaction core's
  upgrade switch / request body hook / response-chain tail are named phase functions instead
  of one 439-line block; response bodies are boxed `UnsyncBoxBody` (no consumer requires
  `Sync`), which drops the lock wrapper the compression encoder had been paying for it.

### Removed

- **`wit/v0.3.0/` pre-frozen snapshot** (repo-internal): v0.1/v0.2 were frozen when a
  successor replaced them, but v0.3.0 had been snapshotted in the same commit that made it
  current — referenced by nothing except its own CI drift check, and taxing every contract
  edit with a double maintenance step. The freeze-at-replacement pattern is restored; the
  shipped contract `plecto:filter@0.3.0`, its published OCI artifact, and the v0.1/v0.2
  frozen trees are all unchanged.

## [0.3.8] - 2026-07-16

### Changed

- **Host-state writes now group-commit**: `host-kv` / `host-counter` / `host-ratelimit`
  writes to the redb backend are combined by the calling threads themselves (flat combining:
  callers queue ops and an elected caller applies everything queued in one write
  transaction), instead of paying one `begin_write`→`commit` per op on redb's global
  single-writer lock. Contended writes drop ~60% per-op on an 8-thread benchmark; the
  uncontended path is unchanged (the winner runs its op inline, no thread handoff). Per-op
  atomicity, `set`/`delete` immediate durability, fail-closed behavior, and the periodic
  durable-flush bound (now counted per op, advanced only after a successful commit) are all
  preserved (ADR 000093, amends 000004).

### Added

- **Contended-write micro-benchmark** (`plecto-host/benches/kv_backend.rs`): redb vs
  in-memory backend across writer-thread counts, so the host-state write path's scaling is
  measured in CI instead of only at single-threaded load points.
- **CI: semver gate for published crates** — `cargo-semver-checks` compares
  `plecto-host` / `plecto-control` / `plecto-server` against their latest crates.io
  release on every PR, so an accidental breaking change cannot ride a patch bump. CI
  workflows were also hardened (least-privilege permissions, zizmor audit gate,
  action SHA re-pins).

## [0.3.7] - 2026-07-15

### Added

- **Per-source-IP concurrent connection cap** (`MAX_CONNECTIONS_PER_IP`, default 256): a
  fixed hash-slot admission table — the same CWE-770-bounded design as the existing
  `client-ip` native rate limiter, IPv6 coarsened to /64 — enforced on both the TCP and
  QUIC/HTTP-3 accept loops, so a single source can no longer exhaust the global
  `MAX_CONNECTIONS` pool alone (ADR 000092, amends 000027).

### Fixed

- **`RLIMIT_NOFILE` is now raised at startup**: the process's soft file-descriptor limit is
  raised to the hard limit before serving (a POSIX-permitted operation, no `CAP_SYS_RESOURCE`
  required), so the common distro default soft limit (often 1024) no longer causes the accept
  loop to silently stop admitting new connections on EMFILE long before `MAX_CONNECTIONS`
  (10,000) is reached. A warning is logged if the hard limit itself is still too low for that
  ceiling — raising it further needs an operator's own privileged action (systemd
  `LimitNOFILE=`, `docker --ulimit nofile=`) (ADR 000092, amends 000027).

## [0.3.6] - 2026-07-14

### Added

- **`plecto` crate on crates.io**: the `plecto` binary and its operator CLI (serve /
  `validate` / `conformance` / `new-filter` / `dev` / `schema`) now live in a dedicated
  bin crate, so `cargo install plecto` is the first-class source-install path. The
  signature-verified container image remains the primary distribution channel.

### Changed

- **`plecto-server` is now a pure library** — the `[[bin]] plecto` target and the CLI
  modules moved to the new `plecto` crate. Migration: if you previously ran
  `cargo install plecto-server` for the binary, install `plecto` instead; the
  `plecto-server` library API is unchanged. Examples still run as
  `cargo run -p plecto-server --example <name>`.

## [0.3.5] - 2026-07-13

### Added

- crates.io publish preparation (ADR 000090, reconsiders ADR 000047's earlier decline):
  `plecto-host` / `plecto-control` / `plecto-server` now carry full crates.io package metadata
  (description, license, repository, homepage, readme, documentation, keywords, categories).
  The `plecto:filter` WIT contract and the `plecto new-filter` guest template are vendored into
  `crates/host/` and `crates/server/` respectively so `cargo package` can build each crate from
  its own directory alone; `scripts/check_wit_vendoring.py` (wired into CI) guards the vendored
  copies against drifting from their canonical sources.

### Changed

- Fixed the workspace `repository`/`homepage` URLs, which still pointed at the pre-rename
  `Kaikei-e/Plecto` (redirects to the current `Kaikei-e/PlectoProxy`).
- Internal path dependencies (`plecto-host`, `plecto-control`) now declare an explicit `version`
  alongside `path` — required for `cargo package` / `cargo publish` to accept them.

`publish` stays `false` for all three crates in this release; the first actual `cargo publish`
is a separate, not-yet-taken step.

## [0.3.4] - 2026-07-13

### Fixed

- **Server (fast path):** an h2 client's request carries no literal `Host` header (RFC 9113:
  the authority lives in the `:authority` pseudo-header), and the upstream leg is always
  HTTP/1.1 (ADR 000042); the forwarded header set had no `Host` in that case, so hyper's h1
  upstream client synthesized one from the destination URI — the upstream saw its own
  resolved address instead of the client's original authority. The proxy now fills `Host`
  from the client's derived authority whenever the forwarded headers carry none, leaving an
  HTTP/1.1 client's literal `Host` (or a filter's explicit override) untouched. Found via
  external testing of the multi-replica reference (`plecto/examples/multi-replica/`), whose
  TLS scenarios negotiate h2 via ALPN.

## [0.3.3] - 2026-07-13

### Added

- Multi-replica compose reference (ADR 000082 / 000088): a two-replica, L4-LB-fronted topology
  under `plecto/examples/multi-replica/` proves PROXY protocol v2 propagation, graceful replica
  drain, cross-replica session resumption over the shared STEK, and downstream mTLS end to end
  against the released signed image — switchable between TLS scenarios A/B via compose override
  files.
- Signed-image operator quick start (ADR 000084 / 000087): `docs/quickstart/` (English canonical,
  Japanese mirror) walks tag-to-digest resolution, cosign signature verification, and a first
  proxied response with Docker as the only prerequisite; the README gains a condensed Quick
  start section pointing at it.
- Weekly fuzz smoke workflow and a verification map (ADR 000086 / 000089): a scheduled CI
  workflow runs every libFuzzer target from its committed corpus off the PR/merge path, and
  `docs/verification.md` maps each verification claim this project makes to the CI workflow
  that backs it.
- Reference filters as signed OCI artifacts (ADR 000080). Every release now publishes the
  reference filter components — `filters/jwt`, `filters/cors`, `filters/apikey`,
  `filters/extauthz` — as individual CNCF Wasm OCI Artifacts under
  `ghcr.io/kaikei-e/plecto/filters/<name>:<filter-version>`, each cosign-signed (keyless, by
  digest) with an SPDX SBOM attestation of the shipped component bytes. Filters version
  independently of the runtime (immutable tags — content mismatch fails closed); digests and
  the required runtime capability profile land in the release notes, and
  `docs/reference-filters.md` carries the filter × profile compatibility matrix plus the
  verify-then-load recipe. CI builds the shelf with the same script release uses. Test-fixture
  builds no longer count as shipping; the Redis rate-limit reference stays off the shelf until
  its secure path lands (ADR 000081).
- Named runtime capability profiles in the release artifacts (ADR 000079). Every release now
  ships **two** profiles of the binary and the container image: **minimal** (unsuffixed — the
  former single artifact, default features, smallest attack surface) and **capabilities**
  (`-capabilities` suffix on the tarball name and the image tag) with `outbound-http`,
  `outbound-tcp` and `fat-guest` compiled in — what the capability-backed reference filters
  (JWKS-refreshing JWT auth, ext-authz, Redis-backed global rate limit) and TinyGo/Go guests
  need, prebuilt. Compile-time inclusion is not a runtime grant: the manifest's per-filter
  deny-by-default allowlist + SSRF floor (ADR 000036 / 000060) apply unchanged. Both profiles
  carry the full supply-chain discipline (cargo-auditable, SBOM, cosign, draft release);
  per-profile image digests land in the release notes. `plecto --version` now names the
  compiled profile. Source builds pick a profile with `cargo build -p plecto-server --features
  capabilities` or `docker build --build-arg FEATURES=capabilities .`.
- Mutual TLS in both directions (ADR 000078). Downstream: `[listen.client_auth] ca_path`
  makes a verified client certificate **required** on every TLS handshake the listener
  terminates (HTTP/1.1, h2 and h3/QUIC alike — one verifier for both wire faces; required
  mode only). Upstream: `[upstream.tls] client_cert_path` / `client_key_path` present a
  client identity on every TLS leg to that upstream, health probes included (both-or-neither,
  fail-closed). `[resumption]` shared STEK cannot be combined with `[listen.client_auth]`
  (ADR 000062 (b): resumption accepts a ticket without re-running client-certificate
  verification, and a shared key would let that ticket open on every replica);
  per-node resumption stays on, and its tickets carry the verified identity. The new private
  keys must be owner-only on unix (group/other-readable fails the build closed). Revocation
  (CRL/OCSP) and propagation of the verified identity to filters are declared deferred.

### Fixed

- **Control (config-plane):** `[listen.client_auth]` edits did not participate in the manifest
  content hash, so changing only the trust root and reloading via `SIGHUP` could report
  `ReloadOutcome::Unchanged` and silently keep serving the old CA; the CA bytes now ride the
  hash, are read once per build, and are shared with the verifier so the version always
  describes the roots actually enforced. mTLS listeners get their own per-CA-content session
  ticketer (isolated from the anonymous ticketer, re-keyed on CA rotation); the reload gate
  falls through to a full build rather than failing the `SIGHUP` when the version can't be
  computed (e.g. a momentarily unreadable CA file). Also fixed: a maglev table build could loop
  forever on an empty endpoint set or all-zero weights; `strip_prefix` route matching now
  requires a path-segment boundary (`/api` could previously rewrite `/apix/y` into `/x/y`);
  STEK, upstream client-key, and TLS private key material is zeroized on drop.
- **Server (fast path):** a connection that never completed its TLS handshake could hold a
  `MAX_CONNECTIONS` permit indefinitely (a pre-TLS slowloris), now bounded by the header-read
  timeout; h2 gains keep-alive pings so a vanished peer can no longer pin a permit forever; an
  upstream TLS client-config cache had an ABA bug that could serve a stale upstream's TLS
  config after a reload reused its address; request and response `Content-Length` framing is
  now derived by the host from the actual body instead of trusted from filter-declared output
  (CWE-444). QUIC/h3: a TLS reload now applies to new connections, trailers and a body shorter
  than its declared `Content-Length` are surfaced correctly (RFC 9114 §4.1.2) instead of a
  falsely-successful `finish()`, and hop-by-hop guest headers (RFC 9110 §7.6.1) are dropped at
  the filter boundary instead of failing the whole decision.
- **Host (extension plane):** pool breaker/cooldown timing moved from wall-clock to monotonic
  time (a clock adjustment could reopen or permanently disable the breaker), and an instantiate
  failure on the trusted build path now trips the breaker for builds only — idle instance reuse
  stays available, so a transient allocator trip can't take servable traffic down. Instance
  checkout now waits against an absolute deadline instead of restarting on every spurious
  wakeup; a discarded pooled instance's unterminated stdio partial line is no longer silently
  dropped (fat-guest logging); guest status codes are range-validated at the same gate as
  headers instead of being clamped to 502 downstream; Component resource-table growth is now
  bounded the same way the sync host already bounds it.

## [0.3.0] - 2026-07-11

### Added

- Native response compression (ADR 000074 / 000075): an opt-in `[route.compression]` block
  negotiates `gzip` / `br` / `zstd` against the client's `Accept-Encoding` (RFC 9110 §12.5.3
  qvalues; tie-break by the configured server-preference order, default zstd → br → gzip) and
  compresses eligible responses **after** the response filter chain — filters always see the
  identity representation, on every transport (HTTP/1.1, h2, h3). Safety defaults converge with
  industry practice: content-type allowlist (textual web types + `application/wasm`;
  `text/event-stream` excluded), 1 KiB min-length floor, skips for already-encoded /
  `Cache-Control: no-transform` / 204 / 206 / 304 / HEAD, `Vary: Accept-Encoding` on eligible
  responses, strong-ETag weakening, and per-frame flush so streamed bodies keep streaming.
  zstd frames are pinned to an ≤ 8 MiB window (RFC 9659) and the encoder is compress-only.
  No `[route.compression]` block = never transform (deny-by-default; also the per-route BREACH
  opt-out).
- `plecto:filter@0.3.0` (ADR 000073): `on-response` now receives the **as-forwarded request
  snapshot** (the request as it left the request-side chain — an auth filter's stamp and the
  untouched `Origin` both ride it) as its first parameter, and `response-decision` gains a
  **`replace(http-response)`** arm that supplants the upstream response with a synthesised one
  (terminal — the remaining chain is skipped; the upstream body is dropped unread, keeping the
  zero-copy invariant of ADR 000038). `replace` output passes the same fail-closed header
  validation as a request-side `short-circuit`. The fast path's old in-band "non-empty body
  means synthetic" signal is replaced by the typed `ResponseOutcome`. `0.2.0` is frozen at
  `wit/v0.2.0/` and stays loadable through a thin adapter (the request-context parameter is
  dropped) with a one-time deprecation warning, same rail as `0.1.0` (ADR 000071); a fixture
  guest pinned to the frozen 0.2 contract keeps that rail covered in CI. In-tree examples —
  Rust, MoonBit, JS, C — move to 0.3.0 (the TinyGo Tier-B fixture deliberately stays on 0.1.0
  as the V01 adapter's living coverage), and `plecto new-filter` scaffolds 0.3.0.
- `filter-cors` (ADR 000068 / F2 shelf): a CORS reference filter — the ADR 000073 motivating
  case. Preflight `OPTIONS` short-circuits at the gateway; actual responses gain the
  **dynamic origin echo** (`Access-Control-Allow-Origin` reflecting the request's `Origin`,
  read from the as-forwarded snapshot), with operator-owned policy via `[filter.config]`
  (`allowed-origins` / `allow-methods` / `allow-headers` / `allow-credentials` / `max-age`).

### Changed

- Docs sync to current code (HRT): README / README.ja, design-principles, operations, hardening,
  performance notes, writing-a-filter, ROADMAP, and the filter-template README now describe
  `plecto:filter@0.2.0`, six host capabilities including `host-config`, release `v0.2.6`, and 74
  accepted ADRs. Positioning prose names extension-model types rather than other products.
  `plecto new-filter` self-vendors the WIT contract at build time (ADR 000072) rather than
  fetching it via `wkg`. In-tree `filter-template/wit` refreshed to `@0.2.0`.
- Benchmark methodology aligned to industry practice (RFC 9411 KPI shapes, wrk2 schedule-latency,
  k6 open-model docs): authoritative open-loop is now `plecto-loadgen openloop` (CO-safe);
  `ceiling.csv` adds RR/CRR KPI labels; new `industry` phase and `bench/methodology.md`. Load runs
  stay loopback-only; `REQUIRE_OFFLINE=1` refuses a default IPv4 route. Legacy k6 open-loop via
  `OPENLOOP_GEN=k6`.
- Performance snapshot refreshed (2026-07-11 full `run-perf.sh all`): `performance/README.md`
  numbers and `performance/img/*.webp` charts regenerated; open-loop publishes the auto
  70 %-of-peak schedule-latency figure (0 dropped) instead of the old k6-pinned 60k/s path. A
  second full refresh (plus a fresh `cargo bench` criterion pass) the same day, ahead of the
  v0.3.0 release, confirms every fixed-rate/tail regression invariant this report tracks (the
  pooled WASM dispatch floor, the apikey filter's own cost, rate-limit enforcement, round-robin
  exactness) reproduces number-for-number after landing response compression (ADR 000074 / ADR
  000075) — expected, since compression is opt-in and off by default on every measured route.

### Fixed

- `plecto new-filter --lang rust` (ADR 000072): no longer fetches the `plecto:filter` WIT
  contract over the network via `wkg` — a scaffolded filter was generating against the deprecated
  `plecto:filter@0.1.0` contract even after 0.2.0 became current (ADR 000071), because the
  contract version lived as a string the CLI's own subprocess call had to be hand-bumped in
  lockstep with the host. The contract is now self-vendored: `include_str!`-embedded from the
  same `plecto/wit/world.wit` the host's own bindgen reads, and written into the scaffold at
  scaffold time. A released `plecto` binary can now only ever generate the contract version its
  own host runs, offline, with no dependency on registry reachability or publish ordering. The
  `wkg`/OCI distribution channel (ADR 000064) is unchanged for filter authors who don't use this
  CLI.

## [0.2.6] - 2026-07-10

### Added

- `plecto:filter@0.2.0` (ADR 000071): the WIT contract's `header.value` moves from `string` to
  `list<u8>`, so non-UTF-8 header bytes survive the filter boundary end-to-end instead of being
  lossily re-encoded. `plecto:filter@0.1.0` is frozen at `wit/v0.1.0/` and stays loadable — the
  host dual-binds both versions, detecting which one a component targets from its decoded WIT
  imports (not a byte scan, so it can't be fooled by a string a guest merely embeds) and lossily
  projecting headers into a 0.1 guest only for the duration of its call (`continue` never
  rewrites headers, so a value the guest left untouched still flows on as native bytes). Loading
  a 0.1 component now logs a one-time deprecation warning.
- ADR append-only graph checker (`scripts/check_adr_graph.py`, CI-enforced): validates
  `amends`/`supersedes` edges, `status`, and `[[NNNNNN]]` wikilinks across the ADR corpus.

### Changed

- Fast path header handling (`crates/server/src/headers.rs`): ingress/egress now carry header
  values as raw bytes (`HeaderValue::from_bytes`) instead of a lossy UTF-8 projection, and the
  `copy_headers_preserving` byte-recovery heuristic is removed as no longer needed — the contract
  itself now carries the wire bytes.
- A guest-returned header that violates the contract's byte-level rules (CRLF, a control byte, a
  non-token name, oversize) now fails closed as its own `invalid-output` fault (502), kept apart
  from `trap` so a misbehaving-but-alive filter is distinguishable from a crashing one in
  telemetry.
- The example filter fleet — the in-tree Rust filters and the C / MoonBit / JS polyglot
  conformance fixtures alike — moves to `plecto:filter@0.2.0`'s byte-valued headers.
- `design-principles.md`/`.ja.md`, `CLAUDE.md`, and `ROADMAP.md` synced to the current ADR count,
  the sixth basic capability (`host-config`, ADR 000066), and the byte-valued header contract.

## [0.2.5] - 2026-07-10

### Added

- JWT verification reference filter (ADR 000070): `filter-jwt` ships as Program F2's first
  reference — a Resource-Server-style Bearer JWT gate with ES256 and RS256 only (RFC 8725
  aligned), hybrid key supply (static PEM/JWK XOR `jwks_url` fetched once at `init` over outbound
  HTTP), RFC 6750 short-circuit 401 semantics, and on success `modified` with `x-authenticated-user`
  and `x-jwt-issuer` identity stamps. `isolation = "trusted"` is mandatory on both paths. Host
  integration tests cover the static key path, load-time failures, alg rejection, and JWKS init
  failure when outbound is unusable. Control now permits an empty `[filter.outbound_http] allow`
  as deny-all so wasm32-wasip2 guests can link `wasi:http` without granting any destination.

### Fixed

- Filter Dev Kit / `host-config` audit follow-up (ADR 000065 / 000066): PLECTO-E diagnostic
  codes now render on startup load failures and in SIGHUP reload logs; the PLECTO-E table lands
  in `docs/writing-a-filter.md`; dev signing keys are created atomically at mode 0600 with a
  `Zeroizing` reload buffer; `DevSigner` errors are typed with `thiserror`; `.plecto/` gitignore
  is re-asserted on every dev-key use; ADR 000065's implementation record is corrected
  (conformance-before-sign, signer types, inotify claim retracted).
- host (test deps): host JWT test token minting switches `jsonwebtoken` to the `aws_lc_rs`
  backend, dropping the RUSTSEC-2023-0071 `rsa` crate that `rust_crypto` pulled in and that
  `cargo-deny` correctly blocked.

## [0.2.4] - 2026-07-09

### Added

- WIT contract distribution via `wkg` / OCI Artifact (ADR 000064): `plecto:filter` (and the
  experimental, off-by-default `plecto:filter-streaming`) now publish to `ghcr.io` on every tagged
  release, alongside the existing signed binaries/images — `wkg get plecto:filter@<version>` is
  now the canonical way for a filter author to fetch the contract without cloning this repository.
  The release workflow records the published digest in each tag's release notes. Also formally
  establishes the contract compatibility policy (`docs/writing-a-filter.md` §8): additive changes
  are minor, breaking changes are major, and the host keeps loading every contract major version
  for at least two release series after a newer major ships.
- Filter Dev Kit, Rust slice (ADR 000065): `plecto new-filter --lang rust <name>` scaffolds a
  filter project (fetching the `plecto:filter` WIT via `wkg`, ADR 000064) with a generated
  project-local dev signing key, and `plecto dev <filter-dir>` watches `src/`, rebuilds
  (`wasm32-unknown-unknown` + `wit-component`), runs `plecto conformance` against the build
  (world validity, self-signed load-gate, no-trap, deadline compliance), and only on a pass signs
  it with the dev key and reloads the running gateway via the same SIGHUP path `plecto serve`
  uses — a non-conformant build is discarded without touching the manifest, so the running
  gateway never regresses. `plecto conformance <component.wasm> [--json]` also runs standalone
  against any component. New PLECTO-E0001–E0004 diagnostic codes (signature failure / quota
  exceeded / path-normalization rejection / dev-key-in-trust warning) surface as a stable
  code + cause + suggestion + docs four-tuple. `new-filter` scaffolds for Go/MoonBit/C/JS are
  explicitly deferred (a clear error, not a silent skip) — ADR 000065 records the full scope cut.

## [0.2.3] - 2026-07-09

### Added

- Fat-guest minimal WASI grant (ADR 000063, feature-gated `fat-guest`, off by default): a fixed,
  minimal WASI slice (`wasi:io` / `wasi:clocks` / `wasi:random` / `wasi:cli`, plus an empty
  `wasi:filesystem` — never filesystem access, never sockets) opt-in per filter via manifest
  `wasi = "minimal"`, for guest language runtimes that assume some baseline WASI is present.
  Unlocks Go/TinyGo as the first **Tier B** polyglot filter language (`filter-hello-go`),
  alongside the existing zero-WASI **Tier A** trio (Rust / MoonBit / JS / C, ADR 000055). A fat
  guest's stdout/stderr is bridged into its `host-log` (stdout → debug, stderr → warn; 4 KiB/line,
  64 KiB/request combined, truncate-and-warn-once past the budget) — including an unterminated
  final line — so a trap's own diagnostic output (a TinyGo panic message, say) still reaches the
  request's span instead of being lost with the discarded instance. Deny-by-default holds either
  way: a fat guest fails to instantiate unless BOTH the host's `fat-guest` build and the filter's
  `wasi = "minimal"` declaration are present, and the grant alone does not satisfy a
  `wasi:sockets` / `wasi:http` import — those stay separate, allowlisted capabilities
  (`outbound_http` / `outbound_tcp`, ADR 000036 / 000060).

## [0.2.2] - 2026-07-08

### Added

- Opt-in shared TLS session-ticket keys (ADR 000062, manifest `[resumption] stek_file`): replicas
  behind a round-robin load balancer recover TLS 1.3 resumption hit rate by deriving session
  ticket keys deterministically from (key-file contents, cert set) via HKDF, so every replica
  agrees without coordination, while a shared file cannot cross deployments serving different
  certs (the class of issue behind CVE-2025-23419 / CVE-2025-23048). Ticket construction is
  AES-256-CBC + HMAC-SHA-256 (encrypt-then-MAC), matching rustls' own move away from GCM for
  session tickets. Default per-node behavior (ADR 000052) is unchanged when `[resumption]` is
  absent.

## [0.2.1] - 2026-07-08

### Changed

- wit-bindgen bumped to 0.59.0 (from 0.58.0) across every example/bench filter guest and the
  CI toolchain pin (sha256-verified) — the C polyglot example (ADR 000055) now builds against
  this version too. Verified byte-identical Rust codegen for a `stream<u8>`-returning export
  between 0.58.0 and 0.59.0: the ergonomics gap ADR 000025 deferred true `stream<u8>` streaming
  on (a low-level `RawStreamReader` / private `StreamVtable` return type) and the
  wit-bindgen#1554 placeholder gating `wasi:http` convergence (ADR 000020 / 000025) both remain
  open — this release carries no contract or behavior change.

## [0.2.0] - 2026-07-08

### Added

- Two-tier rate limiting (ADR 000061): the native per-route / per-client-IP token bucket is now
  documented as the **local floor** (an immediate, external-call-free flood shed per replica),
  completed by `filter-ratelimit-redis` — a reference filter that holds the actual fleet-wide cap
  over a general fixed-window counter (`INCRBY` plus an unconditional `EXPIRE ... NX`, Redis ≥ 7.0
  / Valkey, no Lua dependency) consulted over the outbound-TCP capability. Running both together
  is now the recommended shape for multi-replica deployments (see the hardening guide).
- `host-config` capability (ADR 000066): a filter's own business settings (backend address,
  window, limit, `on_backend_error`, ...) can now come from the manifest's `[filter.config]`
  instead of being hardcoded in the guest. A missing or invalid required value fails the filter's
  *load* (with `isolation = "trusted"`) rather than every request.
- Outbound TCP capability for filters (ADR 000060, feature-gated `outbound-tcp`): filters can open
  outbound TCP connections (Redis, Valkey, memcached, ...) over `wasi:sockets`, behind the same
  deny-by-default allowlist, SSRF guard, and IP-pin shape as outbound HTTP. `filter-tcp-gate` is
  the minimal example.
- HTTP/3 GOAWAY graceful drain, a `/readyz` drain contract, and tunnel observability (ADR 000059):
  a drain now sends GOAWAY on every h3 connection and lets in-flight requests finish within the
  same drain window TCP already uses, instead of closing connections immediately; `/readyz` flips
  to not-ready ahead of the drain so a front load balancer stops sending new traffic first; a
  live gauge and byte counters make long-lived WebSocket tunnels visible.

### Changed

- **Breaking (manifest)**: `[filter.outbound]` is renamed to `[filter.outbound_http]`, making room
  for the new `[filter.outbound_tcp]` section — update any manifest that declares outbound HTTP
  for a filter.
- The hardening guide now recommends running the local floor and the `filter-ratelimit-redis`
  global filter together as the default multi-replica rate-limiting shape, and corrects an
  earlier reference to the (then-unshipped) reference filter using `outbound-http` — it uses
  `outbound-tcp`.

## [0.1.4] - 2026-07-06

### Added

- PROXY protocol v2 reception (ADR 000057), opt-in per listener via `[listen.proxy_protocol]`
  with a required trusted-CIDR list: a v2 header arriving from a trusted load balancer restores
  the real client IP end to end (including before a TLS handshake), feeding the edge client-IP
  model, rate limiting and access logs. A missing, malformed or untrusted header cuts the
  connection fail-closed; traffic from peers outside the trusted CIDRs passes through unchanged.
- Polyglot filter examples proving the any-language claim: MoonBit, JavaScript (ComponentizeJS)
  and C (wasi-sdk) guests, each built to a zero-WASI header-only component and verified by the
  same conformance assertions as the Rust fixture (the `polyglot-conformance` test suite).
- Fuzzing scaffold: cargo-fuzz, with a first target on the PROXY protocol v2 parser.

### Changed

- Buffered request bodies now count as replayable for upstream retries (ADR 000058). On a
  `filter-body` route the body is already fully buffered, so a retry re-sends it instead of
  giving up: a connect failure (the upstream never received the request) retries for any
  method, a per-try timeout or gateway-class 5xx (502–504) retries idempotent methods only —
  the retry decision table itself is unchanged. Re-sends share one reference-counted buffer
  (no memory copy), stay inside the existing bounded-retry budget (max retries, jittered
  backoff, overall deadline), and the streaming (non-buffered) path behaves exactly as before.

## [0.1.3] - 2026-07-06

### Fixed

- Filter state quotas: `KvQuota`'s read-decide-write accounting is striped across 64
  hash-picked per-key locks (stripe seed per instance, so a tenant cannot precompute keys that
  pile onto another tenant's stripe) — one stalled `charge_and_apply` (e.g. a slow persistent
  write) no longer blocks unrelated keys. The namespace/global tallies moved to their own lock
  whose critical section is pure arithmetic: no backend I/O ever runs under a shared lock.
  Same-key atomicity (the accounting-race fix from 0.1.2) is preserved.

## [0.1.2] - 2026-07-06

### Added

- Stateless TLS 1.3 session resumption (ADR 000052): RFC 5077-style self-encrypted session
  tickets from one process-lifetime ticketer (6 h key rotation / 12 h acceptance window),
  shared by the TCP and QUIC server configs and across manifest reloads — a reload never
  invalidates outstanding tickets, per-session server memory is zero, and 0-RTT stays
  rejected.
- `plecto-loadgen tls`: full-handshake vs resumed-handshake benchmark rungs for the TLS
  termination path.

### Fixed

- server: a request-body buffer-permit acquisition error now fails closed (503) instead of
  silently proceeding without a permit (a latent bypass of the buffered-body concurrency cap);
  the admin (metrics/health) listener gained the same connection cap and header-read hardening
  the data-plane listener already had.
- control: closed a TOCTOU race in outlier detection where two instances crossing their
  failure threshold in the same instant could both eject and exceed `max_ejection_percent`;
  cut a per-request heap allocation and repeated per-request filter-list resolution on the
  routing hot path.
- host: per-filter quota accounting (`host-kv` / `host-counter` / `host-ratelimit`) is atomic
  under concurrency, closing a race where concurrent same-key calls could double-charge or
  double-release budget and drift the quota cap; the untrusted filter lifecycle gained a
  per-filter circuit breaker so a deterministically failing init stops re-paying its full init
  budget on every request; the in-memory trace sink's retained spans are bounded (FIFO
  eviction).

## [0.1.1] - 2026-07-04

### Added

- `[upstream.tls] sni` (ADR 000050): pins the TLS verification name for a forwarded upstream leg
  independently of the connected address — closes the gap where an IP-literal or DNS-expanded
  (`resolve_interval_ms`, ADR 000044) upstream address sends no SNI and is verified against the
  bare IP, which fails unless the certificate carries an IP SAN. `plecto validate` warns (never
  rejects) when `sni` is absent on an upstream that may resolve to a bare IP.

### Changed

- TLS crypto provider consolidated on `aws-lc-rs` (ADR 000051), replacing `ring`, across
  downstream TLS termination, upstream re-encryption, and QUIC/HTTP-3. `sigstore` (cosign
  signature verification, ADR 000006 / 000047) already links aws-lc-rs unconditionally, so this
  removes a second crypto backend rather than adding a new dependency, and gets X25519MLKEM768
  preferred by default (rustls `prefer-post-quantum`) on both the TCP and QUIC paths.

## [0.1.0] - 2026-07-03

The first tagged release. Everything below ships in `v0.1.0`; the highlights of the
pre-release history are summarised first, the final pre-tag additions follow.

### Highlights (initial release)

- **Fast path**: HTTP/1.1, HTTP/2 (TLS+ALPN), HTTP/3 (QUIC, same port, Alt-Svc advertised);
  rustls TLS termination with SNI selection and certificate hot reload.
- **Routing**: host / path-prefix / method / header / query matching (most-specific wins),
  weighted traffic splits (canary), prefix strip, fail-closed ingress path normalization.
- **Resilience**: round-robin / weighted least-request (P2C) / weighted Maglev load balancing,
  active + passive health checks (pessimistic start), outlier detection, per-upstream circuit
  breaker, two-tier timeouts (per-try + overall), jittered bounded retries, native per-route
  rate limiting.
- **Extension plane**: `plecto:filter` WASM Component Model filters (any language), pooled
  instances, deny-by-default capabilities (log / clock / KV / counter / rate-limit /
  outbound-HTTP with SSRF guard), per-filter quotas and deadlines, cosign + SBOM
  verify-then-load, fail-closed trap handling.
- **Operations**: declarative TOML manifest (strict parse), SIGHUP hot reload + graceful
  shutdown, Prometheus metrics + health/readiness admin endpoint, structured JSON logs,
  opt-in access log, OTLP trace export, redb persistent filter state.

### Added

- HTTP/1.1 Upgrade / WebSocket tunnelling (`[route.upgrade]`, ADR 000048): a per-route token
  allowlist (the h2c-smuggling mitigation shape; `h2c` is rejected at validation) re-issues the
  handshake upstream and splices a bidirectional tunnel on a verified 101, with an
  activity-reset idle timeout (default 5 min, `0` disables) and drain-aware shutdown.
- `plecto schema`: the manifest's JSON Schema (draft-07) on stdout, derived from the parsing
  structs themselves — editor completion (taplo / Even Better TOML) and CI validation from one
  generated artifact (ADR 000049).
- Upstream TLS re-encryption (`[upstream.tls]`, ADR 000042): per-upstream rustls client with
  ALPN-negotiated HTTP/2 / HTTP/1.1, optional custom CA (`ca_path`), `TE: trailers` pass-through
  and response-trailer forwarding — gRPC now works end-to-end through the proxy. Health probes
  follow the upstream scheme; certificate verification has no off switch (fail-closed).
- `plecto validate <manifest>` (config-test shape): static manifest validation for CI and
  pre-reload checks — strict parse plus every fail-closed startup check that needs no artifact —
  and `plecto --version`.
- `[listen]` manifest section: the data-plane bind address (`addr`) and the Alt-Svc h3
  advertisement port (`advertised_port`) are declared in the manifest, fixing container
  deployments (`0.0.0.0` binds; internal-vs-published port mismatch).
- Periodic DNS re-resolution (`resolve_interval_ms` on `[[upstream]]`): each A/AAAA record a
  hostname resolves to becomes a load-balancing endpoint with its own health, refreshed on an
  interval — Compose service names and k8s headless Services now track container re-creation.
- Release engineering: reference `Dockerfile` (distroless runtime), tag-triggered release
  workflow producing signed binaries (cosign keyless + SBOM) and a signed multi-arch GHCR image.
