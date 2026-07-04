//! E2E (tdd-workflow Phase 0) for upstream TLS re-encryption (ADR 000042): drive a real request
//! through `plecto-server` to a **TLS upstream** and assert the forward leg re-encrypts with
//! rustls, negotiates HTTP/2 via ALPN, passes `TE: trailers` through on the h2 path, forwards
//! response trailers end-to-end (the gRPC prerequisite), and fails **closed** when the upstream's
//! certificate does not chain to the configured CA (no insecure bypass exists).
//!
//! The upstream is a fresh rcgen self-signed server for `localhost`; the manifest trusts it via
//! `[upstream.tls] ca_path`. Health probes must also speak TLS (ADR 000042 decision 5) — every
//! readiness poll below implicitly proves that, since an instance only enters rotation after a
//! passing probe.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto::aws_lc_rs};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use plecto_control::{Control, Host, Manifest, MemoryStore};
use plecto_host::test_support::TestSigner;
use plecto_server::serve;

/// A fresh self-signed cert for `localhost` written to a temp dir: the PEM path feeds the
/// manifest (`ca_path` for the upstream leg, `cert_path`/`key_path` for downstream termination),
/// the DER pair builds the in-process TLS upstream / test client trust store.
struct TestCert {
    _dir: tempfile::TempDir,
    cert_pem_path: String,
    key_pem_path: String,
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

fn make_cert() -> TestCert {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cert_pem_path = dir.path().join("cert.pem");
    let key_pem_path = dir.path().join("key.pem");
    std::fs::write(&cert_pem_path, generated.cert.pem()).unwrap();
    std::fs::write(&key_pem_path, generated.key_pair.serialize_pem()).unwrap();
    TestCert {
        cert_der: generated.cert.der().clone(),
        key_der: PrivateKeyDer::try_from(generated.key_pair.serialize_der()).unwrap(),
        cert_pem_path: cert_pem_path.to_str().unwrap().to_string(),
        key_pem_path: key_pem_path.to_str().unwrap().to_string(),
        _dir: dir,
    }
}

/// A response body that streams one data frame then a trailers frame — the shape a gRPC backend
/// produces (`grpc-status` lives ONLY in trailers). Hand-rolled because the test needs precise
/// frame control, not a buffered body.
struct TrailersBody {
    data: Option<Bytes>,
    trailers: Option<HeaderMap>,
}

impl hyper::body::Body for TrailersBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(data) = self.data.take() {
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        if let Some(trailers) = self.trailers.take() {
            return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
        }
        Poll::Ready(None)
    }
}

/// Spawn a TLS upstream (ALPN `[h2, http/1.1]`, h2 preferred) that reports what it observed:
/// `x-upstream-version` (the HTTP version of the forwarded leg) and `x-upstream-te` (the `te`
/// header it received, or `absent`), and answers with a body plus gRPC-style response trailers
/// (`grpc-status: 0`) so the pass-through can be asserted end-to-end.
async fn spawn_tls_upstream(cert: &TestCert) -> SocketAddr {
    let mut config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert_der.clone()], cert.key_der.clone_key())
    .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(stream).await else {
                    return; // an untrusting client aborting the handshake is expected in tests
                };
                let h2 = tls.get_ref().1.alpn_protocol() == Some(b"h2".as_ref());
                let service = service_fn(|req: Request<Incoming>| async move {
                    let te = req
                        .headers()
                        .get("te")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("absent")
                        .to_string();
                    let mut trailers = HeaderMap::new();
                    trailers.insert("grpc-status", "0".parse().unwrap());
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .header("x-upstream-version", format!("{:?}", req.version()))
                            .header("x-upstream-te", te)
                            .body(TrailersBody {
                                data: Some(Bytes::from_static(b"tls-upstream-ok")),
                                trailers: Some(trailers),
                            })
                            .unwrap(),
                    )
                });
                if h2 {
                    let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                } else {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(tls), service)
                        .await;
                }
            });
        }
    });
    addr
}

/// A filterless manifest: one route `/api` → the TLS upstream at `localhost:<port>` trusted via
/// `[upstream.tls] ca_path`, plus an optional downstream `[[tls]]` block.
fn manifest_toml(upstream_port: u16, ca_path: &str, tls_block: &str) -> String {
    format!(
        r#"
[[upstream]]
name = "echo"
addresses = ["localhost:{upstream_port}"]
[upstream.tls]
ca_path = "{ca_path}"
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
upstream = "echo"
strip_prefix = "/api"
[route.match]
path_prefix = "/api"
{tls_block}
"#
    )
}

fn loaded_control(toml: &str) -> Result<Control, plecto_control::ControlError> {
    let manifest = Manifest::from_toml(toml)?;
    let signer = TestSigner::new().unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Control::load(host, &manifest, Box::new(MemoryStore::new()))
}

async fn spawn_proxy(control: Arc<Control>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, listener).await;
    });
    addr
}

