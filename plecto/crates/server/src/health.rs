//! Active health checks (ADR 000017).
//!
//! One supervisor task drives ALL upstream instances. Each loop it reads the current upstream groups
//! from Control — so a reload's reconciled add/remove is picked up automatically, with no per-
//! instance task lifecycle to manage — and probes every instance whose `interval_ms` has elapsed. A
//! brand-new instance (not yet seen) is probed immediately: that cold-start fast probe, with the
//! first-success-promotes rule (ADR 000017), shrinks the pessimistic startup window to ~one probe
//! RTT. Probes follow the upstream's scheme (ADR 000042): plain HTTP/1.1, or TLS with the same
//! verification the forward leg uses — so an upstream whose certificate cannot be verified never
//! enters rotation (fail-closed). Each probe runs on its own task so a slow or timing-out probe
//! never stalls the others.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hyper::Request;
use plecto_control::{Control, HealthConfig, UpstreamInstance};

use crate::upstream_client::{HyperUpstreamClient, UpstreamClient, UpstreamClients};

/// Run the health-check supervisor until the server stops (ADR 000017). Drives `GET {health.path}`
/// to each upstream instance on its configured interval and feeds the result into the instance's
/// shared health state, which `proxy_core`'s round-robin then reads.
pub(crate) async fn serve_health_checks(control: Arc<Control>) {
    // Dedicated per-scheme probe clients (empty bodies), separate from the request path's pools —
    // a probe should validate the upstream, not ride (or disturb) a live traffic connection pool.
    let clients = UpstreamClients::new();
    // per-(upstream, address) last-probe instant, so each instance is probed on ITS interval even
    // though one task drives them all. An instance not yet in the map is probed now (cold start).
    let mut last: HashMap<(String, String), Instant> = HashMap::new();
    loop {
        let groups = control.upstream_groups();
        let now = Instant::now();
        let mut live: HashSet<(String, String)> = HashSet::new();
        // wake at the shortest configured interval; idle a few seconds when there are no upstreams.
        let mut period = Duration::from_secs(5);
        for g in &groups {
            let interval = Duration::from_millis(g.health.interval_ms.max(1));
            period = period.min(interval);
            for inst in &g.instances {
                let key = (g.name.clone(), inst.address().to_string());
                let due = last
                    .get(&key)
                    .is_none_or(|t| now.duration_since(*t) >= interval);
                if due {
                    last.insert(key.clone(), now);
                    // The probe client matches the group's security context (ADR 000042): the
                    // shared plain client, or the TLS client for its `[upstream.tls]` config.
                    let client = clients.for_group(g);
                    let scheme = g.scheme();
                    let inst = inst.clone();
                    let health = g.health.clone();
                    tokio::spawn(async move { probe_once(&client, scheme, &health, &inst).await });
                }
                live.insert(key);
            }
        }
        // forget bookkeeping for instances that vanished on a reload.
        last.retain(|k, _| live.contains(k));
        tokio::time::sleep(period.max(Duration::from_millis(20))).await;
    }
}

/// Probe one instance once: `GET {health.path}` over the group's scheme (ADR 000042), bounded by
/// `timeout_ms`. A 2xx is a success; a non-2xx, a timeout, or a connect/transport/TLS-verification
/// error is a failure. Never panics (data-plane discipline) — a malformed address/path is simply a
/// failed probe.
async fn probe_once(
    client: &HyperUpstreamClient,
    scheme: &str,
    health: &HealthConfig,
    inst: &UpstreamInstance,
) {
    let uri = format!(
        "{}://{}{}",
        scheme,
        probe_address(inst.address(), health.port),
        health.path
    );
    let req = match Request::builder()
        .method("GET")
        .uri(&uri)
        .body(crate::body::empty_req())
    {
        Ok(req) => req,
        Err(_) => {
            inst.record_probe_failure();
            return;
        }
    };
    let timeout = Duration::from_millis(health.timeout_ms.max(1));
    match tokio::time::timeout(timeout, client.request(req)).await {
        Ok(Ok(resp)) if resp.status().is_success() => inst.record_probe_success(),
        _ => inst.record_probe_failure(),
    }
}

/// `address` (`host:port`) with its port swapped for a dedicated health-check `port`, when the
/// upstream's health policy names one — a separate metrics/health listener distinct from the
/// traffic port. `None` (the common case) or an address with no `:port` suffix to replace (never
/// happens for a validated manifest, but this is the data-plane path — no panics) both fall back to
/// the instance's own address unchanged.
fn probe_address(address: &str, port: Option<u16>) -> Cow<'_, str> {
    match (port, address.rsplit_once(':')) {
        (Some(port), Some((host, _))) => Cow::Owned(format!("{host}:{port}")),
        _ => Cow::Borrowed(address),
    }
}

#[cfg(test)]
mod tests {
    use super::probe_address;

    #[test]
    fn no_override_keeps_the_traffic_address() {
        assert_eq!(probe_address("127.0.0.1:9000", None), "127.0.0.1:9000");
    }

    #[test]
    fn override_swaps_only_the_port() {
        assert_eq!(
            probe_address("127.0.0.1:9000", Some(9100)),
            "127.0.0.1:9100"
        );
        assert_eq!(
            probe_address("upstream.example:9000", Some(9100)),
            "upstream.example:9100"
        );
    }

    #[test]
    fn malformed_address_falls_back_unchanged_rather_than_panicking() {
        assert_eq!(
            probe_address("not-a-host-port", Some(9100)),
            "not-a-host-port"
        );
    }
}
