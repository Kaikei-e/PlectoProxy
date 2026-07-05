//! ADR 000009 host observability stage. The native side — never the WASM filter — owns trace
//! state and emits one span per filter execution. We borrow the OpenTelemetry **data model**
//! (the `opentelemetry` API crate's `trace` types: `TraceId` / `SpanId` / `SpanKind` /
//! `Status` / `KeyValue` / `Event`) and define a **sync** [`TelemetrySink`] seam. The async
//! SDK `SpanExporter`, OTLP network export, and the wasi-otel guest contract are all
//! named-deferred — the proxy stays no-tokio for now, and the sink maps to them later (the
//! `config version` of the observability stack).
//!
//! Trace context is host-propagated: a [`RequestTrace`] (created by the chain driver, ADR
//! 000009 "host manages span state across the filter boundary") parents every filter span, so
//! a filter never manages its own trace context — it just runs, and the host times + records
//! it. W3C `traceparent` in/out lets the (future) fast-path server continue an inbound trace.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use opentelemetry::KeyValue;
use opentelemetry::trace::{Event, SpanId, SpanKind, Status, TraceFlags, TraceId};
use parking_lot::Mutex;

use crate::{Isolation, LogLevel, LogLine, RequestDecision, ResponseDecision, RunError};

// --- id generation -------------------------------------------------------------------------
//
// Trace/span ids need only be unique within a run (W3C ids are correlation handles, not
// secrets, so a counter + a once-per-process time seed is enough — a cryptographic
// RandomIdGenerator is a refinement). Ids must be non-zero (OTel treats all-zero as invalid),
// which the `1`-based counter and the time seed guarantee.

static ID_SEED: OnceLock<u64> = OnceLock::new();
static NEXT: AtomicU64 = AtomicU64::new(1);

fn seed() -> u64 {
    *ID_SEED.get_or_init(|| {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1
    })
}

fn next_trace_id() -> TraceId {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&seed().to_be_bytes());
    b[8..].copy_from_slice(&n.to_be_bytes());
    TraceId::from_bytes(b)
}

fn next_span_id() -> SpanId {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    SpanId::from_bytes(n.to_be_bytes())
}

/// The host-owned trace context for one request transaction. Created by the chain driver (the
/// [`ConfigSnapshot`](crate) / fast-path server), it parents every filter span so the whole
/// chain — request side and response side — shares one trace. Cheap to copy.
///
/// `request_span_id` is ALWAYS locally minted — it names the request (SERVER) span Plecto itself
/// owns. An inbound `traceparent`'s span id is kept separately as `parent_span_id` (the remote
/// parent), never reused as our own: emitting a span under an id another process minted would
/// collide with that process's exporter (ADR 000040). This is the standard proxy shape — the
/// upstream and every filter span nest under Plecto's request span.
#[derive(Debug, Clone, Copy)]
pub struct RequestTrace {
    trace_id: TraceId,
    /// The inbound (remote) span this transaction continues under, `None` for a local root.
    parent_span_id: Option<SpanId>,
    request_span_id: SpanId,
    flags: TraceFlags,
}

impl RequestTrace {
    /// Start a fresh, sampled root trace for a request with no inbound context.
    pub fn root() -> Self {
        Self {
            trace_id: next_trace_id(),
            parent_span_id: None,
            request_span_id: next_span_id(),
            flags: TraceFlags::SAMPLED,
        }
    }

    /// Continue an inbound trace from a W3C `traceparent` (`00-{trace}-{span}-{flags}`). The
    /// inbound span becomes this request's REMOTE PARENT; the request span id itself is minted
    /// locally. Returns `None` on any malformed field (fail-soft: a bad header just starts no
    /// continuation, never a panic on untrusted input).
    pub fn from_traceparent(traceparent: &str) -> Option<Self> {
        let mut parts = traceparent.split('-');
        let (version, trace, span, flags) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || version != "00" {
            return None;
        }
        let tb: [u8; 16] = hex::decode(trace).ok()?.try_into().ok()?;
        let sb: [u8; 8] = hex::decode(span).ok()?.try_into().ok()?;
        let trace_id = TraceId::from_bytes(tb);
        let inbound_span_id = SpanId::from_bytes(sb);
        if trace_id == TraceId::INVALID || inbound_span_id == SpanId::INVALID {
            return None;
        }
        let flag_byte = u8::from_str_radix(flags, 16).ok()?;
        Some(Self {
            trace_id,
            parent_span_id: Some(inbound_span_id),
            request_span_id: next_span_id(),
            flags: TraceFlags::new(flag_byte),
        })
    }

