//! The fast-path listener: bind, spawn the health supervisor + the HTTP/3 endpoint, and run the
//! TCP accept loop. Each connection is HTTP/1.1, or HTTP/2 when TLS-ALPN negotiates `h2` (ADR
//! 000015); the per-request handling (route → chain → forward) is shared with all transports via
//! `proxy_core`. Graceful shutdown (ADR 000039 / 000059): when the caller's shutdown future
//! resolves, `/readyz` flips not-ready and the readiness grace elapses (accepts continue, so a
//! front LB can take the replica out of rotation first); then the accept loops stop and every
//! connection is told to finish its in-flight work and close (HTTP/1.1 stops keep-alive, HTTP/2
//! and HTTP/3 send GOAWAY), and connections still open at the drain window are cut.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::header::HeaderValue;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use plecto_control::Control;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use crate::body::MAX_INFLIGHT_BODY_BUFFERS;
use crate::dispatch::handle;
use crate::error::ServerError;
use crate::h3::{build_h3_endpoint, serve_h3};
use crate::health::serve_health_checks;
use crate::metrics::ServerMetrics;
use crate::upstream_client::UpstreamClients;
use crate::{MAX_CONCURRENT_STREAMS, MAX_CONNECTIONS, ServerState, admin};

/// Explicit cap on inbound request header lines. hyper's http1 default (~100) is documented
/// as not API-stable, so pin it — as `MAX_CONCURRENT_STREAMS` already does for h2.
const MAX_HEADERS: usize = 100;
/// How long a connection may take to send its request headers before it is dropped (slowloris on
/// headers). hyper enforces a header-read timeout ONLY when a timer is configured, so the
/// server sets both the timer and this value rather than relying on the (timer-less, inert) default.
const INBOUND_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Default drain window for graceful shutdown (ADR 000039), used when `[listen.drain] window_ms`
/// is not declared (ADR 000059): generous enough for normal in-flight requests (the default
/// per-try upstream timeout is 30 s too) and aligned with the common 30 s termination grace of
/// process supervisors.
pub const DEFAULT_DRAIN_DEADLINE: Duration = Duration::from_secs(30);

/// Serve the fast path on an already-bound `listener` until it errors unrecoverably. Each accepted
/// connection is handled on its own task; the protocol is HTTP/1.1, or HTTP/2 when TLS-ALPN
/// negotiates `h2` (ADR 000015). A per-connection error is logged, not fatal. Bind with
/// `TcpListener::bind` (the caller picks the addr, so a test can use an ephemeral `127.0.0.1:0`
/// and read `local_addr`).
///
/// Public boundary stays `anyhow::Result` (bp-rust: typed errors are a library-internal
/// convention, not a public-API commitment) — the internal `ServerError` is `pub(crate)`, so a
/// caller in another crate could not even name it. `serve_inner` does the typed work.
pub async fn serve(control: Arc<Control>, listener: TcpListener) -> anyhow::Result<()> {
    serve_inner(control, listener, std::future::pending::<()>())
        .await
        .map_err(Into::into)
}

/// Serve like [`serve`], but run the graceful-shutdown sequence when `shutdown` resolves
/// (ADR 000039 / 000059): `/readyz` flips not-ready, accepts continue for the readiness grace
/// (`[listen.drain] readiness_grace_ms`, default zero), then the drain starts and in-flight
/// connections get the drain window (`[listen.drain] window_ms`, default 30 s) before the
/// stragglers are cut.
pub async fn serve_with_shutdown(
    control: Arc<Control>,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send,
) -> anyhow::Result<()> {
    serve_inner(control, listener, shutdown)
        .await
        .map_err(Into::into)
}

