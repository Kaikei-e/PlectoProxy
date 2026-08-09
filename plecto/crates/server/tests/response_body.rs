//! E2E (tdd-workflow Phase 0) for buffered response-body inspection (`on-response-body`,
//! ADR 000098).
//!
//! The keystone is that the host holds the response headers until the body decision returns. That
//! is what makes every scenario below expressible at all: a filter can still rewrite or replace a
//! response whose headers a streaming proxy would already have sent, and the host can still refuse
//! one it could not inspect. So the assertions are deliberately client-visible — what arrived, not
//! what the chain decided.
//!
//! `filter-respbody` drives them: it exports `on-response-body` and NOT `on-request-body`, which
//! also makes this file the end-to-end proof that buffering is decided per direction.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

use plecto_control::{Control, Host, Manifest, MemoryStore, ResolvedArtifact};
use plecto_host::test_support::{
    TestSigner, bound_sbom, filter_apikey_component, filter_respbody_component,
};
use plecto_server::serve;

/// What the fake upstream answers `/api/*` with. `/healthz` always answers 200 so the instance
/// goes healthy regardless of the scenario under test.
#[derive(Clone, Copy)]
struct UpstreamReply {
    status: u16,
    content_type: &'static str,
    extra: &'static [(&'static str, &'static str)],
    body: &'static [u8],
}

impl UpstreamReply {
    const fn json(body: &'static [u8]) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            extra: &[],
            body,
        }
    }
}

/// What the upstream saw of the FORWARDED request — the request-normalisation assertion. Per
/// upstream rather than global, so the scenarios stay independent when run in parallel.
#[derive(Default)]
struct Observed {
    accept_encoding: AtomicBool,
    range: AtomicBool,
}

async fn spawn_upstream(reply: UpstreamReply) -> (SocketAddr, Arc<Observed>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let observed = Arc::new(Observed::default());
    let seen = observed.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| {
                            let seen = seen.clone();
                            async move {
                                if req.uri().path() == "/healthz" {
                                    return Ok::<_, Infallible>(
                                        Response::builder()
                                            .status(200)
                                            .body(Full::new(Bytes::from_static(b"ok")))
                                            .unwrap(),
                                    );
                                }
                                if req.headers().contains_key("accept-encoding") {
                                    seen.accept_encoding.store(true, Ordering::Relaxed);
                                }
                                if req.headers().contains_key("range")
                                    || req.headers().contains_key("if-range")
                                {
                                    seen.range.store(true, Ordering::Relaxed);
                                }
                                let mut builder = Response::builder()
                                    .status(reply.status)
                                    .header("content-type", reply.content_type)
                                    .header("x-drop-me", "1");
                                for (name, value) in reply.extra {
                                    builder = builder.header(*name, *value);
                                }
                                Ok(builder
                                    .body(Full::new(Bytes::from_static(reply.body)))
                                    .unwrap())
                            }
                        }),
                    )
                    .await;
            });
        }
    });
    (addr, observed)
}

/// An upstream whose body is generated at request time, so a scenario can exceed a cap without a
/// megabyte-sized literal in the source.
async fn spawn_big_upstream(len: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |req: Request<Incoming>| async move {
                            if req.uri().path() == "/healthz" {
                                return Ok::<_, Infallible>(
                                    Response::builder()
                                        .status(200)
                                        .body(Full::new(Bytes::from_static(b"ok")))
                                        .unwrap(),
                                );
                            }
                            Ok(Response::builder()
                                .status(200)
                                .header("content-type", "text/plain")
                                .body(Full::new(Bytes::from(vec![b'z'; len])))
                                .unwrap())
                        }),
                    )
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

