//! E2E (tdd-workflow Phase 0) for HTTP/1.1 Upgrade passthrough (ADR 000048): a route that
//! declares `[route.upgrade] protocols = ["websocket"]` forwards the client's Upgrade handshake
//! to the upstream (controlled re-issue — hop-by-hop stripping stays the default), and on the
//! upstream's 101 the proxy splices a bidirectional byte tunnel between the two connections.
//! A route WITHOUT the declaration keeps today's deny-by-default behaviour: the Upgrade header
//! never reaches the upstream and the exchange stays plain HTTP.
//!
//! The tunnel test speaks a websocket-shaped handshake but ships opaque bytes through the
//! tunnel — the proxy is protocol-agnostic after the 101, so no WS framing is needed to pin
//! the contract.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use plecto_control::{Control, Host, Manifest, MemoryStore};
use plecto_host::test_support::TestSigner;
use plecto_server::serve;

fn loaded_control(toml: &str) -> Result<Control, plecto_control::ControlError> {
    let manifest = Manifest::from_toml(toml)?;
    let signer = TestSigner::new().unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Control::load(host, &manifest, Box::new(MemoryStore::new()))
}

async fn read_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    buf
}

/// A raw-TCP upstream: answers health probes with 200, answers a websocket handshake with 101
/// (echoing an end-to-end `sec-websocket-accept` header), then echoes every tunnel byte back.
async fn spawn_echo_upgrade_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let head = read_head(&mut stream).await;
                let head = String::from_utf8_lossy(&head).to_lowercase();
                if head.starts_with("get /healthz") {
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    return;
                }
                if !head.contains("upgrade: websocket") {
                    let _ = stream
                        .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                        .await;
                    return;
                }
                stream
                    .write_all(
                        b"HTTP/1.1 101 Switching Protocols\r\n\
                          upgrade: websocket\r\n\
                          connection: upgrade\r\n\
                          sec-websocket-accept: e2e-accept\r\n\r\n",
                    )
                    .await
                    .unwrap();
                let mut tmp = [0u8; 1024];
                loop {
                    match stream.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if stream.write_all(&tmp[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

async fn spawn_proxy(toml: &str) -> SocketAddr {
    let control = Arc::new(loaded_control(toml).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, listener).await;
    });
    proxy
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

/// One `GET /metrics` scrape off the admin endpoint (`connection: close`, so read-to-EOF works).
async fn scrape_metrics(admin: SocketAddr) -> String {
    let mut s = TcpStream::connect(admin).await.unwrap();
    s.write_all(b"GET /metrics HTTP/1.1\r\nhost: admin\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test]
async fn upgrade_route_tunnels_bytes_bidirectionally_after_the_101() {
    let upstream = spawn_echo_upgrade_upstream().await;
    let toml = format!(
        r#"
[[upstream]]
name = "ws"
addresses = ["127.0.0.1:{port}"]
[upstream.health]
path = "/healthz"
interval_ms = 50
healthy_threshold = 1

[[route]]
upstream = "ws"
[route.match]
path_prefix = "/ws"
[route.upgrade]
protocols = ["websocket"]
"#,
        port = upstream.port()
    );
    let proxy = spawn_proxy(&toml).await;

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut s = TcpStream::connect(proxy).await.unwrap();
            s.write_all(
                b"GET /ws HTTP/1.1\r\n\
                  host: e2e\r\n\
                  connection: upgrade\r\n\
                  upgrade: websocket\r\n\r\n",
            )
            .await
            .unwrap();
            let head = read_head(&mut s).await;
            let head = String::from_utf8_lossy(&head).to_lowercase();
            if !head.starts_with("http/1.1 101") {
                // instances start pessimistic (ADR 000017): retry until the probe promotes.
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            assert!(
                head.contains("upgrade: websocket"),
                "the 101 re-issues the Upgrade token: {head}"
            );
            assert!(
                head.contains("sec-websocket-accept: e2e-accept"),
                "end-to-end handshake headers pass through the 101: {head}"
            );
            // Opaque bytes flow BOTH ways through the spliced tunnel.
            s.write_all(b"ping-through-tunnel").await.unwrap();
            let mut echo = [0u8; 19];
            s.read_exact(&mut echo).await.unwrap();
            assert_eq!(&echo, b"ping-through-tunnel");
            s.write_all(b"second-frame").await.unwrap();
            let mut echo = [0u8; 12];
            s.read_exact(&mut echo).await.unwrap();
            assert_eq!(&echo, b"second-frame");
            break;
        }
    })
    .await
    .expect("the upgrade handshake never tunnelled through the proxy");
}

#[tokio::test]
async fn tunnel_occupancy_and_bytes_show_on_the_admin_metrics() {
    // Tunnel observability (ADR 000059): a live tunnel is visible as `plecto_tunnels_active`
    // (it left the RED tally at its 101 but still holds a breaker permit + LB pick), and its
    // per-direction byte totals land on the counters when it closes.
    let upstream = spawn_echo_upgrade_upstream().await;
    let admin = free_addr().await;
    let toml = format!(
        r#"
[observability]
admin_addr = "{admin}"

[[upstream]]
name = "ws"
addresses = ["127.0.0.1:{port}"]
[upstream.health]
path = "/healthz"
interval_ms = 50
healthy_threshold = 1

[[route]]
upstream = "ws"
[route.match]
path_prefix = "/ws"
[route.upgrade]
protocols = ["websocket"]
"#,
        port = upstream.port()
    );
    let proxy = spawn_proxy(&toml).await;

    tokio::time::timeout(Duration::from_secs(10), async {
        // Establish a tunnel (retrying past the pessimistic-start window) and echo bytes.
        let mut s = loop {
            let mut s = TcpStream::connect(proxy).await.unwrap();
            s.write_all(
                b"GET /ws HTTP/1.1\r\n\
                  host: e2e\r\n\
                  connection: upgrade\r\n\
                  upgrade: websocket\r\n\r\n",
            )
            .await
            .unwrap();
            let head = read_head(&mut s).await;
            if String::from_utf8_lossy(&head)
                .to_lowercase()
                .starts_with("http/1.1 101")
            {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        s.write_all(b"ping-through-tunnel").await.unwrap();
        let mut echo = [0u8; 19];
        s.read_exact(&mut echo).await.unwrap();

        assert!(
            scrape_metrics(admin)
                .await
                .contains("plecto_tunnels_active 1"),
            "a live tunnel must be visible on the gauge"
        );

        // Close the client end: the tunnel unwinds and records its byte totals exactly once.
        drop(s);
        loop {
            let text = scrape_metrics(admin).await;
            if text.contains("plecto_tunnels_active 0") {
                assert!(
                    text.contains("plecto_tunnel_bytes_up_total 19"),
                    "client→upstream bytes recorded at close: {text}"
                );
                assert!(
                    text.contains("plecto_tunnel_bytes_down_total 19"),
                    "upstream→client bytes recorded at close: {text}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the tunnel metrics never reached the expected values");
}

#[tokio::test]
async fn a_route_without_upgrade_keeps_stripping_the_upgrade_header() {
    // deny-by-default (ADR 000048): no `[route.upgrade]` → the Upgrade header must never reach
    // the upstream, and the exchange stays plain HTTP (the upstream's normal response returns).
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(|req: Request<Incoming>| async move {
                            let body = if req.headers().contains_key(hyper::header::UPGRADE) {
                                "upgrade-leaked"
                            } else {
                                "upgrade-stripped"
                            };
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(body))))
                        }),
                    )
                    .await;
            });
        }
    });
    let toml = format!(
        r#"
[[upstream]]
name = "plain"
addresses = ["127.0.0.1:{port}"]
[upstream.health]
path = "/healthz"
interval_ms = 50
healthy_threshold = 1

[[route]]
upstream = "plain"
[route.match]
path_prefix = "/plain"
"#,
        port = upstream.port()
    );
    let proxy = spawn_proxy(&toml).await;

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut s = TcpStream::connect(proxy).await.unwrap();
            s.write_all(
                b"GET /plain HTTP/1.1\r\n\
                  host: e2e\r\n\
                  connection: upgrade\r\n\
                  upgrade: websocket\r\n\r\n",
            )
            .await
            .unwrap();
            // Read until one of the upstream's body markers arrives (the proxy keeps the
            // connection alive, so read_to_end would block forever).
            let mut resp = Vec::new();
            let mut tmp = [0u8; 1024];
            let outcome = loop {
                // a pessimistic-window 503 never sends a marker; time the read out and retry.
                match tokio::time::timeout(Duration::from_millis(500), s.read(&mut tmp)).await {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break None,
                    Ok(Ok(n)) => resp.extend_from_slice(&tmp[..n]),
                }
                let text = String::from_utf8_lossy(&resp).to_string();
                if text.contains("upgrade-stripped") || text.contains("upgrade-leaked") {
                    break Some(text);
                }
            };
            if let Some(text) = outcome
                && text.starts_with("HTTP/1.1 200")
            {
                assert!(
                    text.contains("upgrade-stripped"),
                    "the Upgrade header must not reach an undeclared route's upstream: {text}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the plain route never served the normal (non-upgrade) response");
}
