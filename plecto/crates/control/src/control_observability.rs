//! `Control`'s observability surface (ADR 000009 Stage A): the host-aggregated filter-execution
//! metrics snapshot and the operator-configured admin/access-log settings the fast path reads.

use std::sync::Arc;

use plecto_host::MetricsSnapshot;
use plecto_host::otlp::OtlpBuffer;

use crate::Control;

impl Control {
    /// A snapshot of the host-aggregated filter-execution metrics (ADR 000009): the tally the
    /// `MetricsSink` wired at construction has accumulated. The fast path's admin `/metrics`
    /// endpoint renders this alongside its native RED metrics.
    pub fn filter_metrics(&self) -> MetricsSnapshot {
        self.filter_metrics.snapshot()
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
}
