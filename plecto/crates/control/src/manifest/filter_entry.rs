//! One filter to load (`[[filter]]`), pinned by OCI digest, plus its host-side rate-limit and
//! outbound-HTTP sub-config (ADR 000006 / 000026 / 000036).

use serde::{Deserialize, Serialize};

/// Host-side token-bucket spec for a filter's `host-ratelimit` (ADR 000026), set in the manifest
/// as an inline table `ratelimit = { capacity = .., refill_tokens = .., refill_interval_ms = .. }`.
/// The operator owns it, so an untrusted filter cannot supply or override its own limit (the WIT
/// `try-acquire` carries only `key` + `cost`).
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Maximum tokens the bucket can hold.
    pub capacity: u64,
    /// Tokens added each refill interval.
    pub refill_tokens: u64,
    /// Refill interval in milliseconds (the host advances by whole intervals).
    pub refill_interval_ms: u64,
}

/// A URL scheme an outbound allowlist entry may name (ADR 000036). Defaults to `https`.
#[derive(
    Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "lowercase")]
pub enum SchemeKind {
    #[default]
    Https,
    Http,
}

impl SchemeKind {
    #[cfg(feature = "outbound-http")]
    pub(super) fn default_port(self) -> u16 {
        match self {
            SchemeKind::Https => 443,
            SchemeKind::Http => 80,
        }
    }
}

/// One allowed outbound destination — an exact `(scheme, host, port)` triple (ADR 000036). No
/// wildcards: the target endpoints (JWKS / introspection / ext_authz) are fixed and known.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AllowDest {
    /// Exact host — a DNS name (matched case-insensitively) or an IP literal.
    pub host: String,
    /// Destination port. Defaults to the scheme's default (443/80) when omitted.
    pub port: Option<u16>,
    /// URL scheme (default `https`).
    #[serde(default)]
    pub scheme: SchemeKind,
}

/// A filter's outbound HTTP policy (ADR 000036), declared as `[filter.outbound_http]`. The operator
/// owns it — an untrusted filter cannot supply, widen, or override it (the
/// `wasi:http/outgoing-handler` import carries only the request). Absent = the filter is lent no
/// outbound HTTP capability.
///
/// `allow` is deny-by-default: only listed destinations are reachable. `allow_private` opts specific
/// RFC1918 / ULA CIDRs past the SSRF guard's private-range block (for internal ext_authz); it never
/// opens the always-blocked reserved floor (loopback / link-local / cloud-metadata). Timeouts and
/// sizes are host-clamped.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutboundHttpConfig {
    /// The exact destinations this filter may call. Must be non-empty when the section is present.
    pub allow: Vec<AllowDest>,
    /// Private/ULA CIDRs (e.g. `"10.1.0.0/16"`) this filter may reach despite the SSRF private-range
    /// block. Empty (default) leaves all private space blocked.
    #[serde(default)]
    pub allow_private: Vec<String>,
    /// TCP connect timeout (ms). Host default/ceiling apply when omitted/exceeded.
    pub connect_timeout_ms: Option<u64>,
    /// Whole-call wall-clock timeout (ms). Host default/ceiling apply when omitted/exceeded.
    pub total_timeout_ms: Option<u64>,
    /// Cap on the response body the host buffers back (bytes). Host default/ceiling apply.
    pub max_response_bytes: Option<u64>,
    /// Cap on concurrent in-flight outbound calls for this filter. Host default/ceiling apply.
    pub max_concurrent: Option<u32>,
}

/// One filter to load, pinned by OCI digest.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilterEntry {
    /// Host-assigned identity; namespaces the filter's KV (ADR 000011) and names it in chains.
    ///
    /// KV continuity is **per-id and survives reload** (f000004 #4): reloading the same `id`
    /// with a new `digest` keeps the same KV namespace, so the new version inherits the old
    /// version's bytes (good for rate-limit counters — a reload doesn't reset them). If a new
    /// version changes its state encoding, handle the migration or use a new `id` (= a fresh
    /// namespace).
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
    /// Host-side token bucket for this filter's `host-ratelimit` (ADR 000026). Absent = the filter
    /// has no limiter (its `try-acquire` fails closed). Operator-configured so an untrusted filter
    /// cannot override its own limit.
    pub ratelimit: Option<RateLimitConfig>,
    /// This filter's outbound HTTP policy (ADR 000036), `[filter.outbound_http]`. Absent = no
    /// outbound HTTP capability. Requires the `outbound-http` build; otherwise a present section is
    /// rejected at validate (fail-closed).
    #[serde(default)]
    pub outbound_http: Option<OutboundHttpConfig>,
}

