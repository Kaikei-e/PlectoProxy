//! E2E (tdd-workflow Phase 0) for native response compression (`[route.compression]`,
//! ADR 000074 handover → implementation ADR): RFC 9110 §12.5.3 negotiation against the client's
//! `Accept-Encoding`, applied AFTER the response filter chain (filters always see identity),
//! with the industry-converged safety defaults — content-type allowlist, min-length threshold,
//! already-encoded / `no-transform` / 206 / HEAD skips, `Vary: Accept-Encoding`, strong-ETag
//! weakening — and deny-by-default: no `[route.compression]` block, no compression.

use std::convert::Infallible;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;

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

/// A body comfortably over every threshold in these tests, and repetitive enough that any codec
/// visibly shrinks it — so "compressed" vs "identity" is assertable by size, not just headers.
fn big_text() -> String {
    "All work and no play makes the fast path a dull proxy. ".repeat(100)
}

/// A fake upstream serving the eligibility matrix by path. Every response carries `x-upstream`
/// so a test can prove the bytes really crossed the proxy.
async fn upstream(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let builder = Response::builder().status(200).header("x-upstream", "1");
    let resp = match req.uri().path() {
        "/big.html" => builder
            .header("content-type", "text/html; charset=utf-8")
            .header("etag", "\"v1\"")
            .header("accept-ranges", "bytes")
            .body(Full::new(Bytes::from(big_text())))
            .unwrap(),
        "/small.html" => builder
            .header("content-type", "text/html")
            .body(Full::new(Bytes::from_static(b"tiny")))
            .unwrap(),
        "/image.png" => builder
            .header("content-type", "image/png")
            .body(Full::new(Bytes::from(big_text())))
            .unwrap(),
        "/no-transform.html" => builder
            .header("content-type", "text/html")
            .header("cache-control", "no-transform")
            .body(Full::new(Bytes::from(big_text())))
            .unwrap(),
        "/pre-encoded.html" => {
            // The upstream compressed this itself (e.g. it honoured the forwarded
            // Accept-Encoding); the proxy must forward it untouched, ETag included.
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(5));
            std::io::Write::write_all(&mut gz, big_text().as_bytes()).unwrap();
            builder
                .header("content-type", "text/html")
                .header("content-encoding", "gzip")
                .header("etag", "\"v1\"")
                .body(Full::new(Bytes::from(gz.finish().unwrap())))
                .unwrap()
        }
        "/partial.html" => Response::builder()
            .status(206)
            .header("x-upstream", "1")
            .header("content-type", "text/html")
            .header("content-range", "bytes 0-5599/5600")
            .body(Full::new(Bytes::from(big_text())))
            .unwrap(),
        "/weak-etag.html" => builder
            .header("content-type", "text/html")
            .header("etag", "W/\"v1\"")
            .body(Full::new(Bytes::from(big_text())))
            .unwrap(),
        _ => builder
            .header("content-type", "text/plain")
            .body(Full::new(Bytes::from_static(b"ok")))
            .unwrap(),
    };
    Ok(resp)
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
                    .serve_connection(TokioIo::new(stream), service_fn(upstream))
                    .await;
            });
        }
    });
    addr
}

/// Spawn a filterless proxy routing `/` to the matrix upstream, with `compression` appended
/// verbatim to the route (empty string = the deny-by-default control case).
async fn spawn_proxy(compression: &str) -> SocketAddr {
    let upstream_addr = spawn_upstream().await;
    let toml = format!(
        r#"
[[upstream]]
name = "app"
addresses = ["{upstream_addr}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
upstream = "app"
[route.match]
path_prefix = "/"
{compression}
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let signer = TestSigner::new().unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let control = Control::load(host, &manifest, Box::new(MemoryStore::new())).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(Arc::new(control), listener).await;
    });
    proxy
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn request(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (hyper::http::response::Parts, Bytes) {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("http://{proxy}{path}"));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let resp = client
        .request(builder.body(Empty::<Bytes>::new()).unwrap())
        .await
        .expect("proxy request");
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts, bytes)
}

async fn get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (hyper::http::response::Parts, Bytes) {
    request(client, proxy, "GET", path, headers).await
}

