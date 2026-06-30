//! E2E for the request-side body hook (ADR 000025) wired into the fast path: a filtered route with
//! a request body must buffer it, run the chain's `on-request-body`, and forward the (possibly
//! transformed) body — or short-circuit before upstream. `filter-hello` uppercases the body, or
//! short-circuits 403 on a `deny-body` marker. A body-echoing upstream reflects what it received so
//! the transform is observable; a bodyless request must keep the zero-copy streaming path.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
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

/// A body-echoing upstream: collects the request body and returns it verbatim as the response body
/// (with `x-from: upstream`), so a test can observe exactly what reached the upstream — and prove a
/// short-circuit never did.
async fn echo_body(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let received = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    Ok(Response::builder()
        .status(200)
        .header("x-from", "upstream")
        .body(Full::new(received))
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
                    .serve_connection(TokioIo::new(stream), service_fn(echo_body))
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

/// filter-hello signed + loaded trusted, on a route `/api` (strip `/api`) → the body-echo upstream.
fn control_for(upstream_addr: SocketAddr) -> Arc<Control> {
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
name = "echo"
addresses = ["{upstream_addr}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
filters = ["fh"]
upstream = "echo"
strip_prefix = "/api"
[route.match]
path_prefix = "/api"
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Arc::new(Control::load(host, &manifest, Box::new(store)).unwrap())
}

fn client() -> Client<HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn post(
    client: &Client<HttpConnector, Full<Bytes>>,
    proxy: SocketAddr,
    path: &str,
    body: &'static [u8],
) -> (StatusCode, hyper::HeaderMap, String) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("http://{proxy}{path}"))
        .body(Full::new(Bytes::from_static(body)))
        .unwrap();
    let resp = client.request(req).await.expect("proxy request");
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Poll a forwarding path until the upstream's first health probe lands (ADR 000017).
async fn wait_ready(client: &Client<HttpConnector, Full<Bytes>>, proxy: SocketAddr) {
    for _ in 0..100 {
        let (status, _, _) = post(client, proxy, "/api/__ready", b"").await;
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

#[tokio::test]
async fn request_body_is_transformed_by_the_hook() {
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (status, headers, body) = post(&client, proxy, "/api/hello", b"hello world").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-from").and_then(|v| v.to_str().ok()),
        Some("upstream"),
        "the request reached the upstream (the body hook continued)"
    );
    assert_eq!(
        body, "HELLO WORLD",
        "the upstream received the body uppercased by the on-request-body hook"
    );
}

#[tokio::test]
async fn request_body_can_short_circuit_before_upstream() {
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (status, headers, body) = post(&client, proxy, "/api/hello", b"please deny-body now").await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the deny-body marker short-circuits 403"
    );
    assert_eq!(
        body, "blocked body by filter-hello",
        "the filter synthesised the short-circuit body"
    );
    assert!(
        !headers.contains_key("x-from"),
        "a body short-circuit must not reach the upstream"
    );
}

#[tokio::test]
async fn bodyless_request_skips_the_hook() {
    // A request with no body keeps the zero-copy streaming path: the hook never runs, so the
    // upstream is reached normally (and nothing is uppercased because there is nothing to buffer).
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (status, headers, _body) = post(&client, proxy, "/api/hello", b"").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-from").and_then(|v| v.to_str().ok()),
        Some("upstream"),
        "a bodyless request forwards normally"
    );
}
