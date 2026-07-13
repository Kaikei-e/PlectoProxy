//! E2E (tdd-workflow Phase 0) for the SHIPPED binary's signal wiring (ADR 000039): `plecto` must
//! pick up a manifest edit on SIGHUP (reload, ADR 000008) and exit cleanly on SIGTERM after
//! draining. Drives the real compiled binary (`CARGO_BIN_EXE_plecto`) over real signals — the
//! in-process drain semantics are covered by `tests/shutdown.rs`; this file pins the `main.rs`
//! wiring that ADR 000039 found missing.
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;

use plecto_control::ResolvedArtifact;
use plecto_control::oci::write_layout;
use plecto_host::test_support::{TestSigner, bound_sbom, filter_hello_component};

/// An echo upstream: reflects the path it received (`x-upstream-path`) so a reloaded
/// `strip_prefix` is observable through the proxy (same trick as `examples/hot-reload`).
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
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(|req: Request<Incoming>| async move {
                            let path = req
                                .uri()
                                .path_and_query()
                                .map(|p| p.as_str().to_string())
                                .unwrap_or_default();
                            Ok::<_, std::convert::Infallible>(
                                Response::builder()
                                    .status(200)
                                    .header("x-upstream-path", path)
                                    .body(Full::new(Bytes::from_static(b"upstream-ok")))
                                    .unwrap(),
                            )
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

fn manifest_toml(digest: &str, upstream: SocketAddr, strip_prefix: bool) -> String {
    let strip = if strip_prefix {
        "strip_prefix = \"/api\"\n"
    } else {
        ""
    };
    format!(
        r#"[trust]
keys = ["trust.pem"]

[[filter]]
id = "hello"
source = "filters/hello"
digest = "{digest}"
isolation = "trusted"

[[upstream]]
name = "app"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
filters = ["hello"]
upstream = "app"
{strip}[route.match]
path_prefix = "/api"
"#
    )
}

/// Write the signed fixture filter + trust root + manifest into `base` (the production load
/// path: sign → OCI layout → verify), returning the manifest's OCI digest.
fn write_fixture(base: &Path, upstream: SocketAddr) -> String {
    let signer = TestSigner::new().unwrap();
    std::fs::write(base.join("trust.pem"), signer.public_key_pem()).unwrap();
    let component = filter_hello_component();
    let component_signature = signer.sign(&component).unwrap();
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let digest = write_layout(
        &base.join("filters/hello"),
        &ResolvedArtifact {
            component,
            component_signature,
            sbom,
            sbom_signature,
        },
    )
    .unwrap();
    std::fs::write(
        base.join("plecto.toml"),
        manifest_toml(&digest, upstream, true),
    )
    .unwrap();
    digest
}

/// Reserve an ephemeral port by binding and immediately dropping a listener. Slightly racy, but
/// the binary takes an explicit listen addr and does not log its bound port, so this is the
/// portable way to point a test client at it without hardcoding a port.
fn free_port_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// GET through the proxy; `Ok((status, x-upstream-path))` or `Err` while it is not up / gone.
async fn try_get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    path: &str,
) -> anyhow::Result<(u16, String)> {
    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{proxy}{path}"))
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = client.request(req).await?;
    let upstream_path = resp
        .headers()
        .get("x-upstream-path")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let status = resp.status().as_u16();
    resp.into_body().collect().await?;
    Ok((status, upstream_path))
}

/// Poll until the proxy answers 200 on `/api/hello` with the expected echoed upstream path.
/// Tolerates connection refusal (binary still starting) and 503 (health probe pending / reload
/// re-probing). Panics with `context` after ~15 s.
async fn wait_for_upstream_path(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    expected: &str,
    context: &str,
) {
    for _ in 0..300 {
        if let Ok((200, path)) = try_get(client, proxy, "/api/hello").await
            && path == expected
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{context}: proxy never echoed upstream path {expected}");
}

fn send_signal(child: &Child, signal: libc::c_int) {
    // SAFETY: plain kill(2) on a child pid this test owns; the pid is live (child not reaped yet).
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, signal) };
    assert_eq!(rc, 0, "kill({signal}) must reach the child");
}

/// Wait up to `timeout` for the child to exit; `None` when it is still running.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[tokio::test]
async fn binary_reloads_on_sighup_and_exits_cleanly_on_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let upstream = spawn_upstream().await;
    let digest = write_fixture(base, upstream);
    let proxy = free_port_addr();

    let mut child = Command::new(env!("CARGO_BIN_EXE_plecto"))
        .arg(base.join("plecto.toml"))
        .arg(proxy.to_string())
        .current_dir(base)
        .stdout(Stdio::from(
            std::fs::File::create(base.join("plecto.stdout.log")).unwrap(),
        ))
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let client = Client::builder(TokioExecutor::new()).build_http();

    // Startup: with strip_prefix="/api", the upstream sees /hello.
    wait_for_upstream_path(&client, proxy, "/hello", "startup").await;

    // Edit the manifest (drop strip_prefix) and SIGHUP: the binary must survive the signal and
    // serve the new config — the upstream now sees the unstripped /api/hello.
    std::fs::write(
        base.join("plecto.toml"),
        manifest_toml(&digest, upstream, false),
    )
    .unwrap();
    send_signal(&child, libc::SIGHUP);
    wait_for_upstream_path(&client, proxy, "/api/hello", "after SIGHUP reload").await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "the binary must survive SIGHUP (reload, not terminate)"
    );

    // SIGTERM: graceful shutdown — the process exits 0 well within the drain deadline (idle only).
    send_signal(&child, libc::SIGTERM);
    let status = wait_with_timeout(&mut child, Duration::from_secs(10))
        .expect("the binary must exit after SIGTERM");
    assert!(
        status.success(),
        "SIGTERM is a clean shutdown (exit 0), got {status:?}"
    );
}
