//! Operational observability config (`[observability]`, ADR 000009 Stage A).

use serde::{Deserialize, Serialize};

/// Operational observability config (`[observability]`, ADR 000009 Stage A): a separate admin
/// listener exposing Prometheus metrics + liveness/readiness, and an opt-in structured access log.
/// Off by default — Plecto stays quiet and exposes nothing extra unless asked (operational
/// simplicity). Captured at construction; a reload does not re-bind the admin listener.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Observability {
    /// `host:port` the admin endpoint binds (e.g. `127.0.0.1:9090`). `None` = no admin listener
    /// (the default). Serves `/metrics`, `/healthz`, `/readyz` — never on the data-plane port, so
    /// proxied routes never collide with it and the metrics surface is not exposed to clients.
    #[serde(default)]
    pub admin_addr: Option<String>,
    /// Emit one structured access-log event per request (the `plecto::access` tracing target,
    /// rendered as JSON by the binary's subscriber). `false` by default.
    #[serde(default)]
    pub access_log: bool,
    /// OTLP/HTTP collector base URL (e.g. `http://localhost:4318`) — the exporter appends
    /// `/v1/traces`, mirroring `OTEL_EXPORTER_OTLP_ENDPOINT` semantics (ADR 000040). `None` = no
    /// trace export (the default). Captured at construction, like `admin_addr`: changing it
    /// requires a restart, not a reload.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
}

#[cfg(test)]
mod tests {
    use crate::manifest::Manifest;

    #[test]
    fn observability_defaults_off_and_parses_when_present() {
        // Absent `[observability]` → admin endpoint off, access log off, no OTLP export
        // (operational simplicity).
        let bare = Manifest::from_toml("").unwrap();
        assert_eq!(bare.observability.admin_addr, None);
        assert!(!bare.observability.access_log);
        assert_eq!(bare.observability.otlp_endpoint, None);

        // Present → the knobs are read.
        let m = Manifest::from_toml(
            r#"
[observability]
admin_addr = "127.0.0.1:9090"
access_log = true
otlp_endpoint = "http://localhost:4318"
"#,
        )
        .unwrap();
        assert_eq!(
            m.observability.admin_addr.as_deref(),
            Some("127.0.0.1:9090")
        );
        assert!(m.observability.access_log);
        assert_eq!(
            m.observability.otlp_endpoint.as_deref(),
            Some("http://localhost:4318")
        );
    }

    #[test]
    fn observability_is_not_part_of_the_content_hash() {
        // `[observability]` is operational, not config identity (`skip_serializing`): toggling it
        // must NOT change the `content_hash` / config version, so an admin-only edit is a reload
        // no-op rather than a spurious "config changed".
        let without = Manifest::from_toml("").unwrap();
        let with = Manifest::from_toml(
            r#"
[observability]
admin_addr = "127.0.0.1:9090"
access_log = true
otlp_endpoint = "http://localhost:4318"
"#,
        )
        .unwrap();
        assert_eq!(
            without.content_hash().unwrap(),
            with.content_hash().unwrap(),
            "observability config must not affect the semantic content hash"
        );
    }
}
