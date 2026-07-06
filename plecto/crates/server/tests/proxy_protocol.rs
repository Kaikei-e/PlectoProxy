//! E2E (tdd-workflow Phase 0) for PROXY protocol v2 reception (ADR 000057): with
//! `[listen.proxy_protocol]` enabled, a trusted L4 LB's v2 header — read after accept, BEFORE
//! the TLS handshake — replaces the connection peer, so `X-Forwarded-For` / `X-Real-IP`
//! re-issuing (and every other `peer.ip()` consumer) sees the real client. Every receipt-rule
//! violation is fail-closed: the connection is cut, never passed through as-is.
//!
//! The echo upstream reflects the forwarding headers, so the restored peer is observable
//! end-to-end without reaching into server internals.

use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto::aws_lc_rs};

use plecto_control::{Control, Host, Manifest, MemoryStore};
use plecto_host::test_support::TestSigner;
use plecto_server::serve;

const SIGNATURE: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";

/// A fake upstream reflecting the forwarding headers the proxy set, so a test can prove which
/// peer the proxy believed it was talking to.
async fn echo(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let reflect = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    Ok(Response::builder()
        .status(200)
        .header("x-upstream-xff", reflect("x-forwarded-for"))
        .header("x-upstream-xrealip", reflect("x-real-ip"))
        .body(Full::new(Bytes::from_static(b"ok")))
        .unwrap())
}

async fn spawn_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(echo))
                    .await;
            });
        }
    });
    addr
}

/// Spawn an in-process proxy whose manifest enables `[listen.proxy_protocol]` with `trusted`,
/// routing `/api` to the echo upstream. `extra` appends manifest sections (e.g. `[[tls]]`).
async fn spawn_proxy(trusted: &str, extra: &str) -> SocketAddr {
    let upstream = spawn_upstream().await;
    let toml = format!(
        r#"[listen.proxy_protocol]
trusted = ["{trusted}"]

[[upstream]]
name = "app"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"

[[route]]
upstream = "app"
[route.match]
path_prefix = "/api"
{extra}
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let signer = TestSigner::new().unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let control = Control::load(host, &manifest, Box::new(MemoryStore::new())).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(Arc::new(control), listener).await;
    });
    proxy
}

/// A v2 `PROXY` header for TCP/IPv4 with `tlv` bytes appended to the address block.
fn v2_proxy_ipv4(src: SocketAddrV4, dst: SocketAddrV4, tlv: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(SIGNATURE);
    h.push(0x21); // v2, PROXY
    h.push(0x11); // AF_INET, STREAM
    h.extend_from_slice(&u16::try_from(12 + tlv.len()).unwrap().to_be_bytes());
    h.extend_from_slice(&src.ip().octets());
    h.extend_from_slice(&dst.ip().octets());
    h.extend_from_slice(&src.port().to_be_bytes());
    h.extend_from_slice(&dst.port().to_be_bytes());
    h.extend_from_slice(tlv);
    h
}

/// A v2 `LOCAL` header (zero declared length) — what an LB health check sends.
fn v2_local() -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(SIGNATURE);
    h.push(0x20); // v2, LOCAL
    h.push(0x00); // UNSPEC (ignored for LOCAL per spec)
    h.extend_from_slice(&0u16.to_be_bytes());
    h
}

fn sample_header() -> Vec<u8> {
    v2_proxy_ipv4(
        SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 9), 51234),
        SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 443),
        &[],
    )
}

/// Connect, write `preamble` raw, then drive one plaintext HTTP/1.1 GET `/api/x`. Returns the
/// response or the connection-level error (a cut connection surfaces here).
async fn request_after(
    proxy: SocketAddr,
    preamble: &[u8],
) -> Result<Response<Incoming>, Box<dyn std::error::Error>> {
    let fut = async {
        let mut stream = TcpStream::connect(proxy).await?;
        if !preamble.is_empty() {
            stream.write_all(preamble).await?;
        }
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .uri("/api/x")
            .header("host", "localhost")
            .body(Empty::<Bytes>::new())?;
        Ok(sender.send_request(req).await?)
    };
    tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .map_err(|_| "timed out")?
}