/// A `/api` route running one signed filter, plus whatever `[route.response_body]` a scenario
/// declares. `component` picks which filter: the response-body one, or a header-only filter for
/// the zero-copy control case.
fn control_with(component: Vec<u8>, upstream_addr: SocketAddr, route_extra: &str) -> Arc<Control> {
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(&component).unwrap();
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let mut store = MemoryStore::new();
    let digest = store.insert(
        "f",
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
id = "f"
source = "f"
digest = "{digest}"
isolation = "trusted"

[[upstream]]
name = "backend"
addresses = ["{upstream_addr}"]
[upstream.health]
path = "/healthz"
interval_ms = 50

[[route]]
filters = ["f"]
upstream = "backend"
[route.match]
path_prefix = "/api"
{route_extra}
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Arc::new(Control::load(host, &manifest, Box::new(store)).unwrap())
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (hyper::http::response::Parts, Bytes) {
    let mut builder = Request::builder()
        .method("GET")
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

async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr) {
    for _ in 0..150 {
        let (parts, _) = get(client, proxy, "/api/__ready", &[]).await;
        if parts.status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

// --- (a) `%continue`: the inspected body reaches the client byte for byte ---

#[tokio::test]
async fn a_continue_decision_delivers_the_upstream_body_unchanged() {
    let (upstream, _seen) = spawn_upstream(UpstreamReply::json(b"{\"ok\":true}")).await;
    let proxy = spawn_proxy(control_with(filter_respbody_component(), upstream, "")).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/api/thing", &[]).await;

    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(
        body.as_ref(),
        b"{\"ok\":true}",
        "an inspected-but-unmodified body arrives verbatim"
    );
    assert_eq!(
        parts.headers.get("content-length").map(|v| v.as_bytes()),
        Some(b"11".as_slice()),
        "the host frames what it actually sends"
    );
    assert!(
        parts.headers.contains_key("x-drop-me"),
        "a `continue` touches no header"
    );
}

// --- (b) `modified`: the rewritten body is what the client receives ---

#[tokio::test]
async fn a_modified_decision_reaches_the_client_with_its_header_edits_and_framing() {
    let (upstream, _seen) = spawn_upstream(UpstreamReply::json(b"{\"token\":\"SECRET\"}")).await;
    let proxy = spawn_proxy(control_with(filter_respbody_component(), upstream, "")).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/api/thing", &[]).await;

    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(body.as_ref(), b"{\"token\":\"[redacted]\"}");
    assert_eq!(
        parts.headers.get("x-plecto-redacted").map(|v| v.as_bytes()),
        Some(b"1".as_slice()),
        "the header edits a transform forces ride along"
    );
    assert!(
        !parts.headers.contains_key("x-drop-me"),
        "and so do its removals"
    );
    assert_eq!(
        parts.headers.get("content-length").map(|v| v.as_bytes()),
        Some(b"22".as_slice()),
        "content-length is the HOST's, re-derived from the transformed bytes"
    );
}

// --- (c) `replace`: the upstream body is discarded for a synthesised response ---

#[tokio::test]
async fn a_replace_decision_discards_the_upstream_body_and_answers_with_its_own() {
    let (upstream, _seen) =
        spawn_upstream(UpstreamReply::json(b"{\"upstream\":\"payload\"}")).await;
    let proxy = spawn_proxy(control_with(filter_respbody_component(), upstream, "")).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/api/thing?respbody=replace", &[]).await;

    assert_eq!(parts.status, StatusCode::IM_A_TEAPOT);
    assert_eq!(body.as_ref(), b"replaced by filter-respbody");
    assert_eq!(
        parts.headers.get("x-plecto-respbody").map(|v| v.as_bytes()),
        Some(b"replaced".as_slice())
    );
}

// --- (d) the uninspectable classes fail closed by default ---

#[tokio::test]
async fn a_streaming_content_type_is_refused_by_default() {
    // Buffering an event stream would break it, so its absence from the allowlist must not read as
    // "quietly skip": the route declared an inspecting filter, and the host says so.
    let (upstream, _seen) = spawn_upstream(UpstreamReply {
        status: 200,
        content_type: "text/event-stream",
        extra: &[],
        body: b"data: hello\n\n",
    })
    .await;
    let proxy = spawn_proxy(control_with(filter_respbody_component(), upstream, "")).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, _) = get(&client, proxy, "/api/events", &[]).await;

    assert_eq!(parts.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        parts.headers.get("x-plecto-fault").map(|v| v.as_bytes()),
        Some(b"response-body-uninspectable".as_slice())
    );
}

#[tokio::test]
async fn a_content_encoded_response_is_refused_by_default() {
    // `Accept-Encoding` is stripped from the forwarded request, so an encoded response means the
    // upstream encoded unbidden — the bytes are not the ones a filter would be reading.
    let (upstream, seen) = spawn_upstream(UpstreamReply {
        status: 200,
        content_type: "application/json",
        extra: &[("content-encoding", "gzip")],
        body: b"\x1f\x8b not really gzip",
    })
    .await;
    let proxy = spawn_proxy(control_with(filter_respbody_component(), upstream, "")).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, _) = get(&client, proxy, "/api/thing", &[("accept-encoding", "gzip")]).await;

    assert_eq!(parts.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        parts.headers.get("x-plecto-fault").map(|v| v.as_bytes()),
        Some(b"response-body-uninspectable".as_slice())
    );
    assert!(
        !seen.accept_encoding.load(Ordering::Relaxed),
        "the forwarded request must carry no Accept-Encoding on an inspecting route"
    );
}

#[tokio::test]
async fn a_partial_content_response_is_refused_by_default() {
    let (upstream, seen) = spawn_upstream(UpstreamReply {
        status: 206,
        content_type: "application/json",
        extra: &[("content-range", "bytes 0-9/100")],
        body: b"0123456789",
    })
    .await;
    let proxy = spawn_proxy(control_with(filter_respbody_component(), upstream, "")).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, _) = get(
        &client,
        proxy,
        "/api/thing",
        &[("range", "bytes=0-9"), ("if-range", "\"v1\"")],
    )
    .await;

    assert_eq!(parts.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        parts.headers.get("x-plecto-fault").map(|v| v.as_bytes()),
        Some(b"response-body-uninspectable".as_slice())
    );
    assert!(
        !seen.range.load(Ordering::Relaxed),
        "Range / If-Range must not reach the upstream on an inspecting route"
    );
}

// --- (e) over-cap fails closed by default; the other two modes are explicit opt-ins ---

#[tokio::test]
async fn an_over_cap_body_is_refused_by_default() {
    let upstream = spawn_big_upstream(4096).await;
    let proxy = spawn_proxy(control_with(
        filter_respbody_component(),
        upstream,
        "[route.response_body]\nmax_bytes = 1024\n",
    ))
    .await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, _) = get(&client, proxy, "/api/big", &[]).await;

    assert_eq!(parts.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        parts.headers.get("x-plecto-fault").map(|v| v.as_bytes()),
        Some(b"response-body-too-large".as_slice()),
        "502, not 413 — that status is the request plane's vocabulary"
    );
}