    /// Format as a W3C `traceparent` for downstream propagation. Carries the (locally-minted)
    /// request span id, so the upstream's spans nest under Plecto's request span.
    pub fn to_traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            hex::encode(self.trace_id.to_bytes()),
            hex::encode(self.request_span_id.to_bytes()),
            if self.flags.is_sampled() { 1u8 } else { 0u8 },
        )
    }

    pub fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    /// The request (root) span id — the parent of every filter span in this transaction.
    pub fn request_span_id(&self) -> SpanId {
        self.request_span_id
    }

    /// The inbound (remote) span this transaction continues under, or `None` for a local root.
    pub fn parent_span_id(&self) -> Option<SpanId> {
        self.parent_span_id
    }

    pub fn is_sampled(&self) -> bool {
        self.flags.is_sampled()
    }

    /// A fresh child span id for one filter execution under this request.
    pub(crate) fn new_child_span_id(&self) -> SpanId {
        next_span_id()
    }
}

/// What a filter execution resulted in — the union of its intentional `decision` and the
/// `RunError` failure modes, so a span records traps and deadlines as faithfully as a
/// `continue`. Maps to an OTel [`Status`] (the decisions are `Ok`; the faults are `Error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanOutcome {
    Continue,
    Modified,
    ShortCircuit,
    Deadline,
    Trap,
    InstantiateError,
    Unavailable,
}

impl SpanOutcome {
    /// The stable attribute value (`plecto.outcome`).
    pub fn as_str(self) -> &'static str {
        match self {
            SpanOutcome::Continue => "continue",
            SpanOutcome::Modified => "modified",
            SpanOutcome::ShortCircuit => "short-circuit",
            SpanOutcome::Deadline => "deadline",
            SpanOutcome::Trap => "trap",
            SpanOutcome::InstantiateError => "instantiate-error",
            SpanOutcome::Unavailable => "unavailable",
        }
    }

    /// A filter that ran and returned a decision is `Ok`; a `RunError` fault is `Error`.
    pub fn status(self) -> Status {
        match self {
            SpanOutcome::Continue | SpanOutcome::Modified | SpanOutcome::ShortCircuit => Status::Ok,
            SpanOutcome::Deadline
            | SpanOutcome::Trap
            | SpanOutcome::InstantiateError
            | SpanOutcome::Unavailable => Status::Error {
                description: self.as_str().into(),
            },
        }
    }
}

impl From<&RequestDecision> for SpanOutcome {
    fn from(d: &RequestDecision) -> Self {
        match d {
            RequestDecision::Continue => SpanOutcome::Continue,
            RequestDecision::Modified(_) => SpanOutcome::Modified,
            RequestDecision::ShortCircuit(_) => SpanOutcome::ShortCircuit,
        }
    }
}

impl From<&ResponseDecision> for SpanOutcome {
    fn from(d: &ResponseDecision) -> Self {
        match d {
            ResponseDecision::Continue => SpanOutcome::Continue,
            ResponseDecision::Modified(_) => SpanOutcome::Modified,
        }
    }
}

impl From<&RunError> for SpanOutcome {
    fn from(e: &RunError) -> Self {
        match e {
            RunError::Deadline => SpanOutcome::Deadline,
            RunError::Trap(_) => SpanOutcome::Trap,
            RunError::Instantiate(_) => SpanOutcome::InstantiateError,
            RunError::Unavailable => SpanOutcome::Unavailable,
        }
    }
}

