//! E2E (tdd-workflow Phase 0) for the **0.3.0 response-side contract** (ADR 000073), through a
//! real listener → route → chain → upstream → response-chain transaction:
//!   - `on-response` receives the AS-FORWARDED request snapshot — filter-hello's echo mode
//!     reflects the request's path and `Origin` into response headers, and the path it sees is
//!     the chain-output path (before the egress `strip_prefix` rewrite): Declared Semantics,
//!     pinned here.
//!   - `replace` supplants the upstream response entirely (status, headers, body) — the typed
//!     successor of the old "non-empty body means synthetic" in-band signal.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;

use plecto_control::{Control, Host, Manifest, MemoryStore, ResolvedArtifact};
use plecto_host::test_support::{TestSigner, bound_sbom, filter_hello_component};
use plecto_server::serve;

/// Answers every request 200 with a fixed marker body, so a test can tell an upstream-streamed
/// response from a filter-synthesised one.
async fn upstream_ok(_req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::builder()
        .status(200)
        .header("x-upstream", "1")
        .body(Full::new(Bytes::from_static(b"upstream body")))
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
                    .serve_connection(TokioIo::new(stream), service_fn(upstream_ok))
                    .await;
            });
        }
    });
    addr
}

async fn spawn_proxy(control: Arc<Control>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, listener).await;
    });
    addr
}

/// An `/api` route (strip `/api`) through signed filter-hello (trusted) → the given upstream.
fn control_with_filter(upstream_addr: SocketAddr) -> Arc<Control> {
    control_with_filter_health(upstream_addr, 50)
}

fn control_with_filter_health(upstream_addr: SocketAddr, health_interval_ms: u64) -> Arc<Control> {
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
    let toml = format!(
        r#"
[[filter]]
id = "fh"
source = "fh"
digest = "{digest}"
isolation = "trusted"

[[upstream]]
name = "backend"
addresses = ["{upstream_addr}"]
[upstream.health]
path = "/healthz"
interval_ms = {health_interval_ms}

[[route]]
filters = ["fh"]
upstream = "backend"
strip_prefix = "/api"
[route.match]
path_prefix = "/api"
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Arc::new(Control::load(host, &manifest, Box::new(store)).unwrap())
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (hyper::http::response::Parts, Bytes) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("http://{proxy}{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let resp = client
        .request(builder.body(Empty::<Bytes>::new()).unwrap())
        .await
        .expect("proxy request");
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts, bytes)
}

async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr) {
    for _ in 0..100 {
        let (parts, _) = get(client, proxy, "/api/__ready", &[]).await;
        if parts.status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

#[tokio::test]
async fn on_response_sees_the_as_forwarded_request_snapshot() {
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_with_filter(upstream)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(
        &client,
        proxy,
        "/api/thing",
        &[
            ("x-plecto-resp-echo", "1"),
            ("origin", "https://app.example.test"),
        ],
    )
    .await;

    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(
        parts.headers.get("x-plecto-req-path").map(|v| v.as_bytes()),
        Some(b"/api/thing".as_slice()),
        "the snapshot is the CHAIN-OUTPUT request: the path before the egress strip_prefix \
         rewrite (Declared Semantics, ADR 000073)"
    );
    assert_eq!(
        parts
            .headers
            .get("x-plecto-echo-origin")
            .map(|v| v.as_bytes()),
        Some(b"https://app.example.test".as_slice()),
        "the dynamic origin echo — the inbound Origin rode the as-forwarded snapshot into \
         on-response"
    );
    assert_eq!(
        body.as_ref(),
        b"upstream body",
        "a modified edit still streams the upstream body through"
    );
}

#[tokio::test]
async fn replace_supplants_the_upstream_response_entirely() {
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_with_filter(upstream)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(
        &client,
        proxy,
        "/api/thing",
        &[("x-plecto-resp-replace", "1")],
    )
    .await;

    assert_eq!(
        parts.status,
        StatusCode::IM_A_TEAPOT,
        "the synthesised status supplants the upstream 200"
    );
    assert_eq!(
        parts.headers.get("x-plecto-replaced").map(|v| v.as_bytes()),
        Some(b"1".as_slice())
    );
    assert!(
        parts.headers.get("x-upstream").is_none(),
        "no upstream header survives a replace — the response is synthesised whole"
    );
    assert_eq!(
        body.as_ref(),
        b"replaced by filter-hello",
        "the synthesised body is sent; the upstream stream is discarded (drained or closed)"
    );
}

#[tokio::test]
async fn as_forwarded_snapshot_carries_chain_stamps_but_not_host_egress_injections() {
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_with_filter(upstream)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    // `x-plecto-addheader` stamps `x-plecto-added` on the request chain; echo mode must see it
    // on the as-forwarded snapshot. The client sends no `traceparent`, so the host's egress
    // injection must NOT appear in the snapshot either.
    let (parts, _) = get(
        &client,
        proxy,
        "/api/thing",
        &[("x-plecto-addheader", "1"), ("x-plecto-resp-echo", "1")],
    )
    .await;

    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(
        parts
            .headers
            .get("x-plecto-echo-stamp")
            .map(|v| v.as_bytes()),
        Some(b"1".as_slice()),
        "a request-chain stamp must ride the as-forwarded snapshot into on-response"
    );
    assert_eq!(
        parts
            .headers
            .get("x-plecto-echo-has-traceparent")
            .map(|v| v.as_bytes()),
        Some(b"0".as_slice()),
        "host egress traceparent injection must not appear in the as-forwarded snapshot"
    );
}

#[tokio::test]
async fn replace_drains_upstream_body_so_the_pooled_connection_can_be_reused() {
    // Count TCP accepts: after a replace, a drained body returns the socket to the pool so the
    // next request needs no new accept. Health interval is parked so it cannot race the count.
    let accepts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accepts_bg = accepts.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            accepts_bg.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(upstream_ok))
                    .await;
            });
        }
    });

    let proxy = spawn_proxy(control_with_filter_health(upstream, 3_600_000)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, _) = get(
        &client,
        proxy,
        "/api/thing",
        &[("x-plecto-resp-replace", "1")],
    )
    .await;
    assert_eq!(parts.status, StatusCode::IM_A_TEAPOT);
    let after_replace = accepts.load(std::sync::atomic::Ordering::SeqCst);

    // Give the background drain a moment to finish before the next request.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let (parts, body) = get(&client, proxy, "/api/thing", &[]).await;
    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(body.as_ref(), b"upstream body");

    let after_second = accepts.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        after_second, after_replace,
        "replace must drain the upstream body so the pooled connection serves the next request \
         without a new accept (after_replace={after_replace}, after_second={after_second})"
    );
}
