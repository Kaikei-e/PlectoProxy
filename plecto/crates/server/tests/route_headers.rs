//! E2E (tdd-workflow Phase 0) for declarative per-route response headers (`[route.headers]`,
//! ADR 000100): the operator's literal `set` / `remove` declaration is a **floor**. It lands on
//! every response the route is answerable for — the forwarded 200, a filter's `replace`, a
//! request-side `short-circuit`, the native rate limit's 429, and the forward-side 503 / 504 —
//! so neither a filter nor a fault can drop it. The one gap is deliberate and named by the ADR:
//! a response returned BEFORE a route is chosen (the no-route 404) has no route to declare it.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

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

/// The declaration under test, shared by every scenario. It exercises all three behaviours in one
/// block: a name whose manifest spelling differs in case from the upstream's, a name the upstream
/// never sends, and a removal of one the upstream does send.
const DECLARED: &str = r#"
[route.headers]
set = { "X-Frame-Options" = "DENY", "x-content-type-options" = "nosniff" }
remove = ["server"]
"#;

/// Assert the declaration reached the client.
fn assert_declared(parts: &hyper::http::response::Parts, context: &str) {
    assert_eq!(
        parts.headers.get("x-frame-options").map(|v| v.as_bytes()),
        Some(b"DENY".as_slice()),
        "the declared value must win on {context} (manifest name case is irrelevant)"
    );
    assert_eq!(
        parts
            .headers
            .get("x-content-type-options")
            .map(|v| v.as_bytes()),
        Some(b"nosniff".as_slice()),
        "a declared header the upstream never sent must be added on {context}"
    );
    assert!(
        parts.headers.get("server").is_none(),
        "a declared removal must hold on {context}"
    );
}

/// Answers everything 200, with a `server` header (to be removed) and its own weaker
/// `x-frame-options` (to be overridden), so both directions of the declaration are observable.
async fn upstream_ok(_req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::builder()
        .status(200)
        .header("x-upstream", "1")
        .header("server", "fake-upstream/1.0")
        .header("x-frame-options", "SAMEORIGIN")
        .body(Full::new(Bytes::from_static(b"upstream body")))
        .unwrap())
}

async fn spawn_upstream() -> SocketAddr {
    spawn_upstream_with_delay(Duration::ZERO).await
}

