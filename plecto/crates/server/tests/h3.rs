//! E2E (tdd-workflow Phase 0) for HTTP/3 termination (ADR 000016): drive a real **HTTP/3** request
//! through `plecto-server` over QUIC, negotiated via ALPN `h3`. Asserts the QUIC handshake selects
//! h3, then an h3 request routes, runs the chain, and forwards to the (HTTP/1.1) upstream — the
//! request processing path is identical to the TCP slices, only the wire transport differs.
//!
//! A fresh self-signed cert (rcgen) backs the listener; a quinn client that advertises `h3` in its
//! ALPN list drives an `h3` client connection to the proxy's UDP port (same number as the TCP one).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use quinn::crypto::rustls::QuicClientConfig;
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto::aws_lc_rs};

use plecto_control::{Control, Host, Manifest, MemoryStore, ResolvedArtifact};
use plecto_host::test_support::{TestSigner, bound_sbom, filter_hello_component};
use plecto_server::{serve, serve_with_shutdown};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// A fresh self-signed cert for `localhost`, written to a temp dir. Returns the dir (kept alive),
/// the cert + key paths for the manifest, and the cert DER for the client's trust store.
struct TestCert {
    _dir: tempfile::TempDir,
    cert_path: String,
    key_path: String,
    cert_der: CertificateDer<'static>,
}

fn make_cert() -> TestCert {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, generated.cert.pem()).unwrap();
    std::fs::write(&key_path, generated.key_pair.serialize_pem()).unwrap();
    TestCert {
        cert_der: generated.cert.der().clone(),
        cert_path: cert_path.to_str().unwrap().to_string(),
        key_path: key_path.to_str().unwrap().to_string(),
        _dir: dir,
    }
}

/// An HTTP/1.1 upstream that echoes a fixed body — Plecto terminates h3 on the client side but
/// forwards to the upstream over HTTP/1.1 (ADR 000016: upstream stays HTTP/1.1).
async fn echo(_req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::builder()
        .status(200)
        .header("x-from", "upstream")
        .body(Full::new(Bytes::from_static(b"upstream-ok")))
        .unwrap())
}

async fn spawn_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(echo))
                    .await;
            });
        }
    });
    addr
}

/// Like [`spawn_upstream`], but sleeps `delay` on `/slow` (the health probe and everything else
/// stay instant) — so a drain test can hold an h3 request in flight across the shutdown trigger.
async fn spawn_slow_upstream(delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| async move {
                            if req.uri().path() == "/slow" {
                                tokio::time::sleep(delay).await;
                            }
                            echo(req).await
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

/// A manifest declaring filter-hello, a `/api`→echo route, and a default (host-less) `[[tls]]`
/// cert. `extra` is appended — the drain settings under test (`[listen.drain]`, ADR 000059).
fn manifest_toml(upstream: SocketAddr, digest: &str, cert: &TestCert, extra: &str) -> String {
    format!(
        r#"
[[filter]]
id = "fh"
source = "fh"
digest = "{digest}"
isolation = "trusted"

[[upstream]]
name = "echo"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
filters = ["fh"]
upstream = "echo"
strip_prefix = "/api"
[route.match]
path_prefix = "/api"

[[tls]]
cert_path = "{cert_path}"
key_path = "{key_path}"

{extra}
"#,
        cert_path = cert.cert_path,
        key_path = cert.key_path,
    )
}

fn loaded_control(toml: &str) -> Control {
    let component = filter_hello_component();
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(&component).unwrap();
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let mut store = MemoryStore::new();
    let digest = store.insert(
        "fh",
        ResolvedArtifact {
            component,
            component_signature,
            sbom,
            sbom_signature,
        },
    );
    let toml = toml.replace("{digest}", &digest);
    let manifest = Manifest::from_toml(&toml).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Control::load(host, &manifest, Box::new(store)).unwrap()
}

/// Bind the proxy on an ephemeral TCP port and serve. Returns the bound address; the QUIC/UDP
/// listener is bound by `serve` on the SAME port number (ADR 000016).
async fn spawn_proxy(control: Arc<Control>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, listener).await;
    });
    addr
}

