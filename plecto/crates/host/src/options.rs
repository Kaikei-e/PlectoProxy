//! Load-time options: [`Isolation`] (the instance-lifecycle choice) and [`LoadOptions`] (the
//! full knob set for `Host::load`), plus their defaults.

#[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
use std::time::Duration;

use crate::Bucket;
#[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
use crate::outbound;

/// How a filter is instantiated and isolated (ADR 000004 / 000011). Not a "trust score":
/// it selects the **instance lifecycle**, mirroring how Fastly/Spin model per-request vs
/// reusable sandboxes. *Who* is trusted is decided elsewhere (OCI signing, ADR 000006);
/// this only says which lifecycle a loaded filter gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// Own filters: a pool of reusable instances, `init` once per instance, checked out per
    /// request (ADR 000012). No per-request zeroization (same trust domain). Statelessness
    /// (Fork 4) is therefore honored by *trust*, not *enforced*: a trusted filter that stashes
    /// mutable state in its own linear memory silently carries it across requests on a reused
    /// instance — and, with a pool, *which* instance a request lands on becomes observable
    /// (§6.6 footgun). That is not a security boundary (same trust domain); periodic recycling
    /// (`max_requests_per_instance`) bounds the accumulation, but only `Untrusted`'s
    /// fresh-per-request memory enforces statelessness structurally (ADR 000011).
    Trusted,
    /// Third-party filters: fresh instance per request, memory fresh by construction.
    Untrusted,
}

/// Generous default budget for the heavy once-per-instance `init` of a **trusted** filter
/// (Tenet 4): regex compile, schema build, config parse. Trusted init runs once per instance
/// and is then reused, so a large budget is paid once — separate from, and much larger than,
/// the per-request budget so a legitimately heavy init is not mistaken for a runaway (ADR 000006).
const DEFAULT_INIT_DEADLINE_MS: u64 = 5_000;
/// Tight default `init` budget for an **untrusted** filter. Untrusted filters instantiate fresh
/// and re-run `init` on EVERY request (the isolation trade, ADR 000011), on the worker thread, so
/// init is on the hot path: the generous 5s trusted budget would let an adversarial untrusted
/// `init` busy-loop and pin a core for ~5s per request (CWE-770). Bound it near the
/// per-request budget; an operator may still raise it per filter via the manifest.
const DEFAULT_UNTRUSTED_INIT_DEADLINE_MS: u64 = 250;
/// Tight default budget for the hot per-request hooks. This is a *safety* bound that traps
/// runaway filters (infinite loops), not a latency SLA; header-only filters finish in well
/// under a millisecond.
const DEFAULT_REQUEST_DEADLINE_MS: u64 = 100;

/// Default per-instance linear-memory cap enforced via a `StoreLimits` (ADR 000006). Matches
/// the pooling engine's per-slot reservation so trusted and untrusted agree.
pub(crate) const DEFAULT_MAX_MEMORY_BYTES: u64 = 64 << 20;

/// Bounded wait (ms) for a free trusted instance before a checkout fails closed (ADR 000012).
/// wasmtime's pooling allocator has no internal queue and the official guidance is for the
/// embedder to apply its own backpressure; this is that wait. Kept short — orders of magnitude
/// below a connection pool's seconds-long default — because on a gateway hot path it is better
/// to shed load (`Unavailable`) than to queue unboundedly. M2 ties this to the real SLO.
const DEFAULT_CHECKOUT_TIMEOUT_MS: u64 = 250;
/// Recycle (discard + rebuild) a trusted instance after it has served this many requests
/// (ADR 000012 / §6.6). Generous so steady-state reuse dominates (init-once still effectively
/// holds), while still bounding accidental linear-memory state accumulation over an instance's
/// life. Following Fastly's reusable-sandbox `max-requests`.
const DEFAULT_MAX_REQUESTS_PER_INSTANCE: u64 = 1 << 16;
/// Default ceiling for the auto-sized trusted pool (`available_parallelism`, clamped here).
/// Modest so a multi-filter manifest does not, by default, multiply out past the engine's
/// global pooling budget before the manifest registry (ADR 000007) can apportion it.
const TRUSTED_POOL_DEFAULT_CEIL: usize = 8;

