//! The per-request hyper service entry: adapt the inbound body, run `proxy_core`, map an error to
//! a 502, and attach `Alt-Svc` (ADR 000016). Shared by every TCP-based transport (HTTP/1.1, HTTP/2);
//! HTTP/3 has its own request loop (`h3::request`) since its body/response plumbing is QUIC-specific,
//! but funnels into the same `proxy_core`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::{Request, Response, StatusCode};

use crate::ResponseBody;
use crate::ServerState;
use crate::body::box_incoming;
use crate::proxy::proxy_core;
use crate::respond::{fault, synth};

/// The hyper service entry: never fails the connection (a proxy synthesises an error response
/// instead of dropping the socket), so the request handling's errors are mapped to a 502. `scheme`
/// is the connection's wire scheme (`"https"` if TLS-terminated, else `"http"`), surfaced to the
/// chain (ADR 000015).
pub(crate) async fn handle(
    state: Arc<ServerState>,
    scheme: &'static str,
    peer: SocketAddr,
    req: Request<Incoming>,
) -> Result<Response<ResponseBody>, Infallible> {
    // adapt the hyper inbound body (HTTP/1.1 + HTTP/2) into the transport-agnostic `ReqBody`.
    let (parts, incoming) = req.into_parts();
    let mut resp =
        match proxy_core(state.clone(), scheme, peer, parts, box_incoming(incoming)).await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!(error = %e, "fast-path error");
                synth(StatusCode::BAD_GATEWAY, &fault::UPSTREAM, b"upstream error")
            }
        };
    attach_alt_svc(&mut resp, state.alt_svc.as_ref());
    Ok(resp)
}

/// Advertise HTTP/3 on a TCP response (ADR 000016) when a QUIC listener is bound; a no-op
/// otherwise (h3 responses are never passed through here — they're already h3). Pure enough to
/// unit-test directly: given a response and an optional `Alt-Svc` value, does the header end up
/// exactly where expected?
pub(crate) fn attach_alt_svc(resp: &mut Response<ResponseBody>, alt_svc: Option<&HeaderValue>) {
    if let Some(av) = alt_svc {
        resp.headers_mut()
            .insert(hyper::header::ALT_SVC, av.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::full;

    fn resp() -> Response<ResponseBody> {
        Response::new(full(Vec::new()))
    }

    #[test]
    fn attach_alt_svc_sets_the_header_when_configured() {
        let mut r = resp();
        let av = HeaderValue::from_static("h3=\":443\"; ma=86400");
        attach_alt_svc(&mut r, Some(&av));
        assert_eq!(r.headers().get(hyper::header::ALT_SVC), Some(&av));
    }

    #[test]
    fn attach_alt_svc_is_a_noop_when_absent() {
        let mut r = resp();
        attach_alt_svc(&mut r, None);
        assert!(r.headers().get(hyper::header::ALT_SVC).is_none());
    }
}
