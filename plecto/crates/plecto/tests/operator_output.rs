//! E2E (tdd-workflow Phase 0) for the binary's operator-facing output contract (ADR 000099):
//! the JSON log lines an ingestion layer actually consumes. Drives the real compiled binary
//! (`CARGO_BIN_EXE_plecto`) with `[observability] access_log = true`, captures its stdout, and
//! asserts on the parsed lines — the access log's HTTP fields sit at the TOP LEVEL of the line
//! (not nested under `fields`), carry `trace_id` / `span_id` whether or not the transaction was
//! sampled, and each loaded filter reports the contract version it bound and its isolation.
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

/// The inbound W3C trace context this test pins the transaction to. The `-00` flags make it
/// explicitly NOT sampled: the ids must still reach the access log, because a log line is the
/// only handle an operator has on a transaction no exporter will ever see.
const UNSAMPLED_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";
const INBOUND_TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

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
                        service_fn(|_req: Request<Incoming>| async move {
                            Ok::<_, std::convert::Infallible>(
                                Response::builder()
                                    .status(200)
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

fn manifest_toml(digest: &str, upstream: SocketAddr) -> String {
    format!(
        r#"[trust]
keys = ["trust.pem"]

[observability]
access_log = true

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
[route.match]
path_prefix = "/api"
"#
    )
}

/// Write the signed fixture filter + trust root + manifest into `base` (the production load
/// path: sign → OCI layout → verify).
fn write_fixture(base: &Path, upstream: SocketAddr) {
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
    std::fs::write(base.join("plecto.toml"), manifest_toml(&digest, upstream)).unwrap();
}

fn free_port_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// GET through the proxy, carrying the unsampled inbound trace context.
async fn traced_get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    path: &str,
) -> anyhow::Result<u16> {
    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{proxy}{path}"))
        .header("traceparent", UNSAMPLED_TRACEPARENT)
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = client.request(req).await?;
    let status = resp.status().as_u16();
    resp.into_body().collect().await?;
    Ok(status)
}

/// Poll until the proxy answers 200 (binary starting, then the health probe passing).
async fn wait_until_serving(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr) {
    for _ in 0..300 {
        if let Ok(200) = traced_get(client, proxy, "/api/hello").await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the binary never served a 200 within the startup window");
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Every stdout line that parsed as a JSON object.
fn json_lines(raw: &str) -> Vec<serde_json::Map<String, serde_json::Value>> {
    raw.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| match v {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn access_log_lines_are_flat_and_carry_the_trace_ids() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let upstream = spawn_upstream().await;
    write_fixture(base, upstream);
    let proxy = free_port_addr();

    let log_path = base.join("plecto.stdout.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_plecto"))
        .arg(base.join("plecto.toml"))
        .arg(proxy.to_string())
        .current_dir(base)
        .stdout(Stdio::from(std::fs::File::create(&log_path).unwrap()))
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let client = Client::builder(TokioExecutor::new()).build_http();
    wait_until_serving(&client, proxy).await;
    let status = traced_get(&client, proxy, "/api/hello").await.unwrap();
    assert_eq!(status, 200);
    // stdout is line-buffered, so the line is already on disk; give the write a moment anyway.
    tokio::time::sleep(Duration::from_millis(200)).await;
    kill(&mut child);

    let raw = std::fs::read_to_string(&log_path).unwrap();
    let lines = json_lines(&raw);
    assert!(!lines.is_empty(), "the binary logs JSON lines:\n{raw}");

    // The LAST access line is the request driven above; the readiness poll before it legitimately
    // logged 503s while the upstream had not yet passed a probe.
    let access = lines
        .iter()
        .rev()
        .find(|l| l.get("target").and_then(|t| t.as_str()) == Some("plecto::access"))
        .unwrap_or_else(|| panic!("an access-log line is present:\n{raw}"));

    assert!(
        !access.contains_key("fields"),
        "the HTTP fields are flattened onto the line, not nested under `fields`: {access:?}"
    );
    for field in [
        "client",
        "scheme",
        "method",
        "authority",
        "path",
        "status",
        "duration_ms",
        "trace_id",
        "span_id",
    ] {
        assert!(
            access.contains_key(field),
            "the access log's `{field}` sits at the top level of the line: {access:?}"
        );
    }
    assert_eq!(
        access.get("method").and_then(|v| v.as_str()),
        Some("GET"),
        "{access:?}"
    );
    assert_eq!(
        access.get("path").and_then(|v| v.as_str()),
        Some("/api/hello"),
        "{access:?}"
    );
    assert_eq!(
        access.get("status").and_then(|v| v.as_u64()),
        Some(200),
        "the status is a number an ingestion layer can put in a typed slot: {access:?}"
    );
    assert_eq!(
        access.get("trace_id").and_then(|v| v.as_str()),
        Some(INBOUND_TRACE_ID),
        "the line joins the caller's trace even though it is NOT sampled: {access:?}"
    );
    let span_id = access
        .get("span_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert_eq!(
        span_id.len(),
        16,
        "span_id is the W3C 8-byte hex form: {access:?}"
    );
    assert!(
        span_id.chars().all(|c| c.is_ascii_hexdigit()) && span_id != "0000000000000000",
        "span_id is a valid, non-zero span id: {access:?}"
    );
}

#[tokio::test]
async fn each_loaded_filter_reports_its_bound_contract_version_and_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let upstream = spawn_upstream().await;
    write_fixture(base, upstream);
    let proxy = free_port_addr();

    let log_path = base.join("plecto.stdout.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_plecto"))
        .arg(base.join("plecto.toml"))
        .arg(proxy.to_string())
        .current_dir(base)
        .stdout(Stdio::from(std::fs::File::create(&log_path).unwrap()))
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let client = Client::builder(TokioExecutor::new()).build_http();
    wait_until_serving(&client, proxy).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    kill(&mut child);

    let raw = std::fs::read_to_string(&log_path).unwrap();
    let load = json_lines(&raw)
        .into_iter()
        .find(|l| l.get("filter").and_then(|v| v.as_str()) == Some("hello"))
        .unwrap_or_else(|| panic!("a per-filter load line is present:\n{raw}"));

    assert_eq!(
        load.get("contract").and_then(|v| v.as_str()),
        Some("plecto:filter@0.3.0"),
        "the load line names the contract version this filter actually bound: {load:?}"
    );
    assert_eq!(
        load.get("isolation").and_then(|v| v.as_str()),
        Some("trusted"),
        "the load line names the instance lifecycle the manifest asked for: {load:?}"
    );
}
