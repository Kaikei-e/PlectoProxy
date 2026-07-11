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
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
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
    key_der: PrivateKeyDer<'static>,
}

fn make_cert() -> TestCert {
    make_cert_for("localhost")
}

fn make_cert_for(host: &str) -> TestCert {
    let generated = rcgen::generate_simple_self_signed(vec![host.to_string()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, generated.cert.pem()).unwrap();
    std::fs::write(&key_path, generated.key_pair.serialize_pem()).unwrap();
    TestCert {
        cert_der: generated.cert.der().clone(),
        key_der: PrivateKeyDer::try_from(generated.key_pair.serialize_der()).unwrap(),
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
    resuming_client_config_with_roots(vec![root])
}

/// [`resuming_client_config`] trusting several roots — for the cross-cert-set tests, where one
/// client talks to proxies presenting different self-signed certs.
fn resuming_client_config_with_roots(
    root_certs: Vec<CertificateDer<'static>>,
) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for root in root_certs {
        roots.add(root).unwrap();
    }
    Arc::new(
        ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// A resuming client config that ALSO presents `identity` as a client certificate (downstream
/// mTLS, ADR 000078), trusting `root` for the server side.
fn client_config_with_identity(
    root: CertificateDer<'static>,
    identity: &TestCert,
) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.add(root).unwrap();
    Arc::new(
        ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(
                vec![identity.cert_der.clone()],
                identity.key_der.clone_key(),
            )
            .unwrap(),
    )
}

/// One HTTPS GET that reports failure instead of panicking — for asserting a client-auth
/// listener REFUSES a peer. A required-client-cert refusal can surface at the connect (alert
/// during the handshake) or on the first request (TLS 1.3 post-handshake alert), so both legs
/// fold into `Err`.
async fn try_https_get(
    proxy: SocketAddr,
    config: Arc<ClientConfig>,
    path: &str,
) -> Result<StatusCode, String> {
    let connector = TlsConnector::from(config);
    let tcp = TcpStream::connect(proxy).await.map_err(|e| e.to_string())?;
    let server_name = ServerName::try_from("localhost").unwrap();
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "localhost")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
    Ok(resp.status())
}

/// A fresh shared STEK file (ADR 000062): 64 raw random-ish bytes, owner-only. Returns the dir
/// (kept alive) and the absolute path for the manifest's `[resumption] stek_file`.
fn make_stek(fill: u8) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stek.key");
    std::fs::write(&path, [fill; 64]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    (dir, path.to_str().unwrap().to_string())
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
    https_get_kind_sni(proxy, config, "localhost", path).await
}

/// [`https_get_kind`] with an explicit SNI — for the cross-SNI resumption tests.
async fn https_get_kind_sni(
    proxy: SocketAddr,
    config: Arc<ClientConfig>,
    sni: &str,
    path: &str,
) -> (StatusCode, HandshakeKind) {
    let connector = TlsConnector::from(config);
    let tcp = TcpStream::connect(proxy).await.unwrap();
    let server_name = ServerName::try_from(sni.to_string()).unwrap();
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
async fn shared_stek_ticket_resumes_across_replicas() {
    // ADR 000062: two INDEPENDENT proxies (two replicas behind a round-robin LB in miniature)
    // pointed at one [resumption] stek_file and serving the same cert. A ticket issued by one
    // must resume on the other — that recovered hit rate is the entire point of the opt-in.
    // Unlike `ticket_resumes_across_config_rebuilds` (process-lifetime OnceLock key), each
    // Control here builds its own shared-STEK ticketer; only the deterministic derivation from
    // (file, cert set) makes their tickets interchangeable.
    let cert = make_cert();
    let (_stek_dir, stek_path) = make_stek(7);
    let upstream = spawn_upstream().await;
    let block = format!(
        "\n[[tls]]\ncert_path = \"{}\"\nkey_path = \"{}\"\n\n[resumption]\nstek_file = \"{}\"\n",
        cert.cert_path, cert.key_path, stek_path
    );
    let toml = manifest_toml(upstream, "{digest}", &block);
    let proxy_a = spawn_proxy(Arc::new(loaded_control(&toml).unwrap())).await;
    let proxy_b = spawn_proxy(Arc::new(loaded_control(&toml).unwrap())).await;
    let config = resuming_client_config(cert.cert_der.clone());

    let (_, first) = https_get_kind(proxy_a, config.clone(), "/api/hello").await;
    assert_eq!(first, HandshakeKind::Full);
    let (_, cross) = https_get_kind(proxy_b, config, "/api/hello").await;
    assert_eq!(
        cross,
        HandshakeKind::Resumed,
        "replica B accepts replica A's ticket (shared STEK, same cert set — ADR 000062)"
    );
}

#[tokio::test]
async fn shared_stek_ticket_does_not_cross_cert_sets() {
    // ADR 000062 (a), the E2E the ADR's Cons section demands: two deployments SHARING the key
    // file but serving DIFFERENT certs (the USENIX'25 "STEK Sharing is Not Caring" shape behind
    // nginx CVE-2025-23419 / Apache CVE-2025-23048). The HKDF cert binding must make replica B
    // reject replica A's ticket — a full handshake, not a cross-deployment resumption.
    let cert_a = make_cert();
    let cert_b = make_cert(); // same SNI "localhost", different key pair = different cert set
    let (_stek_dir, stek_path) = make_stek(7);
    let upstream = spawn_upstream().await;
    let block = |cert: &TestCert| {
        format!(
            "\n[[tls]]\ncert_path = \"{}\"\nkey_path = \"{}\"\n\n[resumption]\nstek_file = \"{}\"\n",
            cert.cert_path, cert.key_path, stek_path
        )
    };
    let proxy_a = spawn_proxy(Arc::new(
        loaded_control(&manifest_toml(upstream, "{digest}", &block(&cert_a))).unwrap(),
    ))
    .await;
    let proxy_b = spawn_proxy(Arc::new(
        loaded_control(&manifest_toml(upstream, "{digest}", &block(&cert_b))).unwrap(),
    ))
    .await;
    // One client trusting both certs, one shared session cache: it WILL offer A's ticket to B.
    let config =
        resuming_client_config_with_roots(vec![cert_a.cert_der.clone(), cert_b.cert_der.clone()]);

    let (_, first) = https_get_kind(proxy_a, config.clone(), "/api/hello").await;
    assert_eq!(first, HandshakeKind::Full);
    let (_, cross) = https_get_kind(proxy_b, config, "/api/hello").await;
    assert_eq!(
        cross,
        HandshakeKind::Full,
        "a different cert set must NOT accept the ticket despite the shared key file \
         (cert binding, ADR 000062 (a))"
    );
}

/// A deliberately SNI-confused client session store: every ticket is cached — and offered —
/// under one pinned key, so a ticket obtained from `a.localhost` is offered when connecting to
/// `b.localhost`. This is the client half of the CVE-2025-23419 crossing shape; a correct stack
/// must answer it with a full handshake (rustls' server refuses resumption when the ticket's SNI
/// differs — `can_resume`, verified at rustls 0.23.41 `server/hs.rs:57`).
#[derive(Debug)]
struct SniConfusedStore(tokio_rustls::rustls::client::ClientSessionMemoryCache);

fn pinned() -> ServerName<'static> {
    ServerName::try_from("pinned.invalid").unwrap()
}

impl tokio_rustls::rustls::client::ClientSessionStore for SniConfusedStore {
    fn set_kx_hint(&self, _: ServerName<'static>, group: tokio_rustls::rustls::NamedGroup) {
        self.0.set_kx_hint(pinned(), group);
    }
    fn kx_hint(&self, _: &ServerName<'_>) -> Option<tokio_rustls::rustls::NamedGroup> {
        self.0.kx_hint(&pinned())
    }
    fn set_tls12_session(
        &self,
        _: ServerName<'static>,
        value: tokio_rustls::rustls::client::Tls12ClientSessionValue,
    ) {
        self.0.set_tls12_session(pinned(), value);
    }
    fn tls12_session(
        &self,
        _: &ServerName<'_>,
    ) -> Option<tokio_rustls::rustls::client::Tls12ClientSessionValue> {
        self.0.tls12_session(&pinned())
    }
    fn remove_tls12_session(&self, _: &ServerName<'static>) {
        self.0.remove_tls12_session(&pinned());
    }
    fn insert_tls13_ticket(
        &self,
        _: ServerName<'static>,
        value: tokio_rustls::rustls::client::Tls13ClientSessionValue,
    ) {
        self.0.insert_tls13_ticket(pinned(), value);
    }
    fn take_tls13_ticket(
        &self,
        _: &ServerName<'static>,
    ) -> Option<tokio_rustls::rustls::client::Tls13ClientSessionValue> {
        self.0.take_tls13_ticket(&pinned())
    }
}

#[tokio::test]
async fn ticket_does_not_resume_across_sni_hosts_within_one_proxy() {
    // The within-one-listener half of the CVE-2025-23419 shape: one proxy, two SNI vhosts,
    // shared STEK on. A client that (maliciously or buggily) replays vhost A's ticket at vhost B
    // must get a full handshake. Our ticketer cannot see the SNI (it is per-config), so this
    // rests on rustls' own resumption SNI match — which is exactly why the test pins the
    // OUTCOME end-to-end: if a rustls upgrade ever relaxed it, this fails and the shared-STEK
    // threat model needs revisiting.
    let cert_a = make_cert_for("a.localhost");
    let cert_b = make_cert_for("b.localhost");
    let (_stek_dir, stek_path) = make_stek(7);
    let upstream = spawn_upstream().await;
    let block = format!(
        "\n[[tls]]\nhost = \"a.localhost\"\ncert_path = \"{}\"\nkey_path = \"{}\"\n\
         \n[[tls]]\nhost = \"b.localhost\"\ncert_path = \"{}\"\nkey_path = \"{}\"\n\
         \n[resumption]\nstek_file = \"{}\"\n",
        cert_a.cert_path, cert_a.key_path, cert_b.cert_path, cert_b.key_path, stek_path
    );
    let control = loaded_control(&manifest_toml(upstream, "{digest}", &block)).unwrap();
    let proxy = spawn_proxy(Arc::new(control)).await;

    let mut roots = RootCertStore::empty();
    roots.add(cert_a.cert_der.clone()).unwrap();
    roots.add(cert_b.cert_der.clone()).unwrap();
    let mut config = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.resumption =
        tokio_rustls::rustls::client::Resumption::store(Arc::new(SniConfusedStore(
            tokio_rustls::rustls::client::ClientSessionMemoryCache::new(256),
        )));
    let config = Arc::new(config);

    let (_, first) = https_get_kind_sni(proxy, config.clone(), "a.localhost", "/api/hello").await;
    assert_eq!(first, HandshakeKind::Full);
    // Positive control: the confused store still resumes on the SAME SNI, so the Full below is
    // a refusal, not a broken client cache.
    let (_, same) = https_get_kind_sni(proxy, config.clone(), "a.localhost", "/api/hello").await;
    assert_eq!(same, HandshakeKind::Resumed, "same-SNI resumption works");
    let (_, cross) = https_get_kind_sni(proxy, config, "b.localhost", "/api/hello").await;
    assert_eq!(
        cross,
        HandshakeKind::Full,
        "a ticket from a.localhost must not resume a b.localhost handshake"
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

// ----- ADR 000078: downstream client-certificate verification ([listen.client_auth]) -----

/// The `[[tls]]` + `[listen.client_auth]` manifest block: terminate with `server`, require
/// client certificates chaining to `client_ca` (here: the self-signed client cert itself).
fn client_auth_block(server: &TestCert, client_ca: &TestCert) -> String {
    format!(
        "\n[[tls]]\ncert_path = \"{}\"\nkey_path = \"{}\"\n\n[listen.client_auth]\nca_path = \"{}\"\n",
        server.cert_path, server.key_path, client_ca.cert_path
    )
}

/// Poll one authenticated GET past the pessimistic-start 503 window (ADR 000017).
async fn https_get_kind_ready(
    proxy: SocketAddr,
    config: Arc<ClientConfig>,
    path: &str,
) -> (StatusCode, HandshakeKind) {
    for _ in 0..100 {
        let r = https_get_kind(proxy, config.clone(), path).await;
        if r.0 != StatusCode::SERVICE_UNAVAILABLE {
            return r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

/// Downstream mTLS over HTTP/1.1 (ADR 000078): a client presenting a certificate that chains to
/// `ca_path` is served; an anonymous client is refused AT THE HANDSHAKE — required mode has no
/// "request but allow none" fallback.
#[tokio::test]
async fn client_auth_listener_serves_an_authenticated_client_and_refuses_an_anonymous_one() {
    let server = make_cert();
    let identity = make_cert_for("plecto-client");
    let upstream = spawn_upstream().await;
    let control = loaded_control(&manifest_toml(
        upstream,
        "{digest}",
        &client_auth_block(&server, &identity),
    ))
    .unwrap();
    let proxy = spawn_proxy(Arc::new(control)).await;

    let authed = client_config_with_identity(server.cert_der.clone(), &identity);
    let (status, _) = https_get_kind_ready(proxy, authed, "/api/hello").await;
    assert_eq!(status, StatusCode::OK, "an authenticated client is served");

    let anon = resuming_client_config(server.cert_der.clone());
    assert!(
        try_https_get(proxy, anon, "/api/hello").await.is_err(),
        "an anonymous client must be refused at the TLS layer, not served an HTTP response"
    );
}

/// ADR 000062 (b) end-to-end: `[listen.client_auth]` and `[resumption]` (shared STEK) together
/// must fail the LOAD closed — resumption accepts a ticket without re-running client-certificate
/// verification, and a shared key would let that ticket open on every replica.
#[tokio::test]
async fn client_auth_with_shared_stek_fails_the_load_closed() {
    let server = make_cert();
    let identity = make_cert_for("plecto-client");
    let (_stek_dir, stek_path) = make_stek(9);
    let upstream = spawn_upstream().await;
    let tls_block = format!(
        "{}\n[resumption]\nstek_file = \"{stek_path}\"\n",
        client_auth_block(&server, &identity)
    );
    match loaded_control(&manifest_toml(upstream, "{digest}", &tls_block)) {
        Ok(_) => panic!("[listen.client_auth] + [resumption] must fail the load (ADR 000062 (b))"),
        Err(plecto_control::ControlError::Stek { reason, .. }) => {
            assert!(
                reason.contains("client_auth"),
                "the error should name the crossing rule, got: {reason}"
            );
        }
        Err(e) => panic!("expected the Stek crossing error, got: {e}"),
    }
}

/// Required mode refuses a presented certificate that does not chain to `ca_path` — not only
/// anonymous peers. Pins the untrusted-presentation path the happy-path tests leave uncovered.
#[tokio::test]
async fn client_auth_listener_refuses_an_untrusted_client_certificate() {
    let server = make_cert();
    let trusted = make_cert_for("plecto-client");
    let untrusted = make_cert_for("imposter");
    let upstream = spawn_upstream().await;
    let control = loaded_control(&manifest_toml(
        upstream,
        "{digest}",
        &client_auth_block(&server, &trusted),
    ))
    .unwrap();
    let proxy = spawn_proxy(Arc::new(control)).await;

    let bad = client_config_with_identity(server.cert_der.clone(), &untrusted);
    assert!(
        try_https_get(proxy, bad, "/api/hello").await.is_err(),
        "a client certificate that does not chain to ca_path must be refused at the TLS layer"
    );

    let good = client_config_with_identity(server.cert_der, &trusted);
    let (status, _) = https_get_kind_ready(proxy, good, "/api/hello").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "sanity: a trusted identity is still served on the same listener"
    );
}

/// Client-authenticated sessions still resume per-node (grill 確定 4): 0-RTT stays refused, an
/// anonymous peer still cannot get a ticket, and the ticket-restored `peer_certificates` identity
/// is pinned by `plecto-control`'s
/// `client_auth_peer_certificates_survive_per_node_resumption` (HTTP E2E cannot see server-side
/// peer certs while identity propagation stays deferred).
#[tokio::test]
async fn client_authenticated_sessions_still_resume_within_a_node() {
    let server = make_cert();
    let identity = make_cert_for("plecto-client");
    let upstream = spawn_upstream().await;
    let control = loaded_control(&manifest_toml(
        upstream,
        "{digest}",
        &client_auth_block(&server, &identity),
    ))
    .unwrap();
    let proxy = spawn_proxy(Arc::new(control)).await;

    let anon = resuming_client_config(server.cert_der.clone());
    assert!(
        try_https_get(proxy, anon, "/api/hello").await.is_err(),
        "sanity: the listener really requires a client certificate"
    );

    let config = client_config_with_identity(server.cert_der.clone(), &identity);
    let (status, first) = https_get_kind_ready(proxy, config.clone(), "/api/hello").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first, HandshakeKind::Full);
    let (status, second) = https_get_kind(proxy, config, "/api/hello").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        second,
        HandshakeKind::Resumed,
        "per-node resumption stays on for client-authenticated sessions (ADR 000078)"
    );
}
