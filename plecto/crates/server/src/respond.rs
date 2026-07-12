//! Response construction: synthesised short-circuit / fail-closed responses (buffered) and the
//! forwarded response that streams the upstream body back. All three stay total — a hostile filter
//! status / header can never panic the data plane.

use hyper::header::{HeaderName, HeaderValue};
use hyper::{Response, StatusCode};
use plecto_control::{Header, HttpResponse};

use crate::ResponseBody;
use crate::body::full;
use crate::headers::{copy_headers_direct, copy_headers_synth};

const X_PLECTO_FAULT: HeaderName = HeaderName::from_static("x-plecto-fault");
const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");
const X_PLECTO_ERROR_CODE: HeaderName = HeaderName::from_static("x-plecto-error-code");

/// Static `x-plecto-fault` marker values for [`synth`] / [`synth_retry_after`]. `static` (not a
/// bare literal) so every call site passes an already compile-time-validated `&'static
/// HeaderValue` — `synth` itself then has no fallible header-build step left (bp-rust: no
/// hot-path `.expect()`).
pub(crate) mod fault {
    use hyper::header::HeaderValue;

    pub(crate) static BAD_PATH: HeaderValue = HeaderValue::from_static("bad-path");
    pub(crate) static NO_ROUTE: HeaderValue = HeaderValue::from_static("no-route");
    pub(crate) static RATE_LIMITED: HeaderValue = HeaderValue::from_static("rate-limited");
    pub(crate) static NO_HEALTHY_UPSTREAM: HeaderValue =
        HeaderValue::from_static("no-healthy-upstream");
    pub(crate) static BODY_TOO_LARGE: HeaderValue = HeaderValue::from_static("body-too-large");
    pub(crate) static BODY_TIMEOUT: HeaderValue = HeaderValue::from_static("body-timeout");
    pub(crate) static BODY_READ_ERROR: HeaderValue = HeaderValue::from_static("body-read-error");
    pub(crate) static BODY_BUFFER_UNAVAILABLE: HeaderValue =
        HeaderValue::from_static("body-buffer-unavailable");
    pub(crate) static CIRCUIT_OPEN: HeaderValue = HeaderValue::from_static("circuit-open");
    pub(crate) static REQUEST_TIMEOUT: HeaderValue = HeaderValue::from_static("request-timeout");
    pub(crate) static UPSTREAM_TIMEOUT: HeaderValue = HeaderValue::from_static("upstream-timeout");
    pub(crate) static UPSTREAM: HeaderValue = HeaderValue::from_static("upstream");
    pub(crate) static BAD_UPGRADE: HeaderValue = HeaderValue::from_static("bad-upgrade");
    pub(crate) static BAD_CONTENT_LENGTH: HeaderValue =
        HeaderValue::from_static("bad-content-length");
}

/// The total fallback for the "impossible" `Response::builder()` error paths in this module: the
/// inputs are guarded above each use, but if one were ever reached it must fail CLOSED — a plain
/// `Response::new` would default to 200 OK and report a build fault as success.
fn build_error_response() -> Response<ResponseBody> {
    let mut resp = Response::new(full(b"response build error".to_vec()));
    *resp.status_mut() = StatusCode::BAD_GATEWAY;
    resp
}

/// A synthesised response (short-circuit / `replace` / fail-closed) → a hyper `Response` with a
/// buffered body. Guest-supplied `Content-Length` / `Transfer-Encoding` are stripped — the host
/// owns framing for a body it materialised (`full`), so a hostile filter cannot desync the wire
/// length from the bytes we send (ADR 000073 review).
pub(crate) fn http_response(resp: HttpResponse) -> Response<ResponseBody> {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    copy_headers_synth(builder.headers_mut(), &resp.headers);
    // builder only errors on an invalid status/header already guarded above; stay total.
    builder
        .body(full(resp.body))
        .unwrap_or_else(|_| build_error_response())
}

