//! The public per-request filter API: [`LoadedFilter`].

use std::time::{Instant, SystemTime};

use crate::observe;
use crate::pool::{HookResult, LoadedInner, TrustedPool};
use crate::runtime::{WasmtimeInstance, WasmtimeRuntime};
use crate::{
    ContractVersion, Hook, HttpRequest, HttpResponse, Isolation, LogLine, RequestBodyDecision,
    RequestDecision, RequestTrace, ResponseBodyDecision, ResponseDecision, RunError, SpanOutcome,
};

/// The outcome of attempting a non-blocking hook call on a loaded filter.
#[derive(Debug)]
pub enum TryHookOutcome<T> {
    Executed(std::result::Result<(T, Vec<LogLine>), RunError>),
    WouldBlock,
}

/// Which optional body hooks a loaded component exports, per direction (ADR 000098 decision 2).
/// Read off the component's exports at load, so it is sound (fail-closed): a filter cannot read a
/// body without declaring it in the contract. The two flags are independent — the acceptance
/// lattice admits any subset, and the fast path decides buffering separately for each direction,
/// so a filter that reads only one body leaves the other on the zero-copy path (ADR 000038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyHooks {
    /// The filter exports `on-request-body`.
    pub request: bool,
    /// The filter exports `on-response-body`.
    pub response: bool,
}

impl BodyHooks {
    /// Neither direction — the base `filter` world, and the identity of the OR-fold a route does
    /// over its chain.
    pub const NONE: BodyHooks = BodyHooks {
        request: false,
        response: false,
    };

    /// Fold another filter's hooks in: a route buffers a direction when ANY of its filters reads
    /// that direction.
    pub fn union(self, other: BodyHooks) -> BodyHooks {
        BodyHooks {
            request: self.request || other.request,
            response: self.response || other.response,
        }
    }
}

/// A loaded filter, ready to run per request. Trusted filters reuse instances from a
/// `TrustedPool` (checked out per request, ADR 000012); untrusted filters instantiate fresh
/// each request.
///
/// A trap leaves the guest's linear memory undefined, so the host discards that instance and a
/// later checkout rebuilds + re-inits one (self-heal, ADR 000006), with a pool-wide cooldown
/// bounding re-init storms (review f000003 #5). The `Option` is the isolation discriminator —
/// `None` means untrusted (fresh instance per request).
pub struct LoadedFilter {
    pub(crate) inner: LoadedInner<WasmtimeRuntime>,
    pub(crate) trusted: Option<TrustedPool<WasmtimeInstance>>,
    pub(crate) runs_inline: bool,
}

impl LoadedFilter {
    /// Returns whether this filter may execute inline on an async worker.
    ///
    /// Eligible iff isolation is [`Isolation::Trusted`], no blocking capabilities (outbound,
    /// WASI) are attached, and all imported interfaces are non-blocking: state interfaces
    /// (`host-kv`, `host-counter`, `host-ratelimit`) require `!KvBackend::may_block()`.
    pub fn runs_inline(&self) -> bool {
        self.runs_inline
    }

    pub fn isolation(&self) -> Isolation {
        self.inner.isolation
    }

    /// The `plecto:filter` version this component bound at load (ADR 000071 / 000073). Distinct
    /// from the set the binary can load: this is what THIS filter, in THIS configuration, is
    /// actually being driven through.
    pub fn contract_version(&self) -> ContractVersion {
        self.inner.runtime.contract_version()
    }

    /// Which body hooks this filter exports (ADR 000038 / ADR 000098). The fast path buffers a
    /// direction ONLY for a route with at least one filter that reads it; every other route keeps
    /// the zero-copy streaming path in that direction.
    pub fn body_hooks(&self) -> BodyHooks {
        BodyHooks {
            request: self.inner.runtime.body_export.is_some(),
            response: self.inner.runtime.response_body_export.is_some(),
        }
    }