async fn serve_inner(
    control: Arc<Control>,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), ServerError> {
    let tcp_addr = listener.local_addr().map_err(ServerError::Bind)?;

    // HTTP/3 (ADR 000016): when QUIC TLS is configured (i.e. there is `[[tls]]`), bind an
    // independent QUIC/UDP listener on the SAME port number as the TCP one and advertise it via
    // `Alt-Svc` on TCP responses. No TLS → no h3 (QUIC requires TLS), and no `Alt-Svc`. The
    // advertised port is `[listen] advertised_port` when set (container port mapping — the
    // PUBLISHED port, field report §3.4), else the bound port.
    let quic_cfg = control.quic_tls_config();
    let advertised_port = control.advertised_port().unwrap_or_else(|| tcp_addr.port());
    let alt_svc = quic_cfg
        .as_ref()
        .and_then(|_| HeaderValue::from_str(&format!("h3=\":{advertised_port}\"; ma=86400")).ok());

    // OTLP export wiring (ADR 000040): grab the buffer + endpoint before `control` moves into
    // the state; the pump task itself is spawned below, once the drain channel exists.
    let otlp_export = control
        .otlp_endpoint()
        .map(str::to_string)
        .zip(control.otlp_buffer());

    // The drain flag (ADR 000039): flipped to `true` exactly once, at shutdown. Every connection
    // task holds a receiver and gracefully closes its connection when it flips; the h3 loops send
    // GOAWAY and stop accepting; spawned upgrade tunnels (ADR 000048) close. `false` for the
    // serving lifetime.
    let (drain_tx, drain_rx) = watch::channel(false);
    // The readiness flag (ADR 000059): flipped to `false` at the shutdown signal, BEFORE the
    // drain — `/readyz` goes 503 while accepts continue, so a front LB can take the replica out
    // of rotation during the readiness grace.
    let (ready_tx, ready_rx) = watch::channel(true);

    let state = Arc::new(ServerState {
        control,
        clients: UpstreamClients::new(),
        alt_svc,
        conn_limit: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        body_buffer_limit: Arc::new(Semaphore::new(MAX_INFLIGHT_BODY_BUFFERS)),
        metrics: Arc::new(ServerMetrics::new()),
        otlp: otlp_export.as_ref().map(|(_, buffer)| buffer.clone()),
        drain: drain_rx.clone(),
        ready: ready_rx,
    });

    // Drain settings (ADR 000059), captured once like the rest of `[listen]` (a reload does not
    // change them): one window shared by every drain path — TCP in-flight, h3 GOAWAY, tunnels.
    let drain_window = state
        .control
        .drain_window()
        .unwrap_or(DEFAULT_DRAIN_DEADLINE);
    let readiness_grace = state.control.readiness_grace();

    // Admin endpoint (Stage A observability, ADR 000009): a SEPARATE listener for `/metrics` +
    // liveness/readiness, bound only when `[observability] admin_addr` is set. A bad address disables
    // it (logged) without affecting the data plane — observability never fails serving closed.
    if let Some(admin_addr) = state.control.admin_addr() {
        match admin_addr.parse::<SocketAddr>() {
            Ok(addr) => {
                tokio::spawn(admin::serve_admin(state.clone(), addr));
            }
            Err(e) => {
                tracing::error!(addr = admin_addr, error = %e, "invalid observability.admin_addr; admin endpoint disabled");
            }
        }
    }

    // Active health checks (ADR 000017): a background supervisor probes each upstream instance and
    // flips its healthy/unhealthy state, so the round-robin in `proxy_core` only ever picks live
    // instances. Spawned like the reload loop — the server owns the task, Control owns the state.
    tokio::spawn(serve_health_checks(state.control.clone()));

    // Periodic DNS re-resolution of hostname upstreams (`resolve_interval_ms`): a second
    // supervisor beside the health checks, swapping each resolving group's endpoint set in place
    // (nginx `resolve` / Envoy STRICT_DNS shape). Idles cheaply when no upstream opts in.
    tokio::spawn(crate::dns::serve_dns_refresh(state.control.clone()));

    // OTLP export pump (ADR 000040): drains the span buffer to the collector. The handle is kept
    // (unlike the fire-and-forget tasks above) so shutdown can await its final flush.
    let otlp_pump = otlp_export.map(|(endpoint, buffer)| {
        tokio::spawn(crate::otlp::serve_otlp_export(
            buffer,
            endpoint,
            drain_rx.clone(),
        ))
    });

    if let Some(cfg) = quic_cfg {
        match build_h3_endpoint(cfg, tcp_addr) {
            Ok(endpoint) => {
                tracing::info!(port = tcp_addr.port(), "HTTP/3 (QUIC) listener bound");
                tokio::spawn(serve_h3(
                    state.clone(),
                    endpoint,
                    drain_rx.clone(),
                    drain_window,
                ));
            }
            // a QUIC bind failure must not take down the TCP fast path; log and serve TCP only.
            Err(e) => {
                tracing::error!(error = %e, "failed to bind HTTP/3 listener; serving TCP only")
            }
        }
    }

    // PROXY protocol v2 (ADR 000057): read once at startup, like the bind itself — `[listen]`
    // is fixed for the process lifetime, a reload does not change it. `Some` = every trusted
    // peer must present a v2 header (and only trusted peers may), `None` = feature off.
    let proxy_trust = state.control.proxy_protocol_trust();
    if proxy_trust.is_some() {
        tracing::info!(
            "PROXY protocol v2 enabled on the TCP listener (the h3/UDP listener is not covered — ADR 000057)"
        );
    }

    // The readiness contract (ADR 000059): the caller's shutdown signal first flips `/readyz`
    // to not-ready, then — while the accept loops keep serving — waits the readiness grace so
    // the front LB stops routing here, and only then lets the drain begin. Zero grace (the
    // default) collapses to "not-ready and drain in the same instant".
    let shutdown = async move {
        shutdown.await;
        let _ = ready_tx.send(false);
        if !readiness_grace.is_zero() {
            tracing::info!(
                grace_ms = readiness_grace.as_millis() as u64,
                "shutdown signal: /readyz is not-ready; accepting through the readiness grace"
            );
            tokio::time::sleep(readiness_grace).await;
        }
    };

    // Connection tasks live in a JoinSet so the drain deadline can cut the stragglers
    // (`abort_all`). Finished tasks are reaped opportunistically in the accept loop below.
    let mut conns = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        // Acquire a connection permit BEFORE accepting: at saturation we stop pulling
        // connections off the backlog (backpressure) rather than spawning tasks without bound. The
        // permit is moved into the connection task and released when it ends.
        let permit = tokio::select! {
            _ = &mut shutdown => break,
            // reap finished connection tasks so the JoinSet does not grow unboundedly
            Some(_) = conns.join_next() => continue,
            permit = state.conn_limit.clone().acquire_owned() => match permit {
                Ok(p) => p,
                Err(_) => return Ok(()), // semaphore closed → stop serving
            },
        };
        let (mut stream, peer) = tokio::select! {
            _ = &mut shutdown => break,
            Some(_) = conns.join_next() => continue, // the permit is re-acquired next iteration
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                // a transient accept error (e.g. fd exhaustion) must not kill the listener.
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    continue;
                }
            },
        };
        // Disable Nagle downstream, symmetric with `upstream_connector`: a streamed response
        // relayed to the client in several writes must not stall on the peer's delayed-ACK timer.
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        // The TLS config is read PER accept (ADR 000014): a reload's new certs apply to new
        // connections, while in-flight ones keep the cert they negotiated with. `None` → plain.
        let tls = state.control.tls_config();
        let drain = drain_rx.clone();
        let proxy_trust = proxy_trust.clone();
        conns.spawn(async move {
            let _permit = permit; // released when this connection task ends
            // PROXY v2 (ADR 000057) sits below TLS: consume (trusted) or peek (untrusted) the
            // header BEFORE the handshake, and let the restored peer replace `peer` for every
            // downstream consumer (rate-limit key, X-Forwarded-*, Maglev SourceIp, access log).
            // Any receipt-rule violation cuts the connection (fail-closed), with the fault code.
            let peer = match &proxy_trust {
                Some(trust) => {
                    match crate::proxy_protocol::resolve_peer(
                        &mut stream,
                        peer,
                        trust,
                        INBOUND_HEADER_READ_TIMEOUT,
                    )
                    .await
                    {
                        Ok(resolved) => resolved,
                        Err(fault) => {
                            tracing::warn!(peer = %peer, fault = %fault, "proxy-protocol: connection rejected");
                            return;
                        }
                    }
                }
                None => peer,
            };
            match tls {
                Some(cfg) => match TlsAcceptor::from(cfg).accept(stream).await {
                    Ok(tls_stream) => {
                        // ALPN picks the protocol: `h2` → HTTP/2, anything else (`http/1.1`, or no
                        // ALPN) → HTTP/1.1 (ADR 000015 — h2 over TLS+ALPN only). The connection
                        // terminated TLS, so the chain sees `https`.
                        let h2 = tls_stream.get_ref().1.alpn_protocol() == Some(b"h2".as_ref());
                        serve_conn(state, TokioIo::new(tls_stream), "https", h2, peer, drain).await;
                    }
                    // a failed TLS handshake (incl. ALPN mismatch) just drops the connection
                    // (fail-closed; nothing is forwarded), it is not a server error.
                    Err(e) => tracing::debug!(error = %e, "TLS handshake failed"),
                },
                // plaintext: HTTP/1.1 only — no h2c / prior-knowledge (ADR 000015). `http` scheme.
                None => serve_conn(state, TokioIo::new(stream), "http", false, peer, drain).await,
            }
        });
    }

    // Graceful shutdown (ADR 000039): stop accepting (drop the listener), flip the drain flag so
    // every connection finishes its in-flight work and closes, then wait for ALL connection
    // permits (TCP + QUIC share `conn_limit`) to come home — bounded by the drain deadline, after
    // which the stragglers are cut.
    drop(listener);
    let _ = drain_tx.send(true);
    let all_drained = state.conn_limit.acquire_many(MAX_CONNECTIONS as u32);
    if tokio::time::timeout(drain_window, all_drained)
        .await
        .is_ok()
    {
        tracing::info!("graceful shutdown: all connections drained");
    } else {
        tracing::warn!(
            deadline_ms = drain_window.as_millis() as u64,
            "graceful shutdown: drain window expired; cutting remaining connections"
        );
        conns.abort_all();
    }
    // Flush the OTLP queue before returning (the spec's Shutdown-includes-ForceFlush): the pump
    // saw the same drain flip and is flushing under its own deadline; give it that long plus a
    // beat, then move on — telemetry never holds the process open indefinitely.
    if let Some(pump) = otlp_pump {
        let _ = tokio::time::timeout(
            crate::otlp::SHUTDOWN_FLUSH_DEADLINE + Duration::from_secs(1),
            pump,
        )
        .await;
    }
    Ok(())
}

