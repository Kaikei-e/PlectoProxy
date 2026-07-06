//! E2E (tdd-workflow Phase 0) for graceful shutdown (ADR 000039 / 000059): `serve_with_shutdown`
//! must stop accepting when the shutdown future resolves, drain in-flight requests up to the
//! drain window (`[listen.drain] window_ms`), cut connections that outlive it, not let idle
//! keep-alive connections hold the drain open, flip `/readyz` to 503 at the signal (while
//! `/healthz` stays 200), and keep accepting through the readiness grace
//! (`[listen.drain] readiness_grace_ms`) so a front LB can take the replica out first.

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
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use plecto_control::{Control, Host, Manifest, MemoryStore, ResolvedArtifact};
use plecto_host::test_support::{TestSigner, bound_sbom, filter_hello_component};
use plecto_server::serve_with_shutdown;

/// A fake upstream that answers instantly EXCEPT on `/slow`, where it sleeps `delay` before
/// responding — so a test can hold a request in flight across the shutdown trigger while health
/// probes (`/healthz`) and readiness polls stay fast.
async fn spawn_upstream(delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| async move {
                            if req.uri().path() == "/slow" {
                                tokio::time::sleep(delay).await;
                            }
                            Ok::<_, std::convert::Infallible>(
                                Response::builder()
                                    .status(200)
                                    .body(Full::new(Bytes::from_static(b"upstream-ok")))
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

/// Build a control plane: filter-hello signed + loaded as a trusted filter, a route `/api`
/// (strip `/api`) → that chain → the given upstream address (same shape as `tests/e2e.rs`).
/// `extra` is appended to the manifest TOML — the drain settings under test
/// (`[listen.drain]`, ADR 000059) and/or an admin listener (`[observability]`).
fn control_for(upstream_addr: SocketAddr, extra: &str) -> Arc<Control> {
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

{extra}
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Arc::new(Control::load(host, &manifest, Box::new(store)).unwrap())
}

/// Spawn the proxy under test on an ephemeral port with a oneshot-triggered shutdown; the drain
/// window / readiness grace come from the manifest's `[listen.drain]` (ADR 000059). Returns the
/// bound addr, the trigger, and the serve task's handle.
async fn spawn_proxy(
    control: Arc<Control>,
) -> (
    SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<anyhow::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(serve_with_shutdown(control, listener, async move {
        let _ = rx.await;
    }));
    (addr, tx, handle)
}

/// Reserve a distinct loopback port for the admin listener (bound-then-dropped, like
/// `tests/observability.rs`).
async fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// Send a GET through the proxy; `Err` when the connection was refused or cut mid-response.
async fn try_get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    path: &str,
) -> anyhow::Result<(StatusCode, String)> {
    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{proxy}{path}"))
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = client.request(req).await?;
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await?.to_bytes();
    Ok((parts.status, String::from_utf8_lossy(&bytes).into_owned()))
}

/// Wait until the upstream's first health probe lands (instances start pessimistic, ADR 000017).
async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr) {
    for _ in 0..100 {
        if let Ok((status, _)) = try_get(client, proxy, "/api/__ready").await
            && status != StatusCode::SERVICE_UNAVAILABLE
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

#[tokio::test]
async fn graceful_shutdown_drains_inflight_and_stops_accepting() {
    let upstream = spawn_upstream(Duration::from_millis(400)).await;
    let control = control_for(upstream, "[listen.drain]\nwindow_ms = 5000\n");
    let (proxy, shutdown, server) = spawn_proxy(control).await;
    let client = client();
    wait_ready(&client, proxy).await;

    // Hold a request in flight (the upstream sleeps 400 ms on /slow), then trigger shutdown.
    let inflight = {
        let client = client.clone();
        tokio::spawn(async move { try_get(&client, proxy, "/api/slow").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    shutdown.send(()).unwrap();

    let (status, body) = inflight
        .await
        .unwrap()
        .expect("the in-flight request must complete during the drain window");
    assert_eq!(status, StatusCode::OK, "drained response is the real one");
    assert_eq!(body, "upstream-ok");

    let served = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("serve must return once in-flight connections drained")
        .unwrap();
    served.expect("a drained shutdown is a clean (Ok) exit");

    // Accept stopped: a fresh TCP connect must be refused after shutdown.
    assert!(
        tokio::net::TcpStream::connect(proxy).await.is_err(),
        "the listener must be closed after shutdown"
    );
}

#[tokio::test]
async fn drain_deadline_cuts_off_connections_that_outlive_it() {
    // The upstream would hold /slow for 10 s, far beyond the 200 ms drain window declared in
    // the manifest (`[listen.drain] window_ms`, ADR 000059).
    let upstream = spawn_upstream(Duration::from_secs(10)).await;
    let control = control_for(upstream, "[listen.drain]\nwindow_ms = 200\n");
    let (proxy, shutdown, server) = spawn_proxy(control).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let inflight = {
        let client = client.clone();
        tokio::spawn(async move { try_get(&client, proxy, "/api/slow").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    shutdown.send(()).unwrap();

    // The deadline bounds shutdown: serve returns in ~200 ms, NOT after the upstream's 10 s.
    let served = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("serve must return once the drain deadline expires")
        .unwrap();
    served.expect("a deadline-bounded shutdown is still a clean (Ok) exit");

    assert!(
        inflight.await.unwrap().is_err(),
        "a request that outlives the drain deadline is cut, not answered"
    );
}

#[tokio::test]
async fn idle_keepalive_connections_do_not_hold_the_drain_open() {
    let upstream = spawn_upstream(Duration::ZERO).await;
    // Generous window: if idle connections were WAITED on instead of closed, serve would sit
    // here for 10 s and the timeout below would trip.
    let control = control_for(upstream, "[listen.drain]\nwindow_ms = 10000\n");
    let (proxy, shutdown, server) = spawn_proxy(control).await;
    let client = client();
    wait_ready(&client, proxy).await;

    // A completed request leaves an idle pooled keep-alive connection to the proxy.
    let (status, _) = try_get(&client, proxy, "/api/hello").await.unwrap();
    assert_eq!(status, StatusCode::OK);

    shutdown.send(()).unwrap();

    let served = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("idle keep-alive connections must be closed, not drained until the deadline")
        .unwrap();
    served.expect("shutdown with only idle connections is a clean (Ok) exit");
}

#[tokio::test]
async fn readyz_flips_to_503_at_the_signal_while_healthz_stays_200() {
    // The readiness contract (ADR 000059): the shutdown signal makes `/readyz` not-ready so the
    // front LB removes the replica; `/healthz` (liveness) stays 200 through the drain so a
    // supervisor does not restart-loop a process that is exiting on purpose.
    let upstream = spawn_upstream(Duration::from_millis(800)).await;
    let admin = free_addr().await;
    let control = control_for(
        upstream,
        &format!("[observability]\nadmin_addr = \"{admin}\"\n\n[listen.drain]\nwindow_ms = 5000\n"),
    );
    let (proxy, shutdown, server) = spawn_proxy(control).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (status, body) = try_get(&client, admin, "/readyz").await.unwrap();
    assert_eq!(status, StatusCode::OK, "serving → ready");
    assert_eq!(body, "ready\n");

    // Hold a request in flight so the server sits inside its drain window when we probe.
    let inflight = {
        let client = client.clone();
        tokio::spawn(async move { try_get(&client, proxy, "/api/slow").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    shutdown.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (status, body) = try_get(&client, admin, "/readyz").await.unwrap();
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "draining → not ready"
    );
    assert_eq!(body, "draining\n");
    let (status, body) = try_get(&client, admin, "/healthz").await.unwrap();
    assert_eq!(status, StatusCode::OK, "liveness holds through the drain");
    assert_eq!(body, "ok\n");

    let (status, _) = inflight
        .await
        .unwrap()
        .expect("the in-flight request still completes during the drain window");
    assert_eq!(status, StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("serve returns once drained")
        .unwrap()
        .expect("clean exit");
}

#[tokio::test]
async fn readiness_grace_keeps_accepting_before_the_drain_starts() {
    // With `readiness_grace_ms` declared (ADR 000059), the order is: signal → /readyz 503 →
    // grace (accepts continue — the LB may still route here) → drain. A request sent DURING the
    // grace must be served normally, and serve must not return before the grace has elapsed.
    let upstream = spawn_upstream(Duration::ZERO).await;
    let admin = free_addr().await;
    let control = control_for(
        upstream,
        &format!(
            "[observability]\nadmin_addr = \"{admin}\"\n\n\
             [listen.drain]\nreadiness_grace_ms = 600\nwindow_ms = 5000\n"
        ),
    );
    let (proxy, shutdown, server) = spawn_proxy(control).await;
    // A second client whose pool is empty until used: its first request below must open a FRESH
    // connection during the grace (the main client would reuse its pre-signal keep-alive one).
    let fresh = client();
    let client = client();
    wait_ready(&client, proxy).await;

    let signalled = std::time::Instant::now();
    shutdown.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Not-ready is immediate…
    let (status, _) = try_get(&client, admin, "/readyz").await.unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    // …but the data plane still serves new work through the grace, on a fresh connection.
    let (status, body) = try_get(&fresh, proxy, "/api/during-grace")
        .await
        .expect("a request during the readiness grace is accepted and served");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "upstream-ok");

    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("serve returns after grace + drain")
        .unwrap()
        .expect("clean exit");
    assert!(
        signalled.elapsed() >= Duration::from_millis(600),
        "serve must not return before the readiness grace has elapsed"
    );
}
