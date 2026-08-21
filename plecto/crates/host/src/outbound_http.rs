//! The SSRF-guarded outbound connector (ADR 000036).
//!
//! This wires [`OutboundPolicy`] into `wasi:http/outgoing-handler` by implementing
//! [`WasiHttpHooks::send_request`] — the one seam wasmtime-wasi-http gives an embedder to own how an
//! outgoing request is sent. We deliberately do NOT call the crate's default connector, because it
//! dials `TcpStream::connect("host:port")` and never surfaces the resolved IP, so a hostname that
//! passes an allowlist but resolves to `127.0.0.1` / `169.254.169.254` would still be dialed.
//!
//! Our `send_request` enforces, in order:
//!   1. **Allowlist** (sync, deny-by-default) — an unlisted `(scheme, host, port)` is rejected with
//!      no DNS lookup and no socket, as `HttpRequestDenied`.
//!   2. **Concurrency bound** — a per-filter semaphore; over the cap is `ConnectionLimitReached`.
//!   3. **Resolve + classify + pin** (async, under the total deadline) — the host resolves the name
//!      itself, classifies *every* resolved address with the SSRF guard, rejects the whole request
//!      if any is blocked (`DestinationIpProhibited`), and connects to the vetted IP directly. This
//!      closes the DNS-rebinding TOCTOU window. TLS SNI / cert validation still use the original
//!      hostname.
//!   4. **Resource bounds** — connect timeout, whole-call `tokio::time::timeout` (the host-side I/O
//!      deadline epoch interruption cannot provide, ADR 000006 / 000036), a response-body cap, and
//!      a per-frame bound on the response body ([`BodyWithTimeout`]).
//!
//! Every denial reaches the guest as a `wasi:http` `error-code`, never a silent success — fail-closed.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::body::{Body, Frame, SizeHint};
use hyper::{Request, Response, Uri};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time::{Sleep, timeout};
use wasmtime_wasi::runtime::AbortOnDropJoinHandle;
use wasmtime_wasi_http::io::TokioIo;
use wasmtime_wasi_http::p2::bindings::http::types::{DnsErrorPayload, ErrorCode};
use wasmtime_wasi_http::p2::hyper_request_error;
use wasmtime_wasi_http::{Error as WasiHttpError, RequestOptions, WasiBody, WasiHttpHooks};

use crate::outbound::{AddrVerdict, OutboundPolicy, Scheme};
use crate::resolver::Resolver;

/// The companion future returned alongside the response: wasmtime-wasi-http spawns it and retains
/// it for the response body's lifetime, so it is where the connection driver's handle must live.
type IoFuture = Box<dyn Future<Output = Result<(), WasiHttpError>> + Send>;

/// What [`WasiHttpHooks::send_request`] returns: the whole send, resolving to a response plus its
/// [`IoFuture`].
type SendFuture =
    Box<dyn Future<Output = Result<(Response<WasiBody>, IoFuture), WasiHttpError>> + Send>;

/// Per-filter outbound state, held by the loaded filter and shared across its requests. The
/// semaphore lives here so the per-filter concurrency cap is genuinely shared, not per-request.
pub(crate) struct OutboundState {
    policy: Arc<OutboundPolicy>,
    permits: Arc<Semaphore>,
    resolver: Arc<Resolver>,
}

impl OutboundState {
    pub(crate) fn new(policy: OutboundPolicy) -> Self {
        let permits = Arc::new(Semaphore::new(policy.max_concurrent as usize));
        Self {
            policy: Arc::new(policy),
            permits,
            resolver: Arc::new(Resolver::System),
        }
    }

    /// A fresh hooks handle for one request/Store — cheap Arc clones over the shared state.
    pub(crate) fn hooks(&self) -> PlectoHttpHooks {
        PlectoHttpHooks {
            policy: self.policy.clone(),
            permits: self.permits.clone(),
            resolver: self.resolver.clone(),
        }
    }

    #[cfg(test)]
    fn new_with_resolver(policy: OutboundPolicy, resolver: Resolver) -> Self {
        let permits = Arc::new(Semaphore::new(policy.max_concurrent as usize));
        Self {
            policy: Arc::new(policy),
            permits,
            resolver: Arc::new(resolver),
        }
    }
}