/// One filter execution, as a span in the OTel data model. The host builds and emits one of
/// these per `on_request` / `on_response` call; a [`TelemetrySink`] receives it.
#[derive(Debug, Clone)]
pub struct FilterSpan {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_span_id: SpanId,
    /// The filter id (the span name).
    pub name: String,
    pub kind: SpanKind,
    pub start_time: SystemTime,
    pub duration: Duration,
    pub outcome: SpanOutcome,
    /// `filter.id`, `plecto.isolation`, `plecto.outcome`, `plecto.hook`.
    pub attributes: Vec<KeyValue>,
    /// The filter's host-log lines, as span events (this is where dropped logs now land).
    pub events: Vec<Event>,
    /// The transaction's W3C sampled flag. An exporting sink (OTLP, ADR 000040) skips unsampled
    /// spans; the in-process tally sinks count them all (metrics are not sampled).
    pub sampled: bool,
}

impl FilterSpan {
    /// The OTel status derived from the outcome.
    pub fn status(&self) -> Status {
        self.outcome.status()
    }
}

/// Which hook produced a span (an attribute + a help for sinks/tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hook {
    OnRequest,
    OnRequestBody,
    OnResponse,
}

impl Hook {
    pub fn as_str(self) -> &'static str {
        match self {
            Hook::OnRequest => "on-request",
            Hook::OnRequestBody => "on-request-body",
            Hook::OnResponse => "on-response",
        }
    }
}

fn isolation_str(isolation: Isolation) -> &'static str {
    match isolation {
        Isolation::Trusted => "trusted",
        Isolation::Untrusted => "untrusted",
    }
}

fn level_str(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "trace",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

/// Build one filter span (host-internal). `at` is the time the call started; `logs` are the
/// lines the filter emitted via host-log, recorded as span events.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_filter_span(
    trace: &RequestTrace,
    filter_id: &str,
    isolation: Isolation,
    hook: Hook,
    outcome: SpanOutcome,
    start_time: SystemTime,
    duration: Duration,
    logs: &[LogLine],
) -> FilterSpan {
    let events = logs
        .iter()
        .map(|line| {
            Event::new(
                line.message.clone(),
                start_time,
                vec![KeyValue::new("log.level", level_str(line.level))],
                0,
            )
        })
        .collect();
    FilterSpan {
        trace_id: trace.trace_id(),
        span_id: trace.new_child_span_id(),
        parent_span_id: trace.request_span_id(),
        name: filter_id.to_string(),
        kind: SpanKind::Internal,
        start_time,
        duration,
        outcome,
        attributes: vec![
            KeyValue::new("filter.id", filter_id.to_string()),
            KeyValue::new("plecto.isolation", isolation_str(isolation)),
            KeyValue::new("plecto.outcome", outcome.as_str()),
            KeyValue::new("plecto.hook", hook.as_str()),
        ],
        events,
        sampled: trace.is_sampled(),
    }
}

/// Where the host sends each [`FilterSpan`]. Deliberately **sync** (the OTel SDK's
/// `SpanExporter` is async and would pull tokio — ADR 000009 keeps that named-deferred): a sink
/// must not block the data plane. `NoopSink` is the default; an OTLP-mapping sink is added when
/// network export lands.
pub trait TelemetrySink: Send + Sync {
    fn export(&self, span: &FilterSpan);

    /// Whether this sink consumes spans at all. When `false`, the host skips span construction
    /// entirely on the per-request path — `build_filter_span` allocates (name, attributes, one
    /// event per host-log line), which would otherwise be paid per hook call just to be dropped
    /// by `export`. Defaults to `true`; only a sink that discards everything should override.
    fn enabled(&self) -> bool {
        true
    }
}

/// The default: observability off, zero cost.
#[derive(Debug, Default)]
pub struct NoopSink;

impl TelemetrySink for NoopSink {
    fn export(&self, _span: &FilterSpan) {}

    fn enabled(&self) -> bool {
        false
    }
}

/// Cap on spans `InMemorySink` retains (CWE-770). This is a reference/test sink, not a
/// production choice, but nothing in the type system stops a caller from wiring it into a live
/// host's `TelemetrySink` — an unbounded `Vec` here would grow without limit under sustained
/// request volume until the process runs out of memory. At capacity the oldest span is dropped
/// to admit the newest (FIFO), mirroring `OtlpBuffer`'s bounded-queue discipline.
const MAX_RETAINED_SPANS: usize = 10_000;