/// Like [`spawn_proxy`], but with a oneshot-triggered graceful shutdown (ADR 000039 / 000059) —
/// the drain window comes from the manifest's `[listen.drain]`.
async fn spawn_proxy_with_shutdown(
    control: Arc<Control>,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<anyhow::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(serve_with_shutdown(control, listener, async move {
        let _ = rx.await;
    }));
    (addr, tx, handle)
}

/// A quinn client trusting `root` and offering ALPN `h3`.
fn h3_client_endpoint(root: CertificateDer<'static>) -> quinn::Endpoint {
    let mut roots = RootCertStore::empty();
    roots.add(root).unwrap();
    let mut tls = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let client_config =
        quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).unwrap()));
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client_config);
    endpoint
}

/// What the client got from one HTTP/3 GET `/api/hello`.
struct H3Result {
    status: u16,
    body: String,
}

/// Drive one HTTP/3 GET through the proxy at `proxy` (its UDP port). Panics on any QUIC/h3 error —
/// the E2E is RED until the server binds a QUIC listener and terminates h3.
async fn drive_h3(proxy: SocketAddr, root: CertificateDer<'static>) -> H3Result {
    let endpoint = h3_client_endpoint(root);
    // bound the connect so a missing listener fails fast (RED) instead of hanging.
    let connecting = endpoint.connect(proxy, "localhost").unwrap();
    let conn = tokio::time::timeout(Duration::from_secs(8), connecting)
        .await
        .expect("QUIC connect timed out (no h3 listener?)")
        .expect("QUIC connect failed");

    let (mut driver, mut send_request) = h3::client::new(h3_quinn::Connection::new(conn))
        .await
        .unwrap();
    let drive = tokio::spawn(async move { std::future::poll_fn(|cx| driver.poll_close(cx)).await });

    let req = hyper::http::Request::builder()
        .method("GET")
        .uri("https://localhost/api/hello")
        .body(())
        .unwrap();
    let mut stream = send_request.send_request(req).await.unwrap();
    stream.finish().await.unwrap();

    let resp = stream.recv_response().await.unwrap();
    let status = resp.status().as_u16();
    let mut body = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.unwrap() {
        body.extend_from_slice(chunk.copy_to_bytes(chunk.remaining()).as_ref());
    }
    drop(send_request);
    let _ = drive.await;
    endpoint.wait_idle().await;
    H3Result {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// Drive an h3 request, retrying past the pessimistic-start 503 window (ADR 000017): instances
/// begin unhealthy, so a forward is 503 until the upstream's first health probe lands.
async fn drive_h3_ready(proxy: SocketAddr, root: CertificateDer<'static>) -> H3Result {
    for _ in 0..100 {
        let r = drive_h3(proxy, root.clone()).await;
        if r.status != 503 {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

#[tokio::test]
async fn terminates_h3_then_routes_and_forwards() {
    let cert = make_cert();
    let upstream = spawn_upstream().await;
    let control = loaded_control(&manifest_toml(upstream, "{digest}", &cert, ""));
    let proxy = spawn_proxy(Arc::new(control)).await;

    let r = drive_h3_ready(proxy, cert.cert_der.clone()).await;

    assert_eq!(r.status, 200, "the h3 request routes + forwards 200");
    assert_eq!(
        r.body, "upstream-ok",
        "the upstream body streams back over h3"
    );
}

/// One h3 GET on an already-open request handle: send, finish, read status + full body.
async fn h3_get(
    send_request: &mut h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    path: &str,
) -> Result<(u16, String), Box<dyn std::error::Error + Send + Sync>> {
    let req = hyper::http::Request::builder()
        .method("GET")
        .uri(format!("https://localhost{path}"))
        .body(())?;
    let mut stream = send_request.send_request(req).await?;
    stream.finish().await?;
    let resp = stream.recv_response().await?;
    let status = resp.status().as_u16();
    let mut body = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await? {
        body.extend_from_slice(chunk.copy_to_bytes(chunk.remaining()).as_ref());
    }
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

#[tokio::test]
async fn h3_drain_sends_goaway_completes_inflight_and_rejects_new_requests() {
    // GOAWAY drain (ADR 000059): at shutdown an open h3 connection is told `shutdown(0)` —
    // the in-flight request completes inside the drain window (previously the connection was
    // just closed), NEW requests on that connection fail, and serve returns as soon as the
    // in-flight work is done (well before the 5 s window: the connection task must observe
    // request completion itself, not wait for the window).
    let cert = make_cert();
    let upstream = spawn_slow_upstream(Duration::from_millis(500)).await;
    let control = loaded_control(&manifest_toml(
        upstream,
        "{digest}",
        &cert,
        "[listen.drain]\nwindow_ms = 5000\n",
    ));
    let (proxy, shutdown, server) = spawn_proxy_with_shutdown(Arc::new(control)).await;

    // Warm past the pessimistic-start window on throwaway connections first (ADR 000017).
    let r = drive_h3_ready(proxy, cert.cert_der.clone()).await;
    assert_eq!(r.status, 200);

    // Open the connection under test and hold a /slow request in flight.
    let endpoint = h3_client_endpoint(cert.cert_der.clone());
    let conn = endpoint.connect(proxy, "localhost").unwrap().await.unwrap();
    let (mut driver, mut send_request) = h3::client::new(h3_quinn::Connection::new(conn))
        .await
        .unwrap();
    let drive = tokio::spawn(async move { std::future::poll_fn(|cx| driver.poll_close(cx)).await });

    let inflight = {
        let mut send_request = send_request.clone();
        tokio::spawn(async move { h3_get(&mut send_request, "/api/slow").await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    shutdown.send(()).unwrap();

    let (status, body) = inflight.await.unwrap().expect(
        "the in-flight h3 request must complete during the drain window (GOAWAY, not close)",
    );
    assert_eq!(status, 200, "the drained h3 response is the real one");
    assert_eq!(body, "upstream-ok");

    // The GOAWAY pinned the connection to the accepted requests: a NEW request must fail.
    let refused = tokio::time::timeout(
        Duration::from_secs(3),
        h3_get(&mut send_request, "/api/late"),
    )
    .await
    .expect("a post-GOAWAY request fails fast rather than hanging");
    assert!(
        refused.is_err(),
        "a request sent after GOAWAY must be rejected, got: {refused:?}"
    );

    // Drain completes on request completion, NOT at the window (5 s): serve returns promptly.
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("serve must return once the h3 in-flight request finished, before the window")
        .unwrap()
        .expect("a drained shutdown is a clean (Ok) exit");

    drop(send_request);
    let _ = drive.await;
}

#[tokio::test]
async fn h3_drain_window_cuts_requests_that_outlive_it() {
    // The shared drain window (ADR 000059 decision 4): `[listen.drain] window_ms` bounds the h3
    // path exactly like the TCP one — an h3 request that cannot finish inside the window is cut
    // (fail-closed), and serve returns at the window, not after the upstream's 10 s.
    let cert = make_cert();
    let upstream = spawn_slow_upstream(Duration::from_secs(10)).await;
    let control = loaded_control(&manifest_toml(
        upstream,
        "{digest}",
        &cert,
        "[listen.drain]\nwindow_ms = 200\n",
    ));
    let (proxy, shutdown, server) = spawn_proxy_with_shutdown(Arc::new(control)).await;

    let r = drive_h3_ready(proxy, cert.cert_der.clone()).await;
    assert_eq!(r.status, 200);

    let endpoint = h3_client_endpoint(cert.cert_der.clone());
    let conn = endpoint.connect(proxy, "localhost").unwrap().await.unwrap();
    let (mut driver, send_request) = h3::client::new(h3_quinn::Connection::new(conn))
        .await
        .unwrap();
    let drive = tokio::spawn(async move { std::future::poll_fn(|cx| driver.poll_close(cx)).await });

    let inflight = {
        let mut send_request = send_request.clone();
        tokio::spawn(async move { h3_get(&mut send_request, "/api/slow").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    shutdown.send(()).unwrap();

    let served = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("serve must return once the drain window expires, not after the upstream's 10 s")
        .unwrap();
    served.expect("a window-bounded shutdown is still a clean (Ok) exit");

    let cut = tokio::time::timeout(Duration::from_secs(3), inflight)
        .await
        .expect("the over-window request must be cut, not held open")
        .unwrap();
    assert!(
        cut.is_err(),
        "an h3 request that outlives the drain window is cut, got: {cut:?}"
    );

    drop(send_request);
    let _ = drive.await;
}
