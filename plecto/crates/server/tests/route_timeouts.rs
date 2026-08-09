//! E2E for per-route timeout overrides (ADR 000102): two routes sharing ONE upstream must be able
//! to run under different time budgets. The upstream declares the defaults and a `[route.timeouts]`
//! block overrides them — including with an explicit `0`, which disables the bound for that route
//! (the long-poll / streaming opt-out of ADR 000019), and including the overall bound that caps a
//! whole retry sequence (ADR 000031).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;

use plecto_control::{Control, Manifest};
use plecto_server::serve;

/// An upstream that answers `/healthz` immediately (so it joins the rotation) but stalls `delay`
/// on every real path — so whether a request survives is decided purely by the timeout applied.
async fn spawn_slow_upstream(delay: Duration) -> SocketAddr {
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
                            if req.uri().path() != "/healthz" {
                                tokio::time::sleep(delay).await;
                            }
                            Ok::<Response<Full<Bytes>>, Infallible>(
                                Response::builder()
                                    .status(200)
                                    .body(Full::new(Bytes::from_static(b"slow-ok")))
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

async fn spawn_proxy(toml: &str) -> SocketAddr {
    let manifest = Manifest::from_toml(toml).unwrap();
    let control = Arc::new(Control::from_manifest(&manifest, Path::new(".")).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, listener).await;
    });
    addr
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// GET `path` through the proxy, returning the status and the `x-plecto-fault` marker.
async fn get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    path: &str,
) -> (StatusCode, Option<String>) {
    let resp = client
        .request(
            Request::builder()
                .method("GET")
                .uri(format!("http://{proxy}{path}"))
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .expect("proxy request");
    let fault = resp
        .headers()
        .get("x-plecto-fault")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (resp.status(), fault)
}

/// Poll a forwarding path until the upstream's first health probe lands (no longer 503).
async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr, path: &str) {
    for _ in 0..200 {
        if get(client, proxy, path).await.0 != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

#[tokio::test]
async fn routes_sharing_one_upstream_run_under_their_own_per_try_timeouts() {
    // One upstream, one backend container, three routes with genuinely different budgets — the
    // arrangement ADR 000102 exists to make expressible without declaring the upstream twice.
    let upstream = spawn_slow_upstream(Duration::from_millis(300)).await;
    let toml = format!(
        r#"
[[upstream]]
name = "u"
addresses = ["{upstream}"]
request_timeout_ms = 60
max_retries = 0
[upstream.health]
path = "/healthz"
interval_ms = 30

[[route]]
upstream = "u"
[route.match]
path_prefix = "/inherit"

[[route]]
upstream = "u"
[route.match]
path_prefix = "/patient"
[route.timeouts]
request_timeout_ms = 5000

[[route]]
upstream = "u"
[route.match]
path_prefix = "/stream"
[route.timeouts]
request_timeout_ms = 0
"#
    );
    let proxy = spawn_proxy(&toml).await;
    let client = client();
    wait_ready(&client, proxy, "/patient").await;

    let (status, fault) = get(&client, proxy, "/inherit").await;
    assert_eq!(
        status,
        StatusCode::GATEWAY_TIMEOUT,
        "a route with no [route.timeouts] keeps the upstream's 60ms default"
    );
    assert_eq!(fault.as_deref(), Some("upstream-timeout"));

    let (status, _) = get(&client, proxy, "/patient").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a route that declares a longer per-try budget outlives the upstream default"
    );

    let (status, _) = get(&client, proxy, "/stream").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "request_timeout_ms = 0 on the route disables the per-try bound (streaming opt-out) — \
         an explicit 0 is not the same as leaving the block out"
    );
}

#[tokio::test]
async fn a_routes_overall_timeout_caps_the_retry_sequence() {
    // The upstream declares NO overall bound, so a 504 `request-timeout` can only come from the
    // route's own `[route.timeouts]` (a per-try expiry would report `upstream-timeout`).
    let a = spawn_slow_upstream(Duration::from_millis(1000)).await;
    let b = spawn_slow_upstream(Duration::from_millis(1000)).await;
    let toml = format!(
        r#"
[[upstream]]
name = "u"
addresses = ["{a}", "{b}"]
request_timeout_ms = 50
max_retries = 20
[upstream.health]
path = "/healthz"
interval_ms = 30

[[route]]
upstream = "u"
[route.match]
path_prefix = "/"
[route.timeouts]
overall_timeout_ms = 200
"#
    );
    let proxy = spawn_proxy(&toml).await;
    let client = client();
    wait_ready(&client, proxy, "/probe").await;

    let started = Instant::now();
    let (status, fault) = get(&client, proxy, "/slow").await;
    let elapsed = started.elapsed();

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        fault.as_deref(),
        Some("request-timeout"),
        "the route's overall deadline ended the transaction, not a per-try expiry"
    );
    assert!(
        elapsed < Duration::from_millis(900),
        "the route's 200ms overall bound must cap the retry sequence well before 20 × 50ms of \
         attempts (got {elapsed:?})"
    );
}
