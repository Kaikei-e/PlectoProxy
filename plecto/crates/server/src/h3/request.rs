//! Per-request HTTP/3 dispatch: split the bidi stream, feed `proxy_core`, stream the response back.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{Response, StatusCode};

use super::body::H3ReqBody;
use super::http3_err;
use crate::ServerState;
use crate::error::ServerError;
use crate::proxy::proxy_core;
use crate::respond::{fault, synth};

/// Handle one HTTP/3 request: split the bidi stream, wrap the recv half as the request body, run
/// the shared `proxy_core` (scheme is always `https` — h3 is always over TLS), then stream the
/// response head + body back over the send half.
pub(super) async fn handle_h3_request(
    state: Arc<ServerState>,
    peer: SocketAddr,
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
) -> Result<(), ServerError> {
    let (req, stream) = resolver.resolve_request().await.map_err(http3_err)?;
    let (mut send, recv) = stream.split();
    let (parts, ()) = req.into_parts();
    // The declared content-length drives the body's size hint: `content-length: 0` makes the
    // bodyless check (and thus upstream retry) work for h3 exactly like TCP. A request with no
    // content-length keeps the default hint — it may stream a body, so it stays non-retryable.
    let content_length = parts
        .headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body = H3ReqBody::new(recv, content_length).boxed();

    let resp = match proxy_core(state, "https", peer, parts, body).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "h3 fast-path error");
            synth(StatusCode::BAD_GATEWAY, &fault::UPSTREAM, b"upstream error")
        }
    };

    let (rparts, mut rbody) = resp.into_parts();
    send.send_response(Response::from_parts(rparts, ()))
        .await
        .map_err(http3_err)?;
    while let Some(frame) = rbody.frame().await {
        match frame {
            Ok(f) => {
                if let Ok(data) = f.into_data() {
                    send.send_data(data).await.map_err(http3_err)?;
                }
            }
            Err(e) => {
                // a mid-stream upstream body error: stop here and finish the stream.
                tracing::debug!(error = %e, "h3 response body error");
                break;
            }
        }
    }
    send.finish().await.map_err(http3_err)?;
    Ok(())
}