async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr) {
    for _ in 0..100 {
        let (parts, _) = get(client, proxy, "/__ready", &[]).await;
        if parts.status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn content_encoding(parts: &hyper::http::response::Parts) -> Option<&str> {
    parts
        .headers
        .get("content-encoding")
        .map(|v| v.to_str().unwrap())
}

fn vary_names_accept_encoding(parts: &hyper::http::response::Parts) -> bool {
    parts.headers.get_all("vary").iter().any(|v| {
        v.to_str().is_ok_and(|s| {
            s.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("accept-encoding"))
        })
    })
}

#[tokio::test]
async fn gzip_roundtrip_sets_the_transform_headers() {
    let proxy = spawn_proxy("[route.compression]").await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/big.html", &[("accept-encoding", "gzip")]).await;

    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(content_encoding(&parts), Some("gzip"));
    assert_eq!(
        parts.headers.get("x-upstream").map(|v| v.as_bytes()),
        Some(b"1".as_slice()),
        "compression edits the transform headers only; the rest forward untouched"
    );
    assert!(
        parts.headers.get("content-length").is_none(),
        "the identity Content-Length must not describe the compressed stream"
    );
    assert!(
        vary_names_accept_encoding(&parts),
        "a negotiated response must declare Vary: Accept-Encoding"
    );
    assert_eq!(
        parts.headers.get("etag").map(|v| v.as_bytes()),
        Some(b"W/\"v1\"".as_slice()),
        "a strong ETag must weaken across the transform (RFC 9110 §8.8.3)"
    );
    assert!(
        parts.headers.get("accept-ranges").is_none(),
        "Accept-Ranges named the identity representation — drop it when content-coding (RFC 9110 §14)"
    );
    assert!(
        body.len() < big_text().len(),
        "the wire bytes are actually compressed"
    );
    assert_eq!(gunzip(&body), big_text().as_bytes());
}

#[tokio::test]
async fn brotli_and_zstd_roundtrip() {
    let proxy = spawn_proxy("[route.compression]").await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/big.html", &[("accept-encoding", "br")]).await;
    assert_eq!(content_encoding(&parts), Some("br"));
    let mut out = Vec::new();
    brotli::Decompressor::new(body.as_ref(), 4096)
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, big_text().as_bytes());

    let (parts, body) = get(&client, proxy, "/big.html", &[("accept-encoding", "zstd")]).await;
    assert_eq!(content_encoding(&parts), Some("zstd"));
    assert_eq!(
        zstd::decode_all(body.as_ref()).unwrap(),
        big_text().as_bytes()
    );
}

#[tokio::test]
async fn negotiation_honours_qvalues_and_server_preference() {
    let proxy = spawn_proxy("[route.compression]").await;
    let client = client();
    wait_ready(&client, proxy).await;

    // The client's non-zero maximum wins regardless of listing order (RFC 9110 §12.5.3).
    let (parts, _) = get(
        &client,
        proxy,
        "/big.html",
        &[("accept-encoding", "br;q=1.0, gzip;q=0.5")],
    )
    .await;
    assert_eq!(content_encoding(&parts), Some("br"));

    // A q-tie falls to the server preference: the configured algorithm order (zstd first).
    let (parts, _) = get(
        &client,
        proxy,
        "/big.html",
        &[("accept-encoding", "gzip, br, zstd")],
    )
    .await;
    assert_eq!(content_encoding(&parts), Some("zstd"));

    // `q=0` excludes a coding; everything at q=0 → identity, never an error (a proxy MAY
    // transform — declining to is always safe).
    let (parts, body) = get(
        &client,
        proxy,
        "/big.html",
        &[("accept-encoding", "gzip;q=0, br;q=0, zstd;q=0")],
    )
    .await;
    assert_eq!(content_encoding(&parts), None);
    assert_eq!(body.as_ref(), big_text().as_bytes());

    // `*` covers the codings not explicitly listed.
    let (parts, _) = get(&client, proxy, "/big.html", &[("accept-encoding", "*")]).await;
    assert_eq!(content_encoding(&parts), Some("zstd"));

    // Explicit `identity` competes on qvalue (RFC 9110 §12.5.3).
    let (parts, body) = get(
        &client,
        proxy,
        "/big.html",
        &[("accept-encoding", "identity;q=1.0, gzip;q=0.5")],
    )
    .await;
    assert_eq!(content_encoding(&parts), None);
    assert_eq!(body.as_ref(), big_text().as_bytes());
    assert_eq!(
        parts.headers.get("accept-ranges").map(|v| v.as_bytes()),
        Some(b"bytes".as_slice()),
        "identity path must not strip Accept-Ranges"
    );

    let (parts, _) = get(
        &client,
        proxy,
        "/big.html",
        &[("accept-encoding", "gzip;q=1.0, identity;q=0.5")],
    )
    .await;
    assert_eq!(content_encoding(&parts), Some("gzip"));
}