    fn execute_hook<T>(
        &self,
        trace: &RequestTrace,
        hook: Hook,
        capture_logs: bool,
        outcome_of: impl FnOnce(&T) -> SpanOutcome,
        record_inline: bool,
        run: impl FnOnce(bool) -> HookResult<T>,
    ) -> std::result::Result<(T, Vec<LogLine>), RunError> {
        let sink_enabled = self.inner.sink.enabled();
        let full_span = self.inner.sink.needs_full_span();
        if !sink_enabled && !record_inline {
            return run(capture_logs).map_err(|(err, _)| err);
        }
        let capture = capture_logs;
        let start_wall = full_span.then(SystemTime::now);
        let started = Instant::now();
        let result = run(capture);
        let duration = started.elapsed();
        if sink_enabled {
            let outcome = match &result {
                Ok((decision, _)) => outcome_of(decision),
                Err((err, _)) => <SpanOutcome as From<&RunError>>::from(err),
            };
            let logs: &[LogLine] = match &result {
                Ok((_, logs)) | Err((_, logs)) => logs,
            };
            self.inner.sink.record_outcome(outcome, duration, &mut || {
                observe::build_filter_span(
                    trace,
                    &self.inner.filter_id,
                    self.inner.isolation,
                    hook,
                    outcome,
                    start_wall.unwrap_or_else(SystemTime::now),
                    duration,
                    logs,
                )
            });
        }
        if record_inline {
            self.inner.inline_metrics.record_duration(duration);
        }
        result.map_err(|(err, _)| err)
    }

    fn try_execute_inline<T>(
        &self,
        trace: &RequestTrace,
        hook: Hook,
        capture_logs: bool,
        outcome_of: impl FnOnce(&T) -> SpanOutcome,
        call: impl FnOnce(&mut WasmtimeInstance) -> wasmtime::Result<T>,
    ) -> Option<std::result::Result<(T, Vec<LogLine>), RunError>> {
        if !self.runs_inline {
            return None;
        }
        let pool = self.trusted.as_ref()?;
        let pooled = match self.inner.checkout_idle(pool) {
            Ok(inst) => inst,
            Err(miss) => {
                self.inner.inline_metrics.inc_fallback(miss);
                return None;
            }
        };
        Some(
            self.execute_hook(trace, hook, capture_logs, outcome_of, true, |capture| {
                self.inner.execute_pooled_opts(pool, pooled, call, capture)
            }),
        )
    }

    fn chain_capture_logs(&self) -> bool {
        self.inner.sink.needs_full_span()
    }

    /// Run the request-side hook under the request's trace context (`trace`, ADR 000009). The
    /// host times the call and emits one span — parented by `trace`, carrying the outcome and
    /// the filter's host-log lines as events — to its `TelemetrySink`. Returns the typed
    /// decision plus those log lines (the direct-access form), or a `RunError` the caller MUST
    /// fail-closed on (deadline / trap / instantiation — never a pass-through to upstream).
    pub fn on_request(
        &self,
        req: &HttpRequest,
        trace: &RequestTrace,
    ) -> std::result::Result<(RequestDecision, Vec<LogLine>), RunError> {
        self.execute_hook(
            trace,
            Hook::OnRequest,
            true,
            request_outcome,
            false,
            |capture| self.run_on_request(req, capture),
        )
    }

    #[doc(hidden)]
    pub fn on_request_for_chain(
        &self,
        req: &HttpRequest,
        trace: &RequestTrace,
    ) -> std::result::Result<RequestDecision, RunError> {
        self.execute_hook(
            trace,
            Hook::OnRequest,
            self.chain_capture_logs(),
            request_outcome,
            false,
            |capture| self.run_on_request(req, capture),
        )
        .map(|(decision, _)| decision)
    }

    fn run_on_request(&self, req: &HttpRequest, capture_logs: bool) -> HookResult<RequestDecision> {
        self.inner.run_hook_opts(
            self.trusted.as_ref(),
            |inst| self.inner.runtime.call_on_request(inst, req),
            capture_logs,
        )
    }

