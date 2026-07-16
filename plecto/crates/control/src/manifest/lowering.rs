//! Lower a manifest `FilterEntry` into the host's `LoadOptions` (ADR 000006).

use plecto_host::LoadOptions;

#[cfg(feature = "outbound-http")]
use super::SchemeKind;
#[cfg(feature = "fat-guest")]
use super::WasiKind;
use super::{FilterEntry, IsolationKind};

impl FilterEntry {
    /// The host `LoadOptions` for this entry: isolation plus any metering overrides
    /// (ADR 000006). Unset knobs keep the host defaults.
    pub(crate) fn load_options(&self) -> LoadOptions {
        let mut opts = match self.isolation {
            IsolationKind::Trusted => LoadOptions::trusted(),
            IsolationKind::Untrusted => LoadOptions::untrusted(),
        };
        if let Some(ms) = self.init_deadline_ms {
            opts = opts.with_init_deadline_ms(ms);
        }
        if let Some(ms) = self.request_deadline_ms {
            opts = opts.with_request_deadline_ms(ms);
        }
        if let Some(bytes) = self.max_memory_bytes {
            opts = opts.with_max_memory_bytes(bytes);
        }
        if let Some(n) = self.pool_size {
            opts = opts.with_trusted_pool_size(n);
        }
        if let Some(ms) = self.checkout_timeout_ms {
            opts = opts.with_checkout_timeout_ms(ms);
        }
        if let Some(n) = self.max_requests_per_instance {
            opts = opts.with_max_requests_per_instance(n);
        }
        if let Some(rl) = self.ratelimit {
            opts = opts.with_ratelimit_bucket(rl.capacity, rl.refill_tokens, rl.refill_interval_ms);
        }
        #[cfg(feature = "outbound-http")]
        if let Some(ob) = &self.outbound_http {
            // Validated already (`validate`): CIDRs parse, metering is non-zero. An empty `allow`
            // is permitted (deny-all) so wasip2 guests can link `wasi:http` without naming a
            // destination (filter-jwt static PEM path, ADR 000070).
            let allow = ob
                .allow
                .iter()
                .map(|d| plecto_host::AllowEntry {
                    scheme: match d.scheme {
                        SchemeKind::Https => plecto_host::Scheme::Https,
                        SchemeKind::Http => plecto_host::Scheme::Http,
                    },
                    host: d.host.clone(),
                    port: d.port.unwrap_or_else(|| d.scheme.default_port()),
                })
                .collect();
            opts = opts.with_outbound_http(
                allow,
                ob.allow_private.clone(),
                ob.connect_timeout_ms,
                ob.total_timeout_ms,
                ob.max_response_bytes,
                ob.max_concurrent,
            );
        }
        #[cfg(feature = "outbound-tcp")]
        if let Some(ob) = &self.outbound_tcp {
            // Validated already (`validate`): non-empty allowlist, non-zero ports, parsing CIDRs.
            let allow = ob
                .allow
                .iter()
                .map(|d| plecto_host::TcpAllowEntry {
                    host: d.host.clone(),
                    port: d.port,
                })
                .collect();
            opts = opts.with_outbound_tcp(
                allow,
                ob.allow_private.clone(),
                ob.max_connections,
                ob.io_deadline_ms,
            );
        }
        #[cfg(feature = "fat-guest")]
        if self.wasi == WasiKind::Minimal {
            opts = opts.with_wasi_minimal();
        }
        if let Some(cfg) = &self.config {
            opts = opts.with_config(cfg.clone());
        }
        opts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::RateLimitConfig;

    #[test]
    fn load_options_maps_isolation_and_overrides() {
        let entry = FilterEntry {
            id: "x".to_string(),
            source: "s".to_string(),
            digest: "sha256:abc".to_string(),
            isolation: IsolationKind::Trusted,
            init_deadline_ms: None,
            request_deadline_ms: Some(40),
            max_memory_bytes: Some(1024),
            pool_size: Some(3),
            checkout_timeout_ms: Some(75),
            max_requests_per_instance: Some(500),
            ratelimit: Some(RateLimitConfig {
                capacity: 100,
                refill_tokens: 10,
                refill_interval_ms: 1000,
            }),
            outbound_http: None,
            outbound_tcp: None,
            wasi: crate::manifest::WasiKind::None,
            config: None,
        };
        let opts = entry.load_options();

        assert_eq!(opts.isolation, plecto_host::Isolation::Trusted);
        assert_eq!(opts.request_deadline_ms, 40);
        assert_eq!(opts.max_memory_bytes, 1024);
        // the pool lifecycle knobs (ADR 000012) reach the host from the manifest
        assert_eq!(opts.trusted_pool_size, 3);
        assert_eq!(opts.checkout_timeout_ms, 75);
        assert_eq!(opts.max_requests_per_instance, 500);
        // an unset knob keeps the host default
        assert_eq!(
            opts.init_deadline_ms,
            LoadOptions::trusted().init_deadline_ms
        );
        // the per-filter manifest bucket maps to the host-side spec (ADR 000026) — the filter
        // cannot supply or override it.
        let bucket = opts
            .ratelimit_bucket
            .expect("a manifest ratelimit maps to the host bucket");
        assert_eq!(bucket.capacity, 100);
        assert_eq!(bucket.refill_tokens, 10);
        assert_eq!(bucket.refill_interval_ms, 1000);
    }

    #[test]
    fn config_section_lowers_to_load_options() {
        let m = crate::manifest::Manifest::from_toml(
            r#"
[[filter]]
id = "ratelimit-redis"
source = "s"
digest = "sha256:abc"

[filter.config]
on_backend_error = "deny"
"#,
        )
        .unwrap();
        let opts = m.filters[0].load_options();
        assert_eq!(
            opts.config.get("on_backend_error").map(String::as_str),
            Some("deny")
        );
    }
}