/// The `WasiHttpHooks` implementation installed per Store. Enforces [`OutboundPolicy`] at the send
/// seam; keeps every other hook (forbidden headers, body chunking) at the crate default.
pub(crate) struct PlectoHttpHooks {
    policy: Arc<OutboundPolicy>,
    permits: Arc<Semaphore>,
    resolver: Arc<Resolver>,
}

impl PlectoHttpHooks {
    /// A hooks handle that denies every outbound call. Used for filters with no outbound policy —
    /// belt-and-suspenders, since those filters link no `wasi:http` and cannot reach this at all.
    pub(crate) fn deny_all() -> Self {
        Self {
            policy: Arc::new(OutboundPolicy {
                allow: Vec::new(),
                allow_private: Vec::new(),
                connect_timeout: Duration::from_secs(1),
                total_timeout: Duration::from_secs(1),
                max_response_bytes: 1,
                max_concurrent: 1,
            }),
            permits: Arc::new(Semaphore::new(1)),
            resolver: Arc::new(Resolver::System),
        }
    }
}

impl WasiHttpHooks for PlectoHttpHooks {
    fn send_request(
        &mut self,
        request: Request<WasiBody>,
        _options: Option<RequestOptions>,
        fut: Box<dyn Future<Output = Result<(), WasiHttpError>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        Response<WasiBody>,
                        Box<dyn Future<Output = Result<(), WasiHttpError>> + Send>,
                    ),
                    WasiHttpError,
                >,
            > + Send,
    > {
        // `fut` carries a response-processing error back to the request's constructor; on the p2
        // path — the only one this host links — it is always an immediately-`Ok` future, so there
        // is nothing to drive (the crate's own default hook drops it too). The guest's
        // `request-options` are deliberately not consulted either: the operator's `OutboundPolicy`
        // owns every bound, so a filter cannot widen its own deadlines.
        drop(fut);
        // Never returns a trap: denials resolve to `Err(WasiHttpError)`, which wasmtime-wasi-http
        // hands the guest as an `error-code` — the fail-closed, guest-observable outcome.
        self.dispatch(request)
    }
}

impl PlectoHttpHooks {
    fn dispatch(&mut self, request: Request<WasiBody>) -> SendFuture {
        // 48 dropped the `use_tls` flag from the send seam; the scheme now rides on the request
        // URI. Anything but http/https can match no allowlist entry, so refuse it outright.
        let scheme = match request.uri().scheme_str() {
            Some("https") => Scheme::Https,
            Some("http") => Scheme::Http,
            _ => return ready_err(ErrorCode::HttpProtocolError),
        };
        let use_tls = scheme == Scheme::Https;

        let Some((host, port)) = authority_of(&request, use_tls) else {
            return ready_err(ErrorCode::HttpRequestUriInvalid);
        };

        // 1. Allowlist: deny-by-default, before any DNS or socket work.
        if !self.policy.allows(scheme, &host, port) {
            return ready_err(ErrorCode::HttpRequestDenied);
        }

        // 2. Per-filter concurrency bound.
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            return ready_err(ErrorCode::ConnectionLimitReached);
        };

        // 3 + 4. Resolve, classify every address, pin, connect — all under the total deadline.
        let policy = self.policy.clone();
        let resolver = self.resolver.clone();
        let total = policy.total_timeout;
        let connect_timeout = policy.connect_timeout;
        let max_body = policy.max_response_bytes;
        // The operator's total deadline doubles as the response body's per-frame bound: through 47
        // wasmtime-wasi-http wrapped whatever body this connector returned with exactly this value,
        // and 48 dropped that wrapper, so `connect_and_send` now applies it itself.
        let between_bytes = total;

        Box::new(async move {
            let _permit = permit; // held for the whole call, bounding concurrency
            let outcome = timeout(
                total,
                resolve_and_connect(
                    &resolver,
                    &host,
                    port,
                    &policy,
                    use_tls,
                    connect_timeout,
                    max_body,
                    between_bytes,
                    request,
                ),
            )
            .await;
            let (resp, worker) = outcome.unwrap_or(Err(ErrorCode::ConnectionTimeout))?;
            // Park the connection driver's handle in the companion future rather than dropping it:
            // wasmtime-wasi-http keeps that future alive for the body's lifetime and aborts it with
            // the body, which is exactly the driver's required lifetime.
            let io: IoFuture = Box::new(async move {
                worker.await;
                Ok(())
            });
            Ok((resp, io))
        })
    }
}

