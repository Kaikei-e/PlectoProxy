//! The data-plane listener config (`[listen]`).

use std::net::IpAddr;
use std::sync::Arc;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::error::ControlError;

/// The data-plane listener config (`[listen]`). The manifest is the single static source of
/// config, so the bind address lives here rather than only in a positional CLI arg (containers
/// need `0.0.0.0` binds without entrypoint gymnastics); an explicit CLI `listen_addr` still wins
/// as the operator's override.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Listen {
    /// `host:port` the data plane binds (e.g. `0.0.0.0:8443`). `None` = the binary's default
    /// (`127.0.0.1:8080`) unless the CLI arg overrides.
    #[serde(default)]
    pub addr: Option<String>,
    /// The port `Alt-Svc` advertises for HTTP/3 (RFC 7838), when the PUBLISHED port differs from
    /// the bound one (container port mapping: internal 8443 → published 443 would otherwise
    /// advertise a dead h3 port). `None` = advertise the bound port (the default).
    #[serde(default)]
    pub advertised_port: Option<u16>,
    /// `[listen.proxy_protocol]` (ADR 000057): opt-in PROXY protocol v2 reception on the TCP
    /// listener, restoring the real client address behind an L4 load balancer. Absent = off.
    #[serde(default)]
    pub proxy_protocol: Option<ProxyProtocol>,
}

/// `[listen.proxy_protocol]` (ADR 000057): PROXY v2 reception is enabled by the section's
/// presence, and `trusted` is required — "no trust without declaration" (deny-by-default, P4);
/// enabling without naming the load balancers would accept a spoofed source from any peer.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyProtocol {
    /// CIDR blocks of the L4 load balancers allowed to prepend a PROXY v2 header. CIDR notation
    /// only (a single host is `"192.0.2.1/32"`, not a bare IP) — the trust declaration stays
    /// explicit, like the outbound `allow_private` ranges. Must list at least one.
    pub trusted: Vec<String>,
}

/// The parsed, runtime form of `[listen.proxy_protocol]`: the trusted networks behind a
/// containment check. The fast path asks `contains(peer.ip())` and never re-parses CIDRs;
/// keeping the match here keeps the canonicalisation rule in one place.
#[derive(Debug, Clone)]
pub struct ProxyProtocolTrust {
    nets: Arc<[IpNet]>,
}

impl ProxyProtocolTrust {
    /// Whether `ip` belongs to a trusted network. An IPv4-mapped IPv6 peer (`::ffff:a.b.c.d`,
    /// how a dual-stack accept reports an IPv4 client) is collapsed to its IPv4 form first, so
    /// a v4 CIDR matches it.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let canonical = ip.to_canonical();
        self.nets.iter().any(|net| net.contains(&canonical))
    }
}

impl Listen {
    /// Validate the section fail-closed at build (ADR 000057), before any listener consults it.
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        self.proxy_protocol_trust().map(|_| ())
    }

    /// Parse `[listen.proxy_protocol]` into its runtime form — `None` when the section is
    /// absent (PROXY v2 off), an error when it is present but empty or unparseable.
    pub(crate) fn proxy_protocol_trust(&self) -> Result<Option<ProxyProtocolTrust>, ControlError> {
        let Some(pp) = &self.proxy_protocol else {
            return Ok(None);
        };
        if pp.trusted.is_empty() {
            return Err(ControlError::InvalidListenConfig(
                "proxy_protocol.trusted must list at least one CIDR — enabling PROXY v2 without \
                 declaring the load balancers would trust every peer"
                    .to_string(),
            ));
        }
        let nets = pp
            .trusted
            .iter()
            .map(|s| {
                s.parse::<IpNet>().map_err(|e| {
                    ControlError::InvalidListenConfig(format!(
                        "proxy_protocol.trusted has invalid CIDR {s:?}: {e} (a single host \
                         needs an explicit prefix, e.g. \"{s}/32\")"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(ProxyProtocolTrust { nets: nets.into() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listen(trusted: &[&str]) -> Listen {
        Listen {
            addr: None,
            advertised_port: None,
            proxy_protocol: Some(ProxyProtocol {
                trusted: trusted.iter().map(|s| (*s).to_string()).collect(),
            }),
        }
    }

    #[test]
    fn absent_section_means_off() {
        let parsed = Listen::default().proxy_protocol_trust().unwrap();
        assert!(parsed.is_none(), "no section → PROXY v2 off");
    }

    #[test]
    fn empty_trusted_is_rejected() {
        let err = listen(&[]).validate().unwrap_err();
        assert!(
            matches!(err, ControlError::InvalidListenConfig(_)),
            "empty trusted must fail closed, got: {err}"
        );
    }

    #[test]
    fn bare_ip_is_rejected_with_a_prefix_hint() {
        let err = listen(&["192.0.2.1"]).validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("/32"),
            "a bare IP must be rejected with an explicit-prefix hint, got: {msg}"
        );
    }

    #[test]
    fn unparseable_cidr_is_rejected() {
        let err = listen(&["not-a-cidr"]).validate().unwrap_err();
        assert!(
            matches!(err, ControlError::InvalidListenConfig(_)),
            "got: {err}"
        );
    }

    #[test]
    fn contains_matches_v4_v6_and_mapped_peers() {
        let trust = listen(&["10.0.0.0/8", "2001:db8::/32"])
            .proxy_protocol_trust()
            .unwrap()
            .expect("section present");
        assert!(trust.contains("10.1.2.3".parse().unwrap()));
        assert!(trust.contains("2001:db8::9".parse().unwrap()));
        // a dual-stack accept reports an IPv4 LB as ::ffff:10.1.2.3 — the v4 CIDR must match it
        assert!(trust.contains("::ffff:10.1.2.3".parse().unwrap()));
        assert!(!trust.contains("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn manifest_toml_round_trip_and_validate_manifest_reject() {
        let manifest =
            crate::Manifest::from_toml("[listen.proxy_protocol]\ntrusted = [\"10.0.0.0/8\"]\n")
                .unwrap();
        let pp = manifest
            .listen
            .proxy_protocol
            .as_ref()
            .expect("section parsed");
        assert_eq!(pp.trusted, vec!["10.0.0.0/8".to_string()]);

        let bad = crate::Manifest::from_toml("[listen.proxy_protocol]\ntrusted = []\n").unwrap();
        let err = crate::validate_manifest(&bad, std::path::Path::new(".")).unwrap_err();
        assert!(
            matches!(err, ControlError::InvalidListenConfig(_)),
            "plecto validate must reject an empty trusted list, got: {err}"
        );
    }
}