#[tokio::test]
async fn no_accept_encoding_stays_identity() {
    let proxy = spawn_proxy("[route.compression]").await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/big.html", &[]).await;
    assert_eq!(content_encoding(&parts), None);
    assert_eq!(body.as_ref(), big_text().as_bytes());
    assert_eq!(
        parts.headers.get("etag").map(|v| v.as_bytes()),
        Some(b"\"v1\"".as_slice()),
        "an untransformed response keeps its strong ETag"
    );
    assert!(
        vary_names_accept_encoding(&parts),
        "an eligible route varies by Accept-Encoding even when identity is chosen — a shared \
         cache must not serve this identity bytes to a gzip-capable client"
    );
}

#[tokio::test]
async fn safety_skips_leave_the_response_untouched() {
    let proxy = spawn_proxy("[route.compression]").await;
    let client = client();
    wait_ready(&client, proxy).await;
    let ae = [("accept-encoding", "gzip")];

    // Below the min-length threshold: not worth a dictionary + trailer.
    let (parts, body) = get(&client, proxy, "/small.html", &ae).await;
    assert_eq!(content_encoding(&parts), None);
    assert_eq!(body.as_ref(), b"tiny");

    // Content-type outside the allowlist (already-compressed media).
    let (parts, body) = get(&client, proxy, "/image.png", &ae).await;
    assert_eq!(content_encoding(&parts), None);
    assert_eq!(body.as_ref(), big_text().as_bytes());

    // `Cache-Control: no-transform` — transforming is a MUST NOT (RFC 9110 §7.7).
    let (parts, body) = get(&client, proxy, "/no-transform.html", &ae).await;
    assert_eq!(content_encoding(&parts), None);
    assert_eq!(body.as_ref(), big_text().as_bytes());

    // Already encoded by the upstream: forward verbatim, ETag untouched.
    let (parts, body) = get(&client, proxy, "/pre-encoded.html", &ae).await;
    assert_eq!(content_encoding(&parts), Some("gzip"));
    assert_eq!(
        parts.headers.get("etag").map(|v| v.as_bytes()),
        Some(b"\"v1\"".as_slice()),
        "the proxy did not transform, so it must not touch the validator"
    );
    assert_eq!(gunzip(&body), big_text().as_bytes());

    // 206 Partial Content: a range of the identity representation must stay identity bytes.
    let (parts, body) = get(&client, proxy, "/partial.html", &ae).await;
    assert_eq!(parts.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(content_encoding(&parts), None);
    assert_eq!(body.as_ref(), big_text().as_bytes());

    // HEAD: no body to compress; the head must not claim an encoding.
    let (parts, _) = request(&client, proxy, "HEAD", "/big.html", &ae).await;
    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(content_encoding(&parts), None);
}

#[tokio::test]
async fn already_weak_etag_is_not_double_weakened() {
    let proxy = spawn_proxy("[route.compression]").await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, _) = get(
        &client,
        proxy,
        "/weak-etag.html",
        &[("accept-encoding", "gzip")],
    )
    .await;
    assert_eq!(content_encoding(&parts), Some("gzip"));
    assert_eq!(
        parts.headers.get("etag").map(|v| v.as_bytes()),
        Some(b"W/\"v1\"".as_slice())
    );
}

#[tokio::test]
async fn compression_is_deny_by_default() {
    let proxy = spawn_proxy("").await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/big.html", &[("accept-encoding", "gzip")]).await;
    assert_eq!(
        content_encoding(&parts),
        None,
        "no [route.compression] block, no transform — deny-by-default"
    );
    assert_eq!(body.as_ref(), big_text().as_bytes());
    assert!(
        !vary_names_accept_encoding(&parts),
        "a route that never varies must not claim to"
    );
}

#[tokio::test]
async fn config_narrows_algorithms_min_length_and_content_types() {
    let proxy = spawn_proxy(
        r#"[route.compression]
algorithms = ["gzip"]
min_length = 1
content_types = ["image/png"]
"#,
    )
    .await;
    let client = client();
    wait_ready(&client, proxy).await;

    // zstd is not offered on this route even though the client accepts it.
    let (parts, _) = get(
        &client,
        proxy,
        "/image.png",
        &[("accept-encoding", "zstd, gzip;q=0.5")],
    )
    .await;
    assert_eq!(content_encoding(&parts), Some("gzip"));

    // The narrowed allowlist replaced the default: text/html is no longer eligible.
    let (parts, body) = get(&client, proxy, "/big.html", &[("accept-encoding", "gzip")]).await;
    assert_eq!(content_encoding(&parts), None);
    assert_eq!(body.as_ref(), big_text().as_bytes());
}
