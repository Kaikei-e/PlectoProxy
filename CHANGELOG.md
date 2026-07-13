# Changelog

All notable changes to Plecto are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## Versioning policy (pre-1.0)

- **Binary / manifest**: while the version is `0.x`, a **minor** bump (`0.1 → 0.2`) may contain
  breaking changes to the manifest schema or CLI; they are always listed under **Changed** /
  **Removed** with a migration note. Patch bumps are always safe to take.
- **WIT contract**: the filter contract is versioned independently as `plecto:filter@<version>`.
  A manifest declares which contract its filters target; the host keeps loading every contract
  version it ships support for, so a proxy upgrade never silently breaks deployed filters. The
  contract is published as a CNCF Wasm OCI Artifact to `ghcr.io` on every tagged release (`wkg
  publish`, ADR 000064); the published digest is recorded in that tag's release notes, the
  contract-side counterpart of the binary/image supply-chain record below.
- **Release artifacts**: binaries and images are cosign-signed (keyless) with SBOMs attached —
  the same supply-chain bar Plecto's own filter loading enforces. Verify commands are in the
  release notes of each release.

## [Unreleased]

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
