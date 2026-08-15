//! Operational observability config (`[observability]`, ADR 000009 Stage A).

use serde::{Deserialize, Serialize};

use crate::error::ControlError;

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

impl Observability {
    /// Whether the section declares anything at all. Every knob is off by default, so an empty
    /// `[observability]` header configures exactly as much as an absent one — presence is
    /// decided per field, not per section.
    pub(crate) fn is_declared(&self) -> bool {
        self.admin_addr.is_some() || self.access_log || self.otlp_endpoint.is_some()
    }

    /// Validate the section fail-closed at build (ADR 000111): an `otlp_endpoint` the serving
    /// binary cannot export to must never be accepted. The exporter implements the plaintext
    /// subset of OTLP/HTTP — where the scheme alone determines the transport security — so the
    /// value must carry the `http://` scheme and parse as the base URL the exporter appends
    /// `/v1/traces` to. Port and path stay optional: the OTLP spec permits their absence.
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        let Some(endpoint) = self.otlp_endpoint.as_deref() else {
            return Ok(());
        };
        let bad = |reason: String| ControlError::InvalidOtlpEndpoint {
            endpoint: endpoint.to_string(),
            reason,
        };
        if !endpoint.starts_with("http://") {
            return Err(bad("must be an http:// base URL".to_string()));
        }
        // Parse the very string the exporter builds, so validation and export cannot disagree
        // about which base URLs are usable.
        let base = endpoint.trim_end_matches('/');
        format!("{base}/v1/traces")
            .parse::<http::Uri>()
            .map_err(|e| bad(format!("is not a usable base URL ({e})")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn exporting_to(endpoint: &str) -> Observability {
        Observability {
            otlp_endpoint: Some(endpoint.to_string()),
            ..Observability::default()
        }
    }

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

    #[test]
    fn otlp_endpoint_accepts_only_plaintext_base_urls() {
        // The exporter implements the plaintext OTLP/HTTP subset, and in OTLP/HTTP the scheme is
        // the sole transport-security determinant: a value the serving binary cannot honor is
        // rejected here rather than accepted into a process that exports nothing (ADR 000111).
        assert!(
            Observability::default().validate().is_ok(),
            "no endpoint = no export, which is valid"
        );
        for accepted in [
            "http://collector",
            "http://collector:4318",
            "http://collector:4318/",
            "http://127.0.0.1:4318/otlp",
        ] {
            assert!(
                exporting_to(accepted).validate().is_ok(),
                "{accepted} is a base URL the exporter can honor (port and path are optional)"
            );
        }
        for rejected in [
            "https://collector:4318",
            "grpc://collector:4317",
            "collector:4318",
            "http://col lector:4318",
        ] {
            assert!(
                exporting_to(rejected).validate().is_err(),
                "{rejected} is not a base URL the exporter can honor"
            );
        }
    }

    #[test]
    fn a_rejected_otlp_endpoint_names_the_local_collector() {
        // The diagnostic carries the next step (a local collector originating TLS), not just the
        // refusal — the same discipline as the [chain] rejection.
        let err = exporting_to("https://otel.example:4318")
            .validate()
            .expect_err("a TLS endpoint must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("otlp_endpoint"),
            "the error names the rejected field, got: {message}"
        );
        assert!(
            message.contains("plaintext"),
            "the error states the transport constraint, got: {message}"
        );
        assert!(
            message.contains("agent"),
            "the error names the collector agent pattern, got: {message}"
        );
    }
}