#[tokio::test]
async fn an_over_cap_body_passes_through_when_the_route_opts_in() {
    let upstream = spawn_big_upstream(4096).await;
    let proxy = spawn_proxy(control_with(
        filter_respbody_component(),
        upstream,
        "[route.response_body]\nmax_bytes = 1024\nover_cap = \"passthrough\"\n",
    ))
    .await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/api/big", &[]).await;

    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(
        body.len(),
        4096,
        "the already-read head is re-attached to the untouched remainder"
    );
    assert!(body.iter().all(|b| *b == b'z'));
}

#[tokio::test]
async fn process_partial_inspects_the_head_and_forwards_the_whole_body() {
    let upstream = spawn_big_upstream(4096).await;
    let proxy = spawn_proxy(control_with(
        filter_respbody_component(),
        upstream,
        "[route.response_body]\nmax_bytes = 1024\nover_cap = \"process-partial\"\n",
    ))
    .await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/api/big", &[]).await;

    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(body.len(), 4096);
}

#[tokio::test]
async fn process_partial_refuses_a_rewrite_of_a_body_it_only_showed_a_prefix_of() {
    // The host cannot frame a transform applied to a prefix into a coherent representation, so
    // partial processing accepts inspection and refuses rewriting rather than sending a body no
    // filter ever asked for.
    let upstream = spawn_big_upstream(4096).await;
    let proxy = spawn_proxy(control_with(
        filter_respbody_component(),
        upstream,
        "[route.response_body]\nmax_bytes = 1024\nover_cap = \"process-partial\"\ncontent_types = [\"text/plain\"]\n",
    ))
    .await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, _) = get(&client, proxy, "/api/big?respbody=shrink", &[]).await;

    assert_eq!(parts.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        parts.headers.get("x-plecto-fault").map(|v| v.as_bytes()),
        Some(b"response-body-partial-modified".as_slice())
    );
}

