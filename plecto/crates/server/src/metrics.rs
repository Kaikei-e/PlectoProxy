//! Native data-plane metrics for the fast path (Stage A observability, ADR 000009). Plecto stays
//! dependency-free here: a handful of atomics tally the RED signals (Rate / Errors / Duration) plus
//! a fixed-bucket latency histogram, rendered to the Prometheus text exposition format (v0.0.4) by
//! hand and served on the admin endpoint (`crate::admin`). The host-aggregated filter-execution
//! metrics (`MetricsSink`, ADR 000009) are folded in at render time, so one scrape covers both the
//! data plane and the extension plane. Recording is lock-free and cheap enough to run on every
//! request unconditionally; rendering is a cold path (an admin scrape).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use plecto_control::{MetricsSnapshot, UpstreamGroup};

/// Upper bounds (seconds) of the request-latency histogram buckets. Prometheus convention: each
/// `_bucket{le=...}` is cumulative and a final `+Inf` bucket equals `_count`. The spread (1 ms…10 s)
/// covers a proxy's working range so p50/p90/p99 are recoverable via `histogram_quantile`.
const DURATION_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The `state` label values of `plecto_upstream_instances`, indexed by [`instance_state`].
const UPSTREAM_STATES: [&str; 3] = ["healthy", "unhealthy", "ejected"];

/// Which `UPSTREAM_STATES` slot one instance is counted in. Probe health (ADR 000017) and outlier
/// ejection (ADR 000032) are independent axes — a probe-healthy AND ejected instance is a real
/// state — so a mutually-exclusive label has to fold them: take the heaviest, `ejected` >
/// `unhealthy` > `healthy`. Each instance then lands in exactly one series and the per-upstream
/// sum equals its instance count; the cost is that the combination itself is not recoverable.
fn instance_state(healthy: bool, ejected: bool) -> usize {
    match (ejected, healthy) {
        (true, _) => 2,
        (false, true) => 0,
        (false, false) => 1,
    }
}