/// Pure: does EVERY resolved address classify as allowed? A legitimate endpoint resolves only to
/// allowed space; a mix (e.g. a rebinding A-record set) is rejected wholesale (DNS-rebinding
/// TOCTOU guard). No I/O — directly unit-testable against a hand-built address list.
fn all_addresses_allowed(addrs: &[SocketAddr], policy: &OutboundPolicy) -> bool {
    addrs
        .iter()
        .all(|addr| policy.classify(addr.ip()) == AddrVerdict::Allowed)
}

/// Resolve `host`, verify every resolved address against `policy`, then connect to the first one
/// pinned by that resolution (never re-resolving between check and connect). Named so `dispatch`'s
/// spawned closure is a single call, and so this sequence is itself a seam a future test could
/// exercise directly (bypassing `send_request`/`dispatch`'s wasmtime-http plumbing).
#[allow(clippy::too_many_arguments)]
async fn resolve_and_connect(
    resolver: &Resolver,
    host: &str,
    port: u16,
    policy: &OutboundPolicy,
    use_tls: bool,
    connect_timeout: Duration,
    max_response_bytes: u64,
    between_bytes_timeout: Duration,
    request: Request<WasiBody>,
) -> Result<(Response<WasiBody>, AbortOnDropJoinHandle<()>), ErrorCode> {
    let addrs = resolver.resolve(host, port).await.map_err(|e| {
        tracing::debug!(host, port, error = %e, "outbound-http DNS resolution failed");
        dns_err()
    })?;
    let Some(&first_addr) = addrs.first() else {
        return Err(dns_err());
    };
    if !all_addresses_allowed(&addrs, policy) {
        return Err(ErrorCode::DestinationIpProhibited);
    }
    connect_and_send(
        first_addr,
        host,
        use_tls,
        connect_timeout,
        max_response_bytes,
        between_bytes_timeout,
        request,
    )
    .await
}

/// A resolved-error future the guest observes immediately (fail-closed).
fn ready_err(code: ErrorCode) -> SendFuture {
    Box::new(std::future::ready(Err(WasiHttpError::from(code))))
}

fn dns_err() -> ErrorCode {
    ErrorCode::DnsError(DnsErrorPayload {
        rcode: None,
        info_code: None,
    })
}

/// Extract `(host, port)` from a request's authority, applying the scheme's default port.
fn authority_of(request: &Request<WasiBody>, use_tls: bool) -> Option<(String, u16)> {
    let authority = request.uri().authority()?;
    let host = authority.host();
    if host.is_empty() {
        return None;
    }
    let port = authority
        .port_u16()
        .unwrap_or(if use_tls { 443 } else { 80 });
    Some((host.to_string(), port))
}

/// Connect to a pre-vetted, pinned address and send the request. `host` is the ORIGINAL hostname,
/// used only for the `Host` header and TLS SNI / certificate validation — never for connecting
/// (we dial `addr`), so DNS cannot be re-resolved to a different IP between check and connect.
#[allow(clippy::too_many_arguments)]
async fn connect_and_send(
    addr: SocketAddr,
    host: &str,
    use_tls: bool,
    connect_timeout: Duration,
    max_response_bytes: u64,
    between_bytes_timeout: Duration,
    mut request: Request<WasiBody>,
) -> Result<(Response<WasiBody>, AbortOnDropJoinHandle<()>), ErrorCode> {
    // Set the Host header from the authority if the guest didn't. Compute the owned value first so
    // no borrow of `request` is held across the `headers_mut()` insert.
    let host_header = request
        .uri()
        .authority()
        .and_then(|a| hyper::header::HeaderValue::from_str(a.as_str()).ok());
    if !request.headers().contains_key(hyper::header::HOST)
        && let Some(value) = host_header
    {
        request.headers_mut().insert(hyper::header::HOST, value);
    }

    let tcp = timeout(connect_timeout, TcpStream::connect(addr))
        .await
        .map_err(|_| ErrorCode::ConnectionTimeout)?
        .map_err(|_| ErrorCode::ConnectionRefused)?;
    // Best-effort, but not silently discarded (DECREE §3): Nagle staying on is a latency signal.
    if let Err(e) = tcp.set_nodelay(true) {
        tracing::trace!(error = %e, "outbound-http set_nodelay failed");
    }

    let (mut sender, worker) = if use_tls {
        let tls = tls_connect(host, tcp, connect_timeout).await?;
        handshake(TokioIo::new(tls), connect_timeout).await?
    } else {
        handshake(TokioIo::new(tcp), connect_timeout).await?
    };

    // origin-form: an HTTP/1.1 request line carries only path+query, not scheme/authority.
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    *request.uri_mut() = Uri::builder()
        .path_and_query(path)
        .build()
        .map_err(|_| ErrorCode::HttpRequestUriInvalid)?;

    let resp = sender
        .send_request(request)
        .await
        .map_err(hyper_request_error)?;

    // Cap the response body (CWE-770): a filter cannot make the host buffer an unbounded response.
    // The per-frame bound goes outermost, matching where wasmtime-wasi-http used to apply it.
    let resp = resp.map(|body| {
        let capped = Limited::new(body, max_response_bytes as usize)
            .map_err(move |_| WasiHttpError::HttpResponseBodySize(Some(max_response_bytes)))
            .boxed_unsync();
        BodyWithTimeout::new(capped, between_bytes_timeout).boxed_unsync()
    });

    // The caller keeps the connection-driver task alive for the response's lifetime; the handle
    // aborts it on drop, and dropping it here would kill the connection before the body is read.
    Ok((resp, worker))
}

