//! E2E (tdd-workflow Phase 0) for ADR 000040: the host-side OTLP/HTTP exporter. Drive real
//! traffic through a running `plecto-server` whose manifest sets `[observability] otlp_endpoint`,
//! and assert a fake OTLP collector receives protobuf-encoded trace batches: one SERVER request
//! span per transaction (locally-minted id, the inbound `traceparent` as its remote parent) and
//! one INTERNAL span per filter execution parented under it. The W3C sampled flag is honoured
//! (an unsampled inbound trace exports nothing), and graceful shutdown flushes the queue.
//!
//! The decode oracle is the official `opentelemetry-proto` generated types (dev-dependency) —
//! the shipped encoder is hand-written (dependency-less), so the test must not share its code.

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
use parking_lot::Mutex;
use prost::Message;
use tokio::net::TcpListener;

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value as AnyValue;
use opentelemetry_proto::tonic::trace::v1::Span;
use plecto_control::{Control, Host, Manifest, MemoryStore, ResolvedArtifact};
use plecto_host::test_support::{TestSigner, bound_sbom, filter_hello_component};
use plecto_server::{serve, serve_with_shutdown};

const TRACE_HEX: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const CALLER_SPAN_HEX: &str = "00f067aa0ba902b7";

/// Everything the fake collector captured: each POST's path, content-type, and decoded request.
#[derive(Default)]
struct Collected {
    posts: Vec<(String, String, ExportTraceServiceRequest)>,
}

impl Collected {
    /// Every span across every captured batch, flattened.
    fn spans(&self) -> Vec<Span> {
        self.posts
            .iter()
            .flat_map(|(_, _, req)| &req.resource_spans)
            .flat_map(|rs| &rs.scope_spans)
            .flat_map(|ss| &ss.spans)
            .cloned()
            .collect()
    }

    fn spans_for_trace(&self, trace_id: &[u8]) -> Vec<Span> {
        self.spans()
            .into_iter()
            .filter(|s| s.trace_id == trace_id)
            .collect()
    }
}

/// A fake OTLP collector: captures every POST body (protobuf-decoded) and returns 200 with an
/// empty `ExportTraceServiceResponse` (a valid empty message), `application/x-protobuf`.
async fn spawn_collector() -> (SocketAddr, Arc<Mutex<Collected>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let collected = Arc::new(Mutex::new(Collected::default()));
    let sink = collected.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let sink = sink.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let sink = sink.clone();
                    async move {
                        let path = req.uri().path().to_string();
                        let content_type = req
                            .headers()
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        let decoded = ExportTraceServiceRequest::decode(body.as_ref())
                            .expect("collector received valid OTLP protobuf");
                        sink.lock().posts.push((path, content_type, decoded));
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "application/x-protobuf")
                                .body(Full::new(Bytes::new()))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, collected)
}

/// A trivial upstream: 200 for any path, reflecting the inbound `traceparent` so a test can read
/// which span id the proxy propagated.
async fn echo(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let traceparent = req
        .headers()
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    Ok(Response::builder()
        .status(200)
        .header("x-upstream-traceparent", traceparent)
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

/// Build a control plane: filter-hello signed + loaded, a route `/api` → that chain → the given
/// upstream, and `[observability] otlp_endpoint` pointing at the fake collector (base URL — the
/// exporter appends `/v1/traces`, mirroring `OTEL_EXPORTER_OTLP_ENDPOINT` semantics).
fn control_for(upstream_addr: SocketAddr, collector_addr: SocketAddr) -> Arc<Control> {
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
[observability]
otlp_endpoint = "http://{collector_addr}"

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
[route.match]
path_prefix = "/api"
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    Arc::new(Control::load(host, &manifest, Box::new(store)).unwrap())
}

async fn spawn_proxy(control: Arc<Control>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(control, listener).await;
    });
    addr
}

fn client() -> Client<HttpConnector, Empty<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

async fn get(
    client: &Client<HttpConnector, Empty<Bytes>>,
    proxy: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, hyper::HeaderMap) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(format!("http://{proxy}{path}"));
    for (n, v) in headers {
        builder = builder.header(*n, *v);
    }
    let resp = client
        .request(builder.body(Empty::<Bytes>::new()).unwrap())
        .await
        .expect("proxy request");
    let (parts, body) = resp.into_parts();
    let _ = body.collect().await.unwrap();
    (parts.status, parts.headers)
}

