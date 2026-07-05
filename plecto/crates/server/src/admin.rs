//! The admin endpoint (Stage A observability, ADR 000009): a small, SEPARATE HTTP/1.1 listener —
//! never the data-plane port — exposing Prometheus metrics and liveness/readiness. Bound only when
//! `[observability] admin_addr` is set (off by default), so proxied routes never collide with it and
//! the metrics surface is never reachable by data-plane clients (an Envoy-style admin interface).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::ServerState;

/// Cap on concurrent admin connections. The admin endpoint is opt-in, internal-only (never a
/// data-plane client), and low-volume (a Prometheus scraper / orchestrator probe) — far lower
/// than the data plane's `MAX_CONNECTIONS`, just enough to bound worst-case fan-out rather than
/// leave the accept loop fully unbounded.
const MAX_ADMIN_CONNECTIONS: usize = 64;
/// Same slowloris hardening as the data-plane listener (`listener.rs`): hyper's http1 header-read
/// timeout is inert unless a timer is configured, and the header-count default is undocumented,
/// so both are pinned explicitly here too.
const ADMIN_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);
const ADMIN_MAX_HEADERS: usize = 100;

/// Bind `addr` and serve `/metrics`, `/healthz`, `/readyz` until the listener errors. A bind
/// failure disables the admin endpoint (logged) WITHOUT taking down the data plane — observability
/// is best-effort and never a reason to fail closed on serving traffic.
pub(crate) async fn serve_admin(state: Arc<ServerState>, addr: SocketAddr) {
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(%addr, error = %e, "failed to bind admin endpoint; observability disabled");
            return;
        }
    };
    tracing::info!(%addr, "admin endpoint listening (/metrics /healthz /readyz)");
    let conn_limit = Arc::new(Semaphore::new(MAX_ADMIN_CONNECTIONS));
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "admin accept failed");
                continue;
            }
        };
        // Bound worst-case fan-out (slowloris / connection-flood): block accepting the NEXT
        // connection until a permit frees, rather than spawning unboundedly. `Semaphore` is never
        // closed here, so `Err` is unreachable — matched anyway (data-plane no-panic, bp-rust).
        let Ok(permit) = conn_limit.clone().acquire_owned().await else {
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit; // held for the connection's lifetime; releases on drop
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| admin_handle(state.clone(), req));
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                // header_read_timeout only fires with a timer configured (hyper's timer-less
                // default is inert) — same pattern as the data-plane listener.
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(ADMIN_HEADER_READ_TIMEOUT)
                .max_headers(ADMIN_MAX_HEADERS)
                .serve_connection(io, service)
                .await
            {
                tracing::debug!(error = %e, "admin connection closed with error");
            }
        });
    }
}

/// Route an admin request: `/metrics` renders the Prometheus exposition (native + filter-plane),
/// `/healthz` is liveness (the process is up), `/readyz` is readiness (it is serving). Anything
/// else is 404. The response builder is total (a fallback body on the impossible build error), so
/// the admin path never panics either.
async fn admin_handle(
    state: Arc<ServerState>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let (status, content_type, body) = match req.uri().path() {
        "/metrics" => (
            StatusCode::OK,
            "text/plain; version=0.0.4; charset=utf-8",
            state.metrics.render(
                &state.control.filter_metrics(),
                state.otlp.as_ref().map(|b| (b.dropped_spans(), b.len())),
            ),
        ),
        "/healthz" => (
            StatusCode::OK,
            "text/plain; charset=utf-8",
            "ok\n".to_string(),
        ),
        "/readyz" => (
            StatusCode::OK,
            "text/plain; charset=utf-8",
            "ready\n".to_string(),
        ),
        _ => (
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            "not found\n".to_string(),
        ),
    };
    let response = Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, content_type)
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"admin error\n"))));
    Ok(response)
}
