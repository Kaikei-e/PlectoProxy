//! Upstream instances, active-health-check state, and round-robin load balancing (ADR 000017).
//!
//! A manifest [`crate::manifest::Upstream`] becomes an [`UpstreamGroup`] of [`UpstreamInstance`]s.
//! Each instance owns a single health state machine fed by BOTH sources: the background
//! active-health prober (the fast-path server runs it) and passive signals from real forwarded
//! requests (a connect failure demotes). The fast path picks a healthy instance per request by
//! round-robin; when none are healthy the upstream is fail-closed (the server responds 503).
//!
//! **The registry lives on `Control`, OUTSIDE the atomically-swapped `ActiveConfig`**, so health
//! state SURVIVES a reload (ADR 000017). [`UpstreamRegistry::reconcile`] diffs the manifest's
//! upstreams against the running set by `(name, address)`: an unchanged instance keeps its health,
//! a new address starts pessimistic (unhealthy), a removed one is dropped. Routing's
//! `CompiledRoute` holds an `Arc<UpstreamGroup>` rebuilt to point at the reconciled group on every
//! reload, so the per-request hot path never touches the registry lock.
//!
//! Split by concern: `instance` (per-instance health FSM), `lb` (pick algorithms — round-robin /
//! least-request / maglev — plus `Pick` / `HashInput`), `circuit_breaker` (the per-upstream
//! in-flight cap), `outlier` (outlier detection), `registry` (`UpstreamRegistry` / `reconcile`).
//! `UpstreamGroup` itself stays one struct (its fields are genuinely one cohesive unit of
//! per-upstream state); only its `impl` block is split across those files.

mod circuit_breaker;
mod instance;
mod lb;
mod outlier;
mod registry;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use instance::UpstreamInstance;
pub use lb::{HashInput, HashKeySource, Pick};
pub use registry::UpstreamRegistry;

use crate::maglev::MaglevTable;
use crate::manifest::{HealthConfig, LbAlgorithm};
use lb::LbState;

/// Wall-clock milliseconds since the epoch, for outlier-ejection windows (ADR 000032) and as the
/// clock `lb`'s eligibility check reads. Non-monotonic, but the windows are coarse (seconds), so a
/// backward clock step merely shortens or lengthens one window — never a panic on untrusted input.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The swappable endpoint set of a group: the instances plus the LB state compiled from them
/// (a Maglev table indexes into `instances`, so the two must swap together). Behind an `ArcSwap`
/// on the group so periodic DNS re-resolution (the standard periodic-DNS endpoint-discovery
/// technique — the shape of nginx `resolve` / Envoy STRICT_DNS) can replace the set in place
/// while routes keep holding their `Arc<UpstreamGroup>`.
#[derive(Debug)]
pub struct Endpoints {
    /// The instances, in configured (or resolved) address order.
    pub instances: Vec<Arc<UpstreamInstance>>,
    pub(in crate::upstream) lb: LbState,
}

impl Endpoints {
    /// Compile the LB state for `instances` (ADR 000035) and wrap the pair. A Maglev upstream
    /// recomputes its lookup table from the (possibly re-resolved) instance set.
    pub(super) fn build(
        instances: Vec<Arc<UpstreamInstance>>,
        algorithm: LbAlgorithm,
        maglev_table_size: usize,
    ) -> Self {
        let lb = match algorithm {
            LbAlgorithm::RoundRobin => LbState::RoundRobin,
            LbAlgorithm::LeastRequest => LbState::LeastRequest,
            LbAlgorithm::Maglev => {
                let entries: Vec<(&str, u32)> = instances
                    .iter()
                    .map(|i| (i.address(), i.weight()))
                    .collect();
                LbState::Maglev(MaglevTable::build(&entries, maglev_table_size))
            }
        };
        Self { instances, lb }
    }
}

