//! The declarative manifest (ADR 000007 / 000008): the single, static source of truth for
//! which filters are loaded, pinned by OCI digest, with which trust roots, in what chain
//! order. TOML (mirrors Cargo; ADR 000008 static config). Routes are deferred until the
//! fast-path server exists; v0.1 has a single chain.
//!
//! Split by concern: this module holds only `Manifest` itself + `Manifest::from_toml`. Each
//! `[section]`'s schema (every `struct`/`enum` + its serde defaults) lives in its own sibling
//! module — `listen`, `observability`, `tls`, `upstream`, `route`, `state`, `trust`,
//! `filter_entry`, `chain` — and is re-exported here so `crate::manifest::X` keeps resolving for
//! every type. `validate` holds the build-time validation (`Upstream::validate_lb`,
//! `FilterEntry::validate`, `State::validate`), `content_hash` holds the semantic content-hash,
//! and `lowering` holds `FilterEntry::load_options` (manifest → host `LoadOptions`).

mod chain;
mod content_hash;
mod filter_entry;
mod listen;
mod lowering;
mod observability;
mod route;
mod state;
mod tls;
mod trust;
mod upstream;
mod validate;

use serde::{Deserialize, Serialize};

use crate::error::ControlError;

pub use chain::Chain;
pub use filter_entry::{FilterEntry, IsolationKind, OutboundHttpConfig};
// `AllowDest` / `RateLimitConfig` are schema fields reached through `OutboundHttpConfig` /
// `FilterEntry` rather than by name elsewhere in this crate today; `SchemeKind` is only named
// via `crate::manifest::X` from the `outbound-http`-gated half of `lowering.rs`. Re-exported
// anyway so `crate::manifest::X` keeps resolving for every schema type (module doc above).
#[allow(unused_imports)]
pub use filter_entry::{AllowDest, RateLimitConfig, SchemeKind};
pub use listen::{Listen, ProxyProtocolTrust};
// `ProxyProtocol` / `Drain` are schema fields reached through `Listen` rather than by name
// elsewhere in this crate; re-exported for the same schema-type completeness reason as
// `AllowDest` below.
#[allow(unused_imports)]
pub use listen::{Drain, ProxyProtocol};
pub use observability::Observability;
pub(crate) use route::MAX_BACKEND_WEIGHT;
pub use route::{RateLimitKeyKind, Route, RouteRateLimit};
// `Backend` / `RouteMatch` / `RouteUpgrade` are only named via `crate::manifest::X` from
// `#[cfg(test)]` code elsewhere in the crate; re-exported for the same completeness reason.
#[allow(unused_imports)]
pub use route::{Backend, RouteMatch, RouteUpgrade};
pub use state::{State, StateBackendKind};
pub use tls::TlsCert;
pub use trust::Trust;
pub use upstream::{
    CircuitBreaker, HashKeyKind, HealthConfig, LbAlgorithm, OutlierDetection, Upstream, UpstreamTls,
};
// `AddressSpec` / `HashConfig` / `WeightedAddress`: same as `Backend` above — only named via
// `crate::manifest::X` from `#[cfg(test)]` code elsewhere in the crate.
#[allow(unused_imports)]
pub use upstream::{AddressSpec, HashConfig, WeightedAddress};
pub(crate) use upstream::{MAX_HASH_TABLE_SIZE, MAX_INSTANCE_WEIGHT};

