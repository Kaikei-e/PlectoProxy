//! Build-time validation (ADR 000006 / 000020 / 000035 / 000036): reject an out-of-range
//! upstream load-balancing config or filter metering/outbound config fail-closed, before the
//! persistent state (upstream registry / host load) is ever touched.

use super::{
    FilterEntry, HashKeyKind, IsolationKind, LbAlgorithm, MAX_HASH_TABLE_SIZE,
    MAX_INSTANCE_WEIGHT, OutboundHttpConfig, OutboundTcpConfig, State, StateBackendKind, Upstream,
};
use crate::error::ControlError;

impl State {
    /// Validate the `[state]` section (ADR 000041) fail-closed at build, before the backend is
    /// constructed. A half-set section must never silently run on memory: `redb` needs a path
    /// (nowhere to persist otherwise), and a path under `memory` means the operator likely
    /// intended `redb` — running memory anyway would look durable while losing every restart.
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        let path = self.path.as_deref().map(str::trim).unwrap_or("");
        match self.backend {
            StateBackendKind::Redb if path.is_empty() => Err(ControlError::InvalidStateConfig(
                "backend = \"redb\" requires a non-empty `path`".to_string(),
            )),
            StateBackendKind::Memory if self.path.is_some() => {
                Err(ControlError::InvalidStateConfig(
                    "`path` is only valid with backend = \"redb\"".to_string(),
                ))
            }
            StateBackendKind::Redb | StateBackendKind::Memory => Ok(()),
        }
    }
}

impl Upstream {
    /// Validate this upstream's load-balancing config (ADR 000035) fail-closed at build, before the
    /// persistent registry reconciles. Checks per-instance weights, the `lb_algorithm` ↔
    /// `[upstream.hash]` correspondence, and (for Maglev) the hash key and table size. Returns the
    /// reason a caller wraps with the upstream name.
    pub(crate) fn validate_lb(&self) -> Result<(), String> {
        // Health-probe timing: a config typo must not reach the arithmetic (the same rationale as
        // every other zero rejection here). `interval_ms = 0` would clamp to a 1 ms probe loop
        // (~1000 probes/s per instance — a self-inflicted upstream DoS); `timeout_ms = 0` makes
        // every probe fail, so instances stay pessimistic forever (permanent 503) with no
        // build-time diagnostic.
        if self.health.interval_ms == 0 {
            return Err(
                "[upstream.health] interval_ms must be >= 1 (0 would probe in a busy loop)"
                    .to_string(),
            );
        }
        if self.health.timeout_ms == 0 {
            return Err(
                "[upstream.health] timeout_ms must be >= 1 (0 fails every probe, so no instance \
                 could ever become healthy)"
                    .to_string(),
            );
        }
        for spec in &self.addresses {
            let w = spec.weight();
            if w == 0 {
                return Err(format!(
                    "instance {:?} has weight 0; drain an instance by removing its address, not by zeroing weight",
                    spec.address()
                ));
            }
            if w > MAX_INSTANCE_WEIGHT {
                return Err(format!(
                    "instance {:?} weight {w} exceeds the maximum {MAX_INSTANCE_WEIGHT}",
                    spec.address()
                ));
            }
        }

        match (self.lb_algorithm, &self.hash) {
            // Maglev needs a hash key; a key with no algorithm to use it is a config mistake.
            (LbAlgorithm::Maglev, None) => {
                return Err(
                    "lb_algorithm = \"maglev\" requires a [upstream.hash] block".to_string()
                );
            }
            (algo, Some(_)) if algo != LbAlgorithm::Maglev => {
                return Err(
                    "[upstream.hash] is only valid with lb_algorithm = \"maglev\"".to_string(),
                );
            }
            _ => {}
        }

        if let Some(hash) = &self.hash {
            if hash.key == HashKeyKind::Header
                && hash
                    .header
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
            {
                return Err(
                    "[upstream.hash] key = \"header\" requires a non-empty header name".to_string(),
                );
            }
            let m = hash.table_size;
            if m > MAX_HASH_TABLE_SIZE {
                return Err(format!(
                    "[upstream.hash] table_size {m} exceeds the maximum {MAX_HASH_TABLE_SIZE}"
                ));
            }
            if !is_prime(m) {
                return Err(format!(
                    "[upstream.hash] table_size {m} must be prime (the Maglev permutation needs skip coprime to M)"
                ));
            }
            // Each instance needs at least one table entry, and the table indexes instances with a
            // u16, so the pool must fit both bounds.
            let n = self.addresses.len();
            if n > m as usize {
                return Err(format!(
                    "[upstream.hash] table_size {m} is smaller than the {n} instances"
                ));
            }
            if n > u16::MAX as usize {
                return Err(format!(
                    "maglev supports at most {} instances, got {n}",
                    u16::MAX
                ));
            }
        }
        Ok(())
    }

