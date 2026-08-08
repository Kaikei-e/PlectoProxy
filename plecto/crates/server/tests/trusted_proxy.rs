//! E2E (tdd-workflow Phase 0) for client-identity restoration behind a declared trusted proxy
//! (ADR 000103): with `[listen.trusted_proxy]`, a request whose resolved peer falls inside the
//! declared CIDRs has its client address read out of the inbound `X-Forwarded-For` — right to
//! left, past the trusted hops — and every per-client observation point follows the real client
//! (the re-issued forwarding headers, the per-client-IP rate-limit bucket).
//!
//! The security half is the negative case: a peer OUTSIDE those CIDRs keeps the edge default
//! (ADR 000018 / 000022) — the inbound header is dropped and the peer re-issued — so a forged
//! `X-Forwarded-For` buys nothing. Absent section, absent feature.
//!
//! The echo upstream reflects the forwarding headers, so what the proxy decided is observable
//! end-to-end without reaching into server internals.

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

/// The loopback the test client connects from, declared trusted — it stands in for an L7 front
/// proxy that cannot speak PROXY v2.
const TRUSTED_LOOPBACK: &str = "[listen.trusted_proxy]\ntrusted = [\"127.0.0.0/8\"]\n";

/// A fake upstream reflecting the forwarding headers the proxy issued, so a test can read back
/// which address the proxy decided was the client.
async fn echo(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let reflect = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    Ok(Response::builder()
        .status(200)
        .header("x-upstream-xff", reflect("x-forwarded-for"))
        .header("x-upstream-xrealip", reflect("x-real-ip"))
        .header("x-upstream-xfproto", reflect("x-forwarded-proto"))
        .body(Full::new(Bytes::from_static(b"ok")))
        .unwrap())
}

async fn spawn_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(echo))
                    .await;
            });
        }
    });
    addr
}

/// Spawn an in-process proxy routing everything to the echo upstream. `listen` prepends manifest
/// sections (the `[listen.trusted_proxy]` under test), `route_extra` appends sub-tables to the
/// single `[[route]]` (the rate-limit variant).
async fn spawn_proxy(listen: &str, route_extra: &str) -> SocketAddr {
    let upstream = spawn_upstream().await;
    let toml = format!(
        r#"{listen}
[[upstream]]
name = "app"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
upstream = "app"
[route.match]
path_prefix = "/"
{route_extra}
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let control = Arc::new(Control::from_manifest(&manifest, Path::new(".")).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, listener).await;
    });
    proxy
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

/// GET `/api/x` with the given inbound headers → (status, upstream-seen XFF, X-Real-IP, proto).
async fn get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    inbound: &[(&str, &str)],
) -> (StatusCode, String, String, String) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("http://{proxy}/api/x"));
    for (name, value) in inbound {
        builder = builder.header(*name, *value);
    }
    let resp = client
        .request(builder.body(Empty::<Bytes>::new()).unwrap())
        .await
        .expect("request");
    let (parts, body) = resp.into_parts();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let seen = (
        parts.status,
        header("x-upstream-xff"),
        header("x-upstream-xrealip"),
        header("x-upstream-xfproto"),
    );
    let _ = body.collect().await;
    seen
}

/// Poll until the upstream's first health probe has passed (ADR 000017: instances start
/// pessimistic). Each poll claims a DISTINCT client address, so a per-client-IP rate limit in the
/// manifest under test can never conflate readiness probing with the request being measured.
async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr) {
    for i in 0..150u32 {
        let probe = format!("198.51.100.{}", 100 + i % 150);
        let (status, ..) = get(client, proxy, &[("x-forwarded-for", &probe)]).await;
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy");
}

#[tokio::test]
async fn a_trusted_front_proxy_names_the_client_in_x_forwarded_for() {
    let proxy = spawn_proxy(
        "[listen.trusted_proxy]\ntrusted = [\"127.0.0.0/8\", \"10.0.0.0/8\"]\n",
        "",
    )
    .await;
    let client = client();
    wait_ready(&client, proxy).await;

    // Read right to left: `10.0.0.1` is a declared hop, so the first address no declared proxy
    // vouched for is the client.
    let (status, xff, real_ip, _) = get(
        &client,
        proxy,
        &[("x-forwarded-for", "198.51.100.7, 10.0.0.1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        xff, "198.51.100.7",
        "the re-issued X-Forwarded-For must carry the restored client, not the trusted hop"
    );
    assert_eq!(
        real_ip, "198.51.100.7",
        "X-Real-IP is re-issued from the same restored client"
    );
}

#[tokio::test]
async fn an_untrusted_peer_cannot_forge_its_client_address() {
    // Loopback — where this test connects from — is deliberately NOT declared.
    let proxy = spawn_proxy(
        "[listen.trusted_proxy]\ntrusted = [\"203.0.113.0/24\"]\n",
        "",
    )
    .await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (status, xff, real_ip, _) = get(
        &client,
        proxy,
        &[("x-forwarded-for", "198.51.100.7, 10.0.0.1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        xff, "127.0.0.1",
        "a peer outside the declared CIDRs keeps the edge overwrite — a forged XFF is dropped"
    );
    assert_eq!(real_ip, "127.0.0.1");
}

#[tokio::test]
async fn no_declaration_means_no_restoration_path() {
    let proxy = spawn_proxy("", "").await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (status, xff, ..) = get(&client, proxy, &[("x-forwarded-for", "198.51.100.7")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        xff, "127.0.0.1",
        "without the section the feature does not exist (deny-by-default)"
    );
}

#[tokio::test]
async fn inbound_x_forwarded_proto_is_not_honoured_from_a_trusted_proxy() {
    let proxy = spawn_proxy(TRUSTED_LOOPBACK, "").await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (status, xff, _, proto) = get(
        &client,
        proxy,
        &[
            ("x-forwarded-for", "198.51.100.7"),
            ("x-forwarded-proto", "https"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(xff, "198.51.100.7", "the client address IS restored");
    assert_eq!(
        proto, "http",
        "the scheme stays the wire truth even from a trusted proxy (ADR 000103)"
    );
}

#[tokio::test]
async fn the_restored_client_keys_the_per_client_rate_limit() {
    let proxy = spawn_proxy(
        TRUSTED_LOOPBACK,
        "[route.rate_limit]\nrate = 1\nburst = 1\nkey = \"client-ip\"\n",
    )
    .await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (first, ..) = get(&client, proxy, &[("x-forwarded-for", "198.51.100.1")]).await;
    assert_eq!(
        first,
        StatusCode::OK,
        "the restored client spends the one token in its own bucket"
    );

    let (other, ..) = get(&client, proxy, &[("x-forwarded-for", "198.51.100.2")]).await;
    assert_eq!(
        other,
        StatusCode::OK,
        "a different restored client has its OWN bucket — keyed on the loopback peer instead, \
         this request would be shed"
    );

    let (again, ..) = get(&client, proxy, &[("x-forwarded-for", "198.51.100.1")]).await;
    assert_eq!(
        again,
        StatusCode::TOO_MANY_REQUESTS,
        "the first client's bucket is empty, so its own next request is shed"
    );
}