/// Wait until the upstream's first health probe lands (instances start pessimistic).
async fn wait_ready(client: &Client<HttpConnector, Empty<Bytes>>, proxy: SocketAddr) {
    for _ in 0..150 {
        let (status, _) = get(client, proxy, "/api/__ready", &[]).await;
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream never became healthy within the readiness window");
}

/// Poll the collector until `pred` holds on the captured spans (the pump exports on a short tick).
async fn wait_for_spans(
    collected: &Arc<Mutex<Collected>>,
    pred: impl Fn(&Collected) -> bool,
) -> Collected {
    for _ in 0..200 {
        {
            let got = collected.lock();
            if pred(&got) {
                return Collected {
                    posts: got.posts.clone(),
                };
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let got = collected.lock();
    panic!(
        "collector never satisfied the predicate; captured {} posts, {} spans",
        got.posts.len(),
        got.spans().len()
    );
}

fn str_attr(span: &Span, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.as_ref())
        .and_then(|v| match v {
            AnyValue::StringValue(s) => Some(s.clone()),
            _ => None,
        })
}

fn int_attr(span: &Span, key: &str) -> Option<i64> {
    span.attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.as_ref())
        .and_then(|v| match v {
            AnyValue::IntValue(i) => Some(*i),
            _ => None,
        })
}

/// SpanKind proto enum values (trace.proto): INTERNAL = 1, SERVER = 2.
const KIND_INTERNAL: i32 = 1;
const KIND_SERVER: i32 = 2;
/// Span.flags (trace.proto): low byte = W3C trace flags; bit 8 = "is_remote is known";
/// bit 9 = "parent is remote".
const FLAG_SAMPLED: u32 = 0x01;
const FLAG_HAS_IS_REMOTE: u32 = 0x100;
const FLAG_IS_REMOTE: u32 = 0x200;

#[tokio::test]
async fn continued_trace_exports_server_span_and_filter_span_under_it() {
    let (collector_addr, collected) = spawn_collector().await;
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream, collector_addr)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let traceparent = format!("00-{TRACE_HEX}-{CALLER_SPAN_HEX}-01");
    let (status, headers) = get(
        &client,
        proxy,
        "/api/hello",
        &[("traceparent", &traceparent)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let propagated = headers
        .get("x-upstream-traceparent")
        .and_then(|v| v.to_str().ok())
        .expect("the upstream saw a traceparent")
        .to_string();

    let trace_id = hex::decode(TRACE_HEX).unwrap();
    let caller_span = hex::decode(CALLER_SPAN_HEX).unwrap();
    let got = wait_for_spans(&collected, |c| c.spans_for_trace(&trace_id).len() >= 2).await;

    // Transport shape: POSTs land on /v1/traces as binary protobuf.
    let (path, content_type, _) = &got.posts[0];
    assert_eq!(path, "/v1/traces");
    assert_eq!(content_type, "application/x-protobuf");

    // Resource identity: service.name is present on every batch.
    let service_name = got.posts[0]
        .2
        .resource_spans
        .first()
        .and_then(|rs| rs.resource.as_ref())
        .and_then(|r| {
            r.attributes
                .iter()
                .find(|kv| kv.key == "service.name")
                .and_then(|kv| kv.value.as_ref())
                .and_then(|v| v.value.as_ref())
                .and_then(|v| match v {
                    AnyValue::StringValue(s) => Some(s.clone()),
                    _ => None,
                })
        })
        .expect("resource carries service.name");
    assert_eq!(service_name, "plecto");

    let spans = got.spans_for_trace(&trace_id);

    // The SERVER request span: locally-minted id, the caller's span as its REMOTE parent.
    let server = spans
        .iter()
        .find(|s| s.kind == KIND_SERVER)
        .expect("a SERVER request span was exported");
    assert_eq!(server.parent_span_id, caller_span);
    assert_ne!(
        server.span_id, caller_span,
        "the request span id is locally minted, never the caller's"
    );
    assert_eq!(
        server.flags & (FLAG_SAMPLED | FLAG_HAS_IS_REMOTE | FLAG_IS_REMOTE),
        FLAG_SAMPLED | FLAG_HAS_IS_REMOTE | FLAG_IS_REMOTE,
        "a continued trace's request span has a remote, sampled parent context"
    );
    assert!(server.end_time_unix_nano >= server.start_time_unix_nano);
    assert_eq!(
        str_attr(server, "http.request.method").as_deref(),
        Some("GET")
    );
    assert_eq!(str_attr(server, "url.path").as_deref(), Some("/api/hello"));
    assert_eq!(int_attr(server, "http.response.status_code"), Some(200));

    // The filter span: INTERNAL, named by filter id, parented under the request span.
    let filter = spans
        .iter()
        .find(|s| s.kind == KIND_INTERNAL && s.name == "fh")
        .expect("the filter execution span was exported");
    assert_eq!(
        filter.parent_span_id, server.span_id,
        "the filter span nests under the request span"
    );
    assert_eq!(
        str_attr(filter, "plecto.hook").as_deref(),
        Some("on-request")
    );

    // Upstream propagation: the traceparent the upstream saw carries the request span's id, so
    // the upstream's own spans nest under Plecto's SERVER span.
    let expected = format!("00-{TRACE_HEX}-{}-01", hex_lower(&server.span_id));
    assert_eq!(propagated, expected);
}

#[tokio::test]
async fn unsampled_inbound_trace_exports_nothing() {
    let (collector_addr, collected) = spawn_collector().await;
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream, collector_addr)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let traceparent = format!("00-{TRACE_HEX}-{CALLER_SPAN_HEX}-00");
    let (status, _) = get(
        &client,
        proxy,
        "/api/quiet",
        &[("traceparent", &traceparent)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Give the pump several ticks; the unsampled trace must never appear (readiness-probe root
    // traces from `wait_ready` MAY appear — they are sampled — so filter by trace id).
    tokio::time::sleep(Duration::from_millis(900)).await;
    let trace_id = hex::decode(TRACE_HEX).unwrap();
    assert!(
        collected.lock().spans_for_trace(&trace_id).is_empty(),
        "an unsampled (flags 00) inbound trace exports no spans"
    );
}

#[tokio::test]
async fn locally_rooted_trace_exports_parentless_sampled_server_span() {
    let (collector_addr, collected) = spawn_collector().await;
    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(control_for(upstream, collector_addr)).await;
    let client = client();
    wait_ready(&client, proxy).await;

    let (status, _) = get(&client, proxy, "/api/rootcase", &[]).await;
    assert_eq!(status, StatusCode::OK);

    let got = wait_for_spans(&collected, |c| {
        c.spans()
            .iter()
            .any(|s| str_attr(s, "url.path").as_deref() == Some("/api/rootcase"))
    })
    .await;
    let spans = got.spans();
    let server = spans
        .iter()
        .find(|s| str_attr(s, "url.path").as_deref() == Some("/api/rootcase"))
        .expect("the root-case request span was exported");
    assert_eq!(server.kind, KIND_SERVER);
    assert!(
        server.parent_span_id.is_empty(),
        "a locally-rooted request span has no parent"
    );
    assert_ne!(server.trace_id, vec![0u8; 16]);
    assert_eq!(
        server.flags & (FLAG_SAMPLED | FLAG_HAS_IS_REMOTE | FLAG_IS_REMOTE),
        FLAG_SAMPLED | FLAG_HAS_IS_REMOTE,
        "a local root is sampled with a known-local (not remote) parent context"
    );
}

#[tokio::test]
async fn graceful_shutdown_flushes_pending_spans() {
    let (collector_addr, collected) = spawn_collector().await;
    let upstream = spawn_upstream().await;
    let control = control_for(upstream, collector_addr);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_with_shutdown(control, listener, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let client = client();
    wait_ready(&client, proxy).await;
    let traceparent = format!("00-{TRACE_HEX}-{CALLER_SPAN_HEX}-01");
    let (status, _) = get(
        &client,
        proxy,
        "/api/flush",
        &[("traceparent", &traceparent)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Shut down immediately: the exporter must flush what is queued before serve returns.
    let _ = shutdown_tx.send(());
    server
        .await
        .expect("server task joins")
        .expect("serve returns Ok");

    let trace_id = hex::decode(TRACE_HEX).unwrap();
    assert!(
        !collected.lock().spans_for_trace(&trace_id).is_empty(),
        "shutdown flushed the queued spans for the driven request"
    );
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