    /// Warn (ADR 000050) — never reject — when `[upstream.tls]` has no `sni` override while an
    /// address is an IP literal or `resolve_interval_ms` is non-zero: the TLS leg then derives the
    /// SNI / verification name from the connected IP, which sends no SNI extension and verifies
    /// the certificate against the bare IP — failing every handshake unless the certificate
    /// carries an IP SAN. An IP-SAN certificate is a legitimate deployment, so this only warns.
    pub(crate) fn warn_missing_sni(&self) {
        let Some(tls) = &self.tls else { return };
        if tls.sni.is_some() {
            return;
        }
        let has_ip_literal = self
            .addresses
            .iter()
            .any(|a| a.address().parse::<std::net::SocketAddr>().is_ok());
        if has_ip_literal || self.resolve_interval_ms > 0 {
            tracing::warn!(
                upstream = %self.name,
                "[upstream.tls] has no `sni` while this upstream may resolve to a bare IP (an \
                 IP-literal address, or resolve_interval_ms > 0 expanding a hostname to IPs); the \
                 TLS leg sends no SNI and verifies the certificate against the IP, which fails \
                 unless the certificate carries an IP SAN — declare `sni` to pin the verification \
                 name (ADR 000050)"
            );
        }
    }
}

/// Trial-division primality test for the Maglev `table_size` (ADR 000035). Build-time only and `M`
/// is capped at a few million, so trial division to `√M` (~2236 iterations at the cap) is trivial;
/// no need for a probabilistic test or a dependency.
pub(super) fn is_prime(n: u32) -> bool {
    if n < 2 {
        return false;
    }
    if n.is_multiple_of(2) {
        return n == 2;
    }
    let mut d = 3u64;
    let n = n as u64;
    while d * d <= n {
        if n.is_multiple_of(d) {
            return false;
        }
        d += 2;
    }
    true
}

