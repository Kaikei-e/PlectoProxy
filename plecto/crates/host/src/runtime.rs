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
use crate::quota::KvQuota;
use crate::state::HostState;
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
    /// Drain this instance's per-request host-log lines after a call.
    fn take_logs(&self, instance: &mut Self::Instance) -> Vec<LogLine>;
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
    /// This filter's outbound HTTP state (ADR 000036): allowlist + SSRF policy + shared concurrency
    /// semaphore. `Some` only when the manifest lent it an allowlist.
    #[cfg(feature = "outbound-http")]
    pub(crate) outbound: Option<outbound_http::OutboundState>,
    /// The shared tokio runtime, cloned from the `Host`, present only for outbound-using filters —
    /// their guest calls block on real I/O the pollster executor cannot drive.
    #[cfg(feature = "outbound-http")]
    pub(crate) rt: Option<Arc<tokio::runtime::Runtime>>,
}

impl WasmtimeRuntime {
    /// Drive a guest call to completion. A filter without outbound uses the no-reactor `pollster`
    /// (its host-API imports never block); an outbound-using filter uses the tokio runtime so its
    /// `wasi:http` socket I/O is serviced and bounded by `tokio::time::timeout` (ADR 000036).
    pub(crate) fn drive<F: std::future::Future>(&self, fut: F) -> F::Output {
        #[cfg(feature = "outbound-http")]
        if let Some(rt) = &self.rt {
            return rt.block_on(fut);
        }
        pollster::block_on(fut)
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
        let (decision,) = self.drive(func.call_async(&mut inst.store, (body,)))?;
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
                self.kv.clone(),
                self.kv_prefix.clone(),
                self.max_memory_bytes,
                self.ratelimit_bucket,
                self.kv_quota.clone(),
                #[cfg(feature = "outbound-http")]
                self.outbound_hooks(),
            ),
        );
        store.limiter(|s| &mut s.limits);
        // `init` is heavy (Tenet 4) → the generous init budget, not the tight per-request one.
        store.set_epoch_deadline(self.init_deadline_ms);
        // Async (ADR 000021): the guest runs on a fiber; `drive` runs it to completion — pollster for
        // the sync host-API path, the tokio runtime when the guest may issue outbound I/O (ADR 000036).
        // Two-step instantiate (raw instance + typed view) so the raw `Instance` survives for the
        // optional body-hook lookup; `Filter` still drives the required init / on-request / on-response.
        let instance = self.drive(self.pre.instance_pre().instantiate_async(&mut store))?;
        let filter = Filter::new(&mut store, &instance)?;
        self.drive(filter.call_init(&mut store))?;
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
        std::mem::take(&mut instance.store.data_mut().logs)
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
