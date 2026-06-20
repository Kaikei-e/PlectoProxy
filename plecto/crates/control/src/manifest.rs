//! The declarative manifest (ADR 000007 / 000008): the single, static source of truth for
//! which filters are loaded, pinned by OCI digest, with which trust roots, in what chain
//! order. TOML (mirrors Cargo; ADR 000008 static config). Routes are deferred until the
//! fast-path server exists; v0.1 has a single chain.

use plecto_host::LoadOptions;
use serde::Deserialize;

use crate::error::ControlError;

/// A parsed manifest. Deserialised from TOML; no I/O happens here (key files and artifacts
/// are resolved by `Control`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default)]
    pub trust: Trust,
    /// `[[filter]]` entries.
    #[serde(default, rename = "filter")]
    pub filters: Vec<FilterEntry>,
    #[serde(default)]
    pub chain: Chain,
}

/// Trust roots: paths (manifest-relative) to trusted signer public keys, PEM (ADR 000006).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trust {
    #[serde(default)]
    pub keys: Vec<String>,
}

/// One filter to load, pinned by OCI digest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterEntry {
    /// Host-assigned identity; namespaces the filter's KV (ADR 000011) and names it in chains.
    pub id: String,
    /// Manifest-relative path to the local OCI image-layout for this filter.
    pub source: String,
    /// Pinned OCI image-manifest digest, `sha256:...` (reproducibility / supply chain).
    pub digest: String,
    #[serde(default)]
    pub isolation: IsolationKind,
    pub init_deadline_ms: Option<u64>,
    pub request_deadline_ms: Option<u64>,
    pub max_memory_bytes: Option<u64>,
}

/// Manifest spelling of the host's `Isolation`. Defaults to `untrusted` (fail-closed).
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IsolationKind {
    #[default]
    Untrusted,
    Trusted,
}

/// The single ordered chain for v0.1 (named chains / route matching are deferred to the
/// fast-path server).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chain {
    #[serde(default)]
    pub filters: Vec<String>,
}

impl Manifest {
    /// Parse a manifest from a TOML string.
    pub fn from_toml(s: &str) -> Result<Self, ControlError> {
        Ok(toml::from_str(s)?)
    }
}

impl FilterEntry {
    /// The host `LoadOptions` for this entry: isolation plus any metering overrides
    /// (ADR 000006). Unset knobs keep the host defaults.
    pub(crate) fn load_options(&self) -> LoadOptions {
        let mut opts = match self.isolation {
            IsolationKind::Trusted => LoadOptions::trusted(),
            IsolationKind::Untrusted => LoadOptions::untrusted(),
        };
        if let Some(ms) = self.init_deadline_ms {
            opts = opts.with_init_deadline_ms(ms);
        }
        if let Some(ms) = self.request_deadline_ms {
            opts = opts.with_request_deadline_ms(ms);
        }
        if let Some(bytes) = self.max_memory_bytes {
            opts = opts.with_max_memory_bytes(bytes);
        }
        opts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_filters_and_chain_with_defaults() {
        let m = Manifest::from_toml(
            r#"
[[filter]]
id = "auth"
source = "artifacts/auth"
digest = "sha256:abc"

[[filter]]
id = "rl"
source = "artifacts/rl"
digest = "sha256:def"
isolation = "trusted"
request_deadline_ms = 25

[chain]
filters = ["auth", "rl"]
"#,
        )
        .unwrap();

        assert_eq!(m.filters.len(), 2);
        assert_eq!(m.filters[0].isolation, IsolationKind::Untrusted); // default
        assert_eq!(m.filters[1].isolation, IsolationKind::Trusted);
        assert_eq!(m.filters[1].request_deadline_ms, Some(25));
        assert_eq!(m.chain.filters, vec!["auth".to_string(), "rl".to_string()]);
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

    #[test]
    fn load_options_maps_isolation_and_overrides() {
        let entry = FilterEntry {
            id: "x".to_string(),
            source: "s".to_string(),
            digest: "sha256:abc".to_string(),
            isolation: IsolationKind::Trusted,
            init_deadline_ms: None,
            request_deadline_ms: Some(40),
            max_memory_bytes: Some(1024),
        };
        let opts = entry.load_options();

        assert_eq!(opts.isolation, plecto_host::Isolation::Trusted);
        assert_eq!(opts.request_deadline_ms, 40);
        assert_eq!(opts.max_memory_bytes, 1024);
        // an unset knob keeps the host default
        assert_eq!(
            opts.init_deadline_ms,
            LoadOptions::trusted().init_deadline_ms
        );
    }
}
