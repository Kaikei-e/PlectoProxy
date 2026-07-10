//! The public per-request filter API: [`LoadedFilter`].

use std::time::{Duration, Instant, SystemTime};

use crate::observe;
use crate::pool::{HookResult, LoadedInner, TrustedPool};
use crate::runtime::{WasmtimeInstance, WasmtimeRuntime};
use crate::{
    Hook, HttpRequest, HttpResponse, Isolation, LogLine, RequestBodyDecision, RequestDecision,
    RequestTrace, ResponseDecision, RunError, SpanOutcome,
};

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
}

impl LoadedFilter {
    pub fn isolation(&self) -> Isolation {
        self.inner.isolation
    }

    /// Whether this filter reads the request body — i.e. it exports `on-request-body` (world
    /// `filter-body`). The fast path buffers the body ONLY for a route with at least one such
    /// filter; a route of header-only filters keeps the zero-copy streaming path (ADR 000038 /
    /// ADR 000005 mechanism 2). Detected from the component's exports at load, so it is sound
    /// (fail-closed): a filter cannot read the body without declaring it in the contract.
    pub fn reads_body(&self) -> bool {
        self.inner.runtime.body_export.is_some()
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
        if !self.inner.sink.enabled() {
            return self.run_on_request(req).map_err(|(err, _)| err);
        }
        let start = SystemTime::now();
        let elapsed = Instant::now();
        let result = self.run_on_request(req);
        let outcome = match &result {
            Ok((decision, _)) => SpanOutcome::from(decision),
            Err((err, _)) => SpanOutcome::from(err),
        };
        self.emit_span(
            trace,
            Hook::OnRequest,
            outcome,
            start,
            elapsed.elapsed(),
            &result,
        );
        result.map_err(|(err, _)| err)
    }

    fn run_on_request(&self, req: &HttpRequest) -> HookResult<RequestDecision> {
        self.inner.run_hook(self.trusted.as_ref(), |inst| {
            self.inner.runtime.call_on_request(inst, req)
        })
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
        if !self.inner.sink.enabled() {
            return self.run_on_request_body(body).map_err(|(err, _)| err);
        }
        let start = SystemTime::now();
        let elapsed = Instant::now();
        let result = self.run_on_request_body(body);
        let outcome = match &result {
            Ok((RequestBodyDecision::Continue(_), _)) => SpanOutcome::Continue,
            Ok((RequestBodyDecision::ShortCircuit(_), _)) => SpanOutcome::ShortCircuit,
            Err((err, _)) => SpanOutcome::from(err),
        };
        self.emit_span(
            trace,
            Hook::OnRequestBody,
            outcome,
            start,
            elapsed.elapsed(),
            &result,
        );
        result.map_err(|(err, _)| err)
    }

    fn run_on_request_body(&self, body: &[u8]) -> HookResult<RequestBodyDecision> {
        // Header-only filter: no `on-request-body` export, so the body never enters guest memory.
        // The fast path already skips buffering (`reads_body()` is false); this is the defensive
        // floor — pass the body through unchanged without instantiating anything.
        let Some(idx) = self.inner.runtime.body_export.as_ref() else {
            return Ok((RequestBodyDecision::Continue(body.to_vec()), Vec::new()));
        };
        self.inner.run_hook(self.trusted.as_ref(), |inst| {
            self.inner.runtime.call_body_hook(inst, idx, body)
        })
    }

    /// Build and emit the span for one filter execution (ADR 000009). The filter's host-log
    /// lines become span events whether the call succeeded (`Ok`) or trapped (`Err`) — a `RunError`
    /// still carries whatever logs (including fat-guest stdio, ADR 000063) were recovered from the
    /// failed instance before it was discarded, so a trapping guest's own diagnostic output (e.g.
    /// a panic message) still shows up in this span. Errors never abort emission — telemetry is
    /// best-effort and out of the fail-closed path.
    fn emit_span<T>(
        &self,
        trace: &RequestTrace,
        hook: Hook,
        outcome: SpanOutcome,
        start: SystemTime,
        duration: Duration,
        result: &HookResult<T>,
    ) {
        let logs: &[LogLine] = match result {
            Ok((_, logs)) => logs,
            Err((_, logs)) => logs,
        };
        let span = observe::build_filter_span(
            trace,
            &self.inner.filter_id,
            self.inner.isolation,
            hook,
            outcome,
            start,
            duration,
            logs,
        );
        self.inner.sink.export(&span);
    }

    /// Run the response-side hook for one response. Same fail-closed contract as `on_request`.
    pub fn on_response(
        &self,
        resp: &HttpResponse,
        trace: &RequestTrace,
    ) -> std::result::Result<(ResponseDecision, Vec<LogLine>), RunError> {
        if !self.inner.sink.enabled() {
            return self.run_on_response(resp).map_err(|(err, _)| err);
        }
        let start = SystemTime::now();
        let elapsed = Instant::now();
        let result = self.run_on_response(resp);
        let outcome = match &result {
            Ok((decision, _)) => SpanOutcome::from(decision),
            Err((err, _)) => SpanOutcome::from(err),
        };
        self.emit_span(
            trace,
            Hook::OnResponse,
            outcome,
            start,
            elapsed.elapsed(),
            &result,
        );
        result.map_err(|(err, _)| err)
    }

    fn run_on_response(&self, resp: &HttpResponse) -> HookResult<ResponseDecision> {
        self.inner.run_hook(self.trusted.as_ref(), |inst| {
            self.inner.runtime.call_on_response(inst, resp)
        })
    }
}
