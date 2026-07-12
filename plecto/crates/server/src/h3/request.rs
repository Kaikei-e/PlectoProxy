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
    // Malformed or conflicting declarations are rejected fail-closed (bp-rust §6: never a
    // second, laxer framing interpretation) — a duplicated `content-length: 0, content-length:
    // 5` must not be read as its first value.
    let resp = match declared_content_length(&parts.headers) {
        Ok(content_length) => {
            let body = H3ReqBody::new(recv, content_length).boxed();
            match proxy_core(state, "https", peer, parts, body).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "h3 fast-path error");
                    synth(StatusCode::BAD_GATEWAY, &fault::UPSTREAM, b"upstream error")
                }
            }
        }
        Err(()) => synth(
            StatusCode::BAD_REQUEST,
            &fault::BAD_CONTENT_LENGTH,
            b"bad content-length",
        ),
    };

    let (rparts, mut rbody) = resp.into_parts();
    send.send_response(Response::from_parts(rparts, ()))
        .await
        .map_err(http3_err)?;
    while let Some(frame) = rbody.frame().await {
        match frame {
            Ok(f) => match f.into_data() {
                Ok(data) => send.send_data(data).await.map_err(http3_err)?,
                Err(f) => {
                    if let Ok(trailers) = f.into_trailers() {
                        // Trailers are the stream's last frame — gRPC's `grpc-status` rides
                        // here; dropping them would lose the call's real outcome.
                        send.send_trailers(trailers).await.map_err(http3_err)?;
                        return Ok(());
                    }
                }
            },
            Err(e) => {
                // A mid-stream upstream body error must NOT present the truncated response as
                // cleanly complete: return WITHOUT `finish()` — quinn resets an unfinished send
                // stream on drop, so the client sees a stream error (like the TCP transports'
                // connection error), not a short success.
                tracing::debug!(error = %e, "h3 response body error; resetting the response stream");
                return Ok(());
            }
        }
    }
    send.finish().await.map_err(http3_err)?;
    Ok(())
}

/// The request's declared `content-length`, validated strictly: absent → `Ok(None)`; one or more
/// values that all parse to the SAME integer → `Ok(Some(n))` (RFC 9110 §8.6 tolerates the
/// repeated identical value); anything else — unparseable, or conflicting duplicates — is `Err`
/// and the caller answers 400. This is the single place h3 interprets the header; `H3ReqBody`
/// then enforces that the DATA frames stay within it.
fn declared_content_length(headers: &hyper::HeaderMap) -> Result<Option<u64>, ()> {
    let mut declared: Option<u64> = None;
    for value in headers.get_all(hyper::header::CONTENT_LENGTH) {
        let parsed = value
            .to_str()
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .ok_or(())?;
        match declared {
            None => declared = Some(parsed),
            Some(existing) if existing == parsed => {}
            Some(_) => return Err(()),
        }
    }
    Ok(declared)
}
