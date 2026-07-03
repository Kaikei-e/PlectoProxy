//! Periodic DNS re-resolution of upstream hostnames — the standard periodic-DNS
//! endpoint-discovery technique (the shape of nginx `resolver`+`resolve` / Envoy STRICT_DNS):
//! each address a hostname resolves to becomes a load-balancing endpoint with its own health,
//! refreshed on the upstream's `resolve_interval_ms`, so a container re-creation's new IP is
//! picked up without a restart. One supervisor task drives all resolving groups, mirroring the
//! health supervisor's shape: groups are re-read from Control each loop (a reload's reconciled
//! groups are picked up automatically), and each due group refreshes on its own task.
//!
//! Failure semantics (fail-closed without self-DoS): a FAILED resolution keeps that hostname's
//! last-known-good expansion (never empties the set on a flaky resolver); a hostname that has
//! never resolved keeps itself as the endpoint (hyper still resolves it per connect). Interval-
//! based rather than TTL-driven — getaddrinfo (which respects /etc/hosts and nsswitch, the
//! self-hosting-friendly choice) carries no TTL.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plecto_control::{Control, UpstreamGroup};

/// Run the DNS re-resolution supervisor until the server stops. No-ops (idles at a coarse tick)
/// while no group sets `resolve_interval_ms`.
pub(crate) async fn serve_dns_refresh(control: Arc<Control>) {
    // per-(group, address) last successful expansion — the last-known-good set a failed
    // re-resolution falls back to. Task-local, like the health supervisor's probe clock.
    let mut last_good: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut last_run: HashMap<String, Instant> = HashMap::new();
    loop {
        let groups = control.upstream_groups();
        let now = Instant::now();
        let mut live: HashSet<String> = HashSet::new();
        let mut period = Duration::from_secs(5);
        for g in &groups {
            let Some(interval) = g.resolve_interval() else {
                continue;
            };
            period = period.min(interval);
            live.insert(g.name.clone());
            let due = last_run
                .get(&g.name)
                .is_none_or(|t| now.duration_since(*t) >= interval);
            if due {
                last_run.insert(g.name.clone(), now);
                refresh_group(g, &mut last_good, lookup).await;
            }
        }
        last_run.retain(|k, _| live.contains(k));
        last_good.retain(|(g, _), _| live.contains(g));
        tokio::time::sleep(period.max(Duration::from_millis(20))).await;
    }
}

/// The production resolver: getaddrinfo via tokio's blocking pool. `addr` is `host:port`, so the
/// resolved `SocketAddr`s already carry the port.
async fn lookup(addr: String) -> std::io::Result<Vec<SocketAddr>> {
    Ok(tokio::net::lookup_host(addr.as_str()).await?.collect())
}

