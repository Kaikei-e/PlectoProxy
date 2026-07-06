//! Typed control-plane errors (bp-rust: domain errors are `thiserror` enums, not `anyhow`).
//! A caller can tell a config mistake (`ManifestParse`, `UnknownChainFilter`) apart from a
//! supply-chain failure (`DigestMismatch`, `Load`) — both are fail-closed, but distinguishable.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("manifest parse error: {0}")]
    ManifestParse(#[from] toml::de::Error),

    /// The manifest could not be canonicalised for content-hashing. Serialising our own
    /// derived types is infallible in practice; a typed variant keeps `content_hash`
    /// panic-free (bp-rust: no `expect` outside binary entry points).
    #[error("manifest serialisation failed: {0}")]
    ManifestSerialize(#[from] serde_json::Error),

    /// `reload_from_disk` was called on a `Control` built from an in-memory manifest (no
    /// backing path). Construct with `from_manifest_path` / `load_at` to enable disk reload.
    #[error("control plane has no manifest path on disk to reload from")]
    NoManifestPath,

    /// A reload's manifest changed the `[trust]` section. Trust roots are fixed at
    /// construction (same `Host`, same epoch ticker — ADR 000006 / 000008); a reload only
    /// swaps the filter set + chain, never the trust policy. Rejecting the change fail-closed
    /// (rather than silently ignoring it) keeps an operator from believing a key rotation took
    /// effect when it did not — rotate trust by restarting with the new manifest.
    #[error("manifest [trust] changed; trust roots are fixed at construction — restart to apply")]
    TrustChangeRequiresRestart,

    /// A reload's manifest changed the `[state]` section. The state backend is fixed at
    /// construction like the trust roots (ADR 000041): the `Host` holds one `KvBackend` for
    /// its life, so a backend/path edit cannot take effect on a reload. Rejecting it
    /// fail-closed keeps an operator from believing a durability change took effect when it
    /// did not — change the backend by restarting with the new manifest.
    #[error(
        "manifest [state] changed; the state backend is fixed at construction — restart to apply"
    )]
    StateChangeRequiresRestart,

    /// The `[state]` section is inconsistent (ADR 000041): `redb` without a `path`, or a
    /// `path` under `memory`. Rejected fail-closed so a half-edited section never silently
    /// runs on memory while the operator believes state is durable.
    #[error("invalid [state] config: {0}")]
    InvalidStateConfig(String),

    /// The configured state backend could not be constructed (ADR 000041): the redb file's
    /// parent directory is missing, or redb failed to open/create the database.
    #[error("state backend init failed: {0}")]
    StateBackendInit(String),

    /// The `[listen]` section is inconsistent (ADR 000057): a `[listen.proxy_protocol]` with an
    /// empty or unparseable `trusted` CIDR list. Rejected fail-closed — enabling PROXY v2
    /// without declaring who may speak it would mean trusting every peer (deny-by-default, P4).
    #[error("invalid [listen] config: {0}")]
    InvalidListenConfig(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("trusted key error: {0}")]
    TrustKey(String),

    #[error("host initialisation failed: {0}")]
    HostInit(String),

    /// The artifact could not be resolved or was malformed (missing layer, bad layout, …).
    #[error("artifact {source_ref:?}: {reason}")]
    Artifact { source_ref: String, reason: String },

    /// The resolved artifact's content digest did not equal the manifest's pin (ADR 000007
    /// reproducibility / supply-chain integrity). Fail-closed.
    #[error(
        "content digest mismatch for {source_ref:?}: manifest pinned {expected}, artifact is {actual}"
    )]
    DigestMismatch {
        source_ref: String,
        expected: String,
        actual: String,
    },

    /// The host rejected the filter at the provenance/load gate (ADR 000006 signature/SBOM,
    /// or instantiation). Carries the host's `anyhow` error for its message.
    #[error("filter {id:?} failed the load gate: {err}")]
    Load { id: String, err: anyhow::Error },

    /// A filter entry carried an out-of-range metering / rate-limit value (e.g. a zero deadline,
    /// zero memory cap, or a rate-limit bucket that can never refill). Rejected fail-closed at
    /// build so a config typo cannot reach the host's metering arithmetic (CWE-20).
    #[error("filter {id:?} has invalid config: {reason}")]
    InvalidFilterConfig { id: String, reason: String },

    #[error("chain references unknown filter {0:?}")]
    UnknownChainFilter(String),

    #[error("duplicate filter id {0:?} in manifest")]
    DuplicateFilterId(String),

    #[error("duplicate upstream name {0:?} in manifest")]
    DuplicateUpstream(String),

    /// An upstream declared no `addresses` (ADR 000017). An upstream must have at least one
    /// instance to forward to; an empty list is fail-closed at build time, not at request time.
    #[error("upstream {0:?} has no addresses")]
    EmptyUpstreamAddresses(String),

    /// The upstream registry's lock was poisoned (a thread panicked while holding it). Surfaced
    /// rather than re-panicked so a reconcile fails closed (the running set stays live).
    #[error("upstream registry lock poisoned")]
    UpstreamRegistryPoisoned,

    /// An upstream's load-balancing config (ADR 000035) was malformed: a per-instance `weight` of
    /// zero or over the cap, a `maglev` upstream without (or a non-`maglev` upstream with) a
    /// `[upstream.hash]` block, a `header` hash key with no header name, a non-prime / out-of-range
    /// `table_size`, or more instances than the maglev table can index. Rejected fail-closed at
    /// build, before the persistent registry mutates, so a bad LB config never reaches the hot path.
    #[error("upstream {name:?} has invalid load-balancing config: {reason}")]
    InvalidUpstreamLb { name: String, reason: String },

    #[error("route (prefix {path_prefix:?}) references unknown upstream {upstream:?}")]
    UnknownRouteUpstream {
        path_prefix: String,
        upstream: String,
    },

    #[error("route (prefix {path_prefix:?}) references unknown filter {filter:?}")]
    UnknownRouteFilter { path_prefix: String, filter: String },

    /// A route's forwarding target or weighted traffic split (ADR 000034) was malformed: both or
    /// neither of `upstream` / `backends` set, an empty `backends`, every weight zero, a weight
    /// over the cap, or a reduced split table too large. Rejected fail-closed at build, before the
    /// upstream registry reconciles, so a bad split never mutates persistent state.
    #[error("route (prefix {path_prefix:?}) has an invalid traffic split: {reason}")]
    InvalidRoute { path_prefix: String, reason: String },

    /// A route's native rate limit (ADR 000033) had an out-of-range value (`rate` or `burst` of
    /// zero — a bucket that can never serve a token). Rejected fail-closed at build, like the
    /// per-filter rate-limit validation, so a config typo cannot reach the limiter arithmetic.
    #[error("route (prefix {path_prefix:?}) has invalid rate_limit: {reason}")]
    InvalidRouteRateLimit { path_prefix: String, reason: String },

    /// A TLS cert/key file could not be read, parsed, or built into a usable certificate
    /// (ADR 000014). Fail-closed: a bad cert aborts the build, so reload never swaps in a TLS
    /// config that cannot serve. `host` is the SNI the entry was for (`None` = default cert).
    #[error("TLS cert for {host:?} ({path:?}): {reason}")]
    TlsCert {
        host: Option<String>,
        path: String,
        reason: String,
    },

    /// An `[upstream.tls]` CA bundle could not be read or parsed, or yielded no usable root
    /// (ADR 000042). Fail-closed at build, like `TlsCert`: a bad CA path aborts the build /
    /// reload before the upstream registry mutates, so the forward leg never silently falls
    /// back to unverified (or plaintext) forwarding.
    #[error("upstream {upstream:?} TLS CA ({path:?}): {reason}")]
    UpstreamTlsCa {
        upstream: String,
        path: String,
        reason: String,
    },

    /// An `[upstream.tls] sni` verification-name override did not parse as a valid DNS name or IP
    /// address (ADR 000050). Fail-closed at build, like `UpstreamTlsCa`: a bad `sni` aborts the
    /// build / reload before the upstream registry mutates, rather than letting every handshake
    /// to this upstream fail at request time.
    #[error("upstream {upstream:?} TLS sni {sni:?}: {reason}")]
    UpstreamTlsSni {
        upstream: String,
        sni: String,
        reason: String,
    },
}