/// Bounds the gap between response-body frames, failing the body with `connection-read-timeout`
/// when one is late. wasmtime-wasi-http wrapped every embedder's response body like this through
/// 47; from 48 the wrapper lives only inside its own default connector, so the host — which owns
/// the connector (ADR 000036) — applies the equivalent bound itself.
struct BodyWithTimeout {
    inner: WasiBody,
    /// The active deadline, re-armed whenever a frame arrives.
    timeout: Pin<Box<Sleep>>,
    /// Whether `timeout` still needs re-arming on the next `poll_frame`.
    reset_sleep: bool,
    /// Longest a frame may take from being first requested to arriving.
    between_bytes_timeout: Duration,
}

impl BodyWithTimeout {
    fn new(inner: WasiBody, between_bytes_timeout: Duration) -> Self {
        Self {
            inner,
            between_bytes_timeout,
            reset_sleep: true,
            timeout: Box::pin(wasmtime_wasi::runtime::with_ambient_tokio_runtime(|| {
                tokio::time::sleep(Duration::from_secs(0))
            })),
        }
    }
}

impl Body for BodyWithTimeout {
    type Data = Bytes;
    type Error = WasiHttpError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, WasiHttpError>>> {
        let me = Pin::into_inner(self);
        if me.reset_sleep {
            me.timeout
                .as_mut()
                .reset(tokio::time::Instant::now() + me.between_bytes_timeout);
            me.reset_sleep = false;
        }
        if me.timeout.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Some(Err(WasiHttpError::ConnectionReadTimeout)));
        }
        let result = Pin::new(&mut me.inner).poll_frame(cx);
        me.reset_sleep = result.is_ready();
        result
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Drive an established connection: spawn the connection future on a background task and return the
/// request sender plus the task handle (whose lifetime must span the response — it aborts on drop).
async fn handshake<S>(
    io: TokioIo<S>,
    connect_timeout: Duration,
) -> Result<
    (
        hyper::client::conn::http1::SendRequest<WasiBody>,
        AbortOnDropJoinHandle<()>,
    ),
    ErrorCode,
>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, conn) = timeout(connect_timeout, hyper::client::conn::http1::handshake(io))
        .await
        .map_err(|_| ErrorCode::ConnectionTimeout)?
        .map_err(hyper_request_error)?;
    let worker = wasmtime_wasi::runtime::spawn(async move {
        // The request side still surfaces its own error via the sender; the driver's error is a
        // diagnostic signal only — logged, not discarded (DECREE §3), and fail-safe either way.
        if let Err(e) = conn.await {
            tracing::trace!(error = %e, "outbound-http connection driver ended with error");
        }
    });
    Ok((sender, worker))
}

