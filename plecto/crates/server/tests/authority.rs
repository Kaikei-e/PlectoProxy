//! E2E authority-consistency checks at the proxy ingress boundary.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

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

async fn upstream(_: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::new(Full::new(Bytes::from_static(b"upstream"))))
}

async fn spawn_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(upstream))
                    .await;
            });
        }
    });
    address
}

async fn spawn_proxy(upstream: SocketAddr) -> SocketAddr {
    let signer = TestSigner::new().unwrap();
    let manifest = Manifest::from_toml(&format!(
        r#"
[[upstream]]
name = "origin"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 10

[[route]]
name = "public"
upstream = "origin"
[route.match]
host = "public.example"
path_prefix = "/"

[[route]]
name = "protected"
upstream = "origin"
[route.match]
host = "protected.example"
path_prefix = "/"
"#
    ))
    .unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let control = Arc::new(Control::load(host, &manifest, Box::new(MemoryStore::new())).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, listener).await;
    });
    address
}

async fn request(proxy: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.read_to_string(&mut response),
    )
    .await
    .expect("proxy did not close a Connection: close response")
    .unwrap();
    response
}

async fn request_ready(proxy: SocketAddr, raw_request: &str) -> String {
    for _ in 0..100 {
        let response = request(proxy, raw_request).await;
        if !response.is_empty() && !response.starts_with("HTTP/1.1 503") {
            return response;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("upstream never became healthy");
}

#[tokio::test]
async fn rejects_an_absolute_form_authority_that_conflicts_with_host() {
    let proxy = spawn_proxy(spawn_upstream().await).await;

    let response = request_ready(
        proxy,
        "GET http://public.example/ HTTP/1.1\r\nHost: protected.example\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 400"),
        "the public route must not forward a request addressed to the protected host: {response:?}"
    );
}

#[tokio::test]
async fn accepts_matching_absolute_form_authority_and_host() {
    let proxy = spawn_proxy(spawn_upstream().await).await;

    let response = request_ready(
        proxy,
        "GET http://public.example/ HTTP/1.1\r\nHost: public.example\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 200"), "{response:?}");
}

#[tokio::test]
async fn rejects_duplicate_or_missing_http11_host() {
    let proxy = spawn_proxy(spawn_upstream().await).await;
    for raw in [
        "GET / HTTP/1.1\r\nHost: public.example\r\nHost: public.example\r\nConnection: close\r\n\r\n",
        "GET / HTTP/1.1\r\nConnection: close\r\n\r\n",
    ] {
        let response = request_ready(proxy, raw).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response:?}");
    }
}
