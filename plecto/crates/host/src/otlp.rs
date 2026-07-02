//! ADR 000040: the OTLP trace-export half of host observability. Two pieces, both
//! dependency-less (the same reasoning as the hand-written Prometheus exposition — the trace
//! protos are Stable/frozen in opentelemetry-proto, so field numbers never change):
//!
//! - [`OtlpBuffer`] — a bounded, pull-based span queue. It IS a [`TelemetrySink`] (filter spans
//!   arrive through the existing seam) and exposes [`push`](OtlpBuffer::push) for the fast
//!   path's request span + [`drain`](OtlpBuffer::drain) for an external pump. The producer side
//!   never blocks and never grows unboundedly: at capacity the incoming span is dropped and
//!   counted (drop-newest, the shape every OTel SDK batch processor converges on).
//! - The wire encoding — a hand-written protobuf encoder for
//!   `ExportTraceServiceRequest` (opentelemetry-proto v1, trace signal) and a fail-soft decoder
//!   for the response's `partial_success`. Verified in tests against the official generated
//!   types (`opentelemetry-proto`, dev-dependency only).
//!
//! The network pump itself (batching tick, retry, shutdown flush) lives in the fast-path server
//! — the host stays runtime-free; this module is pure data + bytes.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use opentelemetry::trace::{Event, SpanId, SpanKind, Status, TraceId};
use opentelemetry::{Array, KeyValue, Value};
use parking_lot::Mutex;

use crate::observe::{FilterSpan, TelemetrySink};

/// OTel batch-processor spec default (`OTEL_BSP_MAX_QUEUE_SIZE`).
pub const DEFAULT_QUEUE_CAPACITY: usize = 2048;

/// One span in the OTel data model, ready for OTLP encoding — the union shape behind both a
/// [`FilterSpan`] (INTERNAL, always a local parent) and the fast path's request span (SERVER,
/// possibly a remote parent from an inbound `traceparent`).
#[derive(Debug, Clone)]
pub struct SpanRecord {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    /// `None` for a trace root (the OTLP field is then omitted entirely).
    pub parent_span_id: Option<SpanId>,
    pub name: String,
    pub kind: SpanKind,
    pub start_time: SystemTime,
    pub end_time: SystemTime,
    pub status: Status,
    pub attributes: Vec<KeyValue>,
    pub events: Vec<Event>,
    /// Whether the parent context arrived over the wire (inbound `traceparent`) — sets the
    /// `Span.flags` CONTEXT_IS_REMOTE bit so backends render the service boundary correctly.
    pub remote_parent: bool,
    pub sampled: bool,
}

impl SpanRecord {
    /// Build the fast path's request (SERVER) span — the root every filter span and the
    /// upstream's own spans nest under (ADR 000040). Semconv HTTP-server shape: named by the
    /// method, `Error` status only for 5xx (a 4xx is the client's fault, not the server's).
    #[allow(clippy::too_many_arguments)]
    pub fn request_span(
        trace: &crate::observe::RequestTrace,
        method: &str,
        path: &str,
        scheme: &str,
        status_code: u16,
        start_time: SystemTime,
        duration: std::time::Duration,
    ) -> Self {
        let status = if status_code >= 500 {
            Status::Error {
                description: std::borrow::Cow::Borrowed(""),
            }
        } else {
            Status::Unset
        };
        Self {
            trace_id: trace.trace_id(),
            span_id: trace.request_span_id(),
            parent_span_id: trace.parent_span_id(),
            name: method.to_string(),
            kind: SpanKind::Server,
            start_time,
            end_time: start_time + duration,
            status,
            attributes: vec![
                KeyValue::new("http.request.method", method.to_string()),
                KeyValue::new("url.path", path.to_string()),
                KeyValue::new("url.scheme", scheme.to_string()),
                KeyValue::new("http.response.status_code", i64::from(status_code)),
            ],
            events: vec![],
            remote_parent: trace.parent_span_id().is_some(),
            sampled: trace.is_sampled(),
        }
    }
}