/// A reference / test sink that retains up to `MAX_RETAINED_SPANS` spans in memory.
#[derive(Debug, Default)]
pub struct InMemorySink {
    spans: Mutex<std::collections::VecDeque<FilterSpan>>,
}

impl InMemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of every span captured so far.
    pub fn spans(&self) -> Vec<FilterSpan> {
        self.spans.lock().iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.spans.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.lock().is_empty()
    }
}

impl TelemetrySink for InMemorySink {
    fn export(&self, span: &FilterSpan) {
        let mut spans = self.spans.lock();
        if spans.len() >= MAX_RETAINED_SPANS {
            spans.pop_front();
        }
        spans.push_back(span.clone());
    }
}

/// Host-aggregated RED-style metrics (ADR 000009 "metrics are host-aggregated"), derived from
/// the span stream in-process: a sink that tallies rather than retains. Errors are the
/// `RunError` outcomes (trap / deadline / instantiate / unavailable); short-circuits are
/// counted separately (a filter blocking is not a fault).
#[derive(Debug, Default)]
pub struct MetricsSink {
    total: AtomicU64,
    errors: AtomicU64,
    short_circuits: AtomicU64,
    duration_nanos: AtomicU64,
}

impl MetricsSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            total: self.total.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            short_circuits: self.short_circuits.load(Ordering::Relaxed),
            total_duration: Duration::from_nanos(self.duration_nanos.load(Ordering::Relaxed)),
        }
    }
}

impl TelemetrySink for MetricsSink {
    fn export(&self, span: &FilterSpan) {
        self.total.fetch_add(1, Ordering::Relaxed);
        match span.outcome {
            SpanOutcome::ShortCircuit => {
                self.short_circuits.fetch_add(1, Ordering::Relaxed);
            }
            SpanOutcome::Deadline
            | SpanOutcome::Trap
            | SpanOutcome::InstantiateError
            | SpanOutcome::Unavailable => {
                self.errors.fetch_add(1, Ordering::Relaxed);
            }
            SpanOutcome::Continue | SpanOutcome::Modified => {}
        }
        self.duration_nanos
            .fetch_add(span.duration.as_nanos() as u64, Ordering::Relaxed);
    }
}

/// A point-in-time read of [`MetricsSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub total: u64,
    pub errors: u64,
    pub short_circuits: u64,
    pub total_duration: Duration,
}

/// Send every span to several sinks (e.g. export + aggregate at once). The host holds one
/// sink; this composes many behind it.
pub struct FanOutSink {
    sinks: Vec<std::sync::Arc<dyn TelemetrySink>>,
}

impl FanOutSink {
    pub fn new(sinks: Vec<std::sync::Arc<dyn TelemetrySink>>) -> Self {
        Self { sinks }
    }
}