    /// Run the request-side hook without blocking. Takes ONLY an idle pooled instance.
    /// If none is idle — or the filter is not eligible to run inline — returns `WouldBlock`
    /// without calling the guest.
    pub fn try_on_request(
        &self,
        req: &HttpRequest,
        trace: &RequestTrace,
    ) -> TryHookOutcome<RequestDecision> {
        match self.try_execute_inline(trace, Hook::OnRequest, true, request_outcome, |inst| {
            self.inner.runtime.call_on_request(inst, req)
        }) {
            Some(outcome) => TryHookOutcome::Executed(outcome),
            None => TryHookOutcome::WouldBlock,
        }
    }

    #[doc(hidden)]
    pub fn try_on_request_for_chain(
        &self,
        req: &HttpRequest,
        trace: &RequestTrace,
    ) -> Option<std::result::Result<RequestDecision, RunError>> {
        self.try_execute_inline(
            trace,
            Hook::OnRequest,
            self.chain_capture_logs(),
            request_outcome,
            |inst| self.inner.runtime.call_on_request(inst, req),
        )
        .map(|res| res.map(|(decision, _)| decision))
    }

    /// Run the request-side BODY hook (buffer-then-decide, ADR 000025). The host hands the filter
    /// the fully-buffered request body; the filter returns the (possibly transformed) body to
    /// continue, or a `short-circuit` response (synthesised before upstream is reached). Same
    /// fail-closed contract and span emission as `on_request`.
    pub fn on_request_body(
        &self,
        body: &[u8],
        trace: &RequestTrace,
    ) -> std::result::Result<(RequestBodyDecision, Vec<LogLine>), RunError> {
        self.execute_hook(
            trace,
            Hook::OnRequestBody,
            true,
            request_body_outcome,
            false,
            |capture| self.run_on_request_body(body, capture),
        )
    }

    #[doc(hidden)]
    pub fn on_request_body_for_chain(
        &self,
        body: &[u8],
        trace: &RequestTrace,
    ) -> std::result::Result<RequestBodyDecision, RunError> {
        self.execute_hook(
            trace,
            Hook::OnRequestBody,
            self.chain_capture_logs(),
            request_body_outcome,
            false,
            |capture| self.run_on_request_body(body, capture),
        )
        .map(|(decision, _)| decision)
    }

    fn run_on_request_body(
        &self,
        body: &[u8],
        capture_logs: bool,
    ) -> HookResult<RequestBodyDecision> {
        // Header-only filter: no `on-request-body` export, so the body never enters guest memory.
        // The fast path already skips buffering (`reads_body()` is false); this is the defensive
        // floor — pass the body through unchanged without instantiating anything.
        if self.inner.runtime.body_export.is_none() {
            return Ok((RequestBodyDecision::Continue, Vec::new()));
        }
        self.inner.run_hook_opts(
            self.trusted.as_ref(),
            |inst| self.inner.runtime.call_body_hook(inst, body),
            capture_logs,
        )
    }

    /// Run the response-side hook for one response. Same fail-closed contract as `on_request`.
    /// `req` is the as-forwarded request snapshot (ADR 000073) — the request as it left the
    /// request-side chain — passed to a 0.3 guest as `on-response`'s first parameter and
    /// dropped by the 0.1 / 0.2 adapters.
    pub fn on_response(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
        trace: &RequestTrace,
    ) -> std::result::Result<(ResponseDecision, Vec<LogLine>), RunError> {
        self.execute_hook(
            trace,
            Hook::OnResponse,
            true,
            response_outcome,
            false,
            |capture| self.run_on_response(req, resp, capture),
        )
    }

    #[doc(hidden)]
    pub fn on_response_for_chain(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
        trace: &RequestTrace,
    ) -> std::result::Result<ResponseDecision, RunError> {
        self.execute_hook(
            trace,
            Hook::OnResponse,
            self.chain_capture_logs(),
            response_outcome,
            false,
            |capture| self.run_on_response(req, resp, capture),
        )
        .map(|(decision, _)| decision)
    }

    fn run_on_response(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
        capture_logs: bool,
    ) -> HookResult<ResponseDecision> {
        self.inner.run_hook_opts(
            self.trusted.as_ref(),
            |inst| self.inner.runtime.call_on_response(inst, req, resp),
            capture_logs,
        )
    }