/// A fake upstream that answers `/healthz` immediately (so the instance goes healthy) and stalls
/// `delay` on every other path — the shape the 504 scenario needs.
async fn spawn_upstream_with_delay(delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| async move {
                            if !delay.is_zero() && req.uri().path() != "/healthz" {
                                tokio::time::sleep(delay).await;
                            }
                            upstream_ok(req).await
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

/// A bound-then-dropped address: nothing listens there, so the upstream never goes healthy.
async fn dead_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
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

/// A filterless `/api` route to `upstream_addr`, carrying the declaration plus whatever extra
/// route-level TOML a scenario needs (`[route.rate_limit]`, a short timeout, …).
fn control_plain(
    upstream_addr: SocketAddr,
    upstream_extra: &str,
    route_extra: &str,
) -> Arc<Control> {
    let toml = format!(
        r#"
[[upstream]]
name = "backend"
addresses = ["{upstream_addr}"]
{upstream_extra}
[upstream.health]
path = "/healthz"
interval_ms = 50
timeout_ms = 200

[[route]]
upstream = "backend"
[route.match]
path_prefix = "/api"
{route_extra}
{DECLARED}
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let signer = TestSigner::new().unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Arc::new(Control::load(host, &manifest, Box::new(MemoryStore::new())).unwrap())
}

/// The same `/api` route, but running signed filter-hello — so a scenario can drive the filter's
/// request-side `short-circuit` and response-side `replace` decisions.
fn control_with_filter(upstream_addr: SocketAddr) -> Arc<Control> {
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
interval_ms = 50

[[route]]
filters = ["fh"]
upstream = "backend"
[route.match]
path_prefix = "/api"
{DECLARED}
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

/// Poll until the upstream is healthy (the proxy stops fail-closing with 503).
async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr) {
    for _ in 0..150 {
        let (parts, _) = get(client, proxy, "/api/__ready", &[]).await;
        if parts.status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

#[tokio::test]
async fn declared_headers_ride_a_forwarded_response() {
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_plain(upstream, "", "")).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/api/thing", &[]).await;

    assert_eq!(parts.status, StatusCode::OK);
    assert_declared(&parts, "a forwarded 200");
    assert!(
        parts.headers.contains_key("x-upstream"),
        "the rest of the upstream's headers pass through untouched"
    );
    assert_eq!(body.as_ref(), b"upstream body");
}

#[tokio::test]
async fn declared_headers_ride_a_filter_replace() {
    // ADR 000100 decision 2: the declaration is a floor a filter cannot drop — a `replace`
    // (ADR 000073) synthesises the whole response, and the declared headers still land on it.
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

    assert_eq!(parts.status, StatusCode::IM_A_TEAPOT);
    assert_eq!(body.as_ref(), b"replaced by filter-hello");
    assert_declared(&parts, "a filter's replace");
}

#[tokio::test]
async fn declared_headers_ride_a_request_side_short_circuit() {
    // The route is chosen before the chain runs, so a short-circuit answer belongs to it.
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_with_filter(upstream)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/api/thing", &[("x-plecto-block", "1")]).await;

    assert_eq!(parts.status, StatusCode::FORBIDDEN);
    assert_eq!(body.as_ref(), b"blocked by filter-hello");
    assert_declared(&parts, "a request-side short-circuit");
}

#[tokio::test]
async fn declared_headers_ride_a_no_healthy_upstream_503() {
    // The failure mode the ADR cares about most: a declaration whose whole value is that it
    // does not disappear when things break.
    let proxy = spawn_proxy(control_plain(dead_addr().await, "", "")).await;
    let client = client();

    let (parts, _) = get(&client, proxy, "/api/thing", &[]).await;

    assert_eq!(parts.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        parts.headers.get("x-plecto-fault").map(|v| v.as_bytes()),
        Some(b"no-healthy-upstream".as_slice())
    );
    assert_declared(&parts, "a fail-closed 503");
}

#[tokio::test]
async fn declared_headers_ride_an_upstream_timeout_504() {
    let upstream = spawn_upstream_with_delay(Duration::from_secs(30)).await;
    let proxy = spawn_proxy(control_plain(
        upstream,
        "request_timeout_ms = 100\nmax_retries = 0",
        "",
    ))
    .await;
    let client = client();
    // `/api/__ready` stalls too, so readiness is observed as the timeout itself, not a 200.
    for _ in 0..150 {
        let (parts, _) = get(&client, proxy, "/api/__ready", &[]).await;
        if parts.status != StatusCode::SERVICE_UNAVAILABLE {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let (parts, _) = get(&client, proxy, "/api/thing", &[]).await;

    assert_eq!(parts.status, StatusCode::GATEWAY_TIMEOUT);
    assert_declared(&parts, "an upstream-timeout 504");
}

#[tokio::test]
async fn declared_headers_ride_the_rate_limit_429() {
    // The limiter is consulted before the chain and before upstream selection, so a dead
    // upstream still lets the second request reach the 429 (the first spends the one token).
    let proxy = spawn_proxy(control_plain(
        dead_addr().await,
        "",
        "[route.rate_limit]\nrate = 1\nburst = 1",
    ))
    .await;
    let client = client();

    let (first, _) = get(&client, proxy, "/api/thing", &[]).await;
    assert_eq!(first.status, StatusCode::SERVICE_UNAVAILABLE);

    let (parts, _) = get(&client, proxy, "/api/thing", &[]).await;
    assert_eq!(parts.status, StatusCode::TOO_MANY_REQUESTS);
    assert_declared(&parts, "the native rate limit's 429");
}

#[tokio::test]
async fn declared_headers_do_not_ride_the_no_route_404() {
    // ADR 000100 decision 3, pinned as a known gap: no route matched, so there is no
    // declaration to apply. A listener-level declaration would be a separate decision.
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_plain(upstream, "", "")).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, _) = get(&client, proxy, "/elsewhere", &[]).await;

    assert_eq!(parts.status, StatusCode::NOT_FOUND);
    assert!(
        parts.headers.get("x-frame-options").is_none(),
        "a response returned before a route is chosen carries no route declaration"
    );
    assert!(
        parts.headers.get("x-content-type-options").is_none(),
        "a response returned before a route is chosen carries no route declaration"
    );
}