/// [`request_after`], retrying while the upstream's first health probe is still pending
/// (ADR 000017: instances start pessimistic, so a forward 503s until a probe passes).
async fn request_after_ready(
    proxy: SocketAddr,
    preamble: &[u8],
) -> Result<Response<Incoming>, Box<dyn std::error::Error>> {
    for _ in 0..100 {
        match request_after(proxy, preamble).await {
            Ok(resp) if resp.status() == StatusCode::SERVICE_UNAVAILABLE => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => return other,
        }
    }
    request_after(proxy, preamble).await
}

fn header_str<'a>(resp: &'a Response<Incoming>, name: &str) -> &'a str {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

#[tokio::test]
async fn trusted_proxy_v2_restores_the_client_ip() {
    let proxy = spawn_proxy("127.0.0.0/8", "").await;
    let resp = request_after_ready(proxy, &sample_header())
        .await
        .expect("request must succeed");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header_str(&resp, "x-upstream-xff"),
        "203.0.113.9",
        "X-Forwarded-For must carry the PROXY-restored client, not the LB peer"
    );
    assert_eq!(header_str(&resp, "x-upstream-xrealip"), "203.0.113.9");
}

#[tokio::test]
async fn trusted_local_command_keeps_the_real_peer() {
    let proxy = spawn_proxy("127.0.0.0/8", "").await;
    let resp = request_after_ready(proxy, &v2_local())
        .await
        .expect("request must succeed");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header_str(&resp, "x-upstream-xff"),
        "127.0.0.1",
        "LOCAL must fall back to the connection's real endpoints (LB health-check shape)"
    );
}

#[tokio::test]
async fn trusted_peer_without_a_header_is_cut() {
    let proxy = spawn_proxy("127.0.0.0/8", "").await;
    let result = request_after(proxy, b"").await;
    assert!(
        result.is_err(),
        "a trusted peer that skips the mandatory PROXY header must be cut, got: {result:?}"
    );
}

#[tokio::test]
async fn trusted_peer_with_garbage_header_is_cut() {
    let proxy = spawn_proxy("127.0.0.0/8", "").await;
    // right signature, then a version/command byte the spec forbids
    let mut bad = sample_header();
    bad[12] = 0x2F;
    let result = request_after(proxy, &bad).await;
    assert!(
        result.is_err(),
        "a malformed header must cut, not pass through, got: {result:?}"
    );
}

#[tokio::test]
async fn untrusted_peer_sending_proxy_v2_is_cut() {
    // loopback is NOT in the trusted set here
    let proxy = spawn_proxy("203.0.113.0/24", "").await;
    let result = request_after(proxy, &sample_header()).await;
    assert!(
        result.is_err(),
        "a PROXY header from outside the trusted CIDRs must be cut, got: {result:?}"
    );
}

#[tokio::test]
async fn untrusted_peer_plain_request_passes_through() {
    let proxy = spawn_proxy("203.0.113.0/24", "").await;
    let resp = request_after_ready(proxy, b"")
        .await
        .expect("mixed mode: direct clients still work");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header_str(&resp, "x-upstream-xff"),
        "127.0.0.1",
        "an untrusted peer keeps its real address"
    );
}

/// The header is read BEFORE the TLS handshake: a trusted LB prepends PROXY v2 to the TLS
/// stream, and the restored client shows in the forwarded headers of an HTTPS request.
#[tokio::test]
async fn proxy_v2_header_precedes_the_tls_handshake() {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, generated.cert.pem()).unwrap();
    std::fs::write(&key_path, generated.key_pair.serialize_pem()).unwrap();
    let tls_section = format!(
        "\n[[tls]]\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
        cert_path.to_str().unwrap(),
        key_path.to_str().unwrap()
    );
    let proxy = spawn_proxy("127.0.0.0/8", &tls_section).await;

    let mut roots = RootCertStore::empty();
    roots.add(generated.cert.der().clone()).unwrap();
    let config = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    let xff = tokio::time::timeout(Duration::from_secs(10), async {
        let mut tcp = TcpStream::connect(proxy).await.unwrap();
        tcp.write_all(&sample_header()).await.unwrap();
        let tls = connector
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .expect("TLS handshake must succeed after the PROXY header is consumed");
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder()
            .uri("/api/x")
            .header("host", "localhost")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let resp = sender.send_request(req).await.unwrap();
        let xff = header_str(&resp, "x-upstream-xff").to_string();
        let _ = resp.into_body().collect().await;
        xff
    })
    .await
    .expect("the proxy never answered over TLS");

    assert_eq!(
        xff, "203.0.113.9",
        "the restored client must survive TLS termination"
    );
}