impl TelemetrySink for FanOutSink {
    fn export(&self, span: &FilterSpan) {
        for sink in &self.sinks {
            sink.export(span);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(outcome: SpanOutcome, dur_ns: u64) -> FilterSpan {
        let trace = RequestTrace::root();
        build_filter_span(
            &trace,
            "f",
            Isolation::Untrusted,
            Hook::OnRequest,
            outcome,
            SystemTime::now(),
            Duration::from_nanos(dur_ns),
            &[],
        )
    }

    #[test]
    fn traceparent_continuation_keeps_trace_and_flags_but_mints_a_local_span_id() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let t = RequestTrace::from_traceparent(tp).expect("valid traceparent parses");
        assert!(t.is_sampled());
        let inbound = SpanId::from_bytes([0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7]);
        assert_eq!(
            t.parent_span_id(),
            Some(inbound),
            "the inbound span is kept as the REMOTE PARENT"
        );
        assert_ne!(
            t.request_span_id(),
            inbound,
            "the request span id is minted locally, never the caller's (ADR 000040)"
        );
        let out = t.to_traceparent();
        assert!(
            out.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-") && out.ends_with("-01"),
            "trace id + flags are preserved downstream, span id is Plecto's own: {out}"
        );
        assert_eq!(
            out.split('-').nth(2),
            Some(hex::encode(t.request_span_id().to_bytes()).as_str()),
            "the propagated span id is the request span's"
        );
    }

    #[test]
    fn root_trace_has_no_remote_parent() {
        let t = RequestTrace::root();
        assert_eq!(t.parent_span_id(), None);
        assert!(t.is_sampled());
    }

    #[test]
    fn malformed_traceparent_is_none_not_panic() {
        for bad in [
            "",
            "garbage",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", // wrong version
            "00-tooshort-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01", // zero trace id
        ] {
            assert!(
                RequestTrace::from_traceparent(bad).is_none(),
                "malformed {bad:?} must be None"
            );
        }
    }

    #[test]
    fn root_trace_parents_filter_spans_within_one_request() {
        let trace = RequestTrace::root();
        let a = trace.new_child_span_id();
        let b = trace.new_child_span_id();
        assert_ne!(a, b, "each filter span gets a distinct id");
        assert_eq!(
            trace.request_span_id(),
            trace.request_span_id(),
            "the request (parent) span id is stable across the transaction"
        );
    }

    #[test]
    fn outcome_status_maps_decisions_ok_and_faults_error() {
        assert_eq!(SpanOutcome::Continue.status(), Status::Ok);
        assert_eq!(SpanOutcome::ShortCircuit.status(), Status::Ok); // a block is not a fault
        assert!(matches!(SpanOutcome::Trap.status(), Status::Error { .. }));
        assert!(matches!(
            SpanOutcome::Deadline.status(),
            Status::Error { .. }
        ));
    }

    #[test]
    fn metrics_sink_tallies_outcomes_and_latency() {
        let m = MetricsSink::new();
        m.export(&span(SpanOutcome::Continue, 1000));
        m.export(&span(SpanOutcome::ShortCircuit, 2000));
        m.export(&span(SpanOutcome::Trap, 3000));
        let s = m.snapshot();
        assert_eq!(s.total, 3);
        assert_eq!(s.short_circuits, 1);
        assert_eq!(s.errors, 1, "only the trap is a fault");
        assert_eq!(s.total_duration, Duration::from_nanos(6000));
    }

    #[test]
    fn in_memory_sink_retains_spans_with_log_events() {
        let trace = RequestTrace::root();
        let logs = vec![LogLine {
            level: LogLevel::Info,
            message: "hello".to_string(),
        }];
        let sp = build_filter_span(
            &trace,
            "auth",
            Isolation::Trusted,
            Hook::OnRequest,
            SpanOutcome::Continue,
            SystemTime::now(),
            Duration::from_micros(10),
            &logs,
        );
        let sink = InMemorySink::new();
        sink.export(&sp);

        let got = sink.spans();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "auth");
        assert_eq!(got[0].parent_span_id, trace.request_span_id());
        assert_eq!(got[0].trace_id, trace.trace_id());
        assert_eq!(
            got[0].events.len(),
            1,
            "the host-log line became a span event"
        );
    }

    #[test]
    fn in_memory_sink_drops_oldest_past_the_retention_cap() {
        // Regression test (CWE-770): nothing in the type system stops a caller from wiring this
        // reference sink into a live host, so it must not grow without bound under sustained
        // export volume — it drops the oldest span, not the newest, to admit each new one.
        let trace = RequestTrace::root();
        let span_named = |name: &str| {
            build_filter_span(
                &trace,
                name,
                Isolation::Trusted,
                Hook::OnRequest,
                SpanOutcome::Continue,
                SystemTime::now(),
                Duration::from_micros(1),
                &[],
            )
        };
        let sink = InMemorySink::new();
        for i in 0..(MAX_RETAINED_SPANS + 10) {
            sink.export(&span_named(&i.to_string()));
        }
        assert_eq!(sink.len(), MAX_RETAINED_SPANS, "retention is capped");
        let got = sink.spans();
        assert_eq!(
            got.first().unwrap().name,
            "10",
            "the oldest 10 spans were evicted, FIFO"
        );
        assert_eq!(
            got.last().unwrap().name,
            (MAX_RETAINED_SPANS + 9).to_string(),
            "the newest span is retained"
        );
    }
}
