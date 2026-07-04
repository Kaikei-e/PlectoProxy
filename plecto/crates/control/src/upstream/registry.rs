//! The live registry of upstream groups (ADR 000017), owned by `Control` OUTSIDE the swapped
//! `ActiveConfig` so health state survives a reload.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::ControlError;
use crate::manifest::{HashKeyKind, Upstream};

use super::UpstreamGroup;
use super::instance::UpstreamInstance;
use super::lb::HashKeySource;

/// The live set of upstreams, keyed by name. Owned by `Control`, OUTSIDE the swapped
/// `ActiveConfig`, so health state survives a reload (ADR 000017). The `Mutex` is contended only by
/// `reconcile` (on reload) and the prober supervisor (`groups`) / a config build (`group`) — never
/// the per-request hot path, which holds an `Arc<UpstreamGroup>` resolved at build time.
#[derive(Debug, Default)]
pub struct UpstreamRegistry {
    groups: Mutex<HashMap<String, Arc<UpstreamGroup>>>,
}

impl UpstreamRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconcile the registry to `upstreams` (ADR 000017). Validation (duplicate name, empty
    /// addresses, the LB config — ADR 000035 — and the `[upstream.tls]` CA load, ADR 000042) runs
    /// FIRST against the whole list, so a bad manifest leaves the running set untouched
    /// (all-or-nothing, like the rest of a reload). Then, per upstream: build a new group whose
    /// instances reuse the existing `Arc<UpstreamInstance>` for any unchanged `(name, address,
    /// weight)` *when the health policy and `[upstream.tls]` are unchanged* (a TLS change makes
    /// prior probe results meaningless, so it re-probes from pessimistic like a health-policy
    /// change), create a fresh pessimistic instance otherwise, build the LB state (a Maglev
    /// upstream recomputes its table from the instance set), and drop upstreams no longer present.
    /// `base_dir` resolves each `[upstream.tls] ca_path`, like the `[[tls]]` cert paths.
    pub fn reconcile(&self, upstreams: &[Upstream], base_dir: &Path) -> Result<(), ControlError> {
        let mut seen = HashSet::new();
        for up in upstreams {
            if up.addresses.is_empty() {
                return Err(ControlError::EmptyUpstreamAddresses(up.name.clone()));
            }
            if !seen.insert(up.name.as_str()) {
                return Err(ControlError::DuplicateUpstream(up.name.clone()));
            }
            up.validate_lb()
                .map_err(|reason| ControlError::InvalidUpstreamLb {
                    name: up.name.clone(),
                    reason,
                })?;
            up.warn_missing_sni();
        }
        // Build (or reuse) each upstream's TLS client config (ADR 000042) BEFORE any mutation —
        // a bad CA file aborts the whole reconcile fail-closed. Reusing the prior group's `Arc`
        // when `[upstream.tls]` is unchanged keeps its identity stable across reloads, so the
        // fast path's per-config connection pool (keyed on that identity) survives. The `sni`
        // override (ADR 000050) is cheap to parse (no I/O), so it is rebuilt fresh every
        // reconcile rather than reuse-cached like the CA-derived `ClientConfig`.
        let mut tls_clients: Vec<Option<Arc<rustls::ClientConfig>>> =
            Vec::with_capacity(upstreams.len());
        let mut tls_snis: Vec<Option<rustls::pki_types::ServerName<'static>>> =
            Vec::with_capacity(upstreams.len());
        for up in upstreams {
            let client = match &up.tls {
                None => None,
                Some(tls) => {
                    let prev = self.group(&up.name);
                    let reusable = prev
                        .as_ref()
                        .filter(|g| g.tls_manifest.as_ref() == Some(tls))
                        .and_then(|g| g.tls_client.clone());
                    match reusable {
                        Some(cfg) => Some(cfg),
                        None => Some(crate::tls::build_upstream_client_config(
                            &up.name, tls, base_dir,
                        )?),
                    }
                }
            };
            tls_clients.push(client);
            let sni = match up.tls.as_ref().and_then(|tls| tls.sni.as_deref()) {
                None => None,
                Some(sni) => Some(crate::tls::parse_upstream_sni(&up.name, sni)?),
            };
            tls_snis.push(sni);
        }

        let mut groups = self
            .groups
            .lock()
            .map_err(|_| ControlError::UpstreamRegistryPoisoned)?;
        let mut next: HashMap<String, Arc<UpstreamGroup>> = HashMap::with_capacity(upstreams.len());
        for ((up, tls_client), tls_sni) in upstreams.iter().zip(tls_clients).zip(tls_snis) {
            let prev_any = groups.get(&up.name);
            // reuse the prior group's instances only if the health policy AND the TLS config are
            // identical; a change re-probes the upstream from pessimistic (new thresholds apply /
            // old probe results were for a different security context).
            let prev = prev_any.filter(|g| g.health == up.health && g.tls_manifest == up.tls);
            let configured: Vec<(String, u32)> = up
                .addresses
                .iter()
                .map(|spec| (spec.address().to_string(), spec.weight()))
                .collect();
            let resolve_interval = Duration::from_millis(up.resolve_interval_ms);
            let maglev_table_size =
                up.hash.as_ref().map(|h| h.table_size).unwrap_or(65537) as usize;
            let prev_endpoints = prev.map(|g| g.endpoints.load_full());
            // A resolving group whose declared addresses / LB config are unchanged carries its
            // CURRENT endpoint set (the resolved IPs + their health) across the reload wholesale —
            // otherwise every reload would discard the resolved set and re-enter the pessimistic
            // window until the next refresh + probe pass.
            let carry_resolved = prev
                .filter(|g| {
                    !resolve_interval.is_zero()
                        && !g.resolve_interval.is_zero()
                        && g.configured == configured
                        && g.lb_algorithm == up.lb_algorithm
                        && g.maglev_table_size == maglev_table_size
                })
                .and_then(|_| prev_endpoints.clone());
            let endpoints = match carry_resolved {
                Some(current) => current,
                None => {
                    let instances: Vec<Arc<UpstreamInstance>> = configured
                        .iter()
                        .map(|(addr, weight)| {
                            // reuse only when address AND weight are unchanged; a weight edit (LB
                            // capacity) builds a fresh instance, like a health-policy change.
                            prev_endpoints
                                .as_ref()
                                .and_then(|ep| {
                                    ep.instances
                                        .iter()
                                        .find(|i| i.address() == addr && i.weight() == *weight)
                                        .cloned()
                                })
                                .unwrap_or_else(|| {
                                    Arc::new(UpstreamInstance::new(
                                        addr.clone(),
                                        *weight,
                                        &up.health,
                                    ))
                                })
                        })
                        .collect();
                    // Build the LB state from the manifest (ADR 000035). Maglev recomputes its
                    // lookup table from the instance set + weights; validation above guaranteed a
                    // hash block and a valid (prime, in-range) table size.
                    Arc::new(super::Endpoints::build(
                        instances,
                        up.lb_algorithm,
                        maglev_table_size,
                    ))
                }
            };
            // carry the round-robin cursor across the reload (independent of which instances or the
            // health policy changed — it is only a rotation counter) so the first post-reload pick
            // continues the rotation instead of restarting at the eligible set's head (ADR 000024).
            let rr = prev_any.map(|g| g.rr.load(Ordering::Relaxed)).unwrap_or(0);
            let hash_key = up.hash.as_ref().map(|h| match h.key {
                HashKeyKind::Header => {
                    HashKeySource::Header(h.header.clone().unwrap_or_default().to_ascii_lowercase())
                }
                HashKeyKind::SourceIp => HashKeySource::SourceIp,
            });
            next.insert(
                up.name.clone(),
                Arc::new(UpstreamGroup {
                    name: up.name.clone(),
                    health: up.health.clone(),
                    endpoints: arc_swap::ArcSwap::new(endpoints),
                    configured,
                    resolve_interval,
                    lb_algorithm: up.lb_algorithm,
                    maglev_table_size,
                    request_timeout: Duration::from_millis(up.request_timeout_ms),
                    overall_timeout: Duration::from_millis(up.overall_timeout_ms),
                    max_retries: up.max_retries,
                    rr: AtomicUsize::new(rr),
                    max_requests: up.circuit_breaker.max_requests as usize,
                    in_flight: AtomicUsize::new(0),
                    outlier_consecutive: up.outlier_detection.consecutive_gateway_failures,
                    outlier_base_ejection: Duration::from_millis(
                        up.outlier_detection.base_ejection_time_ms,
                    ),
                    outlier_max_ejection_percent: up.outlier_detection.max_ejection_percent,
                    hash_key,
                    tls_manifest: up.tls.clone(),
                    tls_client,
                    tls_sni,
                }),
            );
        }
        *groups = next;
        Ok(())
    }

    /// The group named `name`, if present — used to resolve a route's upstream at config-build time.
    pub fn group(&self, name: &str) -> Option<Arc<UpstreamGroup>> {
        self.groups.lock().ok()?.get(name).cloned()
    }

    /// A snapshot of every current group, for the health-check supervisor to probe.
    pub fn groups(&self) -> Vec<Arc<UpstreamGroup>> {
        self.groups
            .lock()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        AddressSpec, CircuitBreaker, HealthConfig, LbAlgorithm, OutlierDetection,
    };

    fn health(healthy_threshold: u32, unhealthy_threshold: u32) -> HealthConfig {
        HealthConfig {
            path: "/healthz".to_string(),
            interval_ms: 100,
            timeout_ms: 50,
            healthy_threshold,
            unhealthy_threshold,
            port: None,
        }
    }

    fn upstream(name: &str, addrs: &[&str], h: HealthConfig) -> Upstream {
        Upstream {
            name: name.to_string(),
            addresses: addrs
                .iter()
                .map(|s| AddressSpec::Bare(s.to_string()))
                .collect(),
            lb_algorithm: LbAlgorithm::RoundRobin,
            hash: None,
            tls: None,
            resolve_interval_ms: 0,
            health: h,
            request_timeout_ms: 30_000,
            max_retries: 1,
            overall_timeout_ms: 0,
            circuit_breaker: CircuitBreaker::default(),
            outlier_detection: OutlierDetection::default(),
        }
    }

    #[test]
    fn reconcile_preserves_unchanged_adds_new_drops_removed() {
        let reg = UpstreamRegistry::new();
        reg.reconcile(
            &[upstream("u", &["a:1", "b:2"], health(1, 3))],
            std::path::Path::new("."),
        )
        .unwrap();
        let g0 = reg.group("u").unwrap();
        g0.endpoints().instances[0].record_probe_success(); // a:1 becomes healthy
        assert!(g0.endpoints().instances[0].is_healthy());

        // reload: drop b:2, keep a:1, add c:3 — same health policy
        reg.reconcile(
            &[upstream("u", &["a:1", "c:3"], health(1, 3))],
            std::path::Path::new("."),
        )
        .unwrap();
        let g1 = reg.group("u").unwrap();
        assert_eq!(g1.endpoints().instances.len(), 2);
        assert!(
            g1.endpoints().instances[0].is_healthy(),
            "the unchanged a:1 keeps its health across reload"
        );
        assert_eq!(g1.endpoints().instances[1].address(), "c:3");
        assert!(
            !g1.endpoints().instances[1].is_healthy(),
            "the new c:3 starts pessimistic"
        );
    }

    #[test]
    fn reconcile_changing_health_policy_reprobes_from_pessimistic() {
        let reg = UpstreamRegistry::new();
        reg.reconcile(
            &[upstream("u", &["a:1"], health(1, 3))],
            std::path::Path::new("."),
        )
        .unwrap();
        reg.group("u").unwrap().endpoints().instances[0].record_probe_success();
        assert!(reg.group("u").unwrap().endpoints().instances[0].is_healthy());

        // same address, different health policy → fresh pessimistic instance, new thresholds apply
        reg.reconcile(
            &[upstream("u", &["a:1"], health(2, 5))],
            std::path::Path::new("."),
        )
        .unwrap();
        assert!(
            !reg.group("u").unwrap().endpoints().instances[0].is_healthy(),
            "a health-policy change re-probes the instance from pessimistic"
        );
    }

    /// A CA PEM written to a temp dir (rcgen self-signed acts as the root), for `[upstream.tls]`.
    fn write_ca_pem(dir: &std::path::Path) -> String {
        let generated = rcgen::generate_simple_self_signed(vec!["ca.example".to_string()]).unwrap();
        let path = dir.join("ca.pem");
        std::fs::write(&path, generated.cert.pem()).unwrap();
        "ca.pem".to_string()
    }

    #[test]
    fn reconcile_keeps_the_tls_client_arc_across_an_unchanged_reload() {
        // ADR 000042: while `[upstream.tls]` is unchanged, the ClientConfig Arc must be REUSED —
        // the fast path keys its per-config connection pool on the Arc's identity, so a stable
        // Arc means a reload never cold-starts upstream connections.
        let dir = tempfile::tempdir().unwrap();
        let ca_path = write_ca_pem(dir.path());
        let mut up = upstream("u", &["a:1"], health(1, 3));
        up.tls = Some(crate::manifest::UpstreamTls {
            ca_path: Some(ca_path),
            ..Default::default()
        });

        let reg = UpstreamRegistry::new();
        reg.reconcile(std::slice::from_ref(&up), dir.path())
            .unwrap();
        let g0 = reg.group("u").unwrap();
        assert_eq!(
            g0.scheme(),
            "https",
            "an [upstream.tls] group forwards https"
        );
        let cfg0 = g0
            .tls_client_config()
            .cloned()
            .expect("a TLS client config");

        reg.reconcile(std::slice::from_ref(&up), dir.path())
            .unwrap();
        let g1 = reg.group("u").unwrap();
        let cfg1 = g1
            .tls_client_config()
            .cloned()
            .expect("a TLS client config");
        assert!(
            Arc::ptr_eq(&cfg0, &cfg1),
            "an unchanged [upstream.tls] must reuse the same ClientConfig Arc across reloads"
        );
    }

    #[test]
    fn reconcile_tls_change_reprobes_from_pessimistic() {
        // A TLS on/off (or CA) change makes prior probe results meaningless — the instance must
        // start pessimistic again, exactly like a health-policy change (ADR 000042 / 000017).
        let dir = tempfile::tempdir().unwrap();
        let ca_path = write_ca_pem(dir.path());
        let reg = UpstreamRegistry::new();
        reg.reconcile(&[upstream("u", &["a:1"], health(1, 3))], dir.path())
            .unwrap();
        reg.group("u").unwrap().endpoints().instances[0].record_probe_success();
        assert!(reg.group("u").unwrap().endpoints().instances[0].is_healthy());

        let mut up = upstream("u", &["a:1"], health(1, 3));
        up.tls = Some(crate::manifest::UpstreamTls {
            ca_path: Some(ca_path),
            ..Default::default()
        });
        reg.reconcile(&[up], dir.path()).unwrap();
        assert!(
            !reg.group("u").unwrap().endpoints().instances[0].is_healthy(),
            "enabling [upstream.tls] must re-probe the instance from pessimistic"
        );
    }

    #[test]
    fn reconcile_rejects_a_bad_ca_path_before_any_mutation() {
        // Fail-closed all-or-nothing (ADR 000042): an unreadable CA aborts the reconcile BEFORE
        // the registry mutates, so the running (healthy) set stays live on a bad reload.
        let dir = tempfile::tempdir().unwrap();
        let reg = UpstreamRegistry::new();
        reg.reconcile(&[upstream("u", &["a:1"], health(1, 3))], dir.path())
            .unwrap();
        reg.group("u").unwrap().endpoints().instances[0].record_probe_success();

        let mut up = upstream("u", &["a:1"], health(1, 3));
        up.tls = Some(crate::manifest::UpstreamTls {
            ca_path: Some("missing-ca.pem".to_string()),
            ..Default::default()
        });
        let err = reg.reconcile(&[up], dir.path());
        assert!(
            matches!(err, Err(ControlError::UpstreamTlsCa { .. })),
            "a missing CA file must be a typed fail-closed error"
        );
        let g = reg.group("u").unwrap();
        assert_eq!(g.scheme(), "http", "the running group is untouched");
        assert!(
            g.endpoints().instances[0].is_healthy(),
            "the running instance keeps its health after the rejected reconcile"
        );
    }

    #[test]
    fn reconcile_exposes_the_parsed_tls_sni_override() {
        // ADR 000050: a declared `sni` is parsed and reachable via `tls_sni()`, independent of
        // whether the upstream address is a hostname or an IP literal.
        let dir = tempfile::tempdir().unwrap();
        let ca_path = write_ca_pem(dir.path());
        let mut up = upstream("u", &["10.0.0.9:1"], health(1, 3));
        up.tls = Some(crate::manifest::UpstreamTls {
            ca_path: Some(ca_path),
            sni: Some("backend.internal".to_string()),
        });

        let reg = UpstreamRegistry::new();
        reg.reconcile(std::slice::from_ref(&up), dir.path())
            .unwrap();
        let g = reg.group("u").unwrap();
        assert_eq!(
            g.tls_sni().map(|n| n.to_str().to_string()),
            Some("backend.internal".to_string()),
            "the declared sni must be reachable for the connector to override with"
        );
    }

    #[test]
    fn reconcile_rejects_an_unparsable_tls_sni_before_any_mutation() {
        // Fail-closed all-or-nothing (ADR 000050), like the CA-path check: a `sni` that parses as
        // neither a DNS name nor an IP aborts the reconcile BEFORE the registry mutates, rather
        // than letting every handshake to this upstream fail at request time.
        let reg = UpstreamRegistry::new();
        reg.reconcile(
            &[upstream("u", &["a:1"], health(1, 3))],
            std::path::Path::new("."),
        )
        .unwrap();
        reg.group("u").unwrap().endpoints().instances[0].record_probe_success();

        let mut up = upstream("u", &["a:1"], health(1, 3));
        up.tls = Some(crate::manifest::UpstreamTls {
            ca_path: None,
            sni: Some("not a valid sni!!".to_string()),
        });
        let err = reg.reconcile(&[up], std::path::Path::new("."));
        assert!(
            matches!(err, Err(ControlError::UpstreamTlsSni { .. })),
            "an unparsable sni must be a typed fail-closed error, got {err:?}"
        );
        let g = reg.group("u").unwrap();
        assert_eq!(g.scheme(), "http", "the running group is untouched");
        assert!(
            g.endpoints().instances[0].is_healthy(),
            "the running instance keeps its health after the rejected reconcile"
        );
    }

    #[test]
    fn reconcile_rejects_empty_addresses_and_duplicate_names() {
        let reg = UpstreamRegistry::new();
        let empty = reg.reconcile(
            &[upstream("u", &[], health(1, 1))],
            std::path::Path::new("."),
        );
        assert!(matches!(
            empty,
            Err(ControlError::EmptyUpstreamAddresses(_))
        ));

        let dup = reg.reconcile(
            &[
                upstream("u", &["a:1"], health(1, 1)),
                upstream("u", &["b:2"], health(1, 1)),
            ],
            std::path::Path::new("."),
        );
        assert!(matches!(dup, Err(ControlError::DuplicateUpstream(_))));
    }
}