/// Drop an upstream response body without blocking the client path. Small bodies are drained in
/// the background so the pooled upstream connection can be reused; anything over the drain cap
/// (or a drain error) drops the remainder and hyper closes the socket — correct for "we do not
/// want these bytes" after a `replace` / fail-closed synthesised response (ADR 000073 review).
pub(crate) fn discard_upstream_body(mut body: ResponseBody) {
    /// Bytes we are willing to read just to return a connection to the pool. Larger leftovers
    /// are not worth the bandwidth; dropping the body closes the socket instead.
    const DRAIN_CAP: usize = 64 << 10; // 64 KiB
    tokio::spawn(async move {
        use http_body_util::BodyExt;
        let mut drained = 0usize;
        while drained < DRAIN_CAP {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        drained = drained.saturating_add(data.len());
                    }
                }
                Some(Err(_)) | None => break,
            }
        }
        // Over-cap or error: dropping `body` here closes the upstream socket (pool-safe).
    });
}

/// A forwarded response: the chain-edited status + headers, with the upstream body streamed.
/// `body` is already boxed into `ResponseBody` — both the real `HyperUpstreamClient` and a test
/// double box their response bodies identically, so this has no transport-specific type to accept.
///
/// The host owns framing here exactly as it does for synthesised responses: the chain is
/// header-only, so the streamed body's true length is the UPSTREAM's — a chain-supplied
/// `Content-Length` (filter output is untrusted, CLAUDE.md) is stripped and the upstream's
/// original re-issued from `upstream_headers`, so a hostile `modified` decision cannot desync the
/// advertised length from the bytes on the wire (CWE-444 response-desync primitive).
pub(crate) fn stream_response(
    status: u16,
    headers: &[Header],
    upstream_headers: &hyper::HeaderMap,
    body: ResponseBody,
) -> Response<ResponseBody> {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    copy_headers_synth(builder.headers_mut(), headers);
    if let (Some(h), Some(len)) = (
        builder.headers_mut(),
        upstream_headers.get(hyper::header::CONTENT_LENGTH),
    ) {
        h.insert(hyper::header::CONTENT_LENGTH, len.clone());
    }
    builder
        .body(body)
        .unwrap_or_else(|_| build_error_response())
}

/// Stream an upstream response through untouched — the filterless fast path. No contract
/// projection: the status and header bytes forward verbatim, with only the hop-by-hop /
/// `Connection`-named strip applied (`copy_headers_direct`).
pub(crate) fn stream_response_direct(
    status: StatusCode,
    headers: &hyper::HeaderMap,
    body: ResponseBody,
) -> Response<ResponseBody> {
    let mut builder = Response::builder().status(status);
    copy_headers_direct(builder.headers_mut(), headers);
    builder
        .body(body)
        .unwrap_or_else(|_| build_error_response())
}

/// A small fail-closed response with an `x-plecto-fault` marker (404 no-route, 502 upstream).
/// Infallible by construction: builds the `Response` directly (never via the fallible
/// `Response::builder()...body()` path) and only ever inserts a compile-time-checked `fault`.
pub(crate) fn synth(
    status: StatusCode,
    fault: &'static HeaderValue,
    body: &'static [u8],
) -> Response<ResponseBody> {
    let mut resp = Response::new(full(body.to_vec()));
    *resp.status_mut() = status;
    resp.headers_mut().insert(X_PLECTO_FAULT, fault.clone());
    resp
}

/// Like [`synth`] but also carries a `Retry-After` (seconds) hint — for the native rate-limit 429
/// (ADR 000033), where the limiter knows when a token next frees up. `retry_after_secs` is the one
/// genuinely computed value here; a decimal-integer string is always a valid `HeaderValue`, but the
/// fallback keeps this function infallible even if that were ever not the case.
pub(crate) fn synth_retry_after(
    status: StatusCode,
    fault: &'static HeaderValue,
    body: &'static [u8],
    retry_after_secs: u64,
) -> Response<ResponseBody> {
    let mut resp = synth(status, fault, body);
    let retry_after = HeaderValue::from_str(&retry_after_secs.to_string())
        .unwrap_or_else(|_| HeaderValue::from_static("0"));
    resp.headers_mut().insert(RETRY_AFTER, retry_after);
    resp
}