/// Re-resolve one group's hostname addresses and swap the endpoint set if it changed. An IP
/// literal passes through unchanged (it never round-trips through DNS, so its health state is
/// never perturbed). The resolved list is sorted per hostname so getaddrinfo's answer-order
/// nondeterminism never causes a spurious swap.
async fn refresh_group<F, Fut>(
    group: &Arc<UpstreamGroup>,
    last_good: &mut HashMap<(String, String), Vec<String>>,
    resolver: F,
) -> bool
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = std::io::Result<Vec<SocketAddr>>>,
{
    let mut resolved: Vec<(String, u32)> = Vec::new();
    for (addr, weight) in group.configured_addresses() {
        if addr.parse::<SocketAddr>().is_ok() {
            resolved.push((addr.clone(), *weight));
            continue;
        }
        let key = (group.name.clone(), addr.clone());
        match resolver(addr.clone()).await {
            Ok(addrs) if !addrs.is_empty() => {
                let mut expansion: Vec<String> =
                    addrs.into_iter().map(|sa| sa.to_string()).collect();
                expansion.sort();
                expansion.dedup();
                resolved.extend(expansion.iter().map(|a| (a.clone(), *weight)));
                last_good.insert(key, expansion);
            }
            _ => match last_good.get(&key) {
                // failed (or empty) resolution: keep the last-known-good expansion rather than
                // emptying the set — a flaky resolver must not take a serving upstream down.
                Some(last) => resolved.extend(last.iter().map(|a| (a.clone(), *weight))),
                // never resolved: keep the hostname itself as the endpoint (per-connect
                // resolution still applies), so startup with a temporarily-dead DNS still serves.
                None => resolved.push((addr.clone(), *weight)),
            },
        }
    }
    group.update_endpoints(&resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plecto_control::{Manifest, UpstreamRegistry};

    fn resolving_group(addresses: &str) -> Arc<UpstreamGroup> {
        let toml = format!(
            r#"
            [[upstream]]
            name = "app"
            addresses = [{addresses}]
            resolve_interval_ms = 100
            [upstream.health]
            path = "/healthz"
            healthy_threshold = 1
            "#
        );
        let manifest = Manifest::from_toml(&toml).unwrap();
        let registry = UpstreamRegistry::new();
        registry
            .reconcile(&manifest.upstreams, std::path::Path::new("."))
            .unwrap();
        registry.group("app").unwrap()
    }

    fn addrs(list: &[&str]) -> std::io::Result<Vec<SocketAddr>> {
        Ok(list.iter().map(|a| a.parse().unwrap()).collect())
    }

    #[tokio::test]
    async fn expansion_swap_preserves_surviving_endpoint_health() {
        // The STRICT_DNS-shaped contract: records become endpoints; a surviving record keeps its
        // instance (health preserved), a vanished one is dropped, a new one starts pessimistic.
        let group = resolving_group("\"app.internal:80\"");
        let mut cache = HashMap::new();

        let swapped = refresh_group(&group, &mut cache, |_| async {
            addrs(&["10.0.0.1:80", "10.0.0.2:80"])
        })
        .await;
        assert!(swapped, "first resolution swaps hostname → IP endpoints");
        let ep = group.endpoints();
        assert_eq!(ep.instances.len(), 2);
        ep.instances[0].record_probe_success(); // 10.0.0.1 becomes healthy

        let swapped = refresh_group(&group, &mut cache, |_| async {
            addrs(&["10.0.0.1:80", "10.0.0.3:80"])
        })
        .await;
        assert!(swapped, "a changed record set swaps again");
        let ep = group.endpoints();
        assert_eq!(ep.instances.len(), 2);
        assert!(
            ep.instances[0].is_healthy(),
            "the surviving 10.0.0.1 keeps its health across the swap"
        );
        assert_eq!(ep.instances[1].address(), "10.0.0.3:80");
        assert!(
            !ep.instances[1].is_healthy(),
            "the new record starts pessimistic (ADR 000017)"
        );
    }

    #[tokio::test]
    async fn unchanged_resolution_does_not_swap() {
        let group = resolving_group("\"app.internal:80\"");
        let mut cache = HashMap::new();
        refresh_group(&group, &mut cache, |_| async { addrs(&["10.0.0.1:80"]) }).await;
        let before = group.endpoints();
        let swapped =
            refresh_group(&group, &mut cache, |_| async { addrs(&["10.0.0.1:80"]) }).await;
        assert!(!swapped, "an identical answer must not swap the set");
        assert!(
            Arc::ptr_eq(&before, &group.endpoints()),
            "the endpoint set Arc is untouched on an unchanged answer"
        );
    }

    #[tokio::test]
    async fn failed_resolution_keeps_the_last_known_good_set() {
        let group = resolving_group("\"app.internal:80\"");
        let mut cache = HashMap::new();
        refresh_group(&group, &mut cache, |_| async { addrs(&["10.0.0.1:80"]) }).await;
        group.endpoints().instances[0].record_probe_success();

        let swapped = refresh_group(&group, &mut cache, |_| async {
            Err(std::io::Error::other("dns down"))
        })
        .await;
        assert!(!swapped, "a failed resolution must not change the set");
        let ep = group.endpoints();
        assert_eq!(ep.instances[0].address(), "10.0.0.1:80");
        assert!(ep.instances[0].is_healthy(), "and health is untouched");
    }

    #[tokio::test]
    async fn never_resolved_hostname_stays_as_its_own_endpoint() {
        // DNS down from the very start: the hostname endpoint stays (hyper resolves per
        // connect), so a late-booting resolver does not black-hole the upstream.
        let group = resolving_group("\"app.internal:80\"");
        let mut cache = HashMap::new();
        let swapped = refresh_group(&group, &mut cache, |_| async {
            Err(std::io::Error::other("dns down"))
        })
        .await;
        assert!(!swapped);
        assert_eq!(group.endpoints().instances[0].address(), "app.internal:80");
    }

    #[tokio::test]
    async fn ip_literals_never_round_trip_through_the_resolver() {
        let group = resolving_group("\"10.9.9.9:80\", \"app.internal:80\"");
        let mut cache = HashMap::new();
        refresh_group(&group, &mut cache, |addr| async move {
            assert_eq!(
                addr, "app.internal:80",
                "only the hostname reaches the resolver"
            );
            addrs(&["10.0.0.1:80"])
        })
        .await;
        let ep = group.endpoints();
        assert_eq!(
            ep.instances
                .iter()
                .map(|i| i.address().to_string())
                .collect::<Vec<_>>(),
            vec!["10.9.9.9:80".to_string(), "10.0.0.1:80".to_string()],
            "the IP literal passes through; the hostname expands"
        );
    }
}
