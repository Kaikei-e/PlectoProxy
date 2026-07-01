//! QUIC/UDP transport setup and the h3 connection-accept loop.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use quinn::crypto::rustls::QuicServerConfig;

use super::http3_err;
use super::request::handle_h3_request;
use crate::error::ServerError;
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
/// `handle_h3_request`. A per-connection / per-request error is logged, never fatal.
pub(crate) async fn serve_h3(state: Arc<ServerState>, endpoint: quinn::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        // Count a QUIC connection against the same global cap as TCP.
        let permit = match state.conn_limit.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return, // semaphore closed → stop accepting
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when this connection task ends
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
            loop {
                match h3conn.accept().await {
                    Ok(Some(resolver)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
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
        });
    }
}