/// Auto-sized default trusted pool capacity: worker-scale (foundation plan §6.3), approximated
/// by `available_parallelism` until the fast-path server brings real worker threads (M2). A
/// single-threaded caller still only ever builds one instance (lazy fill), so this does not
/// change the init-once behaviour observed serially.
fn default_trusted_pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, TRUSTED_POOL_DEFAULT_CEIL)
}

// --- outbound HTTP (ADR 000036) clamps. Operator- and guest-supplied timings/sizes are bounded to
// --- these host maxima so a filter cannot claim an unboundedly long or large outbound call. ---
/// Default TCP connect timeout for an outbound call.
#[cfg(feature = "outbound-http")]
const DEFAULT_OUTBOUND_CONNECT_TIMEOUT_MS: u64 = 2_000;
/// Host ceiling on the connect timeout.
#[cfg(feature = "outbound-http")]
const MAX_OUTBOUND_CONNECT_TIMEOUT_MS: u64 = 10_000;
/// Default wall-clock ceiling for the whole outbound call (connect + request + response). This is
/// the host-side I/O deadline epoch interruption cannot provide (ADR 000006 / 000036).
#[cfg(feature = "outbound-http")]
const DEFAULT_OUTBOUND_TOTAL_TIMEOUT_MS: u64 = 5_000;
/// Host ceiling on the total outbound timeout.
#[cfg(feature = "outbound-http")]
const MAX_OUTBOUND_TOTAL_TIMEOUT_MS: u64 = 30_000;
/// Default cap on the response body the host buffers back to the guest.
#[cfg(feature = "outbound-http")]
const DEFAULT_OUTBOUND_MAX_RESPONSE_BYTES: u64 = 64 * 1024;
/// Host ceiling on the response-body cap.
#[cfg(feature = "outbound-http")]
const MAX_OUTBOUND_MAX_RESPONSE_BYTES: u64 = 1 << 20;
/// Default cap on concurrent in-flight outbound calls per filter.
#[cfg(feature = "outbound-http")]
const DEFAULT_OUTBOUND_MAX_CONCURRENT: u32 = 8;
/// Host ceiling on per-filter outbound concurrency.
#[cfg(feature = "outbound-http")]
const MAX_OUTBOUND_MAX_CONCURRENT: u32 = 64;

// --- outbound TCP (ADR 000060) clamps. ---
/// Default per-request budget of TCP connects. Small: the reference use (a Redis consult) needs
/// one connection, kept across requests on a pooled instance; per-request fan-out is the thing
/// being bounded.
#[cfg(feature = "outbound-tcp")]
const DEFAULT_OUTBOUND_TCP_MAX_CONNECTIONS: u32 = 4;
/// Host ceiling on the per-request connect budget.
#[cfg(feature = "outbound-tcp")]
const MAX_OUTBOUND_TCP_MAX_CONNECTIONS: u32 = 64;
/// Default wall-clock ceiling on each guest hook call of an outbound-TCP filter. With raw TCP the
/// host cannot see request boundaries inside the stream, so the deadline bounds the whole call —
/// the role `total_timeout` plays for outbound HTTP (connect hangs AND read hangs, ADR 000060).
#[cfg(feature = "outbound-tcp")]
const DEFAULT_OUTBOUND_TCP_IO_DEADLINE_MS: u64 = 5_000;
/// Host ceiling on the outbound-TCP hook-call deadline.
#[cfg(feature = "outbound-tcp")]
const MAX_OUTBOUND_TCP_IO_DEADLINE_MS: u64 = 30_000;

