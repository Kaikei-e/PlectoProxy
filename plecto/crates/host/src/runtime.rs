//! The seam between pool/lifecycle-dispatch decision logic and actual wasmtime instance
//! mechanics: [`FilterRuntime`] (the trait tests fake) and [`WasmtimeRuntime`] (its production
//! implementation).

use std::sync::Arc;

use anyhow::Result;
use wasmtime::component::ComponentExportIndex;
use wasmtime::{Engine, Store};

use crate::bindings::{Filter, FilterPre};
#[cfg(feature = "outbound-http")]
use crate::outbound_http;
#[cfg(feature = "outbound-tcp")]
use crate::outbound_tcp;
use crate::quota::KvQuota;
use crate::state::{HostState, HostStateInit};
use crate::{Bucket, KvBackend, LogLine, RequestBodyDecision};

/// The seam between pool / lifecycle-dispatch DECISION logic (`LoadedInner`, below) and the actual
/// instance mechanics. Production has exactly one implementation (`WasmtimeRuntime`); tests
/// substitute a fake to exercise checkout / recycle / circuit-breaker / lifecycle dispatch without
/// compiling or instantiating any real wasm component. A generic parameter bounded by this trait
/// (not `Box<dyn FilterRuntime>`) is how callers plug it in — static dispatch, no vtable.
pub(crate) trait FilterRuntime: Send + Sync {
    /// A live instance ready to run hooks — opaque to the pool/executor logic below.
    type Instance: Send;
    /// Instantiate a fresh instance and run its once-per-instance `init` under the init deadline.
    fn instantiate_initialized(&self) -> Result<Self::Instance>;
    /// Reset an instance's per-request state before reuse (pooled path only — a freshly
    /// instantiated instance from `instantiate_initialized` is already in this state).
    fn begin_request(&self, instance: &mut Self::Instance);
    /// Set the per-request epoch deadline before a hook call.
    fn set_request_deadline(&self, instance: &mut Self::Instance);
    /// Drain this instance's per-request host-log lines after a call, for an instance that will
    /// be reused (a pooled/trusted instance on its `Ok` arm): a still-unterminated stdio partial
    /// line legitimately stays buffered, since a later call on the same instance may complete it.
    fn take_logs(&self, instance: &mut Self::Instance) -> Vec<LogLine>;
    /// Like [`take_logs`](Self::take_logs), but for an instance about to be discarded — a trap
    /// (pooled or fresh), or a fresh/untrusted instance's `Ok` arm, since fresh instances are
    /// always single-use regardless of outcome. Also recovers anything a normal drain would
    /// otherwise leave buffered for a later call that will now never come (ADR 000063: an
    /// unterminated stdio partial line, e.g. a panic message with no trailing newline).
    fn take_logs_final(&self, instance: &mut Self::Instance) -> Vec<LogLine>;
}

/// The production `FilterRuntime`: everything needed to instantiate and drive a real wasmtime
/// component instance for one loaded filter.
pub(crate) struct WasmtimeRuntime {
    pub(crate) engine: Engine,
    pub(crate) kv: Arc<dyn KvBackend>,
    pub(crate) kv_prefix: String,
    pub(crate) pre: FilterPre<HostState>,
    /// Export index of the guest's `on-request-body` hook (world `filter-body`), or `None` for a
    /// header-only filter. `Some` is the ONLY signal that makes the fast path buffer the body
    /// (ADR 000038 / ADR 000005 mechanism 2); absence keeps the body on the zero-copy stream path.
    pub(crate) body_export: Option<ComponentExportIndex>,
    pub(crate) init_deadline_ms: u64,
    pub(crate) request_deadline_ms: u64,
    pub(crate) max_memory_bytes: u64,
    pub(crate) ratelimit_bucket: Option<Bucket>,
    pub(crate) kv_quota: Arc<KvQuota>,
    /// This filter's manifest-declared business config (ADR 000066), shared read-only across
    /// every instance of this filter.
    pub(crate) config: Arc<std::collections::BTreeMap<String, String>>,
    /// This filter's outbound HTTP state (ADR 000036): allowlist + SSRF policy + shared concurrency
    /// semaphore. `Some` only when the manifest lent it an allowlist.
    #[cfg(feature = "outbound-http")]
    pub(crate) outbound: Option<outbound_http::OutboundState>,
    /// This filter's outbound TCP state (ADR 000060): allowlist + SSRF policy + resolver. `Some`
    /// only when the manifest lent it an allowlist.
    #[cfg(feature = "outbound-tcp")]
    pub(crate) outbound_tcp: Option<outbound_tcp::OutboundTcpState>,
    /// The shared tokio runtime, cloned from the `Host`, present only for outbound-using filters —
    /// their guest calls block on real I/O the pollster executor cannot drive.
    #[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
    pub(crate) rt: Option<Arc<tokio::runtime::Runtime>>,
    /// This filter's manifest `wasi = "minimal"` declaration (ADR 000063), copied from
    /// `LoadOptions::wasi_minimal` at `Host::load`.
    #[cfg(feature = "fat-guest")]
    pub(crate) wasi_minimal: bool,
}

