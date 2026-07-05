//! E2E (tdd-workflow Phase 0) for TLS termination (ADR 000014): drive a real **HTTPS/1.1**
//! request through `plecto-server` and assert it terminates TLS, routes, and forwards — and that
//! a bad cert fails the load **closed** (the proxy never comes up serving a cert it cannot use).
//! A fresh self-signed cert (rcgen) backs the listener; a rustls client trusts it.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, HandshakeKind, RootCertStore, crypto::aws_lc_rs};

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

/// A manifest declaring filter-hello, a `/api`→echo route, and the given `[[tls]]` cert block.
fn manifest_toml(upstream: SocketAddr, digest: &str, tls_block: &str) -> String {
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
{tls_block}
"#
    )
}

fn loaded_control(toml: &str) -> Result<Control, plecto_control::ControlError> {
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
    // the manifest is built with the real digest the store assigned
    let toml = toml.replace("{digest}", &digest);
    let manifest = Manifest::from_toml(&toml).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Control::load(host, &manifest, Box::new(store))
}

async fn spawn_proxy(control: Arc<Control>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, listener).await;
    });
    addr
}

/// An HTTPS GET through the proxy, trusting `root` and sending SNI `localhost`. Returns the status,
/// the `Alt-Svc` header value (if any), and the body.
async fn https_get(
    proxy: SocketAddr,
    root: CertificateDer<'static>,
    path: &str,
) -> (StatusCode, Option<String>, String) {
    let mut roots = RootCertStore::empty();
    roots.add(root).unwrap();
    let config = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(proxy).await.unwrap();
    let server_name = ServerName::try_from("localhost").unwrap();
    let tls = connector.connect(server_name, tcp).await.unwrap();

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
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
    let alt_svc = parts
        .headers
        .get("alt-svc")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        alt_svc,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// A client config with a live session cache (rustls client default: tickets accepted + cached),
/// built once so callers can SHARE it across connections — a second connection then offers the
/// first's session ticket, which is what the resumption tests below exercise.
fn resuming_client_config(root: CertificateDer<'static>) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.add(root).unwrap();
    Arc::new(
        ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// One HTTPS GET on a FRESH connection using the shared `config`, returning the handshake kind
/// (Full vs Resumed) with the status. The GET matters even when only the kind is asserted: TLS 1.3
/// NewSessionTickets are post-handshake messages, so the response read is what pulls them into the
/// client's session cache for the next connection to offer.
async fn https_get_kind(
    proxy: SocketAddr,
    config: Arc<ClientConfig>,
    path: &str,
) -> (StatusCode, HandshakeKind) {
    let connector = TlsConnector::from(config);
    let tcp = TcpStream::connect(proxy).await.unwrap();
    let server_name = ServerName::try_from("localhost").unwrap();
    let tls = connector.connect(server_name, tcp).await.unwrap();
    let kind = tls.get_ref().1.handshake_kind().unwrap();

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
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
    let _ = body.collect().await.unwrap();
    (parts.status, kind)
}

#[tokio::test]
async fn terminates_tls_then_routes_and_forwards() {
    let cert = make_cert();
    let upstream = spawn_upstream().await;
    // a host-less (default) cert: any SNI is served it (ADR 000014 default fallback).
    let tls_block = format!(
        "\n[[tls]]\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
        cert.cert_path, cert.key_path
    );
    let control = loaded_control(&manifest_toml(upstream, "{digest}", &tls_block)).unwrap();
    let proxy = spawn_proxy(Arc::new(control)).await;

    // readiness (ADR 000017): instances start pessimistic, so a forward is 503 until the first
    // health probe passes; poll until it does, then assert the forwarded result.
    let (status, alt_svc, body) = loop {
        let r = https_get(proxy, cert.cert_der.clone(), "/api/hello").await;
        if r.0 != StatusCode::SERVICE_UNAVAILABLE {
            break r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };

    assert_eq!(
        status,
        StatusCode::OK,
        "HTTPS request routes + forwards 200"
    );
    assert_eq!(
        body, "upstream-ok",
        "the upstream body streams back over TLS"
    );
    // With TLS configured, a QUIC listener is bound and the TCP response advertises HTTP/3 via
    // Alt-Svc (ADR 000016 / RFC 7838) on the same port, fresh for a day.
    let port = proxy.port();
    assert_eq!(
        alt_svc.as_deref(),
        Some(format!("h3=\":{port}\"; ma=86400").as_str()),
        "TCP responses advertise h3 on the same port via Alt-Svc"
    );
}

#[tokio::test]
async fn second_connection_resumes_with_stateless_ticket() {
    // ADR 000052: TLS 1.3 stateless resumption. A client that cached the first connection's
    // session ticket must complete the second handshake as Resumed — skipping the certificate +
    // signature work that makes the full handshake the TLS path's dominant cost. The HTTP status
    // is irrelevant here (the handshake completes either way); only the handshake kind is pinned.
    let cert = make_cert();
    let upstream = spawn_upstream().await;
    let tls_block = format!(
        "\n[[tls]]\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
        cert.cert_path, cert.key_path
    );
    let control = loaded_control(&manifest_toml(upstream, "{digest}", &tls_block)).unwrap();
    let proxy = spawn_proxy(Arc::new(control)).await;
    let config = resuming_client_config(cert.cert_der.clone());

    let (_, first) = https_get_kind(proxy, config.clone(), "/api/hello").await;
    assert_eq!(
        first,
        HandshakeKind::Full,
        "an empty client session cache means the first handshake is full"
    );
    let (_, second) = https_get_kind(proxy, config, "/api/hello").await;
    assert_eq!(
        second,
        HandshakeKind::Resumed,
        "the second connection offers the first's ticket and resumes"
    );
}

#[tokio::test]
async fn ticket_resumes_across_config_rebuilds() {
    // ADR 000052: the ticket key is PROCESS-lifetime, not config-lifetime. A manifest reload
    // rebuilds the ServerConfigs; tickets issued before the reload must still resume after it —
    // otherwise every SIGHUP silently degrades the fleet to full handshakes. Two independently
    // loaded Controls are a reload in miniature (and also stand in for the stateful default this
    // ADR removes: a per-config session CACHE cannot resume across builds, a shared stateless
    // ticket KEY can). The client caches the ticket under SNI "localhost", so it offers it to
    // the second proxy even though the port differs.
    let cert = make_cert();
    let upstream = spawn_upstream().await;
    let tls_block = format!(
        "\n[[tls]]\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
        cert.cert_path, cert.key_path
    );
    let toml = manifest_toml(upstream, "{digest}", &tls_block);
    let proxy_a = spawn_proxy(Arc::new(loaded_control(&toml).unwrap())).await;
    let proxy_b = spawn_proxy(Arc::new(loaded_control(&toml).unwrap())).await;
    let config = resuming_client_config(cert.cert_der.clone());

    let (_, first) = https_get_kind(proxy_a, config.clone(), "/api/hello").await;
    assert_eq!(first, HandshakeKind::Full);
    let (_, cross) = https_get_kind(proxy_b, config, "/api/hello").await;
    assert_eq!(
        cross,
        HandshakeKind::Resumed,
        "a ticket from before the rebuild resumes after it (process-lifetime key, ADR 000052)"
    );
}

#[tokio::test]
async fn bad_cert_path_fails_closed_at_load() {
    let upstream = spawn_upstream().await;
    let tls_block =
        "\n[[tls]]\ncert_path = \"/nonexistent/cert.pem\"\nkey_path = \"/nonexistent/key.pem\"\n";
    let result = loaded_control(&manifest_toml(upstream, "{digest}", tls_block));
    match result {
        Ok(_) => panic!("a missing cert file must fail the load (fail-closed), not serve plain"),
        Err(plecto_control::ControlError::TlsCert { reason, .. }) => {
            assert!(
                reason.contains("read failed"),
                "reason should name the read failure"
            );
        }
        Err(e) => panic!("expected a TlsCert error, got: {e}"),
    }
}