/// Options for `Host::load`. A struct (not a bare arg) because deny-by-default grows more
/// load-time knobs onto it. Defaults to the safe side: `Untrusted` (fail-closed) with
/// metering on (ADR 000006). A future declarative manifest (ADR 000007) injects these.
///
/// Not `Copy`: the outbound policy (ADR 000036) carries an allowlist `Vec`, so this moves/clones.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub isolation: Isolation,
    /// Epoch deadline (ms) for the once-per-instance `init` export.
    pub init_deadline_ms: u64,
    /// Epoch deadline (ms) for each per-request hook (`on-request` / `on-response`).
    pub request_deadline_ms: u64,
    /// Per-instance linear-memory cap (bytes), enforced by a `StoreLimits`.
    pub max_memory_bytes: u64,
    /// Trusted pool: maximum concurrent reusable instances (lazily filled, ADR 000012).
    /// Clamped to `[1, TRUSTED_POOL_MAX]` at load. Ignored for `Untrusted` (fresh-per-request).
    pub trusted_pool_size: usize,
    /// Trusted pool: bounded wait (ms) for a free instance under saturation before failing
    /// closed (`RunError::Unavailable`). Ignored for `Untrusted`.
    pub checkout_timeout_ms: u64,
    /// Trusted pool: recycle an instance (discard + rebuild) after this many requests, bounding
    /// linear-memory state accumulation (§6.6). Ignored for `Untrusted`.
    pub max_requests_per_instance: u64,
    /// This filter's host-side token-bucket spec for `host-ratelimit` (manifest
    /// `[filter.ratelimit]`, ADR 000026). `None` = the filter has no limiter (its `try-acquire`
    /// fails closed). Host-configured so an untrusted filter cannot override its own limit.
    pub ratelimit_bucket: Option<Bucket>,
    /// This filter's outbound HTTP policy (manifest `[filter.outbound_http]`, ADR 000036): the
    /// deny-by-default allowlist + SSRF opt-in + resource bounds enforced at the `wasi:http`
    /// send seam. `None` = the filter is lent no outbound HTTP capability (the default).
    #[cfg(feature = "outbound-http")]
    pub outbound_http: Option<outbound::OutboundPolicy>,
    /// This filter's outbound TCP policy (manifest `[filter.outbound_tcp]`, ADR 000060): the
    /// deny-by-default allowlist + SSRF opt-in + resource bounds enforced at the host's
    /// ip-name-lookup and connect seams. `None` = the filter is lent no outbound TCP capability
    /// (the default).
    #[cfg(feature = "outbound-tcp")]
    pub outbound_tcp: Option<outbound::OutboundTcpPolicy>,
    /// Test-only DNS override for the outbound TCP capability: `Some(map)` replaces the system
    /// resolver so the feature-gated E2E suite can point an allowlisted NAME at a controlled
    /// address deterministically. NOT production provenance — gated behind `test-support`.
    #[doc(hidden)]
    #[cfg(all(feature = "outbound-tcp", feature = "test-support"))]
    pub outbound_tcp_static_resolver:
        Option<std::collections::HashMap<String, Vec<std::net::IpAddr>>>,
    /// This filter's business config (manifest `[filter.config]`, ADR 000066): an arbitrary
    /// string→string map read back via `host-config::get`. The host never interprets keys or
    /// values. Empty (the default) when the manifest declares no `[filter.config]` section — every
    /// `get` then reads `None`, same as an undeclared key in a non-empty map.
    pub config: std::collections::BTreeMap<String, String>,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            isolation: Isolation::Untrusted,
            // default is untrusted → init re-runs per request, so bound it tight.
            init_deadline_ms: DEFAULT_UNTRUSTED_INIT_DEADLINE_MS,
            request_deadline_ms: DEFAULT_REQUEST_DEADLINE_MS,
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            trusted_pool_size: default_trusted_pool_size(),
            checkout_timeout_ms: DEFAULT_CHECKOUT_TIMEOUT_MS,
            max_requests_per_instance: DEFAULT_MAX_REQUESTS_PER_INSTANCE,
            ratelimit_bucket: None,
            #[cfg(feature = "outbound-http")]
            outbound_http: None,
            #[cfg(feature = "outbound-tcp")]
            outbound_tcp: None,
            #[cfg(all(feature = "outbound-tcp", feature = "test-support"))]
            outbound_tcp_static_resolver: None,
            config: std::collections::BTreeMap::new(),
        }
    }
}

