//! Plecto benchmark harness — **host-enforced rate limiting (ADR 000026) and the request-side body
//! hook (ADR 000025)**, measured honestly. Sibling of `wasm-bench` (which isolates the WASM
//! pooling cost); this one isolates the cost and behaviour of two host-API decision paths.
//!
//! Run it:  `cargo run --release -p plecto-server --example edge-bench`
//!
//! One signed `filter-hello` (trusted, pooled) on three routes that forward to the **same** upstream
//! (which sleeps `BACKEND_LATENCY_MS`, default 0, and returns a `RESP_BYTES`-sized body):
//!
//! - `/baseline/*` — no filter (native fast path only): the control.
//! - `/ratelimit/*` — `filter-hello` consults the host-native token bucket. The request's
//!   `x-plecto-ratelimit: <key>` header selects the bucket KEY, so a generator can spread load
//!   across keys (per-key fairness) or hammer one (enforcement).
//! - `/body/*` — `filter-hello`'s `on-request-body` buffers + uppercases the POST body (or
//!   short-circuits 403 on a `deny-body` marker), so the buffer-then-decide cost shows against
//!   `/baseline`.
//!
//! The bucket SPEC is host-configured here (ADR 000026): `RL_CAPACITY` / `RL_REFILL_TOKENS` /
//! `RL_REFILL_INTERVAL_MS` set it via the manifest — the filter cannot widen its own limit. Plain
//! HTTP/1.1; temp dir cleaned up on exit.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use plecto_control::oci::write_layout;
use plecto_control::{Control, ResolvedArtifact};
use plecto_host::test_support::{TestSigner, bound_sbom, filter_hello_component};
use plecto_server::serve;

const PROXY_ADDR: &str = "127.0.0.1:8086";

/// Read a `u64` env var, defaulting when unset / unparseable.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    let signer = TestSigner::new()?;
    std::fs::write(base.join("trust.pem"), signer.public_key_pem())?;
    let component = filter_hello_component();
    let component_signature = signer.sign(&component)?;
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom)?;
    let artifact = ResolvedArtifact {
        component,
        component_signature,
        sbom,
        sbom_signature,
    };
    let digest = write_layout(&base.join("filters/hello"), &artifact)?;

    // The bucket spec is operator-owned (manifest, ADR 000026). Defaults: a generous bucket that
    // never denies (the overhead run wants to measure the limiter's hot-path cost, not rejections);
    // the enforcement / fairness runs pass a tight bucket via these env vars.
    let capacity = env_u64("RL_CAPACITY", 1_000_000_000);
    let refill_tokens = env_u64("RL_REFILL_TOKENS", 1_000_000_000);
    let refill_interval_ms = env_u64("RL_REFILL_INTERVAL_MS", 1000);
    let resp_bytes = env_u64("RESP_BYTES", 16) as usize;
    let latency_ms = env_u64("BACKEND_LATENCY_MS", 0);

    let upstream = spawn_upstream(latency_ms, resp_bytes).await?;
    let manifest_path = base.join("plecto.toml");
    std::fs::write(
        &manifest_path,
        manifest_toml(
            &digest,
            upstream,
            capacity,
            refill_tokens,
            refill_interval_ms,
        ),
    )?;

    let control = Arc::new(Control::from_manifest_path(&manifest_path)?);

    let proxy_addr = std::env::var("PLECTO_PROXY_ADDR").unwrap_or_else(|_| PROXY_ADDR.to_string());
    let listener = TcpListener::bind(&proxy_addr).await?;
    let proxy = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(e) = serve(control, listener).await {
            eprintln!("proxy stopped: {e}");
        }
    });

    print_banner(
        proxy,
        latency_ms,
        resp_bytes,
        capacity,
        refill_tokens,
        refill_interval_ms,
    );
    tokio::signal::ctrl_c().await?;
    println!("\nshutting down — temp dir cleaned up.");
    Ok(())
}

/// The shared upstream: drains the request body (so POST bodies and keep-alive work), sleeps
/// `latency_ms`, then returns a `resp_bytes`-sized body (a payload-size knob for the body sweep).
async fn spawn_upstream(latency_ms: u64, resp_bytes: usize) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let body = Bytes::from(vec![b'x'; resp_bytes]);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let body = body.clone();
                    async move {
                        // Drain the request body so the connection is reusable (and to model an
                        // upstream that actually consumes what the body hook forwarded).
                        let _ = req.into_body().collect().await;
                        if latency_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(latency_ms)).await;
                        }
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("x-from", "backend")
                                .body(Full::new(body))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    Ok(addr)
}

fn manifest_toml(
    digest: &str,
    upstream: SocketAddr,
    capacity: u64,
    refill_tokens: u64,
    refill_interval_ms: u64,
) -> String {
    // request_deadline_ms is generous so a body-hook buffer never trips a deadline (this is a cost
    // measurement, not an SLA test). The ratelimit bucket is host-side (ADR 000026); only /ratelimit
    // requests (which carry x-plecto-ratelimit) actually consult it.
    format!(
        r#"# Plecto edge benchmark manifest (generated) — rate-limit + body hook on one filter.
[trust]
keys = ["trust.pem"]

[[filter]]
id = "hello"
source = "filters/hello"
digest = "{digest}"
isolation = "trusted"
request_deadline_ms = 1000
ratelimit = {{ capacity = {capacity}, refill_tokens = {refill_tokens}, refill_interval_ms = {refill_interval_ms} }}

[[upstream]]
name = "backend"
addresses = ["{upstream}"]
[upstream.health]
path = "/"

[[route]]
path_prefix = "/baseline"
filters = []
upstream = "backend"
strip_prefix = "/baseline"

[[route]]
path_prefix = "/ratelimit"
filters = ["hello"]
upstream = "backend"
strip_prefix = "/ratelimit"

[[route]]
path_prefix = "/body"
filters = ["hello"]
upstream = "backend"
strip_prefix = "/body"
"#
    )
}

fn print_banner(
    proxy: SocketAddr,
    latency_ms: u64,
    resp_bytes: usize,
    capacity: u64,
    refill_tokens: u64,
    refill_interval_ms: u64,
) {
    let p = proxy.port();
    println!("\n  Plecto edge benchmark — host rate-limit (ADR 000026) + body hook (ADR 000025)\n");
    println!("  proxy  : http://localhost:{p}   (plain HTTP/1.1)");
    println!("  backend: in-process, sleeps {latency_ms} ms, returns {resp_bytes}-byte body");
    println!(
        "  bucket : capacity={capacity} refill={refill_tokens}/{refill_interval_ms}ms  (host-set, per key)\n"
    );
    println!("  routes (same backend, only the decision path differs):");
    println!("    /baseline/*   no filter            curl http://localhost:{p}/baseline/x");
    println!(
        "    /ratelimit/*  host token bucket     curl -H 'x-plecto-ratelimit: alice' http://localhost:{p}/ratelimit/x"
    );
    println!(
        "    /body/*       on-request-body       curl -X POST --data hello http://localhost:{p}/body/x   (-> HELLO)"
    );
    println!(
        "    (over-limit -> 429 retry-after-ms;  body with 'deny-body' -> 403 short-circuit)\n"
    );
}