/// A named upstream: its endpoint set, the round-robin cursor, and the health policy (ADR 000017).
#[derive(Debug)]
pub struct UpstreamGroup {
    /// The upstream `name` routes refer to.
    pub name: String,
    /// The active-health-check policy (the prober reads `path` / `interval_ms` / `timeout_ms`).
    pub health: HealthConfig,
    /// The current endpoint set + its compiled LB state. Swapped atomically by DNS re-resolution
    /// (`update_endpoints`); otherwise fixed for the life of this group value — a reload builds a
    /// NEW group, reusing unchanged instances' `Arc`s to preserve their health.
    endpoints: arc_swap::ArcSwap<Endpoints>,
    /// The manifest-declared `(address, weight)` list — the re-resolution input (each hostname is
    /// re-expanded to its current A/AAAA records; an IP literal passes through unchanged).
    configured: Vec<(String, u32)>,
    /// How often hostname addresses are re-resolved (`resolve_interval_ms`); `ZERO` = never (the
    /// default — hostnames still resolve per connect, but the endpoint set stays as configured).
    resolve_interval: Duration,
    /// The LB algorithm + Maglev table size, retained so `update_endpoints` can recompile the LB
    /// state for a re-resolved instance set.
    lb_algorithm: LbAlgorithm,
    maglev_table_size: usize,
    /// Per-try timeout for ONE forward attempt to this upstream (ADR 000019, reframed as the per-try
    /// bound by ADR 000031); `Duration::ZERO` disables it. Bounds one attempt's time-to-response-
    /// headers, failing closed 504 on overrun. Not part of `health`, so a timeout-only change
    /// rebuilds the group but preserves instance health.
    request_timeout: Duration,
    /// Overall request deadline across the WHOLE transaction — every attempt PLUS the backoff between
    /// them (ADR 000031); `Duration::ZERO` = no overall bound (only the per-try `request_timeout`
    /// applies). The runtime applies the tighter of the two; exceeding it fails closed 504.
    overall_timeout: Duration,
    /// Max retries to a DIFFERENT instance after a retryable forward failure (ADR 000023); `0`
    /// disables retry. Like `request_timeout`, not part of `health`, so a retry-only change rebuilds
    /// the group but preserves instance health.
    max_retries: u64,
    /// Round-robin cursor. `Relaxed` suffices: it only needs to advance, not synchronise memory.
    rr: AtomicUsize,
    /// Circuit-breaker cap (ADR 000028): max concurrent in-flight requests to this upstream; `0` =
    /// unlimited. Rebuilt from the manifest on every reconcile, like `request_timeout`/`max_retries`,
    /// so it is not part of `health` and a breaker-only change preserves instance health.
    max_requests: usize,
    /// Current concurrent in-flight requests (ADR 000028) — held by a [`circuit_breaker::RequestPermit`]
    /// from forward time until the upstream response headers arrive (or it fails). A (re)built group
    /// starts at 0; in-flight requests of a superseded group decrement that group's own counter via
    /// their permit, so a reload never miscounts.
    in_flight: AtomicUsize,
    /// Outlier-detection policy (ADR 000032), rebuilt from the manifest like the other non-health
    /// knobs (so an outlier-config change preserves instance health): the consecutive gateway-5xx
    /// threshold (`0` = disabled), the base ejection window (× exponential backoff), and the cap on
    /// the fraction of the pool ejectable at once.
    outlier_consecutive: u32,
    outlier_base_ejection: Duration,
    outlier_max_ejection_percent: u32,
    /// The request attribute a `Maglev` upstream hashes for affinity (ADR 000035); `None` for the
    /// other algorithms. The fast path reads this to project the hash key from a request.
    hash_key: Option<HashKeySource>,
    /// The `[upstream.tls]` section this group was reconciled from (ADR 000042), kept for the
    /// reuse comparison on reload: an unchanged section reuses `tls_client` (stable `Arc`, so the
    /// fast path's per-config connection pool survives the reload), a changed one rebuilds it.
    tls_manifest: Option<crate::manifest::UpstreamTls>,
    /// The rustls client config the fast path re-encrypts this upstream's forward leg with
    /// (ADR 000042): roots per `ca_path` (or webpki), ALPN `[h2, http/1.1]`. `None` = plain
    /// HTTP/1.1. Built fail-closed at reconcile, like the server-side TLS configs.
    tls_client: Option<Arc<rustls::ClientConfig>>,
}

