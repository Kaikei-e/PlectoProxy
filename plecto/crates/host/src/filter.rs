//! The public per-request filter API: [`LoadedFilter`].

use std::time::{Duration, Instant, SystemTime};

use crate::observe;
use crate::pool::{HookResult, LoadedInner, TrustedPool};
use crate::runtime::{WasmtimeInstance, WasmtimeRuntime};
use crate::{
    ContractVersion, Hook, HttpRequest, HttpResponse, Isolation, LogLine, RequestBodyDecision,
    RequestDecision, RequestTrace, ResponseBodyDecision, ResponseDecision, RunError, SpanOutcome,
};

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
}

impl LoadedFilter {
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
            Ok((RequestBodyDecision::Continue, _)) => SpanOutcome::Continue,
            Ok((RequestBodyDecision::Modified(_), _)) => SpanOutcome::Modified,
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
        if self.inner.runtime.body_export.is_none() {
            return Ok((RequestBodyDecision::Continue, Vec::new()));
        }
        self.inner.run_hook(self.trusted.as_ref(), |inst| {
            self.inner.runtime.call_body_hook(inst, body)
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
    /// `req` is the as-forwarded request snapshot (ADR 000073) — the request as it left the
    /// request-side chain — passed to a 0.3 guest as `on-response`'s first parameter and
    /// dropped by the 0.1 / 0.2 adapters.
    pub fn on_response(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
        trace: &RequestTrace,
    ) -> std::result::Result<(ResponseDecision, Vec<LogLine>), RunError> {
        if !self.inner.sink.enabled() {
            return self.run_on_response(req, resp).map_err(|(err, _)| err);
        }
        let start = SystemTime::now();
        let elapsed = Instant::now();
        let result = self.run_on_response(req, resp);
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

    fn run_on_response(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
    ) -> HookResult<ResponseDecision> {
        self.inner.run_hook(self.trusted.as_ref(), |inst| {
            self.inner.runtime.call_on_response(inst, req, resp)
        })
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
        if !self.inner.sink.enabled() {
            return self
                .run_on_response_body(req, resp, body)
                .map_err(|(err, _)| err);
        }
        let start = SystemTime::now();
        let elapsed = Instant::now();
        let result = self.run_on_response_body(req, resp, body);
        let outcome = match &result {
            Ok((ResponseBodyDecision::Continue, _)) => SpanOutcome::Continue,
            Ok((ResponseBodyDecision::Modified(_), _)) => SpanOutcome::Modified,
            Ok((ResponseBodyDecision::Replace(_), _)) => SpanOutcome::ShortCircuit,
            Err((err, _)) => SpanOutcome::from(err),
        };
        self.emit_span(
            trace,
            Hook::OnResponseBody,
            outcome,
            start,
            elapsed.elapsed(),
            &result,
        );
        result.map_err(|(err, _)| err)
    }

    fn run_on_response_body(
        &self,
        req: &HttpRequest,
        resp: &HttpResponse,
        body: &[u8],
    ) -> HookResult<ResponseBodyDecision> {
        // No `on-response-body` export: the body never enters guest memory. The fast path already
        // skips buffering for such a route; this is the defensive floor.
        if self.inner.runtime.response_body_export.is_none() {
            return Ok((ResponseBodyDecision::Continue, Vec::new()));
        }
        self.inner.run_hook(self.trusted.as_ref(), |inst| {
            self.inner
                .runtime
                .call_response_body_hook(inst, req, resp, body)
        })
    }
}
