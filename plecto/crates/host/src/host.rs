//! The wasmtime host: [`Host`] wires together the engines, trust policy, KV backend + quota,
//! telemetry sink, and epoch ticker, and loads filter components into a [`LoadedFilter`].

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use wasmtime::Engine;
use wasmtime::component::{Component, HasSelf, Linker};

use crate::contract::{
    ContractVersion, FilterPreV01, FilterPreV02, FilterV01, FilterV02, detect_contract_version,
};
use crate::engine::{Allocation, EpochTicker, TRUSTED_POOL_MAX, build_engine};
use crate::errors::{LoadError, sbom_binds_component};
use crate::filter::LoadedFilter;
use crate::observe;
#[cfg(feature = "outbound-http")]
use crate::outbound_http;
#[cfg(feature = "outbound-tcp")]
use crate::outbound_tcp;
use crate::pool::{LoadedInner, TrustedPool};
use crate::quota::KvQuota;
use crate::runtime::{FilterPreBinding, FilterRuntime, WasmtimeRuntime};
#[cfg(any(
    feature = "outbound-http",
    feature = "outbound-tcp",
    feature = "fat-guest"
))]
use crate::state::add_cli_runtime;
use crate::state::{HostState, KV_NS_DELIM};
use crate::{Isolation, KvBackend, LoadOptions, MemoryBackend, NoopSink, SignedArtifact};
use crate::{TelemetrySink, TrustPolicy};

/// The wasmtime host: two engines (one per isolation mode) plus the shared state backend.
/// One per process/worker.
pub struct Host {
    /// Pooling-allocator engine for trusted, reused-instance filters (init-once).
    trusted_engine: Engine,
    /// On-demand engine for untrusted, fresh-per-request filters (memory fresh by
    /// construction). Allocation strategy is per-Engine and immutable, so the trust split
    /// is two engines, not one (confirmed wasmtime 45 behaviour).
    untrusted_engine: Engine,
    kv: Arc<dyn KvBackend>,
    /// Per-namespace host-state accounting + caps, shared into every loaded filter.
    kv_quota: Arc<KvQuota>,
    /// Public keys this host trusts to sign filters (ADR 000006). Verified at every `load`.
    trust: TrustPolicy,
    /// Where loaded filters emit their per-execution spans (ADR 000009). Default `NoopSink`
    /// (observability off); cloned into each filter at `load`, so set it before loading.
    sink: Arc<dyn TelemetrySink>,
    /// Shared tokio runtime that drives outbound-using filters (ADR 000036 / 000060): their guest
    /// calls block on real socket I/O, which the no-reactor pollster executor cannot service.
    /// Cloned into each outbound filter at `load`; unused by (and invisible to) filters without an
    /// outbound policy.
    #[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
    outbound_rt: Arc<tokio::runtime::Runtime>,
    /// Drives epoch deadlines for both engines; stops on drop. Held only for its lifetime.
    _epoch_ticker: EpochTicker,
}

impl Host {
    /// A host backed by an in-memory store (the default; process-lifetime state). `trust` is
    /// the set of keys allowed to sign loadable filters (ADR 000006) — pass `TrustPolicy::empty()`
    /// only if you intend that nothing can load.
    pub fn new(trust: TrustPolicy) -> Result<Self> {
        Self::with_backend(trust, Arc::new(MemoryBackend::default()))
    }

    /// Read-only residency metrics for the trusted (pooling) engine (wasmtime 46
    /// `PoolingAllocatorMetrics`). A cheap, cloneable handle for perf probes / observability —
    /// not the hot path. `unused_memory_bytes_resident()` reports bytes kept resident for
    /// unused-but-warm pool slots (`linear_memory_keep_resident`, left at its default 0 here, so
    /// this is expected to read ~0); `memories()` / `component_instances()` report the live count.
    /// `None` if the trusted engine is not pooling (it always is here, so this returns `Some`).
    pub fn pooling_allocator_metrics(&self) -> Option<wasmtime::PoolingAllocatorMetrics> {
        self.trusted_engine.pooling_allocator_metrics()
    }