impl FilterEntry {
    /// Reject out-of-range metering / rate-limit values before they reach the host (/
    /// CWE-20): a zero deadline would make every call instantly time out, a zero memory cap is
    /// unusable, and a rate-limit bucket with `capacity == 0` or `refill_interval_ms == 0`
    /// (with refills) can never serve a token — a config typo, not an intended state. Fail-closed.
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        let bad = |reason: &str| ControlError::InvalidFilterConfig {
            id: self.id.clone(),
            reason: reason.to_string(),
        };
        if self.init_deadline_ms == Some(0) {
            return Err(bad("init_deadline_ms must be non-zero"));
        }
        if self.request_deadline_ms == Some(0) {
            return Err(bad("request_deadline_ms must be non-zero"));
        }
        if self.max_memory_bytes == Some(0) {
            return Err(bad("max_memory_bytes must be non-zero"));
        }
        if self.pool_size == Some(0) {
            return Err(bad("pool_size must be non-zero"));
        }
        if self.max_requests_per_instance == Some(0) {
            return Err(bad("max_requests_per_instance must be non-zero"));
        }
        // The untrusted lifecycle is fresh-per-request (ADR 000012): the host would silently
        // ignore a pool knob there, so treat it as the config typo it is — fail closed.
        if self.isolation == IsolationKind::Untrusted
            && (self.pool_size.is_some()
                || self.checkout_timeout_ms.is_some()
                || self.max_requests_per_instance.is_some())
        {
            return Err(bad(
                "pool_size / checkout_timeout_ms / max_requests_per_instance apply only to isolation = \"trusted\"",
            ));
        }
        if let Some(rl) = self.ratelimit {
            if rl.capacity == 0 {
                return Err(bad("ratelimit.capacity must be non-zero"));
            }
            // refill_tokens == 0 is a valid one-shot (no-refill) bucket; but a positive refill with
            // a zero interval can never advance — reject that typo.
            if rl.refill_tokens > 0 && rl.refill_interval_ms == 0 {
                return Err(bad(
                    "ratelimit.refill_interval_ms must be non-zero when refill_tokens > 0",
                ));
            }
        }
        if let Some(ob) = &self.outbound_http {
            self.validate_outbound_http(ob)?;
        }
        if let Some(ob) = &self.outbound_tcp {
            self.validate_outbound_tcp(ob)?;
        }
        if self.wasi == super::WasiKind::Minimal {
            self.validate_wasi_minimal()?;
        }
        Ok(())
    }

    /// Validate a `wasi = "minimal"` declaration (ADR 000063): without the `fat-guest` build the
    /// host cannot provide the grant, so it is rejected (fail-closed) — same rule as
    /// `outbound_http`/`outbound_tcp`. Unlike those, there is no sub-config to check once the
    /// build has the capability: the grant is fixed, so presence alone is valid.
    #[cfg(not(feature = "fat-guest"))]
    fn validate_wasi_minimal(&self) -> Result<(), ControlError> {
        Err(ControlError::InvalidFilterConfig {
            id: self.id.clone(),
            reason: "wasi = \"minimal\" requested but this build lacks the `fat-guest` feature"
                .to_string(),
        })
    }

    #[cfg(feature = "fat-guest")]
    fn validate_wasi_minimal(&self) -> Result<(), ControlError> {
        Ok(())
    }

    /// Validate an outbound_http section. Without the `outbound-http` build the host cannot provide
    /// the capability, so any declared outbound_http is rejected (fail-closed). With it,
    /// `allow_private` CIDRs must parse and any explicit metering value must be non-zero.
    /// An empty `allow` is permitted: it links `wasi:http` (needed by wasip2 guests that import
    /// the interface even when unused, e.g. filter-jwt static PEM path) while remaining
    /// deny-by-default — no destination is reachable.
    #[cfg(not(feature = "outbound-http"))]
    fn validate_outbound_http(&self, _ob: &OutboundHttpConfig) -> Result<(), ControlError> {
        Err(ControlError::InvalidFilterConfig {
            id: self.id.clone(),
            reason: "outbound_http requested but this build lacks the `outbound-http` feature"
                .to_string(),
        })
    }

    #[cfg(feature = "outbound-http")]
    fn validate_outbound_http(&self, ob: &OutboundHttpConfig) -> Result<(), ControlError> {
        let bad = |reason: String| ControlError::InvalidFilterConfig {
            id: self.id.clone(),
            reason,
        };
        for dest in &ob.allow {
            if dest.host.trim().is_empty() {
                return Err(bad("outbound_http.allow entry has an empty host".into()));
            }
            if dest.port == Some(0) {
                return Err(bad(format!(
                    "outbound_http.allow host {} has port 0",
                    dest.host
                )));
            }
        }
        for cidr in &ob.allow_private {
            cidr.parse::<ipnet::IpNet>().map_err(|e| {
                bad(format!(
                    "outbound_http.allow_private has invalid CIDR {cidr:?}: {e}"
                ))
            })?;
        }
        if ob.connect_timeout_ms == Some(0) {
            return Err(bad(
                "outbound_http.connect_timeout_ms must be non-zero".into()
            ));
        }
        if ob.total_timeout_ms == Some(0) {
            return Err(bad("outbound_http.total_timeout_ms must be non-zero".into()));
        }
        if ob.max_response_bytes == Some(0) {
            return Err(bad(
                "outbound_http.max_response_bytes must be non-zero".into()
            ));
        }
        if ob.max_concurrent == Some(0) {
            return Err(bad("outbound_http.max_concurrent must be non-zero".into()));
        }
        Ok(())
    }

    /// Validate an outbound_tcp section (ADR 000060) — the same fail-closed rule as outbound_http:
    /// a build without the `outbound-tcp` feature cannot lend `wasi:sockets`, so a declared section
    /// is rejected outright rather than silently ignored.
    #[cfg(not(feature = "outbound-tcp"))]
    fn validate_outbound_tcp(&self, _ob: &OutboundTcpConfig) -> Result<(), ControlError> {
        Err(ControlError::InvalidFilterConfig {
            id: self.id.clone(),
            reason: "outbound_tcp requested but this build lacks the `outbound-tcp` feature"
                .to_string(),
        })
    }

    #[cfg(feature = "outbound-tcp")]
    fn validate_outbound_tcp(&self, ob: &OutboundTcpConfig) -> Result<(), ControlError> {
        let bad = |reason: String| ControlError::InvalidFilterConfig {
            id: self.id.clone(),
            reason,
        };
        if ob.allow.is_empty() {
            return Err(bad(
                "outbound_tcp.allow must list at least one destination".into()
            ));
        }
        for dest in &ob.allow {
            if dest.host.trim().is_empty() {
                return Err(bad("outbound_tcp.allow entry has an empty host".into()));
            }
            if dest.port == 0 {
                return Err(bad(format!(
                    "outbound_tcp.allow host {} has port 0",
                    dest.host
                )));
            }
        }
        for cidr in &ob.allow_private {
            cidr.parse::<ipnet::IpNet>().map_err(|e| {
                bad(format!(
                    "outbound_tcp.allow_private has invalid CIDR {cidr:?}: {e}"
                ))
            })?;
        }
        if ob.max_connections == Some(0) {
            return Err(bad("outbound_tcp.max_connections must be non-zero".into()));
        }
        if ob.io_deadline_ms == Some(0) {
            return Err(bad("outbound_tcp.io_deadline_ms must be non-zero".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{IsolationKind, RateLimitConfig, WasiKind};

    #[test]
    fn is_prime_is_correct() {
        for p in [2u32, 3, 5, 97, 1009, 65537, 5_000_011] {
            assert!(is_prime(p), "{p} is prime");
        }
        for c in [0u32, 1, 4, 9, 100, 1000, 65536] {
            assert!(!is_prime(c), "{c} is not prime");
        }
    }

    #[test]
    fn invalid_filter_metering_is_rejected() {
        // out-of-range metering / rate-limit values are rejected fail-closed at build.
        let base = FilterEntry {
            id: "x".to_string(),
            source: "s".to_string(),
            digest: "sha256:abc".to_string(),
            isolation: IsolationKind::Untrusted,
            init_deadline_ms: None,
            request_deadline_ms: None,
            max_memory_bytes: None,
            pool_size: None,
            checkout_timeout_ms: None,
            max_requests_per_instance: None,
            ratelimit: None,
            outbound_http: None,
            outbound_tcp: None,
            wasi: WasiKind::None,
            config: None,
        };
        assert!(base.validate().is_ok(), "defaults are valid");

        assert!(
            FilterEntry {
                request_deadline_ms: Some(0),
                ..base.clone()
            }
            .validate()
            .is_err(),
            "a zero request deadline is rejected"
        );
        assert!(
            FilterEntry {
                max_memory_bytes: Some(0),
                ..base.clone()
            }
            .validate()
            .is_err(),
            "a zero memory cap is rejected"
        );
        assert!(
            FilterEntry {
                ratelimit: Some(RateLimitConfig {
                    capacity: 10,
                    refill_tokens: 1,
                    refill_interval_ms: 0,
                }),
                ..base.clone()
            }
            .validate()
            .is_err(),
            "a refilling bucket with a zero interval is rejected"
        );
        assert!(
            FilterEntry {
                ratelimit: Some(RateLimitConfig {
                    capacity: 10,
                    refill_tokens: 0,
                    refill_interval_ms: 0,
                }),
                ..base.clone()
            }
            .validate()
            .is_ok(),
            "a one-shot (no-refill) bucket is valid"
        );
    }

    #[test]
    fn pool_knobs_are_trusted_only_and_non_zero() {
        let trusted = FilterEntry {
            id: "x".to_string(),
            source: "s".to_string(),
            digest: "sha256:abc".to_string(),
            isolation: IsolationKind::Trusted,
            init_deadline_ms: None,
            request_deadline_ms: None,
            max_memory_bytes: None,
            pool_size: None,
            checkout_timeout_ms: None,
            max_requests_per_instance: None,
            ratelimit: None,
            outbound_http: None,
            outbound_tcp: None,
            wasi: WasiKind::None,
            config: None,
        };
        assert!(
            FilterEntry {
                pool_size: Some(4),
                checkout_timeout_ms: Some(0),
                max_requests_per_instance: Some(1),
                ..trusted.clone()
            }
            .validate()
            .is_ok(),
            "trusted pool knobs are valid (checkout 0 = fail immediately at saturation)"
        );
        assert!(
            FilterEntry {
                pool_size: Some(0),
                ..trusted.clone()
            }
            .validate()
            .is_err(),
            "a zero pool size can never serve a checkout — a typo, rejected"
        );
        assert!(
            FilterEntry {
                max_requests_per_instance: Some(0),
                ..trusted.clone()
            }
            .validate()
            .is_err(),
            "recycling after zero requests is degenerate — rejected"
        );
        // The untrusted lifecycle is fresh-per-request (ADR 000012): a pool knob there is a
        // config typo the host would silently ignore — fail closed instead.
        for knob in [
            FilterEntry {
                pool_size: Some(4),
                isolation: IsolationKind::Untrusted,
                ..trusted.clone()
            },
            FilterEntry {
                checkout_timeout_ms: Some(100),
                isolation: IsolationKind::Untrusted,
                ..trusted.clone()
            },
            FilterEntry {
                max_requests_per_instance: Some(10),
                isolation: IsolationKind::Untrusted,
                ..trusted.clone()
            },
        ] {
            assert!(
                knob.validate().is_err(),
                "a pool knob under isolation = untrusted is rejected"
            );
        }
    }

    #[test]
    fn invalid_state_config_is_rejected() {
        // redb without a path (nowhere to persist) and memory with one (the operator likely
        // meant redb) are both fail-closed at build — a half-set [state] must never silently
        // run on memory.
        assert!(
            State {
                backend: StateBackendKind::Redb,
                path: Some("s.redb".into()),
            }
            .validate()
            .is_ok()
        );
        assert!(
            State::default().validate().is_ok(),
            "absent [state] is valid (memory)"
        );

        assert!(
            State {
                backend: StateBackendKind::Redb,
                path: None,
            }
            .validate()
            .is_err(),
            "redb without a path is rejected"
        );
        assert!(
            State {
                backend: StateBackendKind::Redb,
                path: Some("  ".into()),
            }
            .validate()
            .is_err(),
            "redb with a blank path is rejected"
        );
        assert!(
            State {
                backend: StateBackendKind::Memory,
                path: Some("s.redb".into()),
            }
            .validate()
            .is_err(),
            "memory with a path is rejected (the operator likely meant redb)"
        );
    }
}