/// Manifest spelling of the host's `Isolation`. Defaults to `untrusted` (fail-closed).
#[derive(
    Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "lowercase")]
pub enum IsolationKind {
    #[default]
    Untrusted,
    Trusted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

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

    const OUTBOUND_TOML: &str = r#"
[[filter]]
id = "extauthz"
source = "oci/extauthz"
digest = "sha256:abc"

[filter.outbound_http]
allow = [
  { host = "authz.example.com", port = 8443, scheme = "https" },
  { host = "jwks.example.com" },
]
allow_private = ["10.1.0.0/16"]
connect_timeout_ms = 1500
"#;

    #[test]
    fn outbound_section_parses() {
        let m = Manifest::from_toml(OUTBOUND_TOML).unwrap();
        let ob = m.filters[0]
            .outbound_http
            .as_ref()
            .expect("outbound_http present");
        assert_eq!(ob.allow.len(), 2);
        assert_eq!(ob.allow[0].host, "authz.example.com");
        assert_eq!(ob.allow[0].port, Some(8443));
        assert_eq!(ob.allow[0].scheme, SchemeKind::Https);
        assert_eq!(ob.allow[1].port, None); // defaulted at lowering time
        assert_eq!(ob.allow_private, vec!["10.1.0.0/16".to_string()]);
        assert_eq!(ob.connect_timeout_ms, Some(1500));
    }

    #[cfg(feature = "outbound-http")]
    #[test]
    fn outbound_validates_and_lowers_to_policy() {
        let m = Manifest::from_toml(OUTBOUND_TOML).unwrap();
        let entry = &m.filters[0];
        entry.validate().expect("valid outbound section");

        let opts = entry.load_options();
        let policy = opts
            .outbound_http
            .expect("outbound_http lowered into LoadOptions");
        assert_eq!(policy.allow.len(), 2);
        // exact allowlist matching + scheme default port
        assert!(policy.allows(plecto_host::Scheme::Https, "authz.example.com", 8443));
        assert!(policy.allows(plecto_host::Scheme::Https, "jwks.example.com", 443));
        assert!(!policy.allows(plecto_host::Scheme::Http, "authz.example.com", 8443));
        // allow_private opt-in reaches the SSRF classifier
        assert_eq!(
            policy.classify("10.1.2.3".parse().unwrap()),
            plecto_host::AddrVerdict::Allowed
        );
        assert_eq!(
            policy.classify("169.254.169.254".parse().unwrap()),
            plecto_host::AddrVerdict::BlockedReserved
        );
        // small value passes through; the connect timeout was 1500ms
        assert_eq!(
            policy.connect_timeout,
            std::time::Duration::from_millis(1500)
        );
    }

    #[cfg(feature = "outbound-http")]
    #[test]
    fn outbound_clamps_oversized_values() {
        let toml = r#"
[[filter]]
id = "x"
source = "s"
digest = "sha256:abc"
[filter.outbound_http]
allow = [{ host = "a.example.com" }]
total_timeout_ms = 999999999
max_concurrent = 100000
"#;
        let m = Manifest::from_toml(toml).unwrap();
        let policy = m.filters[0].load_options().outbound_http.unwrap();
        assert!(policy.total_timeout <= std::time::Duration::from_secs(30));
        assert!(policy.max_concurrent <= 64);
    }

    #[cfg(feature = "outbound-http")]
    #[test]
    fn outbound_rejects_bad_config() {
        let cases = [
            // empty allowlist
            ("allow = []", "empty allowlist"),
            // unparseable CIDR
            (
                "allow = [{ host = \"a\" }]\nallow_private = [\"not-a-cidr\"]",
                "bad CIDR",
            ),
            // zero timeout
            (
                "allow = [{ host = \"a\" }]\nconnect_timeout_ms = 0",
                "zero connect timeout",
            ),
        ];
        for (body, why) in cases {
            let toml = format!(
                "[[filter]]\nid = \"x\"\nsource = \"s\"\ndigest = \"sha256:abc\"\n[filter.outbound_http]\n{body}\n"
            );
            let m = Manifest::from_toml(&toml).unwrap();
            assert!(m.filters[0].validate().is_err(), "{why} must be rejected");
        }
    }

    #[cfg(not(feature = "outbound-http"))]
    #[test]
    fn outbound_rejected_without_feature() {
        // A manifest that asks for outbound must fail closed on a build that cannot provide it.
        let m = Manifest::from_toml(OUTBOUND_TOML).unwrap();
        assert!(
            m.filters[0].validate().is_err(),
            "outbound_http requires the outbound-http build"
        );
    }
}