/// Build a TLS stream to the pinned TCP socket, validating the certificate against the ORIGINAL
/// hostname (SNI = `host`), not the pinned IP. Uses the workspace's `aws_lc_rs` provider explicitly
/// (ADR 000051) — the same backend as the control-plane TLS stack, so the binary links a single
/// crypto provider instead of `ring` alongside the `sigstore` dependency's own aws-lc-rs.
async fn tls_connect(
    host: &str,
    tcp: TcpStream,
    connect_timeout: Duration,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, ErrorCode> {
    use rustls::pki_types::ServerName;

    let config = client_config()?;
    let connector = tokio_rustls::TlsConnector::from(config);
    let server_name = ServerName::try_from(host.to_string()).map_err(|_| dns_err())?;
    timeout(connect_timeout, connector.connect(server_name, tcp))
        .await
        .map_err(|_| ErrorCode::ConnectionTimeout)?
        .map_err(|_| ErrorCode::TlsProtocolError)
}

/// A process-wide rustls client config (webpki roots, aws_lc_rs provider), built once.
fn client_config() -> Result<Arc<rustls::ClientConfig>, ErrorCode> {
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    if let Some(cfg) = CFG.get() {
        return Ok(cfg.clone());
    }
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| ErrorCode::TlsProtocolError)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let cfg = Arc::new(cfg);
    // Build-then-store-then-fetch in one step: no separate "set, then unwrap the get" — a losing
    // racer's freshly-built `cfg` is simply discarded in favour of whichever thread's `set` won.
    Ok(CFG.get_or_init(|| cfg).clone())
}