impl From<&FilterSpan> for SpanRecord {
    fn from(span: &FilterSpan) -> Self {
        Self {
            trace_id: span.trace_id,
            span_id: span.span_id,
            parent_span_id: Some(span.parent_span_id),
            name: span.name.clone(),
            kind: span.kind.clone(),
            start_time: span.start_time,
            end_time: span.start_time + span.duration,
            status: span.status(),
            attributes: span.attributes.clone(),
            events: span.events.clone(),
            remote_parent: false,
            sampled: span.sampled,
        }
    }
}

/// The bounded span queue between the sync data plane and the async OTLP pump. Producers
/// ([`TelemetrySink::export`] for filter spans, [`push`](Self::push) for the request span) take
/// one uncontended mutex acquisition; the pump [`drain`](Self::drain)s in batches under the same
/// lock and does all encoding/IO outside it. At capacity the INCOMING span is dropped and
/// tallied — the data plane is never back-pressured by a slow collector (fail-open for
/// telemetry, the inverse of the filter chain's fail-closed).
pub struct OtlpBuffer {
    queue: Mutex<VecDeque<SpanRecord>>,
    capacity: usize,
    dropped: AtomicU64,
    drop_logged: AtomicBool,
}

impl OtlpBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(
                capacity.min(DEFAULT_QUEUE_CAPACITY),
            )),
            capacity,
            dropped: AtomicU64::new(0),
            drop_logged: AtomicBool::new(false),
        }
    }

    /// Enqueue one span; at capacity the span is dropped and counted (never blocks). Logs a
    /// single warning on the first drop (the otel-rust convention — one line, not one per span),
    /// leaving the running total to the dropped-spans counter.
    pub fn push(&self, record: SpanRecord) {
        {
            let mut queue = self.queue.lock();
            if queue.len() < self.capacity {
                queue.push_back(record);
                return;
            }
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
        if !self.drop_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                capacity = self.capacity,
                "OTLP span queue full; dropping spans (further drops are counted, not logged)"
            );
        }
    }

    /// Dequeue up to `max` spans, FIFO. O(batch) under the lock — encoding and network IO
    /// belong to the caller, outside it.
    pub fn drain(&self, max: usize) -> Vec<SpanRecord> {
        let mut queue = self.queue.lock();
        let n = queue.len().min(max);
        queue.drain(..n).collect()
    }

    pub fn len(&self) -> usize {
        self.queue.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }

    /// Spans lost so far: queue-full drops plus batches the pump gave up on (`record_dropped`).
    pub fn dropped_spans(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Tally spans the pump dropped after export gave up (non-retryable failure / retries
    /// exhausted), so one counter covers every loss path.
    pub fn record_dropped(&self, n: u64) {
        self.dropped.fetch_add(n, Ordering::Relaxed);
    }
}

impl Default for OtlpBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_QUEUE_CAPACITY)
    }
}

impl TelemetrySink for OtlpBuffer {
    fn export(&self, span: &FilterSpan) {
        // Honour the W3C sampled flag: an unsampled transaction exports nothing (the caller's
        // SDK already decided). Tally sinks (metrics) see every span; only export skips.
        if !span.sampled {
            return;
        }
        self.push(span.into());
    }
}

// --- OTLP protobuf encoding ------------------------------------------------------------------
//
// Hand-written proto3 wire format against opentelemetry-proto's STABLE trace/common/resource
// protos (field types, numbers and names are frozen — the crate's maturity guarantee). Field
// numbers below cite trace.proto / common.proto / resource.proto / trace_service.proto @ v1.
// proto3 rules honoured here: scalar fields at their default (0 / "" / empty bytes) are
// omitted, EXCEPT inside a oneof (`AnyValue.value`), where presence is explicit.

const WIRE_VARINT: u32 = 0;
const WIRE_I64: u32 = 1;
const WIRE_LEN: u32 = 2;
const WIRE_I32: u32 = 5;

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn put_tag(buf: &mut Vec<u8>, field: u32, wire: u32) {
    put_varint(buf, u64::from((field << 3) | wire));
}

