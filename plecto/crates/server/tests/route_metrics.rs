//! E2E tests for per-route metrics (ADR 000112): `plecto_requests_total` and `plecto_rate_limited_total`
//! labeled by `route`, unmatched sentinel series, pre-registration at scrape time, and persistence
//! across configuration reload.

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

use plecto_control::{Control, Manifest};
use plecto_server::serve;

/// Trivial echo upstream answering 200 OK.
async fn echo(_req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
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
                    .serve_connection(TokioIo::new(stream), service_fn(echo))
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

/// Poll a forwarding path until the upstream's first health probe passes.
async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr, path: &str) {
    for _ in 0..150 {
        let (status, _) = get(client, proxy, path).await;
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

/// Poll the admin endpoint until it is bound and serving /healthz.
async fn wait_admin(client: &Client<HttpConnector, Empty<Bytes>>, admin: SocketAddr) {
    for _ in 0..150 {
        let req = Request::builder()
            .uri(format!("http://{admin}/healthz"))
            .body(Empty::<Bytes>::new())
            .unwrap();
        if let Ok(resp) = client.request(req).await
            && resp.status() == StatusCode::OK
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("admin endpoint never came up within the window");
}

async fn get_with_host(
    client: &Client<HttpConnector, Empty<Bytes>>,
    addr: SocketAddr,
    path: &str,
    host: &str,
) -> (StatusCode, String) {
    let resp = client
        .request(
            Request::builder()
                .method("GET")
                .uri(format!("http://{addr}{path}"))
                .header("Host", host)
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .expect("request");
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Poll a forwarding path until the upstream's first health probe passes with a given Host header.
async fn wait_ready_with_host(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    path: &str,
    host: &str,
) {
    for _ in 0..150 {
        let (status, _) = get_with_host(client, proxy, path, host).await;
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

#[tokio::test]
async fn metrics_expose_per_route_labels_and_unmatched_route_series() {
    let upstream = spawn_upstream().await;
    let admin = free_addr().await;

    let toml = format!(
        r#"
[observability]
admin_addr = "{admin}"

[[upstream]]
name = "echo"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
name = "api"
upstream = "echo"
[route.match]
path_prefix = "/api"

[[route]]
upstream = "echo"
[route.match]
path_prefix = "/web"
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let control = Arc::new(Control::from_manifest(&manifest, std::path::Path::new(".")).unwrap());

    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let data_addr = data_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, data_listener).await;
    });

    let client = client();
    wait_ready(&client, data_addr, "/api/").await;

    let (s_api, _) = get(&client, data_addr, "/api/x").await;
    assert_eq!(s_api, StatusCode::OK);
    let (s_web, _) = get(&client, data_addr, "/web/y").await;
    assert_eq!(s_web, StatusCode::OK);
    let (s_nope, _) = get(&client, data_addr, "/nope").await;
    assert_eq!(s_nope, StatusCode::NOT_FOUND);

    wait_admin(&client, admin).await;

    let (mstatus, metrics) = get(&client, admin, "/metrics").await;
    assert_eq!(mstatus, StatusCode::OK);

    let api_2xx = metrics
        .lines()
        .find(|l| l.starts_with("plecto_requests_total{route=\"api\",status_class=\"2xx\"}"))
        .expect("plecto_requests_total{route=\"api\",status_class=\"2xx\"} series present");
    let api_2xx_val: u64 = api_2xx
        .rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("counter line ends in a number");
    assert!(api_2xx_val >= 1, "api 2xx count >= 1, got {api_2xx_val}");

    let web_2xx = metrics
        .lines()
        .find(|l| l.starts_with("plecto_requests_total{route=\"/web\",status_class=\"2xx\"}"))
        .expect("plecto_requests_total{route=\"/web\",status_class=\"2xx\"} series present");
    let web_2xx_val: u64 = web_2xx
        .rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("counter line ends in a number");
    assert!(web_2xx_val >= 1, "web 2xx count >= 1, got {web_2xx_val}");

    let unmatched_4xx = metrics
        .lines()
        .find(|l| l.starts_with("plecto_requests_total{route=\"unmatched\",status_class=\"4xx\"}"))
        .expect("plecto_requests_total{route=\"unmatched\",status_class=\"4xx\"} series present");
    let unmatched_4xx_val: u64 = unmatched_4xx
        .rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("counter line ends in a number");
    assert!(
        unmatched_4xx_val >= 1,
        "unmatched 4xx count >= 1, got {unmatched_4xx_val}"
    );

    assert!(
        metrics.contains("plecto_requests_total{route=\"api\",status_class=\"3xx\"} 0"),
        "pre-registered zero line for api 3xx is present:\n{metrics}"
    );

    assert!(
        metrics.contains("plecto_rate_limited_total{route=\"api\"} 0"),
        "rate limited series for api route is present at 0:\n{metrics}"
    );

    assert!(
        !metrics.contains("plecto_request_duration_seconds_count{"),
        "duration histogram count must not carry route labels:\n{metrics}"
    );
    assert!(
        metrics.contains("plecto_request_duration_seconds_count"),
        "unlabelled duration histogram count is present:\n{metrics}"
    );
}

#[tokio::test]
async fn reload_preserves_frozen_series_for_dropped_routes_and_registers_new_routes_at_zero() {
    let upstream = spawn_upstream().await;
    let admin = free_addr().await;

    let toml1 = format!(
        r#"
[observability]
admin_addr = "{admin}"

[[upstream]]
name = "echo"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
name = "old"
upstream = "echo"
[route.match]
path_prefix = "/old"
"#
    );
    let manifest1 = Manifest::from_toml(&toml1).unwrap();
    let control = Arc::new(Control::from_manifest(&manifest1, std::path::Path::new(".")).unwrap());

    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let data_addr = data_listener.local_addr().unwrap();
    let control_for_serve = control.clone();
    tokio::spawn(async move {
        let _ = serve(control_for_serve, data_listener).await;
    });

    let client = client();
    wait_ready(&client, data_addr, "/old/").await;

    let (s, b) = get(&client, data_addr, "/old/x").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, "ok");

    wait_admin(&client, admin).await;

    let (mstatus, metrics1) = get(&client, admin, "/metrics").await;
    assert_eq!(mstatus, StatusCode::OK);
    let old_line = metrics1
        .lines()
        .find(|l| l.starts_with("plecto_requests_total{route=\"old\",status_class=\"2xx\"}"))
        .expect("route=old 2xx series present in initial scrape");
    let old_val: u64 = old_line
        .rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("number at end of series");
    assert!(old_val >= 1, "old route has counted at least 1 request");

    let toml2 = format!(
        r#"
[observability]
admin_addr = "{admin}"

[[upstream]]
name = "echo"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
name = "new"
upstream = "echo"
[route.match]
path_prefix = "/new"
"#
    );
    let new_manifest = Manifest::from_toml(&toml2).unwrap();
    control.reload(&new_manifest).expect("reload succeeds");

    let (_, metrics2) = get(&client, admin, "/metrics").await;
    assert!(
        metrics2.contains("plecto_requests_total{route=\"new\",status_class=\"2xx\"} 0"),
        "new route registered at zero:\n{metrics2}"
    );

    let old_line_after = metrics2
        .lines()
        .find(|l| l.starts_with("plecto_requests_total{route=\"old\",status_class=\"2xx\"}"))
        .expect("route=old series still exists after reload dropped it");
    let old_val_after: u64 = old_line_after
        .rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("number at end of series");
    assert_eq!(
        old_val_after, old_val,
        "dropped route series value must stay frozen across reload"
    );
}

#[tokio::test]
async fn host_split_routes_default_to_host_plus_prefix_names() {
    let upstream = spawn_upstream().await;
    let admin = free_addr().await;

    let toml = format!(
        r#"
[observability]
admin_addr = "{admin}"

[[upstream]]
name = "echo"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
upstream = "echo"
[route.match]
host = "public.example"
path_prefix = "/"

[[route]]
upstream = "echo"
[route.match]
host = "protected.example"
path_prefix = "/"
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let control = Arc::new(Control::from_manifest(&manifest, std::path::Path::new(".")).unwrap());

    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let data_addr = data_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, data_listener).await;
    });

    let client = client();
    wait_ready_with_host(&client, data_addr, "/", "public.example").await;

    let (s_pub, _) = get_with_host(&client, data_addr, "/", "public.example").await;
    assert_eq!(s_pub, StatusCode::OK);
    let (s_prot, _) = get_with_host(&client, data_addr, "/", "protected.example").await;
    assert_eq!(s_prot, StatusCode::OK);

    wait_admin(&client, admin).await;

    let (mstatus, metrics) = get(&client, admin, "/metrics").await;
    assert_eq!(mstatus, StatusCode::OK);

    let pub_2xx = metrics
        .lines()
        .find(|l| {
            l.starts_with("plecto_requests_total{route=\"public.example/\",status_class=\"2xx\"}")
        })
        .expect(
            "plecto_requests_total{route=\"public.example/\",status_class=\"2xx\"} series present",
        );
    let pub_2xx_val: u64 = pub_2xx
        .rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("counter line ends in a number");
    assert!(
        pub_2xx_val >= 1,
        "public.example/ 2xx count >= 1, got {pub_2xx_val}"
    );

    let prot_2xx = metrics
        .lines()
        .find(|l| l.starts_with("plecto_requests_total{route=\"protected.example/\",status_class=\"2xx\"}"))
        .expect("plecto_requests_total{route=\"protected.example/\",status_class=\"2xx\"} series present");
    let prot_2xx_val: u64 = prot_2xx
        .rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .expect("counter line ends in a number");
    assert!(
        prot_2xx_val >= 1,
        "protected.example/ 2xx count >= 1, got {prot_2xx_val}"
    );
}