impl LoadOptions {
    pub fn trusted() -> Self {
        Self {
            isolation: Isolation::Trusted,
            // trusted init runs ONCE per instance and is reused → keep the generous budget.
            init_deadline_ms: DEFAULT_INIT_DEADLINE_MS,
            ..Self::default()
        }
    }
    pub fn untrusted() -> Self {
        Self::default()
    }
    /// Override the per-request hook deadline (ms).
    pub fn with_request_deadline_ms(mut self, ms: u64) -> Self {
        self.request_deadline_ms = ms;
        self
    }
    /// Override the `init` deadline (ms).
    pub fn with_init_deadline_ms(mut self, ms: u64) -> Self {
        self.init_deadline_ms = ms;
        self
    }
    /// Override the per-instance linear-memory cap (bytes).
    pub fn with_max_memory_bytes(mut self, bytes: u64) -> Self {
        self.max_memory_bytes = bytes;
        self
    }
    /// Override the trusted pool capacity (max concurrent reusable instances).
    pub fn with_trusted_pool_size(mut self, n: usize) -> Self {
        self.trusted_pool_size = n;
        self
    }
    /// Override the bounded checkout wait (ms) before a saturated trusted pool fails closed.
    pub fn with_checkout_timeout_ms(mut self, ms: u64) -> Self {
        self.checkout_timeout_ms = ms;
        self
    }
    /// Override how many requests a trusted instance serves before it is recycled.
    pub fn with_max_requests_per_instance(mut self, n: u64) -> Self {
        self.max_requests_per_instance = n;
        self
    }
    /// Configure this filter's host-side `host-ratelimit` token bucket (ADR 000026). Without it,
    /// the filter's `try-acquire` fails closed. The filter cannot supply or override these.
    pub fn with_ratelimit_bucket(
        mut self,
        capacity: u64,
        refill_tokens: u64,
        refill_interval_ms: u64,
    ) -> Self {
        self.ratelimit_bucket = Some(Bucket {
            capacity,
            refill_tokens,
            refill_interval_ms,
        });
        self
    }

    /// Lend this filter's manifest-declared business config (`[filter.config]`, ADR 000066) —
    /// a read-only string map the filter reads back via `host-config::get`.
    pub fn with_config(mut self, config: std::collections::BTreeMap<String, String>) -> Self {
        self.config = config;
        self
    }