    /// A host backed by a caller-supplied store (e.g. `RedbBackend` for durability).
    pub fn with_backend(trust: TrustPolicy, kv: Arc<dyn KvBackend>) -> Result<Self> {
        let trusted_engine = build_engine(Allocation::Pooling)?;
        let untrusted_engine = build_engine(Allocation::OnDemand)?;
        let _epoch_ticker =
            EpochTicker::spawn(vec![trusted_engine.clone(), untrusted_engine.clone()]);
        // A MULTI-thread runtime (not current-thread): the host's public API is sync and driven from
        // arbitrary worker threads, so `block_on` must be safe to call concurrently from several
        // threads — a current-thread runtime serializes/contends there. Two workers suffice (outbound
        // is I/O-bound, not CPU-bound) and bound the extra thread count (security-auditor F-001).
        #[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
        let outbound_rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
        );
        Ok(Self {
            trusted_engine,
            untrusted_engine,
            kv,
            kv_quota: Arc::new(KvQuota::new()),
            trust,
            sink: Arc::new(NoopSink),
            #[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
            outbound_rt,
            _epoch_ticker,
        })
    }

    /// Set the telemetry sink (ADR 000009 observability stage). Filters loaded **after** this
    /// emit one span per `on_request` / `on_response` to `sink`; the default is `NoopSink`
    /// (observability off, zero cost). Builder style: `Host::new(trust)?.with_telemetry_sink(sink)`.
    /// The sink is cloned into each filter at `load`, so set it before loading.
    pub fn with_telemetry_sink(mut self, sink: Arc<dyn TelemetrySink>) -> Self {
        self.sink = sink;
        self
    }

    /// Add `sink` ALONGSIDE the current one (fan-out) instead of replacing it — the composition
    /// hook for wiring the OTLP export buffer (ADR 000040) next to whatever sink the caller
    /// already set. Same load-time rule as [`with_telemetry_sink`](Self::with_telemetry_sink):
    /// compose before loading filters.
    pub fn with_added_telemetry_sink(mut self, sink: Arc<dyn TelemetrySink>) -> Self {
        self.sink = Arc::new(observe::FanOutSink::new(vec![self.sink, sink]));
        self
    }

    /// Load a filter component under the given isolation mode. Type-checks and resolves
    /// imports up front (`InstancePre`). For `Trusted`, the single persistent instance is
    /// created now and `init` runs once; for `Untrusted`, instantiation is deferred to
    /// each request.
    ///
    /// `filter_id` is the host-assigned identity used to namespace this filter's keyspace
    /// (ADR 000011). It must be non-empty and free of the namespace delimiter; the filter
    /// never sees or controls it. **Uniqueness is the caller's responsibility**: `load`
    /// rejects an empty or delimiter-bearing id but not a duplicate, so loading the same id
    /// twice shares one keyspace. A manifest-driven registry will assign and dedup ids
    /// (ADR 000007).
    pub fn load(
        &self,
        filter_id: &str,
        artifact: &SignedArtifact<'_>,
        opts: LoadOptions,
    ) -> Result<LoadedFilter> {
        self.load_inner(filter_id, artifact, opts)
            .map_err(anyhow::Error::from)
    }

    /// The typed-error inner of [`Host::load`] (bp-rust: library code uses `thiserror`, not ad hoc
    /// `anyhow::ensure!`). `load` stays `anyhow::Result` at the public boundary — unchanged, so it
    /// keeps matching `plecto-control::ControlError::Load`'s existing `anyhow::Error` passthrough —
    /// but every rejection here is a concrete, `downcast_ref`-able [`LoadError`] variant.
    fn load_inner(
        &self,
        filter_id: &str,
        artifact: &SignedArtifact<'_>,
        opts: LoadOptions,
    ) -> std::result::Result<LoadedFilter, LoadError> {
        if filter_id.is_empty() {
            return Err(LoadError::EmptyFilterId);
        }
        if filter_id.contains(KV_NS_DELIM) {
            return Err(LoadError::FilterIdContainsDelimiter);
        }

        // --- provenance gate (ADR 000006): verify BEFORE instantiate, fail-closed. A
        // --- missing / untrusted / tampered signature or a missing SBOM means we never
        // --- touch the component bytes with wasmtime. Order is cheap-checks first.
        if artifact.sbom.is_empty() {
            return Err(LoadError::MissingSbom);
        }
        if !self
            .trust
            .verifies(artifact.component_signature, artifact.component_bytes)
        {
            return Err(LoadError::UnverifiedComponentSignature);
        }
        if !self.trust.verifies(artifact.sbom_signature, artifact.sbom) {
            return Err(LoadError::UnverifiedSbomSignature);
        }
        // The SBOM must attest THIS component (its subject digest == sha256(component)), so a
        // validly-signed but unrelated SBOM cannot be paired with it (review f000003 #1).
        sbom_binds_component(artifact.sbom, artifact.component_bytes)?;

        let component_bytes = artifact.component_bytes;
        let engine = match opts.isolation {
            Isolation::Trusted => &self.trusted_engine,
            Isolation::Untrusted => &self.untrusted_engine,
        };
        let component = Component::from_binary(engine, component_bytes)?;
        let version = detect_contract_version(&component, engine);
        if version == ContractVersion::V01 {
            // Once per process, not per load (ADR 000071): the operator needs the nudge, not a
            // log line per hot-reload of the same fleet of legacy filters.
            static V01_WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !V01_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    filter = %filter_id,
                    "plecto:filter@0.1.0 is deprecated; rebuild filters for 0.2.0 (byte-valued headers)"
                );
            }
        }
        let mut linker = Linker::<HostState>::new(engine);
        // deny-by-default: lend ONLY the plecto host-API (every basic capability in one call),
        // at the interface version this component targets — 0.1 and 0.2 instance names are
        // distinct semver tracks and never cross-resolve. No WASI is added — unless this filter
        // has an outbound policy (below).
        match version {
            ContractVersion::V02 => {
                FilterV02::add_to_linker::<_, HasSelf<HostState>>(
                    &mut linker,
                    |s: &mut HostState| s,
                )?;
            }
            ContractVersion::V01 => {
                FilterV01::add_to_linker::<_, HasSelf<HostState>>(
                    &mut linker,
                    |s: &mut HostState| s,
                )?;
            }
        }

        // Outbound capabilities (ADR 000036 HTTP / ADR 000060 TCP): only when the operator lent
        // this filter an allowlist do we add the MINIMAL WASI base (io / clocks / random / stdio +
        // the inert wasi:cli runtime slice — NO fs, NO cli args/env of substance) plus the
        // capability's own interfaces, still deny-by-default (every call is gated by the
        // SSRF-guarded hooks / the vetted lookup + connect check). A filter without a policy links
        // no WASI at all, exactly as before.
        #[cfg(feature = "outbound-http")]
        let outbound = opts
            .outbound_http
            .clone()
            .map(outbound_http::OutboundState::new);
        #[cfg(feature = "outbound-tcp")]
        let outbound_tcp = opts.outbound_tcp.clone().map(|policy| {
            let resolver = {
                #[cfg(feature = "test-support")]
                match opts.outbound_tcp_static_resolver.clone() {
                    Some(map) => crate::resolver::Resolver::Static(map),
                    None => crate::resolver::Resolver::System,
                }
                #[cfg(not(feature = "test-support"))]
                crate::resolver::Resolver::System
            };
            outbound_tcp::OutboundTcpState::new(policy, resolver)
        });
        #[cfg(any(
            feature = "outbound-http",
            feature = "outbound-tcp",
            feature = "fat-guest"
        ))]
        {
            let mut needs_wasi_base = false;
            #[cfg(feature = "outbound-http")]
            {
                needs_wasi_base |= outbound.is_some();
            }
            #[cfg(feature = "outbound-tcp")]
            {
                needs_wasi_base |= outbound_tcp.is_some();
            }
            #[cfg(feature = "fat-guest")]
            {
                needs_wasi_base |= opts.wasi_minimal;
            }
            if needs_wasi_base {
                wasmtime_wasi::p2::add_to_linker_proxy_interfaces_async(&mut linker)?;
                // The std guest's runtime also imports the rest of wasi:cli (environment / exit /
                // terminal-*), each inert under the empty `WasiCtx`. Still NO filesystem — the
                // capability boundary that matters (mirrors the streaming path, audit F-002).
                add_cli_runtime(&mut linker)?;
            }
        }
        // Fat guest (ADR 000063): TinyGo's wasip2 runtime unconditionally imports
        // `wasi:filesystem/types` + `wasi:filesystem/preopens` even for a program that never
        // touches a file (confirmed against TinyGo 0.41.1) — link an EMPTY filesystem (no
        // preopened directory, ever) so such a guest instantiates while filesystem access stays
        // structurally unreachable.
        #[cfg(feature = "fat-guest")]
        if opts.wasi_minimal {
            crate::state::add_inert_filesystem(&mut linker)?;
        }
        #[cfg(feature = "outbound-http")]
        if outbound.is_some() {
            wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        }
        // Outbound TCP (ADR 000060): the `wasi:sockets` TCP-connect slice ONLY — network /
        // instance-network / tcp-create-socket / tcp behind the Store's socket_addr_check, plus
        // the host's OWN vetted ip-name-lookup (the upstream one has no hostname filter). The UDP
        // interfaces are never linked: the capability's absence, not a runtime deny.
        #[cfg(feature = "outbound-tcp")]
        if outbound_tcp.is_some() {
            use wasmtime_wasi::p2::bindings::sockets;
            use wasmtime_wasi::sockets::{WasiSockets, WasiSocketsView};
            let getter = <HostState as WasiSocketsView>::sockets;
            let net_opts = sockets::network::LinkOptions::default();
            sockets::network::add_to_linker::<HostState, WasiSockets>(
                &mut linker,
                &net_opts,
                getter,
            )?;
            sockets::instance_network::add_to_linker::<HostState, WasiSockets>(
                &mut linker,
                getter,
            )?;
            sockets::tcp_create_socket::add_to_linker::<HostState, WasiSockets>(
                &mut linker,
                getter,
            )?;
            sockets::tcp::add_to_linker::<HostState, WasiSockets>(&mut linker, getter)?;
            sockets::ip_name_lookup::add_to_linker::<HostState, outbound_tcp::PlectoTcpLookup>(
                &mut linker,
                HostState::tcp_lookup,
            )?;
        }

        let pre = match version {
            ContractVersion::V02 => {
                FilterPreBinding::V02(FilterPreV02::new(linker.instantiate_pre(&component)?)?)
            }
            ContractVersion::V01 => {
                FilterPreBinding::V01(FilterPreV01::new(linker.instantiate_pre(&component)?)?)
            }
        };

        // Zero-copy body bypass (ADR 000038 / ADR 000005 mechanism 2): a filter that inspects or
        // transforms the request body ALSO exports `on-request-body` (world `filter-body`). Detect
        // that export ONCE here; its absence tells the fast path the body never enters guest memory,
        // so it streams straight through instead of buffering (fail-closed: presence ⇒ buffer).
        let body_export = component.get_export_index(None, "on-request-body");

        let runtime = WasmtimeRuntime {
            engine: engine.clone(),
            kv: self.kv.clone(),
            kv_prefix: format!("{filter_id}{KV_NS_DELIM}"),
            pre,
            body_export,
            init_deadline_ms: opts.init_deadline_ms,
            request_deadline_ms: opts.request_deadline_ms,
            max_memory_bytes: opts.max_memory_bytes,
            ratelimit_bucket: opts.ratelimit_bucket,
            kv_quota: self.kv_quota.clone(),
            config: Arc::new(opts.config.clone()),
            #[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
            rt: {
                let mut needs_rt = false;
                #[cfg(feature = "outbound-http")]
                {
                    needs_rt |= outbound.is_some();
                }
                #[cfg(feature = "outbound-tcp")]
                {
                    needs_rt |= outbound_tcp.is_some();
                }
                needs_rt.then(|| self.outbound_rt.clone())
            },
            #[cfg(feature = "outbound-http")]
            outbound,
            #[cfg(feature = "outbound-tcp")]
            outbound_tcp,
            #[cfg(feature = "fat-guest")]
            wasi_minimal: opts.wasi_minimal,
        };

        let trusted = match opts.isolation {
            Isolation::Untrusted => None,
            Isolation::Trusted => {
                let cap = opts.trusted_pool_size.clamp(1, TRUSTED_POOL_MAX);
                // Eager-build ONE instance now so a broken `init` surfaces at load (not on the
                // first request) and a single-threaded caller then reuses it (init-once holds).
                // The rest of the pool fills lazily, only when concurrency demands it (ADR 000012).
                let first = runtime
                    .instantiate_initialized()
                    .map_err(LoadError::Instantiate)?;
                Some(TrustedPool::new(
                    cap,
                    Duration::from_millis(opts.checkout_timeout_ms),
                    opts.max_requests_per_instance,
                    first,
                ))
            }
        };

        let inner = LoadedInner::new(
            runtime,
            filter_id.to_string(),
            self.sink.clone(),
            opts.isolation,
        );

        Ok(LoadedFilter { inner, trusted })
    }
}