/// Attach a stable `x-plecto-error-code` (ADR 000065 decision 5's PLECTO-E four-tuple) to an
/// already-synthesised fail-closed response. Only the faults with a registered
/// `plecto_control::Diagnostic` get this header — most `x-plecto-fault` values don't (the same
/// selective code-assignment principle rustc's own diagnostics use: reserve a code for the
/// messages that need more than the message). Only the CODE crosses the wire: the cause /
/// suggestion / docs parts render at the operator-facing surfaces (startup, reload log,
/// `plecto validate`), not to an arbitrary client. `diagnostic.code` is one of our own
/// `PLECTO-E` constants (always valid header ASCII), but stay total like the rest of this
/// module rather than trusting that at a distance.
pub(crate) fn with_error_code(
    mut resp: Response<ResponseBody>,
    diagnostic: &plecto_control::Diagnostic,
) -> Response<ResponseBody> {
    let code = HeaderValue::from_str(diagnostic.code)
        .unwrap_or_else(|_| HeaderValue::from_static("PLECTO-E0000"));
    resp.headers_mut().insert(X_PLECTO_ERROR_CODE, code);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> Header {
        Header {
            name: name.to_string(),
            value: value.as_bytes().to_vec(),
        }
    }

    #[test]
    fn http_response_clamps_invalid_status_and_drops_invalid_headers_without_panicking() {
        // A short-circuit / fail-closed response carries a filter-supplied `u16` status and
        // arbitrary headers. An out-of-range status must clamp to 502 (never panic), and an
        // invalid header value must be dropped — the data plane must survive hostile filter output.
        for bad_status in [0u16, 99, 1000] {
            let resp = http_response(HttpResponse {
                status: bad_status,
                headers: vec![],
                body: Vec::new(),
            });
            assert_eq!(
                resp.status(),
                StatusCode::BAD_GATEWAY,
                "an out-of-range status ({bad_status}) clamps to 502"
            );
        }

        // a valid status is preserved; a CRLF-bearing header is dropped, a clean one kept.
        let resp = http_response(HttpResponse {
            status: 403,
            headers: vec![header("x-clean", "ok"), header("x-evil", "a\r\nb")],
            body: b"denied".to_vec(),
        });
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(resp.headers().contains_key("x-clean"));
        assert!(
            !resp.headers().contains_key("x-evil"),
            "an invalid header value is dropped from a synthesised response"
        );
    }

    #[test]
    fn http_response_strips_guest_content_length_so_host_framing_wins() {
        // A hostile replace/short-circuit can claim Content-Length: 9999 while the body is
        // five bytes. The host must strip the guest length and let `full(body)` frame the wire.
        let resp = http_response(HttpResponse {
            status: 200,
            headers: vec![header("content-length", "9999"), header("x-ok", "1")],
            body: b"hello".to_vec(),
        });
        assert!(
            !resp.headers().contains_key("content-length"),
            "guest Content-Length must not survive onto a synthesised response"
        );
        assert!(resp.headers().contains_key("x-ok"));
    }

    #[test]
    fn with_error_code_sets_the_plecto_error_code_header() {
        let resp = with_error_code(
            synth(
                StatusCode::TOO_MANY_REQUESTS,
                &fault::RATE_LIMITED,
                b"limited",
            ),
            &plecto_control::QUOTA_EXCEEDED,
        );
        assert_eq!(
            resp.headers().get("x-plecto-error-code").unwrap(),
            "PLECTO-E0002"
        );
        // `synth`'s own header (x-plecto-fault) is untouched by attaching the code alongside it.
        assert_eq!(
            resp.headers().get("x-plecto-fault").unwrap(),
            "rate-limited"
        );
    }
}
