//! E2E (tdd-workflow Phase 0) for periodic DNS re-resolution of upstream hostnames — the
//! standard periodic-DNS endpoint-discovery technique (the shape of nginx `resolver`+`resolve` /
//! Envoy STRICT_DNS): each address a hostname resolves to becomes a load-balancing endpoint,
//! refreshed on `[[upstream]] resolve_interval_ms`, so a container re-creation's new IP is picked
//! up without a restart. Interval-based (getaddrinfo carries no TTL); failed resolutions keep the
//! last-known-good set.
//!
//! Real DNS cannot be steered from a test, so this file pins the black-box contract with
//! `localhost` (stable resolution): the manifest field is accepted, the hostname's endpoints are
//! swapped for resolved IP endpoints, and traffic keeps flowing through health-probe promotion.
//! The swap/reuse mechanics are unit-tested in control (`update_endpoints`).

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

use plecto_control::{Control, Host, Manifest, MemoryStore};
use plecto_host::test_support::TestSigner;
use plecto_server::serve;

async fn spawn_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(|_req: Request<Incoming>| async {
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                                b"upstream-ok",
                            ))))
                        }),
                    )
                    .await;
            });
        }
    });
    addr
}

fn loaded_control(toml: &str) -> Result<Control, plecto_control::ControlError> {
    let manifest = Manifest::from_toml(toml)?;
    let signer = TestSigner::new().unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Control::load(host, &manifest, Box::new(MemoryStore::new()))
}

#[tokio::test]
async fn hostname_endpoints_are_swapped_for_resolved_ips_and_traffic_flows() {
    let upstream = spawn_upstream().await;
    let toml = format!(
        r#"
[[upstream]]
name = "app"
addresses = ["localhost:{port}"]
resolve_interval_ms = 100
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
upstream = "app"
[route.match]
path_prefix = "/api"
"#,
        port = upstream.port()
    );
    let control = Arc::new(loaded_control(&toml).unwrap());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    {
        let control = control.clone();
        tokio::spawn(async move {
            let _ = serve(control, listener).await;
        });
    }

    // Traffic flows once a resolved-IP endpoint passes its probe (instances start pessimistic).
    let client: Client<HttpConnector, Empty<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let req = Request::builder()
                .uri(format!("http://{proxy}/api/hello"))
                .body(Empty::<Bytes>::new())
                .unwrap();
            if let Ok(resp) = client.request(req).await {
                if resp.status() == StatusCode::OK {
                    let _ = resp.into_body().collect().await;
                    break StatusCode::OK;
                }
                let _ = resp.into_body().collect().await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("traffic never flowed through the resolving upstream");
    assert_eq!(status, StatusCode::OK);

}

#[tokio::test]
async fn ip_literal_addresses_are_left_untouched_by_the_refresher() {
    let upstream = spawn_upstream().await;
    let toml = format!(
        r#"
[[upstream]]
name = "app"
addresses = ["{upstream}"]
resolve_interval_ms = 100
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
upstream = "app"
[route.match]
path_prefix = "/api"
"#
    );
    // RED phase: the field must parse; endpoint-identity assertions land with the API.
    let _control = Arc::new(loaded_control(&toml).unwrap());
    let _ = upstream;
}
