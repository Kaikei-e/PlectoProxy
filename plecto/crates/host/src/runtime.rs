//! The seam between pool/lifecycle-dispatch decision logic and actual wasmtime instance
//! mechanics: [`FilterRuntime`] (the trait tests fake) and [`WasmtimeRuntime`] (its production
//! implementation), which dispatches guest calls through versioned `plecto:filter` bindings
//! (0.1 / 0.2 adapters + 0.3 native, ADR 000071 / 000073).

use std::sync::Arc;

use anyhow::Result;
use wasmtime::component::ComponentExportIndex;
use wasmtime::{Engine, Store};

use crate::contract::{
    self, FilterPreV01, FilterPreV02, FilterPreV03, FilterV01, FilterV02, FilterV03,
    request_body_decision_from_v01, request_body_decision_from_v02, request_body_decision_from_v03,
    request_decision_from_v01, request_decision_from_v02, request_decision_from_v03,
    request_to_v01, request_to_v02, response_decision_from_v01, response_decision_from_v02,
    response_decision_from_v03, response_to_v01, response_to_v02,
};
use crate::errors::InvalidGuestOutput;
#[cfg(feature = "outbound-http")]
use crate::outbound_http;
#[cfg(feature = "outbound-tcp")]
use crate::outbound_tcp;
use crate::quota::KvQuota;
use crate::state::{HostState, HostStateInit};
use crate::{
    Bucket, HttpRequest, HttpResponse, KvBackend, LogLine, RequestBodyDecision, RequestDecision,
    ResponseDecision,
};

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

/// The instantiation-ready binding for whichever contract version the component targets
/// (ADR 000071 / 000073). The variant IS the version — dispatch matches on it (and on the
/// [`BoundFilter`] it produces), so no separate version field can disagree with it.
pub(crate) enum FilterPreBinding {
    V01(FilterPreV01<HostState>),
    V02(FilterPreV02<HostState>),
    V03(FilterPreV03<HostState>),
}

/// The production `FilterRuntime`: everything needed to instantiate and drive a real wasmtime
/// component instance for one loaded filter.
pub(crate) struct WasmtimeRuntime {
    pub(crate) engine: Engine,
    pub(crate) kv: Arc<dyn KvBackend>,
    pub(crate) kv_prefix: String,
    pub(crate) pre: FilterPreBinding,
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

    /// Run `on-request` through the instance's bound contract version. A 0.1 guest sees the
    /// lossy-UTF-8 projection of the byte-valued canonical request (ADR 000071); a 0.2 guest a
    /// shape-identical clone; each decision is mapped back through the validating adapter. A
    /// `None` from the adapter means the guest's output failed header validation — surfaced as
    /// [`InvalidGuestOutput`], fail-closed.
    pub(crate) fn call_on_request(
        &self,
        inst: &mut WasmtimeInstance,
        req: &HttpRequest,
    ) -> wasmtime::Result<RequestDecision> {
        match &mut inst.filter {
            BoundFilter::V03(filter) => {
                let raw = self.drive_call(filter.call_on_request(&mut inst.store, req))?;
                request_decision_from_v03(raw).ok_or_else(invalid_guest_header_error)
            }
            BoundFilter::V02(filter) => {
                let guest_req = request_to_v02(req);
                let raw = self.drive_call(filter.call_on_request(&mut inst.store, &guest_req))?;
                request_decision_from_v02(raw).ok_or_else(invalid_guest_header_error)
            }
            BoundFilter::V01(filter) => {
                let guest_req = request_to_v01(req);
                let raw = self.drive_call(filter.call_on_request(&mut inst.store, &guest_req))?;
                request_decision_from_v01(raw).ok_or_else(invalid_guest_header_error)
            }
        }
    }

    /// Run `on-response` — same versioned dispatch and validation as
    /// [`call_on_request`](Self::call_on_request). `req` is the as-forwarded request snapshot
    /// (ADR 000073): a 0.3 guest receives it as the first parameter; the 0.1 / 0.2 adapters
    /// simply drop it (their `on-response` has no request-context parameter).
    pub(crate) fn call_on_response(
        &self,
        inst: &mut WasmtimeInstance,
        req: &HttpRequest,
        resp: &HttpResponse,
    ) -> wasmtime::Result<ResponseDecision> {
        match &mut inst.filter {
            BoundFilter::V03(filter) => {
                let raw = self.drive_call(filter.call_on_response(&mut inst.store, req, resp))?;
                response_decision_from_v03(raw).ok_or_else(invalid_guest_header_error)
            }
            BoundFilter::V02(filter) => {
                let guest_resp = response_to_v02(resp);
                let raw = self.drive_call(filter.call_on_response(&mut inst.store, &guest_resp))?;
                response_decision_from_v02(raw).ok_or_else(invalid_guest_header_error)
            }
            BoundFilter::V01(filter) => {
                let guest_resp = response_to_v01(resp);
                let raw = self.drive_call(filter.call_on_response(&mut inst.store, &guest_resp))?;
                response_decision_from_v01(raw).ok_or_else(invalid_guest_header_error)
            }
        }
    }

