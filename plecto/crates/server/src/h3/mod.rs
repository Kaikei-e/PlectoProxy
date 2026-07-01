//! HTTP/3 (ADR 000016).
//!
//! An independent QUIC/UDP listener terminates HTTP/3, then feeds each request into the SAME
//! `proxy_core` as the TCP path — only the wire transport and the body adapters differ. The request
//! body (the h3 recv stream) is wrapped as an `http_body::Body` so it streams to the upstream, and
//! the response body streams back out over the h3 send stream. RFC 9114 forbids connection-specific
//! headers in HTTP/3 messages; `headers_to_vec`/`copy_headers` already strip the hop-by-hop set both
//! ways, so what we send over h3 is compliant.
//!
//! Split by concern: `endpoint` (QUIC transport setup + accept loop), `request` (per-request
//! dispatch into `proxy_core`), `body` (the recv-stream `Body` adapter).

mod body;
mod endpoint;
mod request;

pub(crate) use endpoint::{build_h3_endpoint, serve_h3};

use crate::error::ServerError;

/// Box any error into [`ServerError::Http3`]. `h3`/`quinn`'s error types don't uniformly implement
/// `std::error::Error + Send + Sync + 'static` for a clean `#[from]` per type, so this one narrow,
/// explicit conversion covers this module's fallible calls instead.
fn http3_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> ServerError {
    ServerError::Http3(Box::new(e))
}
