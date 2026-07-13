//! E2E (tdd-workflow Phase 0) for the `[listen]` manifest section (moka-1 field report §3.2 /
//! §3.4): the data-plane bind address lives in the manifest — the single static source of
//! config — instead of only a positional CLI arg (containers need `0.0.0.0` without entrypoint
//! gymnastics), and the `Alt-Svc` h3 advertisement can name the PUBLISHED port when it differs
//! from the bound one (internal 8443 → published 443 would otherwise advertise a dead port).
//!
//! The bind itself happens in `main.rs`, so the addr tests drive the real compiled binary; the
//! Alt-Svc test drives the in-process server (the advertisement is listener logic).

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto::aws_lc_rs};

use plecto_control::{Control, Host, Manifest, MemoryStore};
use plecto_host::test_support::TestSigner;
use plecto_server::serve;

/// Reserve an ephemeral port by binding and dropping a listener (the binary logs no bound port).
fn free_port_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// A minimal route → upstream manifest; `extra` prepends manifest sections (e.g. `[listen]`).
fn manifest_toml(extra: &str) -> String {
    format!(
        r#"{extra}
[[upstream]]
name = "app"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"

[[route]]
upstream = "app"
[route.match]
path_prefix = "/api"
"#
    )
}

fn spawn_binary(dir: &std::path::Path, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_plecto"))
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Poll until SOMETHING answers HTTP on `addr` (any status — the upstream here is a dead
/// address, so 503 is success for "the proxy is listening there").
async fn wait_for_listener(addr: SocketAddr) -> bool {
    let client: Client<HttpConnector, Empty<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    for _ in 0..200 {
        let req = Request::builder()
            .uri(format!("http://{addr}/api/x"))
            .body(Empty::<Bytes>::new())
            .unwrap();
        if client.request(req).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn manifest_listen_addr_binds_without_a_cli_arg() {
    let dir = tempfile::tempdir().unwrap();
    let addr = free_port_addr();
    std::fs::write(
        dir.path().join("plecto.toml"),
        manifest_toml(&format!("[listen]\naddr = \"{addr}\"\n")),
    )
    .unwrap();

    let mut child = spawn_binary(dir.path(), &["plecto.toml"]);
    let bound = wait_for_listener(addr).await;
    let _ = child.kill();
    let _ = child.wait();
    assert!(bound, "the binary must bind the manifest's [listen] addr");
}

#[tokio::test]
async fn cli_listen_arg_overrides_the_manifest() {
    // The positional arg stays the explicit operator override (backward compatible); the
    // manifest is the default source.
    let dir = tempfile::tempdir().unwrap();
    let manifest_addr = free_port_addr();
    let cli_addr = free_port_addr();
    std::fs::write(
        dir.path().join("plecto.toml"),
        manifest_toml(&format!("[listen]\naddr = \"{manifest_addr}\"\n")),
    )
    .unwrap();

    let mut child = spawn_binary(dir.path(), &["plecto.toml", &cli_addr.to_string()]);
    let bound = wait_for_listener(cli_addr).await;
    let _ = child.kill();
    let _ = child.wait();
    assert!(bound, "an explicit CLI listen addr must win over [listen]");
}

/// In-process: with `[listen] advertised_port`, TCP responses advertise h3 on the PUBLISHED
/// port, not the bound one (container port mapping, field report §3.4).
#[tokio::test]
async fn alt_svc_advertises_the_configured_port_override() {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, generated.cert.pem()).unwrap();
    std::fs::write(&key_path, generated.key_pair.serialize_pem()).unwrap();

    let toml = manifest_toml(&format!(
        "[listen]\nadvertised_port = 443\n\n[[tls]]\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
        cert_path.to_str().unwrap(),
        key_path.to_str().unwrap()
    ));
    let manifest = Manifest::from_toml(&toml).unwrap();
    let signer = TestSigner::new().unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let control = Control::load(host, &manifest, Box::new(MemoryStore::new())).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(Arc::new(control), listener).await;
    });

    // HTTPS GET; the route 503s (dead upstream) but the Alt-Svc header is attached regardless.
    let mut roots = RootCertStore::empty();
    roots.add(generated.cert.der().clone()).unwrap();
    let config = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    let alt_svc = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(tcp) = TcpStream::connect(proxy).await
                && let Ok(tls) = connector
                    .connect(ServerName::try_from("localhost").unwrap(), tcp)
                    .await
                && let Ok((mut sender, conn)) =
                    hyper::client::conn::http1::handshake(TokioIo::new(tls)).await
            {
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                let req = Request::builder()
                    .uri("/api/x")
                    .header("host", "localhost")
                    .body(Empty::<Bytes>::new())
                    .unwrap();
                if let Ok(resp) = sender.send_request(req).await {
                    let value = resp
                        .headers()
                        .get("alt-svc")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let _ = resp.into_body().collect().await;
                    break value;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the proxy never answered over TLS");

    assert_eq!(
        alt_svc.as_deref(),
        Some("h3=\":443\"; ma=86400"),
        "Alt-Svc must advertise the configured published port, not the bound one"
    );
}
