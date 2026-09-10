//! Cancellation regressions for work that has already crossed into synchronous WASM.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use plecto_control::{Control, Host, Manifest, MemoryStore, ResolvedArtifact};
use plecto_host::test_support::{TestSigner, bound_sbom, filter_v04_component};
use plecto_host::{Acquire, Bucket, KvBackend, KvBackendInventoryError, MemoryBackend};
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore, watch};

use super::*;

/// A real storage backend with a deterministic observation point inside the guest's host-counter
/// call. The notification happens before the fixture starts spinning, so aborting the caller here
/// proves permit ownership was transferred into the blocking closure rather than merely surviving
/// a scheduling coincidence.
struct NotifyingBackend {
    inner: MemoryBackend,
    body_started: Arc<Notify>,
}

impl NotifyingBackend {
    fn new(body_started: Arc<Notify>) -> Self {
        Self {
            inner: MemoryBackend::default(),
            body_started,
        }
    }
}

impl KvBackend for NotifyingBackend {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.inner.get(key)
    }

    fn set(&self, key: &[u8], value: Vec<u8>) {
        self.inner.set(key, value);
    }

    fn delete(&self, key: &[u8]) {
        self.inner.delete(key);
    }

    fn increment(&self, key: &[u8], delta: i64) -> i64 {
        if delta != 0 && key.ends_with(b"\x1fc\x1fbody-start") {
            self.body_started.notify_one();
        }
        self.inner.increment(key, delta)
    }

    fn try_acquire(&self, key: &[u8], cost: u64, spec: Bucket, now_ms: u64) -> Acquire {
        self.inner.try_acquire(key, cost, spec, now_ms)
    }

    fn visit_entries(
        &self,
        visit: &mut dyn FnMut(&[u8], usize) -> ControlFlow<()>,
    ) -> Result<(), KvBackendInventoryError> {
        self.inner.visit_entries(visit)
    }
}

async fn spin_upstream() -> SocketAddr {
    spin_upstream_with_content_type("text/plain").await
}