/// Plain-HTTP/1.1 GET against the proxy (the downstream leg needs no TLS to prove the UPSTREAM
/// leg re-encrypts). Returns status + response headers + body.
async fn http_get(proxy: SocketAddr, path: &str) -> (StatusCode, HeaderMap, String) {
    let tcp = TcpStream::connect(proxy).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "localhost")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Poll `path` through the proxy until the first passing health probe puts the upstream into
/// rotation (instances start pessimistic, ADR 000017), bounded so a probe that can never succeed
/// (e.g. a prober that cannot speak TLS) fails the test crisply instead of hanging.
async fn get_when_ready(proxy: SocketAddr, path: &str) -> (StatusCode, HeaderMap, String) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let r = http_get(proxy, path).await;
            if r.0 != StatusCode::SERVICE_UNAVAILABLE {
                break r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("upstream never became healthy — the health probe must follow the upstream's scheme")
}

#[tokio::test]
async fn reencrypts_to_a_tls_upstream_and_negotiates_h2_via_alpn() {
    let cert = make_cert();
    let upstream = spawn_tls_upstream(&cert).await;
    let control = loaded_control(&manifest_toml(upstream.port(), &cert.cert_pem_path, "")).unwrap();
    let proxy = spawn_proxy(Arc::new(control)).await;

    let (status, headers, body) = get_when_ready(proxy, "/api/hello").await;

    assert_eq!(status, StatusCode::OK, "the TLS forward leg succeeds");
    assert_eq!(body, "tls-upstream-ok", "the upstream body streams back");
    assert_eq!(
        headers
            .get("x-upstream-version")
            .and_then(|v| v.to_str().ok()),
        Some("HTTP/2.0"),
        "ALPN must negotiate h2 on the upstream leg (protocol selection is ALPN's, ADR 000042)"
    );
}

#[tokio::test]
async fn untrusted_upstream_cert_fails_closed() {
    let upstream_cert = make_cert();
    let other_ca = make_cert(); // a CA the upstream's cert does NOT chain to
    let upstream = spawn_tls_upstream(&upstream_cert).await;
    let control =
        loaded_control(&manifest_toml(upstream.port(), &other_ca.cert_pem_path, "")).unwrap();
    let proxy = spawn_proxy(Arc::new(control)).await;

    // The health probe itself verifies the server certificate, so the instance must never enter
    // rotation: every request stays 503 (fail-closed) — never a plaintext or unverified forward.
    for _ in 0..10 {
        let (status, _, _) = http_get(proxy, "/api/hello").await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "an upstream whose cert does not chain to ca_path must stay out of rotation"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// The gRPC prerequisite end-to-end (ADR 000042 decision 3): an h2 client sends `te: trailers`
/// through the proxy to an h2 TLS upstream; the upstream must SEE `te: trailers` (not have it
/// stripped as hop-by-hop), and its response trailers must arrive back at the client.
#[tokio::test]
async fn te_trailers_and_response_trailers_pass_through_on_the_h2_path() {
    let upstream_cert = make_cert();
    let downstream_cert = make_cert();
    let upstream = spawn_tls_upstream(&upstream_cert).await;
    let tls_block = format!(
        "\n[[tls]]\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
        downstream_cert.cert_pem_path, downstream_cert.key_pem_path
    );
    let control = loaded_control(&manifest_toml(
        upstream.port(),
        &upstream_cert.cert_pem_path,
        &tls_block,
    ))
    .unwrap();
    let proxy = spawn_proxy(Arc::new(control)).await;

    // h2 client over TLS to the proxy (trailers need h2 on the downstream leg too).
    let mut roots = RootCertStore::empty();
    roots.add(downstream_cert.cert_der.clone()).unwrap();
    let mut config = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let connector = TlsConnector::from(Arc::new(config));

    let send_h2 = || async {
        let tcp = TcpStream::connect(proxy).await.unwrap();
        let tls = connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap();
        let (mut sender, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls))
                .await
                .unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .method("POST")
            .uri("https://localhost/api/grpc.Echo/Say")
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(Empty::<Bytes>::new())
            .unwrap();
        sender.send_request(req).await.unwrap()
    };

    let resp = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let resp = send_h2().await;
            if resp.status() != StatusCode::SERVICE_UNAVAILABLE {
                break resp;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("upstream never became healthy over TLS");

    assert_eq!(resp.status(), StatusCode::OK);
    let (parts, body) = resp.into_parts();
    assert_eq!(
        parts
            .headers
            .get("x-upstream-te")
            .and_then(|v| v.to_str().ok()),
        Some("trailers"),
        "TE: trailers must pass through to the h2 upstream (gRPC proxy-compat detection header)"
    );
    let collected = body.collect().await.unwrap();
    let trailers = collected.trailers().cloned();
    assert_eq!(
        trailers
            .as_ref()
            .and_then(|t| t.get("grpc-status"))
            .and_then(|v| v.to_str().ok()),
        Some("0"),
        "response trailers (grpc-status) must be forwarded end-to-end"
    );
    assert_eq!(collected.to_bytes(), Bytes::from_static(b"tls-upstream-ok"));
}