impl WasmtimeRuntime {
    /// Drive a guest call to completion. A filter without outbound uses the no-reactor `pollster`
    /// (its host-API imports never block); an outbound-using filter uses the tokio runtime so its
    /// `wasi:http` / `wasi:sockets` I/O is serviced (ADR 000036 / 000060).
    pub(crate) fn drive<F: std::future::Future>(&self, fut: F) -> F::Output {
        #[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
        if let Some(rt) = &self.rt {
            return rt.block_on(fut);
        }
        pollster::block_on(fut)
    }

    /// [`drive`](Self::drive) for a fallible guest call, adding the wall-clock deadline an
    /// outbound-TCP filter needs (ADR 000060): epoch interruption cannot reach a guest blocked in
    /// a host socket call, and raw TCP has no per-call seam like outbound HTTP's `send_request`
    /// where a `total_timeout` could live — so the whole hook call is bounded. On expiry the call
    /// future is dropped (the fiber unwinds), the caller sees an error, and the instance is
    /// discarded by the pool's existing trap path (fail-closed).
    pub(crate) fn drive_call<T>(
        &self,
        fut: impl std::future::Future<Output = wasmtime::Result<T>>,
    ) -> wasmtime::Result<T> {
        #[cfg(feature = "outbound-tcp")]
        if let (Some(rt), Some(state)) = (&self.rt, &self.outbound_tcp) {
            return block_on_with_deadline(rt, state.io_deadline(), fut);
        }
        self.drive(fut)
    }

    /// The per-Store outbound TCP guard: the real vetting guard for an outbound-TCP filter, or a
    /// deny-all handle otherwise (belt-and-suspenders; those filters link no `wasi:sockets`).
    #[cfg(feature = "outbound-tcp")]
    pub(crate) fn tcp_guard(&self) -> outbound_tcp::TcpGuard {
        match &self.outbound_tcp {
            Some(state) => state.guard(),
            None => outbound_tcp::TcpGuard::deny_all(),
        }
    }

    /// Call the guest's optional `on-request-body` export (world `filter-body`) on an
    /// already-instantiated instance. Because the export is OPTIONAL it is looked up by index
    /// (`idx`, resolved once at load) rather than through the base `filter` bindgen, then called
    /// with the buffered body borrowed (zero extra host-side copy). `post-return` is driven before
    /// the instance can be reused (a pooled instance survives the call). The caller only reaches
    /// here for a body-reading filter (`body_export` is `Some`).
    pub(crate) fn call_body_hook(
        &self,
        inst: &mut WasmtimeInstance,
        idx: &ComponentExportIndex,
        body: &[u8],
    ) -> wasmtime::Result<RequestBodyDecision> {
        let func = inst
            .instance
            .get_typed_func::<(&[u8],), (RequestBodyDecision,)>(&mut inst.store, idx)?;
        // wasmtime 46: component `post-return` is handled internally and no longer needs an explicit
        // call, so a single `call_async` is the whole interaction.
        let (decision,) = self.drive_call(func.call_async(&mut inst.store, (body,)))?;
        Ok(decision)
    }

    /// The per-Store outbound hooks: the real SSRF-guarded hooks for an outbound filter, or a
    /// deny-all handle otherwise (belt-and-suspenders; those filters link no `wasi:http`).
    #[cfg(feature = "outbound-http")]
    pub(crate) fn outbound_hooks(&self) -> outbound_http::PlectoHttpHooks {
        match &self.outbound {
            Some(state) => state.hooks(),
            None => outbound_http::PlectoHttpHooks::deny_all(),
        }
    }
}

impl FilterRuntime for WasmtimeRuntime {
    type Instance = WasmtimeInstance;