/// Resolve when the drain flag flips to `true` — or when the sender is gone (serve returned),
/// which closes the same way. A helper rather than an inline `wait_for` in `select!` arms:
/// `wait_for` yields a `watch::Ref` (a lock guard, not `Send`), and dropping it INSIDE this fn
/// keeps the surrounding connection future `Send` (spawnable).
pub(crate) async fn drained(drain: &mut watch::Receiver<bool>) {
    let _ = drain.wait_for(|d| *d).await;
}

/// Serve one connection: HTTP/2 when `h2` (the ALPN result), HTTP/1.1 otherwise. `scheme` is the
/// connection's wire scheme, passed through to the chain. Request handling (route → chain →
/// forward) is identical across protocols; only the wire framing differs — for h2 the multiplexed
/// streams each become one transaction, capped at `MAX_CONCURRENT_STREAMS` (ADR 000015). When the
/// drain flag flips (ADR 000039), the connection finishes its in-flight requests and closes —
/// hyper's `graceful_shutdown` disables HTTP/1.1 keep-alive (an idle connection closes at once)
/// and sends the HTTP/2 GOAWAY.
async fn serve_conn<I>(
    state: Arc<ServerState>,
    io: I,
    scheme: &'static str,
    h2: bool,
    peer: SocketAddr,
    mut drain: watch::Receiver<bool>,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let service = service_fn(move |req| handle(state.clone(), scheme, peer, req));
    let result = if h2 {
        let conn = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
            .max_concurrent_streams(MAX_CONCURRENT_STREAMS)
            .serve_connection(io, service);
        tokio::pin!(conn);
        tokio::select! {
            res = conn.as_mut() => res,
            // `wait_for` also completes when the sender is gone (serve returned) — same close.
            _ = drained(&mut drain) => {
                conn.as_mut().graceful_shutdown();
                conn.await
            }
        }
    } else {
        let conn = hyper::server::conn::http1::Builder::new()
            // enforce a header-read timeout (slowloris on headers) and an explicit
            // header-count cap. The header-read timeout only fires with a timer configured, so set
            // both rather than relying on hyper's timer-less (inert) default.
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(INBOUND_HEADER_READ_TIMEOUT)
            .max_headers(MAX_HEADERS)
            .serve_connection(io, service)
            // Upgrade support (ADR 000048): without this, the OnUpgrade a 101 fulfils would
            // error and no tunnel could ever splice. h1 only — h2/h3 use (extended) CONNECT.
            .with_upgrades();
        tokio::pin!(conn);
        tokio::select! {
            res = conn.as_mut() => res,
            _ = drained(&mut drain) => {
                conn.as_mut().graceful_shutdown();
                conn.await
            }
        }
    };
    if let Err(e) = result {
        tracing::debug!(error = %e, "connection closed with error");
    }
}