async fn spin_upstream_with_content_type(content_type: &'static str) -> SocketAddr {
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
                        service_fn(|req: Request<Incoming>| async move {
                            let body = if req.uri().path() == "/healthz" {
                                b"healthy".as_slice()
                            } else {
                                b"response body".as_slice()
                            };
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("content-type", content_type)
                                    .body(Full::new(Bytes::copy_from_slice(body)))
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

fn control_with_spin_filter(backend: Arc<NotifyingBackend>, upstream: SocketAddr) -> Arc<Control> {
    let signer = TestSigner::new().unwrap();
    let component = filter_v04_component();
    let component_signature = signer.sign(&component).unwrap();
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let mut store = MemoryStore::new();
    let digest = store.insert(
        "spin",
        ResolvedArtifact {
            component,
            component_signature,
            sbom,
            sbom_signature,
        },
    );
    let manifest = Manifest::from_toml(&format!(
        r#"
[[filter]]
id = "spin"
source = "spin"
digest = "{digest}"
isolation = "untrusted"
request_deadline_ms = 1000

[[upstream]]
name = "backend"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 20

[[route]]
filters = ["spin"]
upstream = "backend"
[route.match]
path_prefix = "/api"
[route.response_body]
max_bytes = 1024
"#
    ))
    .unwrap();
    let host = Host::with_backend(signer.trust_policy().unwrap(), backend).unwrap();
    Arc::new(Control::load(host, &manifest, Box::new(store)).unwrap())
}

/// The real 0.4 fixture exports `on-response-body`.  Pair it with each native codec so this test
/// exercises the same inspected-response → compression ordering as the fast path, rather than a
/// hand-rolled body wrapper.
fn control_with_compressed_response_filter(
    upstream: SocketAddr,
    algorithm: Option<&str>,
    response_body_cap: usize,
    over_cap: Option<&str>,
    response_content_type: &str,
) -> Arc<Control> {
    let signer = TestSigner::new().unwrap();
    let component = filter_v04_component();
    let component_signature = signer.sign(&component).unwrap();
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let mut store = MemoryStore::new();
    let digest = store.insert(
        "compressed-response",
        ResolvedArtifact {
            component,
            component_signature,
            sbom,
            sbom_signature,
        },
    );
    let over_cap = over_cap
        .map(|mode| format!("over_cap = \"{mode}\""))
        .unwrap_or_default();
    let compression = algorithm
        .map(|algorithm| {
            format!("[route.compression]\nalgorithms = [\"{algorithm}\"]\nmin_length = 1")
        })
        .unwrap_or_default();
    let manifest = Manifest::from_toml(&format!(
        r#"
[[filter]]
id = "compressed-response"
source = "compressed-response"
digest = "{digest}"
isolation = "trusted"

[[upstream]]
name = "backend"
addresses = ["{upstream}"]
[upstream.health]
path = "/healthz"
interval_ms = 20

[[route]]
filters = ["compressed-response"]
upstream = "backend"
[route.match]
path_prefix = "/api"
[route.response_body]
max_bytes = {response_body_cap}
content_types = ["{response_content_type}"]
{over_cap}
{compression}
"#
    ))
    .unwrap();
    Arc::new(
        Control::load(
            Host::new(signer.trust_policy().unwrap()).unwrap(),
            &manifest,
            Box::new(store),
        )
        .unwrap(),
    )
}

fn test_state(control: Arc<Control>, body_budget: usize) -> Arc<ServerState> {
    let (_, drain) = watch::channel(false);
    let (_, ready) = watch::channel(true);
    tokio::spawn(crate::health::serve_health_checks(
        control.clone(),
        drain.clone(),
    ));
    Arc::new(ServerState {
        control,
        clients: crate::upstream_client::UpstreamClients::new(),
        alt_svc: None,
        trusted_proxy: None,
        conn_limit: Arc::new(Semaphore::new(crate::MAX_CONNECTIONS)),
        per_ip_conn_limit: Arc::new(crate::conn_limit::PerIpConnLimit::new(
            crate::MAX_CONNECTIONS_PER_IP,
        )),
        body_buffer_budget: Arc::new(Semaphore::new(body_budget)),
        request_limit: Arc::new(Semaphore::new(1)),
        metrics: Arc::new(crate::metrics::ServerMetrics::new()),
        otlp: None,
        drain,
        ready,
    })
}

fn parts(path: &str) -> hyper::http::request::Parts {
    parts_with_accept(path, None)
}

fn parts_with_accept(path: &str, accept_encoding: Option<&str>) -> hyper::http::request::Parts {
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("http://example.test{path}"))
        .header(hyper::header::HOST, "example.test")
        .body(())
        .unwrap()
        .into_parts();
    if let Some(value) = accept_encoding {
        request
            .0
            .headers
            .insert(hyper::header::ACCEPT_ENCODING, value.parse().unwrap());
    }
    request.0
}

async fn wait_for_healthy_route(state: Arc<ServerState>) {
    for _ in 0..100 {
        let response = proxy_core(
            state.clone(),
            "http",
            "127.0.0.1:1".parse().unwrap(),
            parts("/api/ready"),
            crate::body::empty_req(),
        )
        .await
        .unwrap();
        if response.status() != StatusCode::SERVICE_UNAVAILABLE {
            drop(response);
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("upstream never became healthy");
}

async fn assert_permits_return(state: &ServerState, body_budget: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state.request_limit.available_permits() == 1
                && state.body_buffer_budget.available_permits() == body_budget
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("epoch deadline must release caller-aborted body-hook permits");
}

async fn wait_for_guest_body_start(started: &Notify) {
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .expect("body hook never reached the test guest's counter notification");
}

#[tokio::test]
async fn aborting_request_body_hook_keeps_admission_and_body_budget_until_wasm_stops() {
    let started = Arc::new(Notify::new());
    let upstream = spin_upstream().await;
    let body_budget = MAX_REQUEST_BODY_BUFFER;
    let state = test_state(
        control_with_spin_filter(Arc::new(NotifyingBackend::new(started.clone())), upstream),
        body_budget,
    );
    wait_for_healthy_route(state.clone()).await;

    let task = tokio::spawn(proxy_core(
        state.clone(),
        "http",
        "127.0.0.1:1".parse().unwrap(),
        parts("/api/request-spin"),
        crate::body::req_full(Bytes::from_static(b"admission-request-spin")),
    ));
    wait_for_guest_body_start(&started).await;
    task.abort();
    let _ = task.await;

    assert_eq!(state.request_limit.available_permits(), 0);
    assert_eq!(state.body_buffer_budget.available_permits(), 0);
    assert_permits_return(&state, body_budget).await;
}

#[tokio::test]
async fn aborting_response_body_hook_keeps_admission_and_body_budget_until_wasm_stops() {
    let started = Arc::new(Notify::new());
    let upstream = spin_upstream().await;
    let body_budget = 1024;
    let state = test_state(
        control_with_spin_filter(Arc::new(NotifyingBackend::new(started.clone())), upstream),
        body_budget,
    );
    wait_for_healthy_route(state.clone()).await;

    let task = tokio::spawn(proxy_core(
        state.clone(),
        "http",
        "127.0.0.1:1".parse().unwrap(),
        parts("/api/admission-response-spin"),
        crate::body::empty_req(),
    ));
    wait_for_guest_body_start(&started).await;
    task.abort();
    let _ = task.await;

    assert_eq!(state.request_limit.available_permits(), 0);
    assert_eq!(state.body_buffer_budget.available_permits(), 0);
    assert_permits_return(&state, body_budget).await;
}

#[tokio::test]
async fn compressed_inspected_response_keeps_both_permits_through_the_last_data_clone() {
    // ADR-115 decision 7: the inspection reservation, like request admission, belongs to the
    // DATA allocation that escapes into the transport.  Test every native codec because each
    // allocates a fresh output `Bytes` rather than forwarding the inspected input allocation.
    struct Case {
        label: &'static str,
        algorithm: Option<&'static str>,
        accept_encoding: Option<&'static str>,
        response_body_cap: usize,
        over_cap: Option<&'static str>,
        response_content_type: &'static str,
        compression_expected: bool,
    }
    let cases = [
        Case {
            label: "full-gzip",
            algorithm: Some("gzip"),
            accept_encoding: Some("gzip"),
            response_body_cap: 1024,
            over_cap: None,
            response_content_type: "text/plain",
            compression_expected: true,
        },
        Case {
            label: "full-br",
            algorithm: Some("br"),
            accept_encoding: Some("br"),
            response_body_cap: 1024,
            over_cap: None,
            response_content_type: "text/plain",
            compression_expected: true,
        },
        Case {
            label: "full-zstd",
            algorithm: Some("zstd"),
            accept_encoding: Some("zstd"),
            response_body_cap: 1024,
            over_cap: None,
            response_content_type: "text/plain",
            compression_expected: true,
        },
        Case {
            label: "over-cap-passthrough",
            algorithm: Some("gzip"),
            accept_encoding: Some("gzip"),
            response_body_cap: 4,
            over_cap: Some("passthrough"),
            response_content_type: "text/plain",
            compression_expected: true,
        },
        Case {
            label: "over-cap-process-partial",
            algorithm: Some("br"),
            accept_encoding: Some("br"),
            response_body_cap: 4,
            over_cap: Some("process-partial"),
            response_content_type: "text/plain",
            compression_expected: true,
        },
        Case {
            label: "identity",
            algorithm: None,
            accept_encoding: None,
            response_body_cap: 1024,
            over_cap: None,
            response_content_type: "text/plain",
            compression_expected: false,
        },
        // Response inspection opts into this type, native compression does not.
        Case {
            label: "compression-ineligible",
            algorithm: Some("gzip"),
            accept_encoding: Some("gzip"),
            response_body_cap: 1024,
            over_cap: None,
            response_content_type: "application/octet-stream",
            compression_expected: false,
        },
    ];
    for case in cases {
        let upstream = spin_upstream_with_content_type(case.response_content_type).await;
        // Make the shared pool exactly this route's reservation so a held permit is observable
        // as zero available permits in every arm, including the 4-byte over-cap cases.
        let body_budget = case.response_body_cap;
        let state = test_state(
            control_with_compressed_response_filter(
                upstream,
                case.algorithm,
                case.response_body_cap,
                case.over_cap,
                case.response_content_type,
            ),
            body_budget,
        );
        wait_for_healthy_route(state.clone()).await;

        let response = proxy_core(
            state.clone(),
            "http",
            "127.0.0.1:1".parse().unwrap(),
            parts_with_accept("/api/compressed-inspected-response", case.accept_encoding),
            crate::body::empty_req(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{}", case.label);
        assert_eq!(
            response
                .headers()
                .contains_key(hyper::header::CONTENT_ENCODING),
            case.compression_expected,
            "{} must take its declared compression arm",
            case.label
        );

        let (_, mut body) = response.into_parts();
        let mut retained = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.unwrap();
            if let Ok(data) = frame.into_data() {
                retained.push(data);
            }
        }
        drop(body);
        assert!(
            !retained.is_empty(),
            "{} must produce transport DATA",
            case.label
        );
        assert_eq!(
            state.body_buffer_budget.available_permits(),
            0,
            "{}",
            case.label
        );
        assert_eq!(state.request_limit.available_permits(), 0, "{}", case.label);

        // `Bytes` clone and slice both retain the from_owner allocation.  Drop every original
        // frame and one clone first: the final slice is the proof the counters cannot return
        // merely because the response body reached EOF or was dropped.
        let first = retained.remove(0);
        let clone = first.clone();
        let slice = first.slice(1..);
        drop(first);
        drop(retained);
        drop(clone);
        assert_eq!(
            state.body_buffer_budget.available_permits(),
            0,
            "{}",
            case.label
        );
        assert_eq!(state.request_limit.available_permits(), 0, "{}", case.label);
        drop(slice);
        assert_eq!(
            state.body_buffer_budget.available_permits(),
            body_budget,
            "{}",
            case.label
        );
        assert_eq!(state.request_limit.available_permits(), 1, "{}", case.label);
    }
}
