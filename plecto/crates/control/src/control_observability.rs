//! `Control`'s observability surface (ADR 000009 Stage A): the host-aggregated filter-execution
//! metrics snapshot and the operator-configured admin/access-log settings the fast path reads.

use std::sync::Arc;

use plecto_host::MetricsSnapshot;
use plecto_host::otlp::OtlpBuffer;

use crate::Control;

/// Point-in-time residency of the host's trusted pooling allocator (wasmtime
/// `PoolingAllocatorMetrics`), lowered to plain counters so the fast path renders them on
/// `/metrics` without naming wasmtime types.
#[derive(Debug, Clone, Copy)]
pub struct PoolResidency {
    /// Live pooled component instances.
    pub component_instances: u64,
    /// Live pooled linear memories.
    pub memories: usize,
    /// Bytes kept resident for unused-but-warm pool slots (`linear_memory_keep_resident`;
    /// left at its default 0 by the host, so this is expected to read ~0).
    pub unused_memory_bytes_resident: usize,
}

impl Control {
    /// A snapshot of the host-aggregated filter-execution metrics (ADR 000009): the tally the
    /// `MetricsSink` wired at construction has accumulated. The fast path's admin `/metrics`
    /// endpoint renders this alongside its native RED metrics.
    pub fn filter_metrics(&self) -> MetricsSnapshot {
        self.filter_metrics.snapshot()
    }

    /// Residency of the trusted (pooling) engine, for the admin `/metrics` endpoint. `None` when
    /// the trusted engine is not pooling (it always is today, so callers can expect `Some`).
    pub fn pool_residency(&self) -> Option<PoolResidency> {
        self.host.pooling_allocator_metrics().map(|m| PoolResidency {
            component_instances: m.component_instances(),
            memories: m.memories(),
            unused_memory_bytes_resident: m.unused_memory_bytes_resident(),
        })
    }

    /// The admin endpoint bind address (`[observability] admin_addr`), or `None` when no admin
    /// listener is configured (the default). The fast path binds a separate listener there for
    /// `/metrics` + liveness/readiness (ADR 000009 Stage A).
    pub fn admin_addr(&self) -> Option<&str> {
        self.observability.admin_addr.as_deref()
    }

    /// Whether the structured access log is enabled (`[observability] access_log`, ADR 000009).
    pub fn access_log_enabled(&self) -> bool {
        self.observability.access_log
    }

    /// The OTLP/HTTP collector base URL (`[observability] otlp_endpoint`, ADR 000040), or `None`
    /// when trace export is off (the default). The exporter appends `/v1/traces`.
    pub fn otlp_endpoint(&self) -> Option<&str> {
        self.observability.otlp_endpoint.as_deref()
    }

    /// The OTLP span buffer (ADR 000040): filter spans fan into it from the host sink, the fast
    /// path pushes its request span, and the export pump drains it. Present iff
    /// [`otlp_endpoint`](Self::otlp_endpoint) is set.
    pub fn otlp_buffer(&self) -> Option<Arc<OtlpBuffer>> {
        self.otlp.clone()
    }

    /// The manifest's data-plane bind address (`[listen] addr`), or `None` for the binary default.
    /// Captured at construction, like `admin_addr` — a reload does not re-bind; the CLI's explicit
    /// positional arg overrides it.
    pub fn listen_addr(&self) -> Option<&str> {
        self.listen.addr.as_deref()
    }

    /// The `Alt-Svc` h3 advertisement port override (`[listen] advertised_port`), or `None` to
    /// advertise the bound port. For container port mappings where the published port differs
    /// from the bound one (moka-1 field report §3.4).
    pub fn advertised_port(&self) -> Option<u16> {
        self.listen.advertised_port
    }

    /// The parsed `[listen.proxy_protocol]` trusted networks (ADR 000057), or `None` when PROXY
    /// v2 reception is off (the default). Captured at construction like `listen_addr` — the TCP
    /// listener consults it once at startup; a reload does not change it. The h3/UDP listener is
    /// out of scope (ADR 000057 decision 5).
    pub fn proxy_protocol_trust(&self) -> Option<crate::manifest::ProxyProtocolTrust> {
        self.proxy_protocol.clone()
    }

    /// The readiness grace (`[listen.drain] readiness_grace_ms`, ADR 000059): how long `/readyz`
    /// reports not-ready — while connections are still accepted — before the drain starts, so a
    /// front load balancer can take the replica out of rotation first. Zero (the default) starts
    /// the drain immediately. Captured at construction like the rest of `[listen]`.
    pub fn readiness_grace(&self) -> std::time::Duration {
        let ms = self
            .listen
            .drain
            .as_ref()
            .and_then(|d| d.readiness_grace_ms)
            .unwrap_or(0);
        std::time::Duration::from_millis(ms)
    }

    /// The drain window (`[listen.drain] window_ms`, ADR 000059): how long in-flight work may
    /// finish at shutdown before remaining connections are cut. `None` = the server's default
    /// (30 s). One setting shared by every drain path (TCP requests, h3 GOAWAY, tunnels).
    pub fn drain_window(&self) -> Option<std::time::Duration> {
        self.listen
            .drain
            .as_ref()
            .and_then(|d| d.window_ms)
            .map(std::time::Duration::from_millis)
    }
}