// --- (f) the explicit opt-out passes an uninspectable response through ---

#[tokio::test]
async fn an_opted_out_route_passes_an_uninspectable_response_through() {
    let (upstream, _seen) = spawn_upstream(UpstreamReply {
        status: 200,
        content_type: "text/event-stream",
        extra: &[],
        body: b"data: hello\n\n",
    })
    .await;
    let proxy = spawn_proxy(control_with(
        filter_respbody_component(),
        upstream,
        "[route.response_body]\nuninspectable = \"passthrough\"\n",
    ))
    .await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, body) = get(&client, proxy, "/api/events", &[]).await;

    assert_eq!(parts.status, StatusCode::OK);
    assert_eq!(body.as_ref(), b"data: hello\n\n");
}

// --- guest output the host refuses ---

#[tokio::test]
async fn a_guest_supplied_content_length_fails_closed() {
    let (upstream, _seen) = spawn_upstream(UpstreamReply::json(b"{}")).await;
    let proxy = spawn_proxy(control_with(filter_respbody_component(), upstream, "")).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, _) = get(&client, proxy, "/api/thing?respbody=content-length", &[]).await;

    assert_eq!(parts.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        parts.headers.get("x-plecto-fault").map(|v| v.as_bytes()),
        Some(b"invalid-output".as_slice()),
        "framing is the host's; a guest that claims it fails the decision"
    );
}

#[tokio::test]
async fn a_guest_body_over_the_route_cap_fails_closed() {
    // The cap bounds what the host holds, so it has to bound what a filter hands BACK too —
    // otherwise the cap is a suggestion the guest can talk its way past.
    let (upstream, _seen) = spawn_upstream(UpstreamReply {
        status: 200,
        content_type: "text/plain",
        extra: &[],
        body: b"small",
    })
    .await;
    let proxy = spawn_proxy(control_with(
        filter_respbody_component(),
        upstream,
        "[route.response_body]\nmax_bytes = 1024\n",
    ))
    .await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (parts, _) = get(&client, proxy, "/api/thing?respbody=inflate", &[]).await;

    assert_eq!(parts.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        parts.headers.get("x-plecto-fault").map(|v| v.as_bytes()),
        Some(b"response-body-guest-oversize".as_slice())
    );
}

// --- the zero-copy control case: no body hook, no buffering, in EITHER direction ---

#[tokio::test]
async fn a_route_without_body_hooks_streams_both_directions_untouched() {
    // filter-apikey exports neither body hook, so none of the machinery above is armed: the very
    // responses an inspecting route refuses (a streaming media type) stream straight through, and
    // an arbitrarily large body is never held. This is the ADR 000038 guarantee, per direction.
    let (upstream, _seen) = spawn_upstream(UpstreamReply {
        status: 200,
        content_type: "text/event-stream",
        extra: &[],
        body: b"data: streamed\n\n",
    })
    .await;
    let proxy = spawn_proxy(control_with(
        filter_apikey_component(),
        upstream,
        // A cap so small that ANY buffering would trip it — proving nothing buffers.
        "[route.response_body]\nmax_bytes = 1\n",
    ))
    .await;
    let client = client();
    for _ in 0..150 {
        let (parts, _) = get(
            &client,
            proxy,
            "/api/__ready",
            &[("x-api-key", "alice-secret")],
        )
        .await;
        if parts.status != StatusCode::SERVICE_UNAVAILABLE {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let (parts, body) = get(
        &client,
        proxy,
        "/api/events",
        &[("x-api-key", "alice-secret")],
    )
    .await;

    assert_eq!(
        parts.status,
        StatusCode::OK,
        "a route with no response-body filter never classifies, never buffers, never refuses"
    );
    assert_eq!(body.as_ref(), b"data: streamed\n\n");
}
