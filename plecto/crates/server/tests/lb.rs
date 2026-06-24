//! E2E (tdd-workflow Phase 0) for upstream load balancing + active health checks (ADR 000017):
//! drive real HTTP/1.1 requests through a running `plecto-server` whose route points at a
//! multi-instance upstream, and assert the client-visible behaviour — round-robin across the
//! healthy instances, ejection of an instance that fails its health probe, and a fail-closed 503
//! when no instance is healthy. Each fake upstream tags its response with `x-instance` so the
//! distribution is observable; a "dead" address (bound then dropped) is the unhealthy instance.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{Empty, Full};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;

use plecto_control::{Control, Host, Manifest, MemoryStore};
use plecto_host::test_support::TestSigner;
use plecto_server::serve;

/// A fake upstream that tags every 200 with `x-instance: {label}` so a test can see which instance
/// served. It answers any path (so the `/healthz` probe gets a 200 → the instance becomes healthy).
async fn spawn_labeled_upstream(label: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |_req| async move {
                            Ok::<Response<Full<Bytes>>, Infallible>(
                                Response::builder()
                                    .status(200)
                                    .header("x-instance", label)
                                    .body(Full::new(Bytes::from_static(b"ok")))
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

/// A "dead" address: claim a free ephemeral port, then drop the listener so connections to it are
/// refused. Its health probe never succeeds, so the instance stays unhealthy (pessimistic start).
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

/// A control plane with a single `pool` upstream over `addresses` and a filter-less `/api` route to
/// it. Fast health knobs (50ms interval, 100ms timeout) keep the test snappy.
fn control_for(addresses: &[SocketAddr]) -> Arc<Control> {
    let signer = TestSigner::new().unwrap();
    let addrs = addresses
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = format!(
        r#"
[[upstream]]
name = "pool"
addresses = [{addrs}]
[upstream.health]
path = "/healthz"
interval_ms = 50
timeout_ms = 100

[[route]]
path_prefix = "/api"
upstream = "pool"
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Arc::new(Control::load(host, &manifest, Box::new(MemoryStore::new())).unwrap())
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// One GET through the proxy → (status, `x-instance` value if any).
async fn get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
) -> (StatusCode, Option<String>) {
    let resp = client
        .request(
            Request::builder()
                .method("GET")
                .uri(format!("http://{proxy}/api/x"))
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .expect("proxy request");
    let instance = resp
        .headers()
        .get("x-instance")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (resp.status(), instance)
}

#[tokio::test]
async fn round_robin_distributes_over_healthy_instances() {
    let a = spawn_labeled_upstream("a").await;
    let b = spawn_labeled_upstream("b").await;
    let proxy = spawn_proxy(control_for(&[a, b])).await;
    let client = client();

    // Poll until BOTH instances have served at least once: this both waits past the pessimistic
    // cold-start window (ADR 000017) and proves round-robin spreads across the healthy set.
    let mut seen = HashSet::new();
    for _ in 0..200 {
        let (status, instance) = get(&client, proxy).await;
        if status == StatusCode::OK
            && let Some(i) = instance
        {
            seen.insert(i);
        }
        if seen.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        seen,
        HashSet::from(["a".to_string(), "b".to_string()]),
        "round-robin must distribute across both healthy instances"
    );
}

#[tokio::test]
async fn dead_instance_is_ejected_and_traffic_goes_to_the_healthy_one() {
    let live = spawn_labeled_upstream("live").await;
    let dead = dead_addr().await;
    let proxy = spawn_proxy(control_for(&[live, dead])).await;
    let client = client();

    // wait for the live instance's first probe to land (the dead one never becomes healthy).
    for _ in 0..200 {
        if get(&client, proxy).await.0 != StatusCode::SERVICE_UNAVAILABLE {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // every request now goes to the live instance — the dead one is never picked (pessimistic +
    // never-healthy), so the client sees only 200s from "live".
    for _ in 0..20 {
        let (status, instance) = get(&client, proxy).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "all traffic forwards to the healthy instance"
        );
        assert_eq!(
            instance.as_deref(),
            Some("live"),
            "the dead instance is ejected"
        );
    }
}

#[tokio::test]
async fn all_unhealthy_fails_closed_503() {
    let proxy = spawn_proxy(control_for(&[dead_addr().await, dead_addr().await])).await;
    let client = client();

    // both instances are dead → they never pass a probe → the upstream has no healthy instance.
    // Give the prober a moment to run, then assert a stable fail-closed 503 (ADR 000017).
    tokio::time::sleep(Duration::from_millis(150)).await;
    let resp = client
        .request(
            Request::builder()
                .method("GET")
                .uri(format!("http://{proxy}/api/x"))
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .expect("proxy request");
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "no healthy instance → fail closed with 503"
    );
    assert_eq!(
        resp.headers()
            .get("x-plecto-fault")
            .and_then(|v| v.to_str().ok()),
        Some("no-healthy-upstream"),
        "the 503 carries the no-healthy-upstream fault marker"
    );
}
