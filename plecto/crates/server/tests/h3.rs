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
use plecto_server::serve;

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

/// A manifest declaring filter-hello, a `/api`→echo route, and a default (host-less) `[[tls]]` cert.
fn manifest_toml(upstream: SocketAddr, digest: &str, cert: &TestCert) -> String {
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
    let control = loaded_control(&manifest_toml(upstream, "{digest}", &cert));
    let proxy = spawn_proxy(Arc::new(control)).await;

    let r = drive_h3_ready(proxy, cert.cert_der.clone()).await;

    assert_eq!(r.status, 200, "the h3 request routes + forwards 200");
    assert_eq!(
        r.body, "upstream-ok",
        "the upstream body streams back over h3"
    );
}
