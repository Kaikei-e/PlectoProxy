//! Plecto benchmark harness — **the cost of the WASM extension plane**, measured honestly.
//!
//! Run it:  `cargo run --release -p plecto-server --example wasm-bench`
//!
//! Same as `wasm-auth`, but built for an A/B/C comparison on **one server, one backend**, so the
//! only variable is *how* the per-request decision runs. Three routes forward to the **same**
//! upstream (which sleeps `BACKEND_LATENCY_MS`, default 0, to model a real service):
//!
//!   * `/baseline/*`  — no filter at all (native fast path only)            → the control
//!   * `/trusted/*`   — the signed `filter-apikey` component, **pooled** (init-once, reused)
//!   * `/ondemand/*`  — the same component, **untrusted** (fresh instance per request)
//!
//! The API-key filter (`crates/filter-apikey`) reads `x-api-key`; a valid key
//! (`alice-secret`/`bob-secret`) is stamped + forwarded (200), anything else is short-circuited
//! 401 without touching the upstream. Driving these routes with a load generator yields the
//! filter's per-request overhead, the value of instance pooling, and the cheapness of the
//! short-circuit reject path. Plain HTTP/1.1; temp dir cleaned up on exit.

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
use tokio::net::TcpListener;

use plecto_control::oci::write_layout;
use plecto_control::{Control, ResolvedArtifact};
use plecto_host::test_support::{TestSigner, bound_sbom, filter_apikey_component};
use plecto_server::serve;

const PROXY_ADDR: &str = "127.0.0.1:8085";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    // Sign the real filter once and bundle it as two offline OCI layouts (same bytes, same digest)
    // so it can be loaded under two ids with different isolation — pooled vs fresh-per-request.
    let signer = TestSigner::new()?;
    std::fs::write(base.join("trust.pem"), signer.public_key_pem())?;
    let component = filter_apikey_component();
    let component_signature = signer.sign(&component)?;
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom)?;
    let artifact = ResolvedArtifact {
        component,
        component_signature,
        sbom,
        sbom_signature,
    };
    let digest = write_layout(&base.join("filters/apikey"), &artifact)?;
    write_layout(&base.join("filters/apikey_od"), &artifact)?;

    let latency_ms: u64 = std::env::var("BACKEND_LATENCY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let upstream = spawn_upstream(latency_ms).await?;
    let manifest_path = base.join("plecto.toml");
    std::fs::write(&manifest_path, manifest_toml(&digest, upstream))?;

    let control = Arc::new(Control::from_manifest_path(&manifest_path)?);

    let listener = TcpListener::bind(PROXY_ADDR).await?;
    let proxy = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(e) = serve(control, listener).await {
            eprintln!("proxy stopped: {e}");
        }
    });

    print_banner(proxy, latency_ms);
    tokio::signal::ctrl_c().await?;
    println!("\nshutting down — temp dir cleaned up.");
    Ok(())
}

/// The shared upstream: sleeps `latency_ms` to model a real service, then returns a small JSON
/// body echoing whatever identity the filter stamped (so a successful auth is observable).
async fn spawn_upstream(latency_ms: u64) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| async move {
                    if latency_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(latency_ms)).await;
                    }
                    let user = req
                        .headers()
                        .get("x-authenticated-user")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("anonymous")
                        .to_string();
                    let body = format!("{{\"upstream\":\"backend\",\"user\":\"{user}\"}}\n");
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .header("x-from", "backend")
                            .body(Full::new(Bytes::from(body)))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    Ok(addr)
}

fn manifest_toml(digest: &str, upstream: SocketAddr) -> String {
    // request_deadline_ms is set generously so neither isolation trips a deadline (504) — this is a
    // cost measurement, not an SLA test. The default 100ms could false-fail a cold on-demand init.
    format!(
        r#"# Plecto benchmark manifest (generated) — baseline vs pooled vs on-demand WASM filter.
[trust]
keys = ["trust.pem"]

[[filter]]
id = "apikey"
source = "filters/apikey"
digest = "{digest}"
isolation = "trusted"
request_deadline_ms = 1000

[[filter]]
id = "apikey_od"
source = "filters/apikey_od"
digest = "{digest}"
isolation = "untrusted"
request_deadline_ms = 1000

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
path_prefix = "/trusted"
filters = ["apikey"]
upstream = "backend"
strip_prefix = "/trusted"

[[route]]
path_prefix = "/ondemand"
filters = ["apikey_od"]
upstream = "backend"
strip_prefix = "/ondemand"
"#
    )
}

fn print_banner(proxy: SocketAddr, latency_ms: u64) {
    let p = proxy.port();
    println!("\n  Plecto benchmark — cost of the WASM extension plane\n");
    println!("  proxy  : http://localhost:{p}   (plain HTTP/1.1)");
    println!("  backend: in-process, sleeps {latency_ms} ms  (set BACKEND_LATENCY_MS to change)");
    println!("  keys   : alice-secret -> alice,  bob-secret -> bob\n");
    println!("  routes (same backend, only the decision path differs):");
    println!("    /baseline/*   no filter            curl http://localhost:{p}/baseline/x");
    println!(
        "    /trusted/*    apikey, pooled        curl -H 'x-api-key: alice-secret' http://localhost:{p}/trusted/x"
    );
    println!(
        "    /ondemand/*   apikey, fresh/req     curl -H 'x-api-key: alice-secret' http://localhost:{p}/ondemand/x"
    );
    println!("    (no/invalid key on a filtered route -> 401 short-circuit, upstream untouched)\n");
}