/// Escape a label value per the Prometheus text exposition format. Upstream names come from the
/// manifest and are the only dynamic label value in this exposition; an unescaped quote in one
/// would corrupt the whole scrape, not just its own series.
fn escape_label_value(value: &str) -> String {
    if !value.contains(['\\', '"', '\n']) {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

/// A lock-free latency histogram: per-bucket counts (non-cumulative; summed at render), the total
/// observation count, and the running sum in microseconds (an integer atomic, no float CAS).
struct Histogram {
    buckets: Vec<AtomicU64>,
    count: AtomicU64,
    sum_micros: AtomicU64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            buckets: DURATION_BUCKETS.iter().map(|_| AtomicU64::new(0)).collect(),
            count: AtomicU64::new(0),
            sum_micros: AtomicU64::new(0),
        }
    }

    /// Record one observation. The float→int cast saturates (Rust semantics: NaN→0, negative→0,
    /// overflow→`u64::MAX`), so a pathological duration never panics or wraps (data-plane discipline).
    fn observe(&self, seconds: f64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        let micros = (seconds * 1_000_000.0) as u64;
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
        for (bound, slot) in DURATION_BUCKETS.iter().zip(self.buckets.iter()) {
            if seconds <= *bound {
                slot.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        // Above the last finite bound: counted only in `+Inf` (== count), no per-bucket slot.
    }
}

/// The fast path's native request metrics (Stage A, ADR 000009). Counters are monotonic; the gauge
/// tracks in-flight requests. One instance lives on `ServerState`, shared across all transports.
pub(crate) struct ServerMetrics {
    /// Requests completed, indexed by response status class: `[1xx, 2xx, 3xx, 4xx, 5xx]`.
    status_class: [AtomicU64; 5],
    /// Requests currently being served (incremented at entry, decremented at exit).
    in_flight: AtomicI64,
    /// Upstream retries onto another instance (ADR 000023).
    retries: AtomicU64,
    /// Requests shed by an upstream circuit breaker (ADR 000028) — a fast-fail 503 at the cap.
    circuit_open: AtomicU64,
    /// Requests rejected by a native route rate limit (ADR 000033) — a fast-fail 429 at the front
    /// door. Distinct from `circuit_open` (503, upstream saturated): this is the client over its
    /// inbound rate floor, before the chain or any forward.
    rate_limited: AtomicU64,
    /// Instances ejected from rotation by outlier detection (ADR 000032).
    outlier_ejections: AtomicU64,
    /// Upgrade tunnels currently open (ADR 000059). A separate gauge from `in_flight`: a tunnel
    /// leaves the request accounting at its 101 (correctly — the handshake WAS one request) but
    /// keeps holding a breaker permit and an LB pick for its whole life, and this is what makes
    /// that occupancy visible (why is the breaker open? how many tunnels would a drain cut?).
    tunnels_active: AtomicI64,
    /// Bytes relayed downstream (upstream → client) by upgrade tunnels, added as each closes.
    tunnel_bytes_down: AtomicU64,
    /// Bytes relayed upstream (client → upstream) by upgrade tunnels, added as each closes.
    tunnel_bytes_up: AtomicU64,
    duration: Histogram,
}

/// RAII guard for the `tunnels_active` gauge: increments on creation, decrements on `Drop` —
/// the same cancel-safety pattern as `proxy_core`'s `InFlight` (a dropped tunnel future, e.g.
/// h2 RST_STREAM, must still decrement or the gauge drifts). Owns its `Arc` because it is moved
/// into the spawned tunnel task alongside the breaker permit and the LB pick (ADR 000059).
pub(crate) struct TunnelActive(Arc<ServerMetrics>);

impl TunnelActive {
    pub(crate) fn new(metrics: Arc<ServerMetrics>) -> Self {
        metrics.tunnels_active.fetch_add(1, Ordering::Relaxed);
        Self(metrics)
    }
}

impl Drop for TunnelActive {
    fn drop(&mut self) {
        self.0.tunnels_active.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ServerMetrics {
    pub(crate) fn new() -> Self {
        Self {
            status_class: std::array::from_fn(|_| AtomicU64::new(0)),
            in_flight: AtomicI64::new(0),
            retries: AtomicU64::new(0),
            circuit_open: AtomicU64::new(0),
            rate_limited: AtomicU64::new(0),
            outlier_ejections: AtomicU64::new(0),
            tunnels_active: AtomicI64::new(0),
            tunnel_bytes_down: AtomicU64::new(0),
            tunnel_bytes_up: AtomicU64::new(0),
            duration: Histogram::new(),
        }
    }

    /// Add a closed tunnel's per-direction byte totals (ADR 000059) — recorded once, when the
    /// tunnel ends, by whichever path ended it (peer close, idle timeout, drain).
    pub(crate) fn add_tunnel_bytes(&self, down: u64, up: u64) {
        self.tunnel_bytes_down.fetch_add(down, Ordering::Relaxed);
        self.tunnel_bytes_up.fetch_add(up, Ordering::Relaxed);
    }

    pub(crate) fn inc_in_flight(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn dec_in_flight(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_retries(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_circuit_open(&self) {
        self.circuit_open.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_rate_limited(&self) {
        self.rate_limited.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_outlier_ejection(&self) {
        self.outlier_ejections.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one completed request: tally its status class and observe its total duration. The
    /// class index is clamped into `[1,5]` then offset, and the read is bounds-checked, so an
    /// out-of-range status (a hostile filter could synthesise any `u16`) never panics.
    pub(crate) fn record_request(&self, status: u16, elapsed: Duration) {
        let idx = (status / 100).clamp(1, 5) as usize - 1;
        if let Some(slot) = self.status_class.get(idx) {
            slot.fetch_add(1, Ordering::Relaxed);
        }
        self.duration.observe(elapsed.as_secs_f64());
    }

    /// Render the Prometheus text exposition format: the native RED metrics plus the host-aggregated
    /// filter-execution metrics (ADR 000009), plus — when export is configured — the OTLP queue
    /// telemetry (ADR 000040: `otlp` is the buffer's point-in-time (dropped, queued) pair). Built by
    /// `format!` + `join` (no `write!` Result to swallow); a cold path, so the allocation is
    /// immaterial.
    pub(crate) fn render(
        &self,
        filter: &MetricsSnapshot,
        otlp: Option<(u64, usize)>,
        pool: Option<plecto_control::PoolResidency>,
        upstreams: &[Arc<UpstreamGroup>],
    ) -> String {
        const CLASSES: [&str; 5] = ["1xx", "2xx", "3xx", "4xx", "5xx"];
        let mut out: Vec<String> = Vec::new();

        out.push(
            "# HELP plecto_requests_total Total client requests handled, by response status class."
                .to_string(),
        );
        out.push("# TYPE plecto_requests_total counter".to_string());
        for (class, slot) in CLASSES.iter().zip(self.status_class.iter()) {
            out.push(format!(
                "plecto_requests_total{{status_class=\"{class}\"}} {}",
                slot.load(Ordering::Relaxed)
            ));
        }

        out.push("# HELP plecto_requests_in_flight Requests currently being served.".to_string());
        out.push("# TYPE plecto_requests_in_flight gauge".to_string());
        out.push(format!(
            "plecto_requests_in_flight {}",
            self.in_flight.load(Ordering::Relaxed).max(0)
        ));

        out.push("# HELP plecto_request_duration_seconds Request duration in seconds.".to_string());
        out.push("# TYPE plecto_request_duration_seconds histogram".to_string());
        let mut cumulative = 0u64;
        for (bound, slot) in DURATION_BUCKETS.iter().zip(self.duration.buckets.iter()) {
            cumulative += slot.load(Ordering::Relaxed);
            out.push(format!(
                "plecto_request_duration_seconds_bucket{{le=\"{bound}\"}} {cumulative}"
            ));
        }
        let count = self.duration.count.load(Ordering::Relaxed);
        out.push(format!(
            "plecto_request_duration_seconds_bucket{{le=\"+Inf\"}} {count}"
        ));
        let sum_seconds = self.duration.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        out.push(format!("plecto_request_duration_seconds_sum {sum_seconds}"));
        out.push(format!("plecto_request_duration_seconds_count {count}"));

        out.push(
            "# HELP plecto_upstream_retries_total Upstream request retries onto another instance."
                .to_string(),
        );
        out.push("# TYPE plecto_upstream_retries_total counter".to_string());
        out.push(format!(
            "plecto_upstream_retries_total {}",
            self.retries.load(Ordering::Relaxed)
        ));

        out.push(
            "# HELP plecto_circuit_open_total Requests shed by an upstream circuit breaker (ADR 000028)."
                .to_string(),
        );
        out.push("# TYPE plecto_circuit_open_total counter".to_string());
        out.push(format!(
            "plecto_circuit_open_total {}",
            self.circuit_open.load(Ordering::Relaxed)
        ));

        out.push(
            "# HELP plecto_rate_limited_total Requests rejected by a native route rate limit (ADR 000033)."
                .to_string(),
        );
        out.push("# TYPE plecto_rate_limited_total counter".to_string());
        out.push(format!(
            "plecto_rate_limited_total {}",
            self.rate_limited.load(Ordering::Relaxed)
        ));

        out.push(
            "# HELP plecto_outlier_ejections_total Instances ejected from rotation by outlier detection (ADR 000032)."
                .to_string(),
        );
        out.push("# TYPE plecto_outlier_ejections_total counter".to_string());
        out.push(format!(
            "plecto_outlier_ejections_total {}",
            self.outlier_ejections.load(Ordering::Relaxed)
        ));

        // --- upstream rotation (ADR 000099), walked here rather than tallied: no persistent
        // counter means nothing survives a reload to go stale. Every state is emitted for every
        // upstream (like the status classes above), so a transition never looks like a new series.
        out.push(
            "# HELP plecto_upstream_instances Upstream instances by state (ADR 000017 / 000032)."
                .to_string(),
        );
        out.push("# TYPE plecto_upstream_instances gauge".to_string());
        for group in upstreams {
            let mut counts = [0u64; UPSTREAM_STATES.len()];
            for instance in &group.endpoints().instances {
                let state =
                    instance_state(instance.is_healthy(), instance.is_outlier_ejected_now());
                if let Some(slot) = counts.get_mut(state) {
                    *slot += 1;
                }
            }
            let name = escape_label_value(&group.name);
            for (state, count) in UPSTREAM_STATES.iter().zip(counts) {
                out.push(format!(
                    "plecto_upstream_instances{{upstream=\"{name}\",state=\"{state}\"}} {count}"
                ));
            }
        }

        out.push(
            "# HELP plecto_tunnels_active Upgrade tunnels currently open (ADR 000048/000059); each holds a breaker permit and an LB pick."
                .to_string(),
        );
        out.push("# TYPE plecto_tunnels_active gauge".to_string());
        out.push(format!(
            "plecto_tunnels_active {}",
            self.tunnels_active.load(Ordering::Relaxed).max(0)
        ));

        out.push(
            "# HELP plecto_tunnel_bytes_down_total Bytes relayed downstream (upstream to client) by upgrade tunnels, recorded at tunnel close."
                .to_string(),
        );
        out.push("# TYPE plecto_tunnel_bytes_down_total counter".to_string());
        out.push(format!(
            "plecto_tunnel_bytes_down_total {}",
            self.tunnel_bytes_down.load(Ordering::Relaxed)
        ));

        out.push(
            "# HELP plecto_tunnel_bytes_up_total Bytes relayed upstream (client to upstream) by upgrade tunnels, recorded at tunnel close."
                .to_string(),
        );
        out.push("# TYPE plecto_tunnel_bytes_up_total counter".to_string());
        out.push(format!(
            "plecto_tunnel_bytes_up_total {}",
            self.tunnel_bytes_up.load(Ordering::Relaxed)
        ));

        // --- extension plane: host-aggregated filter-execution metrics (ADR 000009) ---
        out.push(
            "# HELP plecto_filter_executions_total Filter hook executions (host-aggregated)."
                .to_string(),
        );
        out.push("# TYPE plecto_filter_executions_total counter".to_string());
        out.push(format!("plecto_filter_executions_total {}", filter.total));

        out.push(
            "# HELP plecto_filter_errors_total Filter executions that faulted (trap/deadline/instantiate/unavailable)."
                .to_string(),
        );
        out.push("# TYPE plecto_filter_errors_total counter".to_string());
        out.push(format!("plecto_filter_errors_total {}", filter.errors));

        out.push(
            "# HELP plecto_filter_short_circuits_total Filter executions that short-circuited the chain."
                .to_string(),
        );
        out.push("# TYPE plecto_filter_short_circuits_total counter".to_string());
        out.push(format!(
            "plecto_filter_short_circuits_total {}",
            filter.short_circuits
        ));

        out.push(
            "# HELP plecto_filter_duration_seconds_total Total filter execution time in seconds."
                .to_string(),
        );
        out.push("# TYPE plecto_filter_duration_seconds_total counter".to_string());
        out.push(format!(
            "plecto_filter_duration_seconds_total {}",
            filter.total_duration.as_secs_f64()
        ));

        // --- OTLP export queue (ADR 000040), only when an exporter is configured ---
        if let Some((dropped, queued)) = otlp {
            out.push(
                "# HELP plecto_otlp_dropped_spans_total Spans lost to a full queue or failed exports."
                    .to_string(),
            );
            out.push("# TYPE plecto_otlp_dropped_spans_total counter".to_string());
            out.push(format!("plecto_otlp_dropped_spans_total {dropped}"));

            out.push(
                "# HELP plecto_otlp_queue_spans Spans currently queued for export.".to_string(),
            );
            out.push("# TYPE plecto_otlp_queue_spans gauge".to_string());
            out.push(format!("plecto_otlp_queue_spans {queued}"));
        }

        if let Some(pool) = pool {
            out.push(
                "# HELP plecto_pool_component_instances Live component instances in the trusted engine's pooling allocator."
                    .to_string(),
            );
            out.push("# TYPE plecto_pool_component_instances gauge".to_string());
            out.push(format!(
                "plecto_pool_component_instances {}",
                pool.component_instances
            ));

            out.push(
                "# HELP plecto_pool_memories Live pooled linear memories in the trusted engine."
                    .to_string(),
            );
            out.push("# TYPE plecto_pool_memories gauge".to_string());
            out.push(format!("plecto_pool_memories {}", pool.memories));

            out.push(
                "# HELP plecto_pool_unused_memory_resident_bytes Bytes kept resident for unused-but-warm pool slots."
                    .to_string(),
            );
            out.push("# TYPE plecto_pool_unused_memory_resident_bytes gauge".to_string());
            out.push(format!(
                "plecto_pool_unused_memory_resident_bytes {}",
                pool.unused_memory_bytes_resident
            ));
        }

        let mut text = out.join("\n");
        text.push('\n');
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(total: u64, errors: u64, short_circuits: u64) -> MetricsSnapshot {
        MetricsSnapshot {
            total,
            errors,
            short_circuits,
            total_duration: Duration::from_millis(0),
        }
    }

    #[test]
    fn records_status_classes_and_renders_prometheus_exposition() {
        let m = ServerMetrics::new();
        m.record_request(200, Duration::from_millis(3));
        m.record_request(204, Duration::from_millis(7));
        m.record_request(404, Duration::from_millis(1));
        m.record_request(503, Duration::from_millis(50));

        let text = m.render(&snap(0, 0, 0), None, None, &[]);
        assert!(text.contains("plecto_requests_total{status_class=\"2xx\"} 2"));
        assert!(text.contains("plecto_requests_total{status_class=\"4xx\"} 1"));
        assert!(text.contains("plecto_requests_total{status_class=\"5xx\"} 1"));
        assert!(text.contains("plecto_requests_total{status_class=\"3xx\"} 0"));
        assert!(text.contains("plecto_request_duration_seconds_count 4"));
        assert!(text.contains("# TYPE plecto_request_duration_seconds histogram"));
    }

    #[test]
    fn out_of_range_status_does_not_panic() {
        // A hostile filter can synthesise any u16 status; the metric must absorb it, never panic.
        let m = ServerMetrics::new();
        for status in [0u16, 99, 600, 999, u16::MAX] {
            m.record_request(status, Duration::from_millis(1));
        }
        // all five clamp into 1xx or 5xx — the point is simply that none panicked.
        assert!(
            m.render(&snap(0, 0, 0), None, None, &[])
                .contains("plecto_request_duration_seconds_count 5")
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_end_at_total_count() {
        let m = ServerMetrics::new();
        for ms in [2u64, 8, 30, 300] {
            m.record_request(200, Duration::from_millis(ms));
        }
        let text = m.render(&snap(0, 0, 0), None, None, &[]);
        let counts: Vec<u64> = text
            .lines()
            .filter(|l| l.starts_with("plecto_request_duration_seconds_bucket"))
            .filter_map(|l| l.rsplit(' ').next())
            .filter_map(|n| n.parse::<u64>().ok())
            .collect();
        assert!(
            counts.windows(2).all(|w| w[0] <= w[1]),
            "buckets must be cumulative / non-decreasing: {counts:?}"
        );
        assert_eq!(
            counts.last().copied(),
            Some(4),
            "the +Inf bucket equals the total observation count"
        );
    }

    #[test]
    fn in_flight_gauge_tracks_the_balance() {
        let m = ServerMetrics::new();
        m.inc_in_flight();
        m.inc_in_flight();
        m.dec_in_flight();
        assert!(
            m.render(&snap(0, 0, 0), None, None, &[])
                .contains("plecto_requests_in_flight 1")
        );
    }

    #[test]
    fn tunnel_gauge_follows_the_guard_and_byte_counters_accumulate() {
        let m = Arc::new(ServerMetrics::new());
        let g1 = TunnelActive::new(m.clone());
        let g2 = TunnelActive::new(m.clone());
        assert!(
            m.render(&snap(0, 0, 0), None, None, &[])
                .contains("plecto_tunnels_active 2")
        );
        drop(g1);
        m.add_tunnel_bytes(19, 7);
        m.add_tunnel_bytes(1, 2);
        let text = m.render(&snap(0, 0, 0), None, None, &[]);
        assert!(text.contains("plecto_tunnels_active 1"));
        assert!(text.contains("plecto_tunnel_bytes_down_total 20"));
        assert!(text.contains("plecto_tunnel_bytes_up_total 9"));
        drop(g2);
        assert!(
            m.render(&snap(0, 0, 0), None, None, &[])
                .contains("plecto_tunnels_active 0")
        );
    }

    #[test]
    fn folds_in_host_filter_metrics() {
        let m = ServerMetrics::new();
        let text = m.render(&snap(5, 2, 1), None, None, &[]);
        assert!(text.contains("plecto_filter_executions_total 5"));
        assert!(text.contains("plecto_filter_errors_total 2"));
        assert!(text.contains("plecto_filter_short_circuits_total 1"));
    }

    #[test]
    fn otlp_queue_telemetry_renders_only_when_export_is_configured() {
        let m = ServerMetrics::new();
        let off = m.render(&snap(0, 0, 0), None, None, &[]);
        assert!(
            !off.contains("plecto_otlp_"),
            "no OTLP lines without an exporter"
        );
        let on = m.render(&snap(0, 0, 0), Some((7, 3)), None, &[]);
        assert!(on.contains("plecto_otlp_dropped_spans_total 7"));
        assert!(on.contains("plecto_otlp_queue_spans 3"));
    }

    #[test]
    fn pool_residency_gauges_render_only_when_the_host_reports_them() {
        let m = ServerMetrics::new();
        let off = m.render(&snap(0, 0, 0), None, None, &[]);
        assert!(
            !off.contains("plecto_pool_"),
            "no pool lines without a pooling engine"
        );
        let pool = plecto_control::PoolResidency {
            component_instances: 3,
            memories: 2,
            unused_memory_bytes_resident: 4096,
        };
        let on = m.render(&snap(0, 0, 0), None, Some(pool), &[]);
        assert!(
            on.contains("plecto_pool_component_instances 3"),
            "live pooled component instances:\n{on}"
        );
        assert!(on.contains("plecto_pool_memories 2"), "{on}");
        assert!(
            on.contains("plecto_pool_unused_memory_resident_bytes 4096"),
            "{on}"
        );
    }

    /// Live upstream groups reconciled from `toml`, in the order `names` lists them.
    fn groups(toml: &str, names: &[&str]) -> Vec<Arc<UpstreamGroup>> {
        let manifest = plecto_control::Manifest::from_toml(toml).expect("the manifest parses");
        let registry = plecto_control::UpstreamRegistry::new();
        registry
            .reconcile(&manifest.upstreams, std::path::Path::new("."))
            .expect("the upstreams reconcile");
        names
            .iter()
            .map(|n| registry.group(n).expect("a reconciled group"))
            .collect()
    }

    /// The rendered `plecto_upstream_instances` count for one (upstream, state) pair.
    fn state_count(text: &str, upstream: &str, state: &str) -> Option<u64> {
        let prefix =
            format!("plecto_upstream_instances{{upstream=\"{upstream}\",state=\"{state}\"}} ");
        text.lines()
            .find_map(|l| l.strip_prefix(prefix.as_str()))
            .and_then(|n| n.trim().parse().ok())
    }

    const TWO_INSTANCE_UPSTREAM: &str = r#"
[[upstream]]
name = "u"
addresses = ["10.0.0.1:80", "10.0.0.2:80"]
[upstream.health]
path = "/healthz"
[upstream.outlier_detection]
consecutive_gateway_failures = 1
base_ejection_time_ms = 60000
max_ejection_percent = 100
"#;

    #[test]
    fn upstream_instances_count_a_pessimistic_pool_as_unhealthy() {
        // ADR 000017: a fresh instance is out of rotation until a probe passes, and that is
        // exactly the fail-closed 503 this gauge exists to explain.
        let g = groups(TWO_INSTANCE_UPSTREAM, &["u"]);
        let text = ServerMetrics::new().render(&snap(0, 0, 0), None, None, &g);
        assert!(
            text.contains("# TYPE plecto_upstream_instances gauge"),
            "the gauge is declared:\n{text}"
        );
        assert_eq!(state_count(&text, "u", "unhealthy"), Some(2), "{text}");
        assert_eq!(state_count(&text, "u", "healthy"), Some(0), "{text}");
        assert_eq!(state_count(&text, "u", "ejected"), Some(0), "{text}");
    }

    #[test]
    fn upstream_instances_count_a_probed_pool_as_healthy() {
        let g = groups(TWO_INSTANCE_UPSTREAM, &["u"]);
        for inst in &g[0].endpoints().instances {
            inst.record_probe_success();
        }
        let text = ServerMetrics::new().render(&snap(0, 0, 0), None, None, &g);
        assert_eq!(state_count(&text, "u", "healthy"), Some(2), "{text}");
        assert_eq!(state_count(&text, "u", "unhealthy"), Some(0), "{text}");
        assert_eq!(state_count(&text, "u", "ejected"), Some(0), "{text}");
    }

    #[test]
    fn upstream_instances_fold_an_ejected_but_probe_healthy_instance_as_ejected() {
        // ADR 000032: outlier ejection is an axis INDEPENDENT of the health state machine, so a
        // probe-healthy AND outlier-ejected instance genuinely exists. The mutually-exclusive
        // label folds by severity (ejected > unhealthy > healthy), so it is counted exactly once.
        let g = groups(TWO_INSTANCE_UPSTREAM, &["u"]);
        let instances = g[0].endpoints().instances.clone();
        for inst in &instances {
            inst.record_probe_success();
        }
        assert!(
            g[0].record_outcome(&instances[0], true),
            "one gateway failure crosses the threshold and ejects"
        );
        assert!(
            instances[0].is_healthy(),
            "the probe bit is untouched — this is the independent-axes case the fold exists for"
        );

        let text = ServerMetrics::new().render(&snap(0, 0, 0), None, None, &g);
        assert_eq!(state_count(&text, "u", "ejected"), Some(1), "{text}");
        assert_eq!(state_count(&text, "u", "healthy"), Some(1), "{text}");
        assert_eq!(state_count(&text, "u", "unhealthy"), Some(0), "{text}");
    }

    #[test]
    fn upstream_instances_render_every_state_of_every_upstream_exactly_once() {
        // The cardinality bound: upstream count × 3 series, and each upstream's series sum back to
        // its instance count (no instance counted twice, none dropped).
        let toml = r#"
[[upstream]]
name = "a"
addresses = ["10.0.0.1:80", "10.0.0.2:80", "10.0.0.3:80"]
[upstream.health]
path = "/healthz"

[[upstream]]
name = "b"
addresses = ["10.0.1.1:80"]
[upstream.health]
path = "/healthz"
"#;
        let g = groups(toml, &["a", "b"]);
        g[0].endpoints().instances[0].record_probe_success();
        let text = ServerMetrics::new().render(&snap(0, 0, 0), None, None, &g);

        let series: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("plecto_upstream_instances{"))
            .collect();
        assert_eq!(
            series.len(),
            6,
            "two upstreams × three states, no more:\n{text}"
        );
        assert_eq!(state_count(&text, "a", "healthy"), Some(1), "{text}");
        assert_eq!(state_count(&text, "a", "unhealthy"), Some(2), "{text}");
        assert_eq!(state_count(&text, "a", "ejected"), Some(0), "{text}");
        assert_eq!(state_count(&text, "b", "unhealthy"), Some(1), "{text}");
        for upstream in ["a", "b"] {
            let sum: u64 = ["healthy", "unhealthy", "ejected"]
                .iter()
                .filter_map(|s| state_count(&text, upstream, s))
                .sum();
            let instances = g
                .iter()
                .find(|group| group.name == upstream)
                .map(|group| group.endpoints().instances.len() as u64);
            assert_eq!(
                Some(sum),
                instances,
                "upstream {upstream}'s series sum to its instance count:\n{text}"
            );
        }
    }

    #[test]
    fn upstream_label_values_are_escaped() {
        // The first dynamic label value in this exposition comes from the manifest, and an
        // unescaped quote would corrupt the WHOLE scrape, not just this series.
        let toml = r#"
[[upstream]]
name = "we\"ird\\name"
addresses = ["10.0.0.1:80"]
[upstream.health]
path = "/healthz"
"#;
        let g = groups(toml, &["we\"ird\\name"]);
        let text = ServerMetrics::new().render(&snap(0, 0, 0), None, None, &g);
        assert!(
            text.contains(
                "plecto_upstream_instances{upstream=\"we\\\"ird\\\\name\",state=\"unhealthy\"} 1"
            ),
            "the quote and backslash are escaped:\n{text}"
        );
    }
}