/// A length-delimited field (string / bytes / embedded message). Emits even when empty — callers
/// that want proto3 default-omission guard themselves.
fn put_len(buf: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_tag(buf, field, WIRE_LEN);
    put_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

fn put_str(buf: &mut Vec<u8>, field: u32, s: &str) {
    if !s.is_empty() {
        put_len(buf, field, s.as_bytes());
    }
}

fn put_u64(buf: &mut Vec<u8>, field: u32, v: u64) {
    if v != 0 {
        put_tag(buf, field, WIRE_VARINT);
        put_varint(buf, v);
    }
}

fn put_fixed64(buf: &mut Vec<u8>, field: u32, v: u64) {
    if v != 0 {
        put_tag(buf, field, WIRE_I64);
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

fn put_fixed32(buf: &mut Vec<u8>, field: u32, v: u32) {
    if v != 0 {
        put_tag(buf, field, WIRE_I32);
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

fn unix_nanos(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// common.proto `AnyValue`: oneof value { string_value = 1; bool_value = 2; int_value = 3;
/// double_value = 4; array_value = 5; ... }. Oneof presence is explicit — defaults ARE emitted.
fn encode_any_value(value: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    match value {
        Value::String(s) => put_len(&mut buf, 1, s.as_str().as_bytes()),
        Value::Bool(b) => {
            put_tag(&mut buf, 2, WIRE_VARINT);
            put_varint(&mut buf, u64::from(*b));
        }
        Value::I64(i) => {
            put_tag(&mut buf, 3, WIRE_VARINT);
            put_varint(&mut buf, *i as u64);
        }
        Value::F64(f) => {
            put_tag(&mut buf, 4, WIRE_I64);
            buf.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Value::Array(array) => {
            // ArrayValue { repeated AnyValue values = 1; }
            let mut inner = Vec::new();
            let one = |v: Value| encode_any_value(&v);
            match array {
                Array::Bool(items) => {
                    for b in items {
                        put_len(&mut inner, 1, &one(Value::Bool(*b)));
                    }
                }
                Array::I64(items) => {
                    for i in items {
                        put_len(&mut inner, 1, &one(Value::I64(*i)));
                    }
                }
                Array::F64(items) => {
                    for f in items {
                        put_len(&mut inner, 1, &one(Value::F64(*f)));
                    }
                }
                Array::String(items) => {
                    for s in items {
                        put_len(&mut inner, 1, &one(Value::String(s.clone())));
                    }
                }
                // `Array` is #[non_exhaustive] upstream; a future variant encodes as an empty
                // array rather than breaking the build (telemetry is fail-open).
                _ => {}
            }
            put_len(&mut buf, 5, &inner);
        }
        // `Value` is #[non_exhaustive] upstream; encode unknown kinds as their debug string
        // rather than dropping the attribute silently.
        other => put_len(&mut buf, 1, format!("{other:?}").as_bytes()),
    }
    buf
}

/// common.proto `KeyValue { string key = 1; AnyValue value = 2; }`.
fn encode_key_value(kv: &KeyValue) -> Vec<u8> {
    let mut buf = Vec::new();
    put_str(&mut buf, 1, kv.key.as_str());
    put_len(&mut buf, 2, &encode_any_value(&kv.value));
    buf
}

/// trace.proto `Span.Event { fixed64 time_unix_nano = 1; string name = 2;
/// repeated KeyValue attributes = 3; }`.
fn encode_event(event: &Event) -> Vec<u8> {
    let mut buf = Vec::new();
    put_fixed64(&mut buf, 1, unix_nanos(event.timestamp));
    put_str(&mut buf, 2, &event.name);
    for kv in &event.attributes {
        put_len(&mut buf, 3, &encode_key_value(kv));
    }
    buf
}

/// trace.proto `SpanKind`: UNSPECIFIED = 0, INTERNAL = 1, SERVER = 2, CLIENT = 3,
/// PRODUCER = 4, CONSUMER = 5.
fn kind_value(kind: &SpanKind) -> u64 {
    match kind {
        SpanKind::Internal => 1,
        SpanKind::Server => 2,
        SpanKind::Client => 3,
        SpanKind::Producer => 4,
        SpanKind::Consumer => 5,
    }
}

/// trace.proto `Span.flags` (fixed32): low byte = W3C trace flags (bit 0 sampled); bit 8 =
/// "is_remote is known"; bit 9 = "parent is remote". The host always knows remoteness, so bit 8
/// is always set.
fn span_flags(record: &SpanRecord) -> u32 {
    let sampled = u32::from(record.sampled);
    let remote = if record.remote_parent { 0x200 } else { 0 };
    0x100 | remote | sampled
}

/// trace.proto `Status { string message = 2; StatusCode code = 3; }` (field 1 is reserved).
/// `Unset` yields `None` — the whole Status message is omitted.
fn encode_status(status: &Status) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    match status {
        Status::Unset => return None,
        Status::Ok => put_u64(&mut buf, 3, 1),
        Status::Error { description } => {
            put_str(&mut buf, 2, description);
            put_u64(&mut buf, 3, 2);
        }
    }
    Some(buf)
}

/// trace.proto `Span`: trace_id = 1, span_id = 2, parent_span_id = 4, name = 5, kind = 6,
/// start_time_unix_nano = 7 (fixed64), end_time_unix_nano = 8 (fixed64), attributes = 9,
/// events = 11, status = 15, flags = 16 (fixed32).
fn encode_span(record: &SpanRecord) -> Vec<u8> {
    let mut buf = Vec::new();
    put_len(&mut buf, 1, &record.trace_id.to_bytes());
    put_len(&mut buf, 2, &record.span_id.to_bytes());
    if let Some(parent) = record.parent_span_id {
        put_len(&mut buf, 4, &parent.to_bytes());
    }
    put_str(&mut buf, 5, &record.name);
    put_u64(&mut buf, 6, kind_value(&record.kind));
    put_fixed64(&mut buf, 7, unix_nanos(record.start_time));
    put_fixed64(&mut buf, 8, unix_nanos(record.end_time));
    for kv in &record.attributes {
        put_len(&mut buf, 9, &encode_key_value(kv));
    }
    for event in &record.events {
        put_len(&mut buf, 11, &encode_event(event));
    }
    if let Some(status) = encode_status(&record.status) {
        put_len(&mut buf, 15, &status);
    }
    put_fixed32(&mut buf, 16, span_flags(record));
    buf
}

fn string_attr(key: &str, value: &str) -> Vec<u8> {
    encode_key_value(&KeyValue::new(key.to_string(), value.to_string()))
}

/// Encode one `ExportTraceServiceRequest` (trace_service.proto: `repeated ResourceSpans
/// resource_spans = 1`) carrying every span under a single Resource + InstrumentationScope.
///
/// Resource identity (semconv): `service.name` is the operator-visible name; `telemetry.sdk.*`
/// identifies this hand-written exporter as its own SDK (semconv forbids claiming
/// `opentelemetry` for a non-official implementation).
pub fn encode_traces_request(service_name: &str, spans: &[SpanRecord]) -> Vec<u8> {
    // resource.proto Resource { repeated KeyValue attributes = 1; }
    let mut resource = Vec::new();
    put_len(&mut resource, 1, &string_attr("service.name", service_name));
    put_len(
        &mut resource,
        1,
        &string_attr("telemetry.sdk.name", "plecto"),
    );
    put_len(
        &mut resource,
        1,
        &string_attr("telemetry.sdk.language", "rust"),
    );
    put_len(
        &mut resource,
        1,
        &string_attr("telemetry.sdk.version", env!("CARGO_PKG_VERSION")),
    );

    // common.proto InstrumentationScope { string name = 1; string version = 2; }
    let mut scope = Vec::new();
    put_str(&mut scope, 1, "plecto");
    put_str(&mut scope, 2, env!("CARGO_PKG_VERSION"));

    // trace.proto ScopeSpans { scope = 1; repeated Span spans = 2; }
    let mut scope_spans = Vec::new();
    put_len(&mut scope_spans, 1, &scope);
    for record in spans {
        put_len(&mut scope_spans, 2, &encode_span(record));
    }

    // trace.proto ResourceSpans { resource = 1; repeated ScopeSpans scope_spans = 2; }
    let mut resource_spans = Vec::new();
    put_len(&mut resource_spans, 1, &resource);
    put_len(&mut resource_spans, 2, &scope_spans);

    let mut request = Vec::new();
    put_len(&mut request, 1, &resource_spans);
    request
}

/// A collector's partial rejection, from `ExportTraceServiceResponse.partial_success`
/// (trace_service.proto: `partial_success = 1 { int64 rejected_spans = 1;
/// string error_message = 2; }`). Per the OTLP spec the client MUST NOT retry these — log and
/// move on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialSuccess {
    pub rejected_spans: i64,
    pub error_message: String,
}

/// Fail-soft decode of a 200 response body: `Some` only when the collector populated
/// `partial_success` with actual content. Malformed bytes yield `None` (an exporter never
/// panics on what a network peer sent).
pub fn decode_export_partial_success(body: &[u8]) -> Option<PartialSuccess> {
    let inner = read_fields(body, |field, payload| match (field, payload) {
        (1, FieldPayload::Len(bytes)) => Some(bytes.to_vec()),
        _ => None,
    })?;
    let inner = inner?;
    let mut rejected: i64 = 0;
    let mut message = String::new();
    read_fields(&inner, |field, payload| {
        match (field, payload) {
            (1, FieldPayload::Varint(v)) => rejected = v as i64,
            (2, FieldPayload::Len(bytes)) => {
                message = String::from_utf8_lossy(bytes).into_owned();
            }
            _ => {}
        }
        None::<()>
    })?;
    if rejected == 0 && message.is_empty() {
        return None;
    }
    Some(PartialSuccess {
        rejected_spans: rejected,
        error_message: message,
    })
}

enum FieldPayload<'a> {
    Varint(u64),
    Len(&'a [u8]),
}

/// Walk one protobuf message's fields, calling `visit` per field; returns the first `Some` the
/// visitor yields (wrapped), or `Some(None)` after a clean walk, or `None` on malformed input.
fn read_fields<'a, T>(
    mut bytes: &'a [u8],
    mut visit: impl FnMut(u32, FieldPayload<'a>) -> Option<T>,
) -> Option<Option<T>> {
    while !bytes.is_empty() {
        let (tag, rest) = read_varint(bytes)?;
        let field = (tag >> 3) as u32;
        let wire = (tag & 0x7) as u32;
        bytes = rest;
        let payload = match wire {
            WIRE_VARINT => {
                let (v, rest) = read_varint(bytes)?;
                bytes = rest;
                FieldPayload::Varint(v)
            }
            WIRE_I64 => {
                let v = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
                bytes = bytes.get(8..)?;
                FieldPayload::Varint(v)
            }
            WIRE_LEN => {
                let (len, rest) = read_varint(bytes)?;
                let len = usize::try_from(len).ok()?;
                let payload = rest.get(..len)?;
                bytes = rest.get(len..)?;
                FieldPayload::Len(payload)
            }
            WIRE_I32 => {
                let v = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
                bytes = bytes.get(4..)?;
                FieldPayload::Varint(u64::from(v))
            }
            _ => return None,
        };
        if let Some(t) = visit(field, payload) {
            return Some(Some(t));
        }
    }
    Some(None)
}

fn read_varint(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let mut value: u64 = 0;
    for (i, byte) in bytes.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, bytes.get(i + 1..)?));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use opentelemetry_proto::tonic::collector::trace::v1::{
        ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
    };
    use opentelemetry_proto::tonic::common::v1::any_value::Value as ProtoValue;
    use opentelemetry_proto::tonic::trace::v1::status::StatusCode as ProtoStatusCode;
    use prost::Message;

    use super::*;
    use crate::observe::{Hook, RequestTrace, SpanOutcome, build_filter_span};
    use crate::{Isolation, LogLevel, LogLine};

    fn record(name: &str) -> SpanRecord {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        SpanRecord {
            trace_id: TraceId::from_bytes([1; 16]),
            span_id: SpanId::from_bytes([2; 8]),
            parent_span_id: None,
            name: name.to_string(),
            kind: SpanKind::Server,
            start_time: start,
            end_time: start + Duration::from_millis(5),
            status: Status::Unset,
            attributes: vec![],
            events: vec![],
            remote_parent: false,
            sampled: true,
        }
    }

    /// The oracle: decode our hand-encoded bytes with the OFFICIAL generated types.
    fn decode(bytes: &[u8]) -> ExportTraceServiceRequest {
        ExportTraceServiceRequest::decode(bytes).expect("official decoder accepts our bytes")
    }

    #[test]
    fn round_trips_a_full_span_through_the_official_decoder() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let rec = SpanRecord {
            trace_id: TraceId::from_bytes([0xAB; 16]),
            span_id: SpanId::from_bytes([0xCD; 8]),
            parent_span_id: Some(SpanId::from_bytes([0xEF; 8])),
            name: "GET".to_string(),
            kind: SpanKind::Server,
            start_time: start,
            end_time: start + Duration::from_millis(12),
            status: Status::Error {
                description: "upstream 502".into(),
            },
            attributes: vec![
                KeyValue::new("http.request.method", "GET"),
                KeyValue::new("http.response.status_code", 502_i64),
                KeyValue::new("plecto.pi", 3.5_f64),
                KeyValue::new("plecto.flag", true),
                KeyValue::new(
                    "plecto.list",
                    Value::Array(Array::String(vec!["a".into(), "b".into()])),
                ),
            ],
            events: vec![Event::new(
                "auth denied",
                start,
                vec![KeyValue::new("log.level", "warn")],
                0,
            )],
            remote_parent: true,
            sampled: true,
        };

        let decoded = decode(&encode_traces_request("plecto", &[rec]));

        let resource = decoded.resource_spans[0].resource.as_ref().unwrap();
        let attr = |key: &str| {
            resource
                .attributes
                .iter()
                .find(|kv| kv.key == key)
                .and_then(|kv| kv.value.as_ref())
                .and_then(|v| v.value.as_ref())
                .cloned()
        };
        assert_eq!(
            attr("service.name"),
            Some(ProtoValue::StringValue("plecto".into()))
        );
        assert_eq!(
            attr("telemetry.sdk.name"),
            Some(ProtoValue::StringValue("plecto".into())),
            "a hand-written exporter must not claim to be the official SDK"
        );

        let scope_spans = &decoded.resource_spans[0].scope_spans[0];
        assert_eq!(scope_spans.scope.as_ref().unwrap().name, "plecto");
        let span = &scope_spans.spans[0];
        assert_eq!(span.trace_id, vec![0xAB; 16]);
        assert_eq!(span.span_id, vec![0xCD; 8]);
        assert_eq!(span.parent_span_id, vec![0xEF; 8]);
        assert_eq!(span.name, "GET");
        assert_eq!(span.kind, 2, "SERVER");
        assert_eq!(span.start_time_unix_nano, 1_700_000_000_000_000_000);
        assert_eq!(span.end_time_unix_nano, 1_700_000_000_012_000_000);
        assert_eq!(
            span.flags,
            0x300 | 0x01,
            "remote parent: has_is_remote + is_remote + sampled"
        );

        let span_attr = |key: &str| {
            span.attributes
                .iter()
                .find(|kv| kv.key == key)
                .and_then(|kv| kv.value.as_ref())
                .and_then(|v| v.value.as_ref())
                .cloned()
        };
        assert_eq!(
            span_attr("http.request.method"),
            Some(ProtoValue::StringValue("GET".into()))
        );
        assert_eq!(
            span_attr("http.response.status_code"),
            Some(ProtoValue::IntValue(502))
        );
        assert_eq!(span_attr("plecto.pi"), Some(ProtoValue::DoubleValue(3.5)));
        assert_eq!(span_attr("plecto.flag"), Some(ProtoValue::BoolValue(true)));
        match span_attr("plecto.list") {
            Some(ProtoValue::ArrayValue(arr)) => {
                let items: Vec<_> = arr
                    .values
                    .iter()
                    .filter_map(|v| v.value.as_ref())
                    .cloned()
                    .collect();
                assert_eq!(
                    items,
                    vec![
                        ProtoValue::StringValue("a".into()),
                        ProtoValue::StringValue("b".into())
                    ]
                );
            }
            other => panic!("expected an array attribute, got {other:?}"),
        }

        let status = span.status.as_ref().expect("error status is encoded");
        assert_eq!(status.code, ProtoStatusCode::Error as i32);
        assert_eq!(status.message, "upstream 502");

        assert_eq!(span.events.len(), 1);
        assert_eq!(span.events[0].name, "auth denied");
        assert_eq!(span.events[0].time_unix_nano, 1_700_000_000_000_000_000);
    }

    #[test]
    fn root_span_omits_parent_and_unset_status_and_clears_remote_bit() {
        let decoded = decode(&encode_traces_request("plecto", &[record("root")]));
        let span = &decoded.resource_spans[0].scope_spans[0].spans[0];
        assert!(span.parent_span_id.is_empty(), "no parent field for a root");
        assert!(span.status.is_none(), "Unset status is omitted entirely");
        assert_eq!(span.flags, 0x100 | 0x01, "local parent context, sampled");
    }

    #[test]
    fn filter_span_converts_with_local_parent_and_derived_end_time() {
        let trace = RequestTrace::root();
        let logs = vec![LogLine {
            level: LogLevel::Info,
            message: "hi".to_string(),
        }];
        let start = SystemTime::now();
        let span = build_filter_span(
            &trace,
            "auth",
            Isolation::Trusted,
            Hook::OnRequest,
            SpanOutcome::Continue,
            start,
            Duration::from_micros(250),
            &logs,
        );
        let rec = SpanRecord::from(&span);
        assert_eq!(rec.parent_span_id, Some(trace.request_span_id()));
        assert!(!rec.remote_parent, "a filter span's parent is always local");
        assert_eq!(rec.end_time, start + Duration::from_micros(250));
        assert_eq!(rec.status, Status::Ok);
        assert_eq!(rec.events.len(), 1);

        let decoded = decode(&encode_traces_request("plecto", &[rec]));
        let got = &decoded.resource_spans[0].scope_spans[0].spans[0];
        assert_eq!(got.kind, 1, "INTERNAL");
        assert_eq!(got.name, "auth");
    }

    #[test]
    fn buffer_is_fifo_bounded_and_counts_drops() {
        let buffer = OtlpBuffer::new(2);
        buffer.push(record("a"));
        buffer.push(record("b"));
        buffer.push(record("c")); // over capacity → dropped, counted
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.dropped_spans(), 1);

        let first = buffer.drain(1);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "a", "FIFO");
        assert_eq!(buffer.drain(10).len(), 1);
        assert!(buffer.is_empty());

        buffer.record_dropped(3);
        assert_eq!(buffer.dropped_spans(), 4, "export losses share the counter");
    }

    #[test]
    fn sink_export_skips_unsampled_spans() {
        let buffer = OtlpBuffer::default();
        let trace = RequestTrace::from_traceparent(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
        )
        .expect("valid unsampled traceparent");
        let span = build_filter_span(
            &trace,
            "f",
            Isolation::Untrusted,
            Hook::OnRequest,
            SpanOutcome::Continue,
            SystemTime::now(),
            Duration::from_nanos(1),
            &[],
        );
        buffer.export(&span);
        assert!(buffer.is_empty(), "unsampled spans are not exported");

        let sampled = build_filter_span(
            &RequestTrace::root(),
            "f",
            Isolation::Untrusted,
            Hook::OnRequest,
            SpanOutcome::Continue,
            SystemTime::now(),
            Duration::from_nanos(1),
            &[],
        );
        buffer.export(&sampled);
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn decodes_partial_success_and_ignores_clean_responses() {
        let with_rejection = ExportTraceServiceResponse {
            partial_success: Some(ExportTracePartialSuccess {
                rejected_spans: 7,
                error_message: "bad spans".to_string(),
            }),
        }
        .encode_to_vec();
        assert_eq!(
            decode_export_partial_success(&with_rejection),
            Some(PartialSuccess {
                rejected_spans: 7,
                error_message: "bad spans".to_string(),
            })
        );

        let clean = ExportTraceServiceResponse {
            partial_success: None,
        }
        .encode_to_vec();
        assert_eq!(decode_export_partial_success(&clean), None);
        assert_eq!(decode_export_partial_success(b""), None);
        assert_eq!(
            decode_export_partial_success(&[0xff, 0xff, 0xff]),
            None,
            "malformed bytes are fail-soft"
        );
    }
}