/// A parsed manifest. Deserialised from TOML; no I/O happens here (key files and artifacts
/// are resolved by `Control`). `Serialize` exists only to derive the semantic content hash
/// (`content_hash`) — the canonical, representation-independent identity of the config.
///
/// Determinism invariant (f000004 #6): no map fields (`HashMap`). `content_hash` relies on
/// `serde_json` emitting fields and `Vec` elements in a fixed order; a `HashMap` would
/// serialise in nondeterministic order and silently break reload idempotency. If a manifest
/// ever needs a map, use `BTreeMap` (ordered) and keep this invariant.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub trust: Trust,
    /// `[state]`: the host state backend (ADR 000041). Rides the content hash like `[trust]`,
    /// and like `[trust]` it is fixed at construction — a reload rejects a change.
    #[serde(default)]
    pub state: State,
    /// `[[filter]]` entries.
    #[serde(default, rename = "filter")]
    pub filters: Vec<FilterEntry>,
    /// The default `[chain]` — driven by the chain-only `Control::on_request` convenience and
    /// used by a route that names no filters of its own. The fast-path server uses `[[route]]`.
    #[serde(default)]
    pub chain: Chain,
    /// `[[upstream]]` entries: named backends the fast-path server forwards to (ADR 000013).
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<Upstream>,
    /// `[[route]]` entries: host + path-prefix → an (inline) chain and an upstream (ADR 000013).
    /// Empty until the fast-path server is configured; matching is the server's job, declared here.
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
    /// `[[tls]]` entries: server certificates for TLS termination (ADR 000014). Empty = plain
    /// HTTP/1.1 (the fast path serves TLS only when at least one cert is declared).
    #[serde(default, rename = "tls")]
    pub tls: Vec<TlsCert>,
    /// `[observability]`: operational metrics / access-log / admin-endpoint config (ADR 000009),
    /// captured at construction. `skip_serializing` keeps it OUT of the semantic `content_hash`, so
    /// toggling observability never counts as a config-version change (it is not part of the
    /// filter/route identity, and the admin listener binds once at startup — like `[trust]`).
    #[serde(default, skip_serializing)]
    pub observability: Observability,
    /// `[listen]`: the data-plane bind address + h3 advertisement (moka-1 field report §3.2/§3.4).
    /// Captured at construction like `[observability]` — the listener binds once at startup, so a
    /// reload does not re-bind (restart to move the listener); `skip_serializing` keeps it out of
    /// the semantic `content_hash` for the same reason.
    #[serde(default, skip_serializing)]
    pub listen: Listen,
}

impl Manifest {
    /// Parse a manifest from a TOML string.
    pub fn from_toml(s: &str) -> Result<Self, ControlError> {
        Ok(toml::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_routes_and_upstreams() {
        let m = Manifest::from_toml(
            r#"
[[filter]]
id = "auth"
source = "artifacts/auth"
digest = "sha256:abc"

[[upstream]]
name = "api-svc"
addresses = ["127.0.0.1:9000", "127.0.0.1:9001"]
[upstream.health]
path = "/healthz"

[[route]]
filters = ["auth"]
upstream = "api-svc"
strip_prefix = "/api"
[route.match]
host = "example.com"
path_prefix = "/api"
"#,
        )
        .unwrap();

        assert_eq!(m.upstreams.len(), 1);
        let addrs: Vec<&str> = m.upstreams[0]
            .addresses
            .iter()
            .map(|a| a.address())
            .collect();
        assert_eq!(addrs, vec!["127.0.0.1:9000", "127.0.0.1:9001"]);
        assert!(
            m.upstreams[0].addresses.iter().all(|a| a.weight() == 1),
            "bare addresses default to weight 1"
        );
        // health is required; the unspecified knobs take their defaults
        assert_eq!(m.upstreams[0].health.path, "/healthz");
        assert_eq!(m.upstreams[0].health.interval_ms, 2000);
        assert_eq!(m.upstreams[0].health.timeout_ms, 1000);
        assert_eq!(m.upstreams[0].health.healthy_threshold, 2);
        assert_eq!(m.upstreams[0].health.unhealthy_threshold, 3);
        assert_eq!(m.routes.len(), 1);
        let r = &m.routes[0];
        assert_eq!(r.matcher.host.as_deref(), Some("example.com"));
        assert_eq!(r.matcher.path_prefix, "/api");
        assert_eq!(r.filters, vec!["auth".to_string()]);
        assert_eq!(r.upstream.as_deref(), Some("api-svc"));
        assert!(r.backends.is_empty(), "single upstream uses the shorthand");
        assert_eq!(r.strip_prefix.as_deref(), Some("/api"));
        // a routing edit must flip the config version (routes ride the semantic hash)
        let mut m2 = m.clone();
        m2.routes[0].matcher.path_prefix = "/v2".to_string();
        assert_ne!(
            m.content_hash().unwrap(),
            m2.content_hash().unwrap(),
            "a route change must flip the content hash"
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        let parsed = Manifest::from_toml(
            r#"
[[filter]]
id = "x"
source = "s"
digest = "sha256:abc"
typo_field = true
"#,
        );
        assert!(parsed.is_err(), "deny_unknown_fields should reject a typo");
    }
}