    /// Lend this filter the outbound HTTP capability (ADR 000036) with an already-parsed
    /// allowlist and private-range opt-in. Timings and sizes are clamped to host maxima here —
    /// operator-supplied values cannot exceed the host ceiling, and guest-supplied request
    /// options are clamped again at the send seam. The filter cannot supply or widen any of this.
    #[cfg(feature = "outbound-http")]
    #[allow(clippy::too_many_arguments)]
    pub fn with_outbound_http(
        mut self,
        allow: Vec<outbound::AllowEntry>,
        allow_private: Vec<String>,
        connect_timeout_ms: Option<u64>,
        total_timeout_ms: Option<u64>,
        max_response_bytes: Option<u64>,
        max_concurrent: Option<u32>,
    ) -> Self {
        let clamp_ms = |v: Option<u64>, def: u64, max: u64| v.unwrap_or(def).clamp(1, max);
        // Parse the operator's CIDR strings; a malformed one is dropped, leaving that range blocked
        // (fail-closed). The manifest validates them up front, so this is belt-and-suspenders.
        let allow_private = allow_private
            .iter()
            .filter_map(|c| c.parse::<ipnet::IpNet>().ok())
            .collect();
        self.outbound_http = Some(outbound::OutboundPolicy {
            allow,
            allow_private,
            connect_timeout: Duration::from_millis(clamp_ms(
                connect_timeout_ms,
                DEFAULT_OUTBOUND_CONNECT_TIMEOUT_MS,
                MAX_OUTBOUND_CONNECT_TIMEOUT_MS,
            )),
            total_timeout: Duration::from_millis(clamp_ms(
                total_timeout_ms,
                DEFAULT_OUTBOUND_TOTAL_TIMEOUT_MS,
                MAX_OUTBOUND_TOTAL_TIMEOUT_MS,
            )),
            max_response_bytes: max_response_bytes
                .unwrap_or(DEFAULT_OUTBOUND_MAX_RESPONSE_BYTES)
                .clamp(1, MAX_OUTBOUND_MAX_RESPONSE_BYTES),
            max_concurrent: max_concurrent
                .unwrap_or(DEFAULT_OUTBOUND_MAX_CONCURRENT)
                .clamp(1, MAX_OUTBOUND_MAX_CONCURRENT),
        });
        self
    }

    /// Lend this filter the outbound TCP capability (ADR 000060) with an already-parsed allowlist
    /// and private-range opt-in. The budget and deadline are clamped to host maxima here — the
    /// filter cannot supply or widen any of this (same rule as `with_outbound_http`).
    #[cfg(feature = "outbound-tcp")]
    pub fn with_outbound_tcp(
        mut self,
        allow: Vec<outbound::TcpAllowEntry>,
        allow_private: Vec<String>,
        max_connections: Option<u32>,
        io_deadline_ms: Option<u64>,
    ) -> Self {
        // Parse the operator's CIDR strings; a malformed one is dropped, leaving that range blocked
        // (fail-closed). The manifest validates them up front, so this is belt-and-suspenders.
        let allow_private = allow_private
            .iter()
            .filter_map(|c| c.parse::<ipnet::IpNet>().ok())
            .collect();
        self.outbound_tcp = Some(outbound::OutboundTcpPolicy {
            allow,
            allow_private,
            max_connections: max_connections
                .unwrap_or(DEFAULT_OUTBOUND_TCP_MAX_CONNECTIONS)
                .clamp(1, MAX_OUTBOUND_TCP_MAX_CONNECTIONS),
            io_deadline: Duration::from_millis(
                io_deadline_ms
                    .unwrap_or(DEFAULT_OUTBOUND_TCP_IO_DEADLINE_MS)
                    .clamp(1, MAX_OUTBOUND_TCP_IO_DEADLINE_MS),
            ),
        });
        self
    }

    /// Test-only: resolve outbound-TCP names from a static map instead of real DNS (see
    /// `LoadOptions::outbound_tcp_static_resolver`).
    #[doc(hidden)]
    #[cfg(all(feature = "outbound-tcp", feature = "test-support"))]
    pub fn with_outbound_tcp_static_resolver(
        mut self,
        entries: Vec<(String, Vec<std::net::IpAddr>)>,
    ) -> Self {
        self.outbound_tcp_static_resolver = Some(entries.into_iter().collect());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_init_deadline_is_tight_trusted_is_generous() {
        // untrusted filters re-run init per request, so their default init budget must be
        // bounded near the per-request budget, while a trusted filter (init once) keeps the
        // generous budget.
        assert_eq!(
            LoadOptions::untrusted().init_deadline_ms,
            DEFAULT_UNTRUSTED_INIT_DEADLINE_MS
        );
        assert_eq!(
            LoadOptions::trusted().init_deadline_ms,
            DEFAULT_INIT_DEADLINE_MS
        );
        assert!(
            LoadOptions::untrusted().init_deadline_ms < LoadOptions::trusted().init_deadline_ms
        );
    }
}
