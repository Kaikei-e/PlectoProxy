//! E2E (tdd-workflow Phase 0) for **authentication as a WASM filter** — Plecto's showcase, viewed
//! as a security boundary. Real HTTP/1.1 flows through a running `plecto-server` whose `/api` route
//! is gated by the signed `filter-apikey` component, and we assert the client-visible security
//! behaviour:
//!   - no key / unknown key → 401, and the upstream is NEVER reached (a short-circuit cannot leak);
//!   - a valid key → 200, and the upstream sees the key's real `x-authenticated-user`;
//!   - **identity-spoof prevention** — a client that sends its own `x-authenticated-user` cannot
//!     impersonate anyone: with a valid key the filter's stamp REPLACES the client's value
//!     (the upstream sees the key's real user, not the spoofed one); with no key it is rejected 401
//!     so the forged header never reaches the upstream at all.
//!
//! The fake upstream reflects the inbound `x-authenticated-user` into `x-auth-seen`, so what the
//! upstream actually received is observable from the client side.

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
use plecto_host::test_support::{TestSigner, bound_sbom, filter_apikey_component};
use plecto_server::serve;

/// The protected upstream: reflects whatever `x-authenticated-user` it RECEIVED into `x-auth-seen`
/// (so the test can see exactly what the proxy forwarded), tags `x-from: upstream`, and answers any
/// path — including the `/healthz` probe — with 200 so the instance becomes healthy.
async fn echo(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let seen = req
        .headers()
        .get("x-authenticated-user")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(none)")
        .to_string();
    Ok(Response::builder()
        .status(200)
        .header("x-from", "upstream")
        .header("x-auth-seen", seen)
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

async fn spawn_proxy(control: Arc<Control>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, listener).await;
    });
    addr
}

/// A control plane: the signed `filter-apikey` component loaded trusted (init seeds the demo
/// key→user map), gating a `/api` route (strip `/api`) → the given upstream.
fn control_for(upstream_addr: SocketAddr) -> Arc<Control> {
    let component = filter_apikey_component();
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(&component).unwrap();
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let mut store = MemoryStore::new();
    let digest = store.insert(
        "apikey",
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
id = "apikey"
source = "apikey"
digest = "{digest}"
isolation = "trusted"

[[upstream]]
name = "protected"
addresses = ["{upstream_addr}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
path_prefix = "/api"
filters = ["apikey"]
upstream = "protected"
strip_prefix = "/api"
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Arc::new(Control::load(host, &manifest, Box::new(store)).unwrap())
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Send a GET through the proxy; return (status, response headers, body string).
async fn get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, hyper::HeaderMap, String) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("http://{proxy}{path}"));
    for (n, v) in headers {
        builder = builder.header(*n, *v);
    }
    let resp = client
        .request(builder.body(Empty::<Bytes>::new()).unwrap())
        .await
        .expect("proxy request");
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (
        parts.status,
        parts.headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

/// Wait until the upstream is healthy. The route is auth-gated, so the readiness probe must carry a
/// VALID key — otherwise the filter short-circuits 401 before the upstream is ever consulted and we
/// could never observe the 503→200 health transition.
async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr) {
    for _ in 0..100 {
        let (status, _, _) = get(
            client,
            proxy,
            "/api/__ready",
            &[("x-api-key", "alice-secret")],
        )
        .await;
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

#[tokio::test]
async fn missing_key_is_401_and_never_reaches_upstream() {
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream)).await;
    let client = client();

    // No readiness wait needed: a 401 short-circuit happens before upstream selection, so it does
    // not depend on health — and that is exactly the property we assert (it can't leak upstream).
    let (status, headers, _body) = get(&client, proxy, "/api/secret", &[]).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "no key → 401");
    assert!(
        !headers.contains_key("x-from"),
        "a 401 short-circuit must never reach the upstream"
    );
}

#[tokio::test]
async fn unknown_key_is_401() {
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream)).await;
    let client = client();

    let (status, headers, _body) = get(
        &client,
        proxy,
        "/api/secret",
        &[("x-api-key", "not-a-real-key")],
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "an unknown key → 401");
    assert!(
        !headers.contains_key("x-from"),
        "a rejected request must not reach the upstream"
    );
}

#[tokio::test]
async fn valid_key_forwards_with_the_real_user_stamped() {
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (status, headers, body) = get(
        &client,
        proxy,
        "/api/secret",
        &[("x-api-key", "alice-secret")],
    )
    .await;

    assert_eq!(status, StatusCode::OK, "a valid key forwards");
    assert_eq!(body, "upstream-ok", "the upstream body streams back");
    assert_eq!(
        headers.get("x-auth-seen").and_then(|v| v.to_str().ok()),
        Some("alice"),
        "the upstream received the key's real identity"
    );
}

#[tokio::test]
async fn client_supplied_identity_header_cannot_impersonate() {
    // The flagship auth-bypass test. The client sends a valid key for `alice` AND a forged
    // `x-authenticated-user: admin`. The filter stamps the REAL user and the chain applies it as a
    // case-insensitive REPLACE, so the upstream must see `alice`, never the spoofed `admin`.
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (status, headers, _body) = get(
        &client,
        proxy,
        "/api/secret",
        &[
            ("x-api-key", "alice-secret"),
            ("x-authenticated-user", "admin"),
        ],
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("x-auth-seen").and_then(|v| v.to_str().ok()),
        Some("alice"),
        "a spoofed identity header must be overwritten by the key's real user (not 'admin')"
    );
}

#[tokio::test]
async fn spoofed_identity_header_without_a_key_is_rejected_and_not_forwarded() {
    // Forging only the identity header (no key) must be a 401 — and because the rejection
    // short-circuits, the forged header can never reach the upstream.
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream)).await;
    let client = client();

    let (status, headers, _body) = get(
        &client,
        proxy,
        "/api/secret",
        &[("x-authenticated-user", "admin")],
    )
    .await;

    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a forged identity with no key → 401"
    );
    assert!(
        !headers.contains_key("x-from"),
        "the forged header never reaches the upstream (short-circuited)"
    );
}
