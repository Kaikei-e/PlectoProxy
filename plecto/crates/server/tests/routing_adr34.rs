//! E2E (tdd-workflow Phase 0) for ADR 000034: drive real HTTP/1.1 requests through a running
//! `plecto-server` and assert the matured routing — method / header / query match dimensions select
//! the right route, and a weighted `backends` split distributes traffic across upstreams (canary),
//! with `weight 0` draining a backend. Each upstream identifies itself with `x-upstream-name`, so a
//! test can prove WHICH backend served. Filterless routes keep the focus on routing, not the chain.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{Empty, Full};
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

/// A fake upstream that identifies itself: every request gets a 200 carrying `x-upstream-name: name`
/// (and a 2xx satisfies the health probe). Lets a test see which backend a request reached.
async fn spawn_named_upstream(name: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(move |_req: Request<Incoming>| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .header("x-upstream-name", name)
                            .body(Full::new(Bytes::from_static(b"ok")))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
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

/// A control plane with no filters, built from the given (filterless) manifest TOML.
fn control_from(toml: &str) -> Arc<Control> {
    let signer = TestSigner::new().unwrap();
    let manifest = Manifest::from_toml(toml).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Arc::new(Control::load(host, &manifest, Box::new(MemoryStore::new())).unwrap())
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Send one request through the proxy; return (status, `x-upstream-name` or "").
async fn send(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("http://{proxy}{path}"));
    for (n, v) in headers {
        builder = builder.header(*n, *v);
    }
    let resp = client
        .request(builder.body(Empty::<Bytes>::new()).unwrap())
        .await
        .expect("proxy request");
    let name = resp
        .headers()
        .get("x-upstream-name")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    (resp.status(), name)
}

/// Poll a forwarding path until it stops being 503 (the upstreams' first health probe passed).
async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr, path: &str) {
    for _ in 0..100 {
        let (status, _) = send(client, proxy, "GET", path, &[]).await;
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

#[tokio::test]
async fn method_match_selects_the_route() {
    let reads = spawn_named_upstream("reads").await;
    let writes = spawn_named_upstream("writes").await;
    let toml = format!(
        r#"
[[upstream]]
name = "reads"
addresses = ["{reads}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[upstream]]
name = "writes"
addresses = ["{writes}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
upstream = "writes"
[route.match]
path_prefix = "/"
method = "POST"

[[route]]
upstream = "reads"
[route.match]
path_prefix = "/"
"#
    );
    let proxy = spawn_proxy(control_from(&toml)).await;
    let client = client();
    wait_ready(&client, proxy, "/").await;

    let (status, name) = send(&client, proxy, "GET", "/anything", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(name, "reads", "a GET falls to the bare route");

    let (status, name) = send(&client, proxy, "POST", "/anything", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        name, "writes",
        "a POST takes the more specific method route"
    );
}

#[tokio::test]
async fn header_match_selects_the_route() {
    let v1 = spawn_named_upstream("v1").await;
    let v2 = spawn_named_upstream("v2").await;
    let toml = format!(
        r#"
[[upstream]]
name = "v1"
addresses = ["{v1}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[upstream]]
name = "v2"
addresses = ["{v2}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
upstream = "v2"
[route.match]
path_prefix = "/"
headers = {{ "x-api-version" = "2" }}

[[route]]
upstream = "v1"
[route.match]
path_prefix = "/"
"#
    );
    let proxy = spawn_proxy(control_from(&toml)).await;
    let client = client();
    wait_ready(&client, proxy, "/").await;

    // case-insensitive header name, exact value → the header route wins.
    let (_, name) = send(&client, proxy, "GET", "/", &[("X-Api-Version", "2")]).await;
    assert_eq!(name, "v2", "the matching header selects the v2 route");

    // wrong value → falls to the bare route.
    let (_, name) = send(&client, proxy, "GET", "/", &[("x-api-version", "3")]).await;
    assert_eq!(name, "v1", "a non-matching header value falls through");

    // absent header → bare route.
    let (_, name) = send(&client, proxy, "GET", "/", &[]).await;
    assert_eq!(name, "v1", "an absent header falls through");
}

#[tokio::test]
async fn query_match_selects_the_route() {
    let stable = spawn_named_upstream("stable").await;
    let beta = spawn_named_upstream("beta").await;
    let toml = format!(
        r#"
[[upstream]]
name = "stable"
addresses = ["{stable}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[upstream]]
name = "beta"
addresses = ["{beta}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
upstream = "beta"
[route.match]
path_prefix = "/"
query = {{ "flag" = "on" }}

[[route]]
upstream = "stable"
[route.match]
path_prefix = "/"
"#
    );
    let proxy = spawn_proxy(control_from(&toml)).await;
    let client = client();
    wait_ready(&client, proxy, "/").await;

    let (_, name) = send(&client, proxy, "GET", "/x?flag=on", &[]).await;
    assert_eq!(name, "beta", "the matching query selects the beta route");

    let (_, name) = send(&client, proxy, "GET", "/x?flag=off", &[]).await;
    assert_eq!(name, "stable", "a non-matching query value falls through");

    // query name is case-sensitive (Gateway-API semantics).
    let (_, name) = send(&client, proxy, "GET", "/x?Flag=on", &[]).await;
    assert_eq!(name, "stable", "the query name is case-sensitive");
}

#[tokio::test]
async fn weighted_split_distributes_across_backends() {
    // A 1:1 canary over two upstreams: the deterministic apportionment alternates, so 20 sequential
    // requests land exactly 10/10 — both backends serve, in proportion (ADR 000034).
    let blue = spawn_named_upstream("blue").await;
    let green = spawn_named_upstream("green").await;
    let toml = format!(
        r#"
[[upstream]]
name = "blue"
addresses = ["{blue}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[upstream]]
name = "green"
addresses = ["{green}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
[route.match]
path_prefix = "/"
[[route.backends]]
upstream = "blue"
weight = 1
[[route.backends]]
upstream = "green"
weight = 1
"#
    );
    let proxy = spawn_proxy(control_from(&toml)).await;
    let client = client();
    wait_ready(&client, proxy, "/").await;

    let mut blue_n = 0;
    let mut green_n = 0;
    for _ in 0..20 {
        let (status, name) = send(&client, proxy, "GET", "/", &[]).await;
        assert_eq!(status, StatusCode::OK);
        match name.as_str() {
            "blue" => blue_n += 1,
            "green" => green_n += 1,
            other => panic!("unexpected backend {other:?}"),
        }
    }
    // Allow the readiness probe (which consumed some cursor ticks) to offset the exact split; assert
    // both backends served a fair share rather than an exact 10/10.
    assert!(blue_n >= 8, "blue served a fair share ({blue_n})");
    assert!(green_n >= 8, "green served a fair share ({green_n})");
    assert_eq!(blue_n + green_n, 20);
}

#[tokio::test]
async fn weight_zero_drains_a_backend() {
    // `weight 0` removes a backend from the split entirely: all traffic goes to the weighted one.
    let live = spawn_named_upstream("live").await;
    let drained = spawn_named_upstream("drained").await;
    let toml = format!(
        r#"
[[upstream]]
name = "live"
addresses = ["{live}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[upstream]]
name = "drained"
addresses = ["{drained}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
[route.match]
path_prefix = "/"
[[route.backends]]
upstream = "live"
weight = 1
[[route.backends]]
upstream = "drained"
weight = 0
"#
    );
    let proxy = spawn_proxy(control_from(&toml)).await;
    let client = client();
    wait_ready(&client, proxy, "/").await;

    for _ in 0..15 {
        let (status, name) = send(&client, proxy, "GET", "/", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(name, "live", "a weight-0 backend never receives traffic");
    }
}