    /// Call the guest's optional `on-request-body` export (world `filter-body`) on an
    /// already-instantiated instance, with the buffered body borrowed (zero extra host-side
    /// copy). The raw decision type is the instance's bound contract version. The caller only
    /// reaches here for a body-reading filter (`body_export` is `Some`, so `body_func` was
    /// resolved and signature-validated at instantiation — Tenet 4: the lookup/type-check is
    /// init work, not per-request work; a `TypedFunc` itself cannot be cached because its
    /// borrowed `(&[u8],)` params would need a lifetime on the instance). wasmtime 46 handles
    /// component `post-return` internally, so a single `call_async` is the whole interaction.
    pub(crate) fn call_body_hook(
        &self,
        inst: &mut WasmtimeInstance,
        body: &[u8],
    ) -> wasmtime::Result<RequestBodyDecision> {
        let Some(func) = inst.body_func else {
            // Unreachable: the caller gates on `reads_body()`. Fail closed, never panic.
            return Err(wasmtime::Error::msg(
                "on-request-body called on a filter without a body export",
            ));
        };
        match &inst.filter {
            BoundFilter::V03(_) => {
                use contract::types_v03::RequestBodyDecision as Raw;
                let func = func.typed::<(&[u8],), (Raw,)>(&inst.store)?;
                let (decision,) = self.drive_call(func.call_async(&mut inst.store, (body,)))?;
                request_body_decision_from_v03(decision).ok_or_else(invalid_guest_header_error)
            }
            BoundFilter::V02(_) => {
                use contract::types_v02::RequestBodyDecision as Raw;
                let func = func.typed::<(&[u8],), (Raw,)>(&inst.store)?;
                let (decision,) = self.drive_call(func.call_async(&mut inst.store, (body,)))?;
                request_body_decision_from_v02(decision).ok_or_else(invalid_guest_header_error)
            }
            BoundFilter::V01(_) => {
                use contract::types_v01::RequestBodyDecision as Raw;
                let func = func.typed::<(&[u8],), (Raw,)>(&inst.store)?;
                let (decision,) = self.drive_call(func.call_async(&mut inst.store, (body,)))?;
                request_body_decision_from_v01(decision).ok_or_else(invalid_guest_header_error)
            }
        }
    }

    #[cfg(feature = "outbound-http")]
    pub(crate) fn outbound_hooks(&self) -> outbound_http::PlectoHttpHooks {
        match &self.outbound {
            Some(state) => state.hooks(),
            None => outbound_http::PlectoHttpHooks::deny_all(),
        }
    }
}

/// The typed marker (not a bare message) so `RunError::from_call` classifies this as
/// `RunError::InvalidOutput` — observably distinct from a trap (ADR 000071).
fn invalid_guest_header_error() -> wasmtime::Error {
    wasmtime::Error::new(InvalidGuestOutput)
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
        // Async (ADR 000021): the guest runs on a fiber; `drive` runs it to completion — pollster
        // for the sync host-API path, the tokio runtime when the guest may issue outbound I/O
        // (ADR 000036). Two-step instantiate (raw instance + typed view) so the raw `Instance`
        // survives for the optional body-hook lookup; the typed `Filter` binding still drives the
        // required init / on-request / on-response, at whichever contract version `pre` targets.
        let (filter, instance) = match &self.pre {
            FilterPreBinding::V03(pre) => {
                let instance = self.drive_call(pre.instance_pre().instantiate_async(&mut store))?;
                let filter = FilterV03::new(&mut store, &instance)?;
                self.drive_call(filter.call_init(&mut store))?;
                (BoundFilter::V03(filter), instance)
            }
            FilterPreBinding::V02(pre) => {
                let instance = self.drive_call(pre.instance_pre().instantiate_async(&mut store))?;
                let filter = FilterV02::new(&mut store, &instance)?;
                self.drive_call(filter.call_init(&mut store))?;
                (BoundFilter::V02(filter), instance)
            }
            FilterPreBinding::V01(pre) => {
                let instance = self.drive_call(pre.instance_pre().instantiate_async(&mut store))?;
                let filter = FilterV01::new(&mut store, &instance)?;
                self.drive_call(filter.call_init(&mut store))?;
                (BoundFilter::V01(filter), instance)
            }
        };
        // Resolve (and signature-check) the optional `on-request-body` export ONCE per instance
        // (Tenet 4: init vs per-request): a body-hook call then only re-derives the typed view,
        // never the export lookup, and a signature-mismatched guest fails here — at instantiate —
        // instead of on its first request.
        let body_func = match &self.body_export {
            Some(idx) => {
                let func = instance.get_func(&mut store, idx).ok_or_else(|| {
                    anyhow::anyhow!("on-request-body export index did not resolve to a function")
                })?;
                match &filter {
                    BoundFilter::V03(_) => {
                        func.typed::<(&[u8],), (contract::types_v03::RequestBodyDecision,)>(
                            &store,
                        )?;
                    }
                    BoundFilter::V02(_) => {
                        func.typed::<(&[u8],), (contract::types_v02::RequestBodyDecision,)>(
                            &store,
                        )?;
                    }
                    BoundFilter::V01(_) => {
                        func.typed::<(&[u8],), (contract::types_v01::RequestBodyDecision,)>(
                            &store,
                        )?;
                    }
                }
                Some(func)
            }
            None => None,
        };
        Ok(WasmtimeInstance {
            store,
            filter,
            body_func,
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

/// The typed guest binding at the contract version this instance was instantiated with — always
/// the same variant as the runtime's `FilterPreBinding` (both are built in
/// `instantiate_initialized`), so dispatch matches on this alone.
pub(crate) enum BoundFilter {
    V01(FilterV01),
    V02(FilterV02),
    V03(FilterV03),
}

/// A live, initialized filter instance (its `Store` plus the bound component instance).
pub(crate) struct WasmtimeInstance {
    pub(crate) store: Store<HostState>,
    pub(crate) filter: BoundFilter,
    /// The optional `on-request-body` export (world `filter-body`, not part of the base `filter`
    /// bindgen), resolved once at instantiation — `Some` iff the runtime's `body_export` is.
    pub(crate) body_func: Option<wasmtime::component::Func>,
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
