//! Response construction: synthesised short-circuit / fail-closed responses (buffered) and the
//! forwarded response that streams the upstream body back. All three stay total — a hostile filter
//! status / header can never panic the data plane.

use hyper::header::{HeaderName, HeaderValue};
use hyper::{Response, StatusCode};
use plecto_control::{Header, HttpResponse};

use crate::ResponseBody;
use crate::body::full;
use crate::headers::{copy_headers, copy_headers_direct, copy_headers_preserving};

const X_PLECTO_FAULT: HeaderName = HeaderName::from_static("x-plecto-fault");
const RETRY_AFTER: HeaderName = HeaderName::from_static("retry-after");

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
    pub(crate) static CIRCUIT_OPEN: HeaderValue = HeaderValue::from_static("circuit-open");
    pub(crate) static REQUEST_TIMEOUT: HeaderValue = HeaderValue::from_static("request-timeout");
    pub(crate) static UPSTREAM_TIMEOUT: HeaderValue = HeaderValue::from_static("upstream-timeout");
    pub(crate) static UPSTREAM: HeaderValue = HeaderValue::from_static("upstream");
}

/// A synthesised response (short-circuit / fail-closed) → a hyper `Response` with a buffered body.
pub(crate) fn http_response(resp: HttpResponse) -> Response<ResponseBody> {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    copy_headers(builder.headers_mut(), &resp.headers);
    builder.body(full(resp.body)).unwrap_or_else(|_| {
        // builder only errors on an invalid status/header already guarded above; stay total.
        Response::new(full(b"response build error".to_vec()))
    })
}

/// A forwarded response: the chain-edited status + headers, with the upstream body streamed.
/// `original` is the upstream's inbound header map, so headers a response filter left untouched
/// stream back to the client byte-for-byte (P3#6), not via a lossy `string` round-trip. `body` is
/// already boxed into `ResponseBody` — both the real `HyperUpstreamClient` and a test double box
/// their response bodies identically, so this has no transport-specific type to accept.
pub(crate) fn stream_response(
    status: u16,
    headers: &[Header],
    original: &hyper::HeaderMap,
    body: ResponseBody,
) -> Response<ResponseBody> {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    copy_headers_preserving(builder.headers_mut(), headers, original);
    builder
        .body(body)
        .unwrap_or_else(|_| Response::new(full(b"response build error".to_vec())))
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
        .unwrap_or_else(|_| Response::new(full(b"response build error".to_vec())))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, value: &str) -> Header {
        Header {
            name: name.to_string(),
            value: value.to_string(),
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
}
