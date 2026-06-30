//! E2E for the per-upstream circuit breaker (ADR 000028): a `max_requests` cap sheds load with a
//! fast-fail 503 when the upstream is saturated, distinct from health (a shed request does not
//! demote an instance). A slow upstream holds the single permit so a concurrent request trips the
//! breaker; the admin `/metrics` (ADR 000009) then shows the shed count.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
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

use plecto_control::{Control, Manifest};
use plecto_server::serve;

/// An upstream that sleeps on `/slow` (to hold a circuit slot) and answers everything else — the
/// health probe (`/healthz`) and `/fast` — immediately.
async fn svc(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    if req.uri().path().starts_with("/slow") {
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    Ok(Response::builder()
        .status(200)
        .body(Full::new(Bytes::from_static(b"ok")))
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
                    .serve_connection(TokioIo::new(stream), service_fn(svc))
                    .await;
            });
        }
    });
    addr
}

async fn free_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// GET → (status, body).
async fn get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    addr: SocketAddr,
    path: &str,
) -> (StatusCode, String) {
    let resp = client
        .request(
            Request::builder()
                .method("GET")
                .uri(format!("http://{addr}{path}"))
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .expect("request");
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr) {
    for _ in 0..150 {
        let (status, _) = get(client, proxy, "/fast").await;
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy");
}

#[tokio::test]
async fn circuit_breaker_sheds_load_at_the_cap_then_recovers() {
    let upstream = spawn_upstream().await;
    let admin = free_addr().await;
    let toml = format!(
        r#"
[observability]
admin_addr = "{admin}"

[[upstream]]
name = "u"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 50
[upstream.circuit_breaker]
max_requests = 1

[[route]]
upstream = "u"
[route.match]
path_prefix = "/"
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let control = Arc::new(Control::from_manifest(&manifest, Path::new(".")).unwrap());

    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let data = data_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, data_listener).await;
    });

    let client = client();
    wait_ready(&client, data).await;

    // Request 1 occupies the single circuit slot for ~800 ms (the upstream sleeps on /slow).
    let c1 = client.clone();
    let inflight = tokio::spawn(async move { get(&c1, data, "/slow").await });
    // Give request 1 time to reach the upstream and hold the permit.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Request 2 arrives while the slot is taken → fast-fail 503 (circuit-open), NOT queued.
    let (status, body) = get(&client, data, "/fast").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a request over the concurrency cap is shed"
    );
    assert_eq!(
        body, "upstream overloaded",
        "the breaker's fast-fail body (distinct from no-healthy-upstream)"
    );

    // Request 1 still completes successfully — being shed never affected the in-flight one.
    let (s1, _) = inflight.await.unwrap();
    assert_eq!(s1, StatusCode::OK, "the in-flight request is unaffected");

    // The slot is released → capacity is restored.
    let (s3, _) = get(&client, data, "/fast").await;
    assert_eq!(
        s3,
        StatusCode::OK,
        "once the slot frees, requests are served again"
    );

    // The shed request is observable on the admin endpoint (ADR 000009 + 000028).
    let (mstatus, metrics) = get(&client, admin, "/metrics").await;
    assert_eq!(mstatus, StatusCode::OK);
    let shed = metrics
        .lines()
        .find(|l| l.starts_with("plecto_circuit_open_total "))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|n| n.parse::<u64>().ok())
        .expect("the circuit-open counter is exposed");
    assert!(shed >= 1, "the shed request is counted, got {shed}");
}
