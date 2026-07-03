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

/// One group's last successful expansion per hostname address — the last-known-good set a failed
/// re-resolution falls back to. Behind a `tokio::sync::Mutex` so a group's refresh task owns it
/// for the duration of the refresh (the lock IS the "one in-flight refresh per group" guard).
type GroupCache = Arc<tokio::sync::Mutex<HashMap<String, Vec<String>>>>;

/// The supervisor's task-local bookkeeping, keyed by group name (like the health supervisor's
/// probe clock): when each group last started a refresh, and its last-known-good cache.
#[derive(Default)]
struct RefreshState {
    last_run: HashMap<String, Instant>,
    last_good: HashMap<String, GroupCache>,
}

/// Run the DNS re-resolution supervisor until the server stops. No-ops (idles at a coarse tick)
/// while no group sets `resolve_interval_ms`.
pub(crate) async fn serve_dns_refresh(control: Arc<Control>) {
    let mut state = RefreshState::default();
    loop {
        let groups = control.upstream_groups();
        let period = refresh_tick(&groups, &mut state, &lookup).await;
        tokio::time::sleep(period.max(Duration::from_millis(20))).await;
    }
}

/// One supervisor tick: refresh every due group and return how long to sleep until the next
/// tick (the shortest configured interval, or a coarse idle period when no group resolves).
async fn refresh_tick<F, Fut>(
    groups: &[Arc<UpstreamGroup>],
    state: &mut RefreshState,
    resolver: &F,
) -> Duration
where
    F: Fn(String) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = std::io::Result<Vec<SocketAddr>>> + Send,
{
    let now = Instant::now();
    let mut live: HashSet<String> = HashSet::new();
    let mut period = Duration::from_secs(5);
    for g in groups {
        let Some(interval) = g.resolve_interval() else {
            continue;
        };
        period = period.min(interval);
        live.insert(g.name.clone());
        let due = state
            .last_run
            .get(&g.name)
            .is_none_or(|t| now.duration_since(*t) >= interval);
        if due {
            state.last_run.insert(g.name.clone(), now);
            let cache = state.last_good.entry(g.name.clone()).or_default().clone();
            let mut cache = cache.lock().await;
            refresh_group(g, &mut cache, resolver.clone()).await;
        }
    }
    state.last_run.retain(|k, _| live.contains(k));
    state.last_good.retain(|k, _| live.contains(k));
    period
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
    last_good: &mut HashMap<String, Vec<String>>,
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
        match resolver(addr.clone()).await {
            Ok(addrs) if !addrs.is_empty() => {
                let mut expansion: Vec<String> =
                    addrs.into_iter().map(|sa| sa.to_string()).collect();
                expansion.sort();
                expansion.dedup();
                resolved.extend(expansion.iter().map(|a| (a.clone(), *weight)));
                last_good.insert(addr.clone(), expansion);
            }
            _ => match last_good.get(addr) {
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

    fn resolving_group_named(name: &str, addresses: &str) -> Arc<UpstreamGroup> {
        let toml = format!(
            r#"
            [[upstream]]
            name = "{name}"
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
        registry.group(name).unwrap()
    }

    fn resolving_group(addresses: &str) -> Arc<UpstreamGroup> {
        resolving_group_named("app", addresses)
    }

    fn addrs(list: &[&str]) -> std::io::Result<Vec<SocketAddr>> {
        Ok(list.iter().map(|a| a.parse().unwrap()).collect())
    }

    #[tokio::test]
    async fn one_blocked_resolver_does_not_stall_another_groups_refresh() {
        // ADR 000044: each due group refreshes on its own task, so one black-holed resolver
        // (getaddrinfo hanging at resolv.conf timeout × attempts) must not serialize every other
        // group's refresh behind it. The barrier only releases when BOTH groups' resolutions are
        // in flight at once — a serial supervisor deadlocks here and trips the timeout.
        let ga = resolving_group_named("a", "\"a.internal:80\"");
        let gb = resolving_group_named("b", "\"b.internal:80\"");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let resolver = {
            let barrier = barrier.clone();
            move |_addr: String| {
                let barrier = barrier.clone();
                async move {
                    barrier.wait().await;
                    addrs(&["10.0.0.9:80"])
                }
            }
        };
        let groups = vec![ga.clone(), gb.clone()];
        let mut state = RefreshState::default();

        tokio::time::timeout(Duration::from_secs(5), async {
            refresh_tick(&groups, &mut state, &resolver).await;
            loop {
                let done =
                    |g: &Arc<UpstreamGroup>| g.endpoints().instances[0].address() == "10.0.0.9:80";
                if done(&ga) && done(&gb) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("both groups must refresh concurrently, not serially");
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