/// An empty outgoing body for requests without one (test helper).
#[cfg(test)]
fn empty_out_body() -> WasiBody {
    use http_body_util::Empty;
    Empty::<Bytes>::new().map_err(|e| match e {}).boxed_unsync()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbound::AllowEntry;
    use std::collections::HashMap;
    use std::net::IpAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_policy(allow: Vec<AllowEntry>) -> OutboundPolicy {
        OutboundPolicy {
            allow,
            allow_private: vec![],
            connect_timeout: Duration::from_secs(2),
            total_timeout: Duration::from_secs(5),
            max_response_bytes: 64 * 1024,
            max_concurrent: 8,
        }
    }

    fn allow(host: &str, port: u16, scheme: Scheme) -> AllowEntry {
        AllowEntry {
            scheme,
            host: host.to_string(),
            port,
        }
    }

    fn req(uri: &str) -> Request<WasiBody> {
        Request::builder().uri(uri).body(empty_out_body()).unwrap()
    }

    /// The response-processing future `wasi:http`'s p2 path always passes to `send_request`.
    fn no_io() -> IoFuture {
        Box::new(std::future::ready(Ok(())))
    }

    /// Drive a `send_request` future to its error. Every denial resolves this way — 48 folded the
    /// old ready/pending distinction into a single returned future.
    async fn send_err(fut: SendFuture) -> WasiHttpError {
        match Pin::from(fut).await {
            Ok(_) => panic!("expected an error, got a response"),
            Err(e) => e,
        }
    }

    #[test]
    fn all_addresses_allowed_rejects_a_mix_of_allowed_and_blocked() {
        let policy = test_policy(vec![]); // allow_private empty: no private/loopback range opted in
        struct Case {
            name: &'static str,
            addrs: Vec<IpAddr>,
            want: bool,
        }
        let cases = vec![
            Case {
                name: "empty (vacuous truth)",
                addrs: vec![],
                want: true,
            },
            Case {
                name: "all public",
                addrs: vec!["93.184.216.34".parse().unwrap(), "1.1.1.1".parse().unwrap()],
                want: true,
            },
            Case {
                name: "one loopback among public — rejected wholesale",
                addrs: vec![
                    "93.184.216.34".parse().unwrap(),
                    "127.0.0.1".parse().unwrap(),
                ],
                want: false,
            },
            Case {
                name: "link-local metadata address",
                addrs: vec!["169.254.169.254".parse().unwrap()],
                want: false,
            },
        ];
        for case in cases {
            let addrs: Vec<SocketAddr> = case
                .addrs
                .iter()
                .map(|ip| SocketAddr::new(*ip, 80))
                .collect();
            let got = all_addresses_allowed(&addrs, &policy);
            assert_eq!(got, case.want, "case: {}", case.name);
        }
    }

    /// A minimal HTTP/1.1 server on loopback that returns `body` with a correct content-length.
    async fn spawn_server(body: Vec<u8>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.flush().await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn connect_and_send_plaintext_success() {
        let addr = spawn_server(b"hello".to_vec()).await;
        let request = req(&format!("http://{addr}/"));
        let (resp, _worker) = connect_and_send(
            addr,
            &addr.ip().to_string(),
            false,
            Duration::from_secs(2),
            1024,
            Duration::from_secs(5),
            request,
        )
        .await
        .expect("connect_and_send succeeds");
        assert_eq!(resp.status(), 200);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"hello");
    }

    #[tokio::test]
    async fn response_body_cap_is_enforced() {
        let addr = spawn_server(vec![b'x'; 100]).await;
        let request = req(&format!("http://{addr}/"));
        let (resp, _worker) = connect_and_send(
            addr,
            &addr.ip().to_string(),
            false,
            Duration::from_secs(2),
            10, // cap below the 100-byte body
            Duration::from_secs(5),
            request,
        )
        .await
        .expect("headers arrive before the body is read");
        let collected = resp.into_body().collect().await;
        assert!(collected.is_err(), "a body over the cap must error");
    }

    #[tokio::test]
    async fn response_body_per_frame_timeout_is_enforced() {
        // The host now owns the per-frame bound wasmtime-wasi-http used to apply: a body that
        // stalls mid-stream fails rather than hanging on the guest's behalf.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            // announce 100 bytes, send 1, then stall forever
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nx")
                .await;
            let _ = sock.flush().await;
            std::future::pending::<()>().await;
        });
        let request = req(&format!("http://{addr}/"));
        let (resp, _worker) = connect_and_send(
            addr,
            &addr.ip().to_string(),
            false,
            Duration::from_secs(2),
            1024,
            Duration::from_millis(150),
            request,
        )
        .await
        .expect("headers arrive before the body stalls");
        assert!(matches!(
            resp.into_body().collect().await,
            Err(WasiHttpError::ConnectionReadTimeout)
        ));
    }

    #[tokio::test]
    async fn dispatch_denies_unlisted_host() {
        let policy = test_policy(vec![allow("authz.example.com", 443, Scheme::Https)]);
        let mut hooks = OutboundState::new(policy).hooks();
        let fut = hooks.send_request(req("https://evil.example.com/"), None, no_io());
        assert!(matches!(
            send_err(fut).await,
            WasiHttpError::HttpRequestDenied
        ));
    }

    #[tokio::test]
    async fn dispatch_denies_wrong_scheme_and_port() {
        let policy = test_policy(vec![allow("authz.example.com", 443, Scheme::Https)]);
        let mut hooks = OutboundState::new(policy).hooks();
        // right host, wrong scheme (http vs the allowed https)
        let fut = hooks.send_request(req("http://authz.example.com/"), None, no_io());
        assert!(matches!(
            send_err(fut).await,
            WasiHttpError::HttpRequestDenied
        ));
    }

    #[tokio::test]
    async fn dispatch_denies_a_scheme_outside_http_and_https() {
        // 48 reads the scheme off the request URI; anything else matches no allowlist entry.
        let policy = test_policy(vec![allow("authz.example.com", 443, Scheme::Https)]);
        let mut hooks = OutboundState::new(policy).hooks();
        let fut = hooks.send_request(req("ftp://authz.example.com/"), None, no_io());
        assert!(matches!(
            send_err(fut).await,
            WasiHttpError::HttpProtocolError
        ));
    }

    #[tokio::test]
    async fn dispatch_blocks_host_that_resolves_to_loopback() {
        // The core rebinding defense: an allowlisted NAME that resolves to a blocked IP is rejected
        // on the resolved address, even though a server is listening there.
        let live = spawn_server(b"secret".to_vec()).await;
        let policy = test_policy(vec![allow("authz.internal", live.port(), Scheme::Http)]);
        let resolver = Resolver::Static(HashMap::from([(
            "authz.internal".to_string(),
            vec![IpAddr::from([127, 0, 0, 1])],
        )]));
        let mut hooks = OutboundState::new_with_resolver(policy, resolver).hooks();
        let fut = hooks.send_request(
            req(&format!("http://authz.internal:{}/", live.port())),
            None,
            no_io(),
        );
        assert!(matches!(
            send_err(fut).await,
            WasiHttpError::DestinationIpProhibited
        ));
    }

    #[tokio::test]
    async fn dispatch_denies_over_concurrency_limit() {
        let mut policy = test_policy(vec![allow("authz.internal", 80, Scheme::Http)]);
        policy.max_concurrent = 1;
        let state = OutboundState::new(policy);
        let mut hooks = state.hooks();
        // exhaust the single shared permit
        let _held = state.permits.clone().try_acquire_owned().unwrap();
        let fut = hooks.send_request(req("http://authz.internal/"), None, no_io());
        assert!(matches!(
            send_err(fut).await,
            WasiHttpError::ConnectionLimitReached
        ));
    }
}