    /// Instantiate a fresh instance and run `init` once, under the `init` epoch deadline and
    /// the Store memory limit (ADR 000006).
    fn instantiate_initialized(&self) -> Result<WasmtimeInstance> {
        let mut store = Store::new(
            &self.engine,
            HostState::new(
                HostStateInit {
                    kv: self.kv.clone(),
                    kv_prefix: self.kv_prefix.clone(),
                    max_memory_bytes: self.max_memory_bytes,
                    ratelimit_bucket: self.ratelimit_bucket,
                    quota: self.kv_quota.clone(),
                    config: self.config.clone(),
                    #[cfg(feature = "fat-guest")]
                    wasi_minimal: self.wasi_minimal,
                },
                #[cfg(feature = "outbound-http")]
                self.outbound_hooks(),
                #[cfg(feature = "outbound-tcp")]
                self.tcp_guard(),
            ),
        );
        store.limiter(|s| &mut s.limits);
        // `init` is heavy (Tenet 4) → the generous init budget, not the tight per-request one.
        store.set_epoch_deadline(self.init_deadline_ms);
        // Async (ADR 000021): the guest runs on a fiber; `drive` runs it to completion — pollster for
        // the sync host-API path, the tokio runtime when the guest may issue outbound I/O (ADR 000036).
        // Two-step instantiate (raw instance + typed view) so the raw `Instance` survives for the
        // optional body-hook lookup; `Filter` still drives the required init / on-request / on-response.
        let instance = self.drive_call(self.pre.instance_pre().instantiate_async(&mut store))?;
        let filter = Filter::new(&mut store, &instance)?;
        self.drive_call(filter.call_init(&mut store))?;
        Ok(WasmtimeInstance {
            store,
            filter,
            instance,
        })
    }

    fn begin_request(&self, instance: &mut WasmtimeInstance) {
        instance.store.data_mut().begin_request();
    }

    fn set_request_deadline(&self, instance: &mut WasmtimeInstance) {
        instance.store.set_epoch_deadline(self.request_deadline_ms);
    }

    fn take_logs(&self, instance: &mut WasmtimeInstance) -> Vec<LogLine> {
        instance.store.data_mut().take_logs()
    }

    fn take_logs_final(&self, instance: &mut WasmtimeInstance) -> Vec<LogLine> {
        instance.store.data_mut().take_logs_final()
    }
}

/// A live, initialized filter instance (its `Store` plus the bound component instance).
pub(crate) struct WasmtimeInstance {
    pub(crate) store: Store<HostState>,
    pub(crate) filter: Filter,
    /// The raw component instance, kept so the optional `on-request-body` export (world
    /// `filter-body`, not part of the base `filter` bindgen) can be looked up and called by index.
    pub(crate) instance: wasmtime::component::Instance,
}

/// Block on `fut` under a wall-clock `deadline` (the outbound-TCP I/O bound, ADR 000060). A
/// free function so the deadline behaviour is unit-testable without a `WasmtimeRuntime`.
#[cfg(feature = "outbound-tcp")]
pub(crate) fn block_on_with_deadline<T>(
    rt: &tokio::runtime::Runtime,
    deadline: std::time::Duration,
    fut: impl std::future::Future<Output = wasmtime::Result<T>>,
) -> wasmtime::Result<T> {
    rt.block_on(async {
        match tokio::time::timeout(deadline, fut).await {
            Ok(result) => result,
            Err(_) => Err(wasmtime::Error::msg(format!(
                "outbound-tcp io deadline ({deadline:?}) exceeded; the hook call was cancelled \
                 fail-closed and the instance is discarded"
            ))),
        }
    })
}

#[cfg(all(test, feature = "outbound-tcp"))]
mod deadline_tests {
    use super::block_on_with_deadline;
    use std::time::Duration;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn a_prompt_call_completes_within_the_deadline() {
        let out = block_on_with_deadline(&rt(), Duration::from_secs(1), async { Ok(42) });
        assert_eq!(out.unwrap(), 42);
    }

    #[test]
    fn a_hanging_call_is_cancelled_fail_closed() {
        // The wall-clock bound epoch interruption cannot provide: a guest blocked in host socket
        // I/O (connect to a black-holed address, read from a silent server) must surface as an
        // error, not hang the worker (ADR 000060).
        let out = block_on_with_deadline(&rt(), Duration::from_millis(20), async {
            std::future::pending::<wasmtime::Result<()>>().await
        });
        let err = out.expect_err("a hung call must be cancelled");
        assert!(err.to_string().contains("io deadline"));
    }
}
