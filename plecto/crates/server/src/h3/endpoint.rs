//! QUIC/UDP transport setup and the h3 connection-accept loop.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use quinn::crypto::rustls::QuicServerConfig;
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::http3_err;
use super::request::handle_h3_request;
use crate::error::ServerError;
use crate::listener::drained;
use crate::{MAX_CONCURRENT_STREAMS, ServerState};

/// Build the QUIC `Endpoint` for HTTP/3 from control's QUIC TLS config, bound on the same port
/// number as the TCP listener (UDP). Caps concurrent request streams (see below).
pub(crate) fn build_h3_endpoint(
    quic_cfg: Arc<plecto_control::TlsServerConfig>,
    tcp_addr: SocketAddr,
) -> Result<quinn::Endpoint, ServerError> {
    let crypto = QuicServerConfig::try_from(quic_cfg).map_err(http3_err)?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    // Cap concurrent request streams per connection (mirrors ADR 000015's h2 cap): each h3 request
    // is one bidi stream → one chain dispatch, so this bounds one connection's draw on the M1 pool
    // and is defence-in-depth against stream-flood DoS. uni streams (h3 control / QPACK) keep
    // quinn's default. quinn itself enforces QUIC's 3x anti-amplification limit (RFC 9000 §8/§21),
    // so the endpoint can't be turned into a UDP reflector with a spoofed source address.
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(MAX_CONCURRENT_STREAMS.into());
    server_config.transport_config(Arc::new(transport));
    // Same port as the TCP listener, but UDP — an independent protocol namespace.
    let udp_addr = SocketAddr::new(tcp_addr.ip(), tcp_addr.port());
    quinn::Endpoint::server(server_config, udp_addr).map_err(http3_err)
}

/// Accept QUIC connections, set up an h3 connection on each, and drive every request stream through
/// `handle_h3_request`. A per-connection / per-request error is logged, never fatal. When the
/// drain flag flips (graceful shutdown, ADR 000039 / 000059) no new QUIC connections are accepted,
/// each open h3 connection sends GOAWAY and finishes its in-flight requests inside the shared
/// drain window, and the endpoint is closed at the window (cutting the stragglers, fail-closed).
pub(crate) async fn serve_h3(
    state: Arc<ServerState>,
    endpoint: quinn::Endpoint,
    mut drain: watch::Receiver<bool>,
    drain_window: Duration,
) {
    // Connection tasks live in a JoinSet (the TCP accept loop's twin) so the drain below can
    // close the endpoint as soon as they all finish, instead of unconditionally sleeping the
    // full window. Finished tasks are reaped opportunistically in the accept loop.
    let mut conns = JoinSet::new();
    loop {
        // Count a QUIC connection against the same global cap as TCP.
        let permit = tokio::select! {
            _ = drained(&mut drain) => break,
            Some(_) = conns.join_next() => continue,
            permit = state.conn_limit.clone().acquire_owned() => match permit {
                Ok(p) => p,
                Err(_) => return, // semaphore closed → stop accepting
            },
        };
        let incoming = tokio::select! {
            _ = drained(&mut drain) => break,
            Some(_) = conns.join_next() => continue, // the permit is re-acquired next iteration
            incoming = endpoint.accept() => match incoming {
                Some(i) => i,
                None => return, // endpoint closed
            },
        };
        let state = state.clone();
        let drain = drain.clone();
        conns.spawn(async move {
            let _permit = permit; // released when this connection task ends
            serve_h3_connection(state, incoming, drain).await;
        });
    }
    // Drain (ADR 000059): every connection task saw the same flip and sent its GOAWAY. Hold the
    // endpoint open until their in-flight requests finish — bounded by the shared drain window,
    // after which whatever is still open is cut (fail-closed; the TCP side's `abort_all` twin) —
    // then close it and flush the CONNECTION_CLOSE frames so peers learn instead of idling out.
    let all_connections_done = async { while conns.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(drain_window, all_connections_done).await;
    endpoint.close(0u32.into(), b"");
    endpoint.wait_idle().await;
}

/// Serve one h3 connection: accept request streams and spawn each through `handle_h3_request`.
/// On the drain flip, send GOAWAY via `shutdown(0)` — h3 then rejects newer streams with
/// `H3_REQUEST_REJECTED` (safe for the client to retry elsewhere, RFC 9114 §4.1.1) — and keep
/// serving until the in-flight requests complete. Their completion is tracked HERE, in the
/// `requests` JoinSet: h3 0.0.8 only drives `accept()` to `Ok(None)` when the CLIENT closed its
/// side too (its done-check gates on `recv_closing`), so a server-initiated drain waiting on
/// `accept()` alone would hold the connection — and its permit — until the window expires.
async fn serve_h3_connection(
    state: Arc<ServerState>,
    incoming: quinn::Incoming,
    mut drain: watch::Receiver<bool>,
) {
    let conn = match incoming.await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "QUIC connection failed");
            return;
        }
    };
    // the client's address, captured before `conn` is moved into the h3 wrapper — fed to
    // `proxy_core` for X-Forwarded-For (ADR 000018), same as the TCP `accept()` peer.
    let peer = conn.remote_address();
    let mut h3conn = match h3::server::Connection::<h3_quinn::Connection, Bytes>::new(
        h3_quinn::Connection::new(conn),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "h3 connection setup failed");
            return;
        }
    };
    let mut requests = JoinSet::new();
    let mut draining = false;
    loop {
        let accepted = tokio::select! {
            _ = drained(&mut drain), if !draining => {
                // GOAWAY (RFC 9114, ADR 000059): pin the connection to the requests already
                // accepted and let them finish; `accept()` keeps being polled below so h3 can
                // reject late streams.
                draining = true;
                if let Err(e) = h3conn.shutdown(0).await {
                    tracing::debug!(error = %e, "h3 GOAWAY failed; closing the connection");
                    break;
                }
                if requests.is_empty() {
                    break;
                }
                continue;
            }
            Some(_) = requests.join_next(), if !requests.is_empty() => {
                if draining && requests.is_empty() {
                    break; // drain complete: all in-flight requests answered
                }
                continue;
            }
            accepted = h3conn.accept() => accepted,
        };
        match accepted {
            Ok(Some(resolver)) => {
                let state = state.clone();
                requests.spawn(async move {
                    if let Err(e) = handle_h3_request(state, peer, resolver).await {
                        tracing::debug!(error = %e, "h3 request failed");
                    }
                });
            }
            // graceful close (the client sent GOAWAY / closed the connection).
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(error = %e, "h3 accept failed");
                break;
            }
        }
    }
    // A non-drain exit can leave live request tasks (the client vanished mid-request): let them
    // run out on their own rather than aborting them with the JoinSet.
    requests.detach_all();
}
