//! Plecto benchmark harness — **the cost of the WASM extension plane**, measured honestly.
//!
//! Run it:  `cargo run --release -p plecto-server --example wasm-bench`
//!
//! Built as a **cost ladder** on **one server, one backend**, so the only variable is *how* the
//! per-request decision runs. Five routes forward to the **same** upstream (which sleeps
//! `BACKEND_LATENCY_MS`, default 0, to model a real service); each adjacent delta isolates one cost:
//!
//!   * `/baseline/*`     — no filter at all (native fast path only)          → the control
//!   * `/noop-pooled/*`  — a **pure no-op** WASM filter, **pooled**          → dispatch floor
//!   * `/noop-fresh/*`   — the same no-op, **fresh instance per request**    → + instantiation
//!   * `/trusted/*`      — the signed `filter-apikey` component, **pooled**  → + a real filter's work
//!   * `/ondemand/*`     — the apikey filter, **fresh instance per request**
//!
//! The API-key filter (`examples/filters/filter-apikey`) reads `x-api-key`; a valid key
//! (`alice-secret`/`bob-secret`) is stamped + forwarded (200), anything else is short-circuited
//! 401 without touching the upstream. `filter-noop` makes no host-API calls, so baseline→noop
//! isolates the irreducible WASM dispatch tax and noop→trusted the apikey's own work. Driving these
//! routes with a load generator yields the ladder. Plain HTTP/1.1; temp dir cleaned up on exit.

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
use plecto_host::test_support::{
    TestSigner, bound_sbom, filter_apikey_component, filter_noop_component,
};
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

    // The "pure WASM no-op" rung: a filter that makes no host-API calls, signed and bundled the same
    // way, under two ids so it runs pooled (trusted) and fresh-per-request (untrusted). The delta
    // baseline→noop isolates the irreducible dispatch tax; noop→trusted adds the apikey's real work.
    let noop = filter_noop_component();
    let noop_signature = signer.sign(&noop)?;
    let noop_sbom = bound_sbom(&noop);
    let noop_sbom_signature = signer.sign(&noop_sbom)?;
    let noop_artifact = ResolvedArtifact {
        component: noop,
        component_signature: noop_signature,
        sbom: noop_sbom,
        sbom_signature: noop_sbom_signature,
    };
    let noop_digest = write_layout(&base.join("filters/noop"), &noop_artifact)?;
    write_layout(&base.join("filters/noop_od"), &noop_artifact)?;

    let latency_ms: u64 = std::env::var("BACKEND_LATENCY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let upstream = spawn_upstream(latency_ms).await?;
    let manifest_path = base.join("plecto.toml");
    std::fs::write(
        &manifest_path,
        manifest_toml(&digest, &noop_digest, upstream),
    )?;

    let control = Arc::new(Control::from_manifest_path(&manifest_path)?);

    // Bind addr is overridable (PLECTO_PROXY_ADDR) so a benchmark host whose default port is taken
    // by another service can move it without touching the demo's behaviour.
    let proxy_addr = std::env::var("PLECTO_PROXY_ADDR").unwrap_or_else(|_| PROXY_ADDR.to_string());
    let listener = TcpListener::bind(&proxy_addr).await?;
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

fn manifest_toml(digest: &str, noop_digest: &str, upstream: SocketAddr) -> String {
    // request_deadline_ms is set generously so neither isolation trips a deadline (504) — this is a
    // cost measurement, not an SLA test. The default 100ms could false-fail a cold on-demand init.
    //
    // The routes form a cost ladder over the SAME backend, so adjacent deltas isolate one cost each:
    //   /baseline (native)        -> the fast-path floor (no filter)
    //   /noop-pooled (wasm no-op)  -> + dispatch + instance acquisition + one empty crossing
    //   /noop-fresh  (wasm no-op)  -> + per-request instantiation (vs pooled) = pooling ROI
    //   /trusted (apikey, pooled)  -> + a real filter's work (header parse + host-KV + counter)
    //   /ondemand (apikey, fresh)  -> the real filter, fresh-per-request
    format!(
        r#"# Plecto benchmark manifest (generated) — the WASM cost ladder (baseline -> no-op -> real).
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

[[filter]]
id = "noop"
source = "filters/noop"
digest = "{noop_digest}"
isolation = "trusted"
request_deadline_ms = 1000

[[filter]]
id = "noop_od"
source = "filters/noop_od"
digest = "{noop_digest}"
isolation = "untrusted"
request_deadline_ms = 1000

[[upstream]]
name = "backend"
addresses = ["{upstream}"]
[upstream.health]
path = "/"

[[route]]
filters = []
upstream = "backend"
strip_prefix = "/baseline"
[route.match]
path_prefix = "/baseline"

[[route]]
filters = ["noop"]
upstream = "backend"
strip_prefix = "/noop-pooled"
[route.match]
path_prefix = "/noop-pooled"

[[route]]
filters = ["noop_od"]
upstream = "backend"
strip_prefix = "/noop-fresh"
[route.match]
path_prefix = "/noop-fresh"

[[route]]
filters = ["apikey"]
upstream = "backend"
strip_prefix = "/trusted"
[route.match]
path_prefix = "/trusted"

[[route]]
filters = ["apikey_od"]
upstream = "backend"
strip_prefix = "/ondemand"
[route.match]
path_prefix = "/ondemand"
"#
    )
}

fn print_banner(proxy: SocketAddr, latency_ms: u64) {
    let p = proxy.port();
    println!("\n  Plecto benchmark — cost of the WASM extension plane\n");
    println!("  proxy  : http://localhost:{p}   (plain HTTP/1.1)");
    println!("  backend: in-process, sleeps {latency_ms} ms  (set BACKEND_LATENCY_MS to change)");
    println!("  keys   : alice-secret -> alice,  bob-secret -> bob\n");
    println!("  routes (same backend, only the decision path differs — a cost ladder):");
    println!("    /baseline/*     no filter          curl http://localhost:{p}/baseline/x");
    println!("    /noop-pooled/*  wasm no-op, pooled  curl http://localhost:{p}/noop-pooled/x");
    println!("    /noop-fresh/*   wasm no-op, fresh   curl http://localhost:{p}/noop-fresh/x");
    println!(
        "    /trusted/*      apikey, pooled      curl -H 'x-api-key: alice-secret' http://localhost:{p}/trusted/x"
    );
    println!(
        "    /ondemand/*   apikey, fresh/req     curl -H 'x-api-key: alice-secret' http://localhost:{p}/ondemand/x"
    );
    println!("    (no/invalid key on a filtered route -> 401 short-circuit, upstream untouched)\n");
}