    /// Run the response-side hook without blocking. Takes ONLY an idle pooled instance.
    /// If none is idle — or the filter is not eligible to run inline — returns `WouldBlock`
    /// without calling the guest.
    pub fn try_on_response(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
        trace: &RequestTrace,
    ) -> TryHookOutcome<ResponseDecision> {
        match self.try_execute_inline(trace, Hook::OnResponse, true, response_outcome, |inst| {
            self.inner.runtime.call_on_response(inst, req, resp)
        }) {
            Some(outcome) => TryHookOutcome::Executed(outcome),
            None => TryHookOutcome::WouldBlock,
        }
    }

    #[doc(hidden)]
    pub fn try_on_response_for_chain(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
        trace: &RequestTrace,
    ) -> Option<std::result::Result<ResponseDecision, RunError>> {
        self.try_execute_inline(
            trace,
            Hook::OnResponse,
            self.chain_capture_logs(),
            response_outcome,
            |inst| self.inner.runtime.call_on_response(inst, req, resp),
        )
        .map(|res| res.map(|(decision, _)| decision))
    }

    /// Run the response-side BODY hook (buffer-then-decide, ADR 000098). The host has held the
    /// response headers to get here, so the filter can still refuse what it just read: it forwards
    /// the buffered bytes, hands back replacements, or replaces the whole response. `req` is the
    /// as-forwarded snapshot and `resp` carries status + headers only — the bytes are `body`. Same
    /// fail-closed contract and span emission as the other hooks.
    pub fn on_response_body(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
        body: &[u8],
        trace: &RequestTrace,
    ) -> std::result::Result<(ResponseBodyDecision, Vec<LogLine>), RunError> {
        self.execute_hook(
            trace,
            Hook::OnResponseBody,
            true,
            response_body_outcome,
            false,
            |capture| self.run_on_response_body(req, resp, body, capture),
        )
    }

    #[doc(hidden)]
    pub fn on_response_body_for_chain(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
        body: &[u8],
        trace: &RequestTrace,
    ) -> std::result::Result<ResponseBodyDecision, RunError> {
        self.execute_hook(
            trace,
            Hook::OnResponseBody,
            self.chain_capture_logs(),
            response_body_outcome,
            false,
            |capture| self.run_on_response_body(req, resp, body, capture),
        )
        .map(|(decision, _)| decision)
    }

    fn run_on_response_body(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
        body: &[u8],
        capture_logs: bool,
    ) -> HookResult<ResponseBodyDecision> {
        // No `on-response-body` export: the body never enters guest memory. The fast path already
        // skips buffering for such a route; this is the defensive floor.
        if self.inner.runtime.response_body_export.is_none() {
            return Ok((ResponseBodyDecision::Continue, Vec::new()));
        }
        self.inner.run_hook_opts(
            self.trusted.as_ref(),
            |inst| {
                self.inner
                    .runtime
                    .call_response_body_hook(inst, req, resp, body)
            },
            capture_logs,
        )
    }
}

fn request_outcome(decision: &RequestDecision) -> SpanOutcome {
    SpanOutcome::from(decision)
}

fn response_outcome(decision: &ResponseDecision) -> SpanOutcome {
    SpanOutcome::from(decision)
}

fn request_body_outcome(decision: &RequestBodyDecision) -> SpanOutcome {
    match decision {
        RequestBodyDecision::Continue => SpanOutcome::Continue,
        RequestBodyDecision::Modified(_) => SpanOutcome::Modified,
        RequestBodyDecision::ShortCircuit(_) => SpanOutcome::ShortCircuit,
    }
}

fn response_body_outcome(decision: &ResponseBodyDecision) -> SpanOutcome {
    match decision {
        ResponseBodyDecision::Continue => SpanOutcome::Continue,
        ResponseBodyDecision::Modified(_) => SpanOutcome::Modified,
        ResponseBodyDecision::Replace(_) => SpanOutcome::ShortCircuit,
    }
}