impl UpstreamGroup {
    /// The PER-TRY timeout the fast path applies to one forward attempt (ADR 000019, per-try by ADR
    /// 000031). `Duration::ZERO` means no per-try bound (e.g. a streaming / long-poll backend);
    /// otherwise one attempt is bounded and overrun fails closed 504.
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// The OVERALL request deadline across all attempts + backoff (ADR 000031); `Duration::ZERO`
    /// means no overall bound (only the per-try `request_timeout` applies). Exceeding it fails
    /// closed 504 `request-timeout` with no further retry.
    pub fn overall_timeout(&self) -> Duration {
        self.overall_timeout
    }

    /// The max number of retries to a different instance on a retryable forward failure (ADR
    /// 000023); `0` disables retry.
    pub fn max_retries(&self) -> u64 {
        self.max_retries
    }

    /// The hash-key source for a `maglev` upstream (ADR 000035), or `None` for the other algorithms.
    /// The fast path reads this to project a [`HashInput`] from the request.
    pub fn hash_key_source(&self) -> Option<&HashKeySource> {
        self.hash_key.as_ref()
    }

    /// The scheme the fast path forwards (and health-probes) this upstream with (ADR 000042):
    /// `https` when `[upstream.tls]` is declared, else `http`.
    pub fn scheme(&self) -> &'static str {
        if self.tls_client.is_some() {
            "https"
        } else {
            "http"
        }
    }

    /// The rustls client config for this upstream's TLS forward leg (ADR 000042), or `None` for
    /// plain HTTP/1.1. The `Arc` is stable across reloads while `[upstream.tls]` is unchanged, so
    /// the fast path can key its per-config connection pool on the `Arc`'s identity.
    pub fn tls_client_config(&self) -> Option<&Arc<rustls::ClientConfig>> {
        self.tls_client.as_ref()
    }

    /// A snapshot of the current endpoint set (instances + LB state). One atomic load; the
    /// returned `Arc` stays valid across a concurrent re-resolution swap.
    pub fn endpoints(&self) -> Arc<Endpoints> {
        self.endpoints.load_full()
    }

    /// How often the fast path should re-resolve this group's hostname addresses, or `None` when
    /// re-resolution is off (the default).
    pub fn resolve_interval(&self) -> Option<Duration> {
        (!self.resolve_interval.is_zero()).then_some(self.resolve_interval)
    }

    /// The manifest-declared `(address, weight)` list — the re-resolution input.
    pub fn configured_addresses(&self) -> &[(String, u32)] {
        &self.configured
    }

    /// Replace the endpoint set with `resolved` `(address, weight)` pairs — the periodic-DNS
    /// endpoint-discovery swap. An unchanged pair keeps its instance `Arc` (health and in-flight
    /// state survive, exactly like a reload's reconcile); a new pair starts pessimistic (ADR
    /// 000017 — a fresh address must prove itself before entering rotation); a vanished pair is
    /// dropped (in-flight requests finish on their cloned `Arc`). The LB state is recompiled (a
    /// Maglev table indexes the new set). Returns `false` (and swaps nothing) when the set is
    /// unchanged, so an idle refresh tick costs one atomic load and a compare.
    pub fn update_endpoints(&self, resolved: &[(String, u32)]) -> bool {
        let current = self.endpoints.load();
        let unchanged = current.instances.len() == resolved.len()
            && current
                .instances
                .iter()
                .zip(resolved)
                .all(|(inst, (addr, weight))| inst.address() == addr && inst.weight() == *weight);
        if unchanged {
            return false;
        }
        let instances: Vec<Arc<UpstreamInstance>> = resolved
            .iter()
            .map(|(addr, weight)| {
                current
                    .instances
                    .iter()
                    .find(|i| i.address() == addr && i.weight() == *weight)
                    .cloned()
                    .unwrap_or_else(|| {
                        Arc::new(UpstreamInstance::new(addr.clone(), *weight, &self.health))
                    })
            })
            .collect();
        self.endpoints.store(Arc::new(Endpoints::build(
            instances,
            self.lb_algorithm,
            self.maglev_table_size,
        )));
        true
    }
}

impl crate::Control {
    /// A snapshot of the current upstream groups (ADR 000017), for the fast-path server's
    /// health-check supervisor to probe. Reflects the latest reconcile, so a reload's added /
    /// removed instances are picked up on the supervisor's next tick without restarting it.
    pub fn upstream_groups(&self) -> Vec<Arc<UpstreamGroup>> {
        self.upstreams.groups()
    }
}
