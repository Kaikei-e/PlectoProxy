//! Structured access logging (Stage A observability, ADR 000009). Opt-in via `[observability]
//! access_log`; one `tracing` event per request on the `plecto::access` target — never `println!`
//! (bp-rust DECREE 8) — so the binary's JSON subscriber renders it as a structured line and an
//! operator can route the `plecto::access` target wherever they like. Disabled by default; the
//! per-request fields are only captured when it is on, so a disabled log costs nothing.

use std::net::IpAddr;
use std::time::Duration;

use plecto_control::RequestTrace;

/// The request fields captured (in `crate::proxy`) BEFORE the transaction core consumes the request
/// parts. Held only while the access log is enabled.
pub(crate) struct Access {
    pub(crate) method: String,
    pub(crate) authority: String,
    pub(crate) path: String,
}

/// Emit one access-log event. Deliberately carries no secrets (no Authorization / Cookie value, and
/// the path without its query string — bp-rust): only method, authority, path, status, duration,
/// client IP and the connection scheme.
///
/// `client` is the address the transaction is attributed to — the connection peer, or the client
/// a declared front proxy named (ADR 000103). It is the same address the re-issued
/// `X-Forwarded-For` and the per-client rate-limit bucket use, so the log agrees with enforcement.
///
/// `trace_id` / `span_id` are emitted UNCONDITIONALLY, not only for sampled transactions (ADR
/// 000099): the ids join this line to whatever downstream sampling did keep, and for an unsampled
/// transaction the line is the only place the transaction is identifiable at all.
pub(crate) fn record(
    scheme: &str,
    client: IpAddr,
    access: &Access,
    status: u16,
    elapsed: Duration,
    trace: &RequestTrace,
) {
    tracing::info!(
        target: "plecto::access",
        client = %client,
        scheme = scheme,
        method = %access.method,
        authority = %access.authority,
        path = %access.path,
        status = status,
        duration_ms = elapsed.as_millis() as u64,
        trace_id = %trace.trace_id(),
        span_id = %trace.request_span_id(),
        "access"
    );
}
