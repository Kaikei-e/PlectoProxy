//! Plecto demo — **a production-shaped deployment: the real `plecto` binary + a manifest on disk**
//! (ADR 000007 / 000009 / 000033 / 000035 / 000039).
//!
//! Run it:  `cargo run -p plecto-server --example production`
//!
//! Every other demo wires the proxy in-process so one file tells one story. This one shows the
//! shape you actually operate: a **deploy directory** (`target/production-demo/`) holding
//! `manifest.toml`, a trust root, and a **signed, digest-pinned OCI layout** of the auth filter —
//! served by the real `plecto` binary, started in a second terminal:
//!
//! ```text
//! cargo run -q -p plecto -- target/production-demo/manifest.toml 127.0.0.1:8086
//! ```
//!
//! This process plays the backend fleet (three `api` instances) and stays alive; the binary is the
//! gateway. The manifest is a realistic composition, not a single-feature demo:
//!
//!   * a signed WASM **auth filter** gates `/api` (verify-then-load, fail-closed);
//!   * a **native rate-limit floor** on the route (5 rps / burst 10, per client IP, ADR 000033) —
//!     consulted before the chain, no WASM involved;
//!   * `lb_algorithm = "least_request"` (P2C) over the three instances (ADR 000035);
//!   * a **circuit breaker** + **outlier detection** on the upstream (ADR 000028 / 000032);
//!   * `[observability]`: an **admin endpoint** (`/metrics`, `/healthz`, `/readyz`) on its own
//!     port + a structured **access log** (ADR 000009).
//!
//! The signing here uses the example signer for a self-contained run; production signs out of band
//! with `cosign sign-blob` and pins the digest — the layout and manifest are exactly what that
//! flow produces (see docs/writing-a-filter.md §5). The binary answers SIGHUP (edit the manifest,
//! reload with zero downtime) and SIGTERM (graceful drain) like any supervised process.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use plecto_control::ResolvedArtifact;
use plecto_control::oci::write_layout;
use plecto_host::test_support::{TestSigner, bound_sbom, filter_apikey_component};

const PROXY_ADDR: &str = "127.0.0.1:8086";
const ADMIN_ADDR: &str = "127.0.0.1:9099";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A persistent deploy dir (under target/, so `cargo clean` reclaims it) — the point is that
    // you can open and edit what the binary serves, unlike the other demos' temp dirs.
    let deploy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/production-demo");
    let _ = std::fs::remove_dir_all(&deploy);
    std::fs::create_dir_all(&deploy)?;

    // Sign the auth filter and bundle it as an offline, digest-pinned OCI layout — the same
    // artifact an out-of-band `cosign sign-blob` flow produces for a real deployment.
    let signer = TestSigner::new()?;
    std::fs::write(deploy.join("trust.pem"), signer.public_key_pem())?;
    let component = filter_apikey_component();
    let component_signature = signer.sign(&component)?;
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom)?;
    let digest = write_layout(
        &deploy.join("filters/apikey"),
        &ResolvedArtifact {
            component,
            component_signature,
            sbom,
            sbom_signature,
        },
    )?;

    // The backend fleet this process keeps serving while the binary (terminal 2) proxies to it.
    let i1 = spawn_instance("api-1").await?;
    let i2 = spawn_instance("api-2").await?;
    let i3 = spawn_instance("api-3").await?;

    // The [state] redb file's parent must exist before the binary starts (ADR 000041: directory
    // preparation is the operator's responsibility — a typo'd path errors instead of growing a
    // new tree). This dir is that ops step.
    std::fs::create_dir_all(deploy.join("state"))?;

    // Overridable (PLECTO_PROXY_ADDR / PLECTO_ADMIN_ADDR) so a host whose default ports are
    // taken can move them. PROXY_ADDR is only ever printed (STEP 2 starts a separate binary
    // with it as a CLI arg); ADMIN_ADDR is also baked into the manifest's [observability].
    let proxy_addr = std::env::var("PLECTO_PROXY_ADDR").unwrap_or_else(|_| PROXY_ADDR.to_string());
    let admin_addr = std::env::var("PLECTO_ADMIN_ADDR").unwrap_or_else(|_| ADMIN_ADDR.to_string());

    let manifest_path = deploy.join("manifest.toml");
    std::fs::write(
        &manifest_path,
        manifest_toml(&digest, i1, i2, i3, &proxy_addr, &admin_addr),
    )?;

    print_banner(&deploy, &manifest_path, &proxy_addr, &admin_addr);
    tokio::signal::ctrl_c().await?;
    println!(
        "\nbackend fleet stopped. The deploy dir stays for inspection: {}",
        deploy.display()
    );
    Ok(())
}

/// One `api` instance: echoes the authenticated user (stamped by the filter) and its own name, so
/// both the auth outcome and the least-request spread are visible. `/healthz` for the prober.
async fn spawn_instance(label: &'static str) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| async move {
                    let resp = if req.uri().path() == "/healthz" {
                        Response::builder()
                            .status(200)
                            .body(Full::new(Bytes::from_static(b"ok\n")))
                    } else {
                        let user = req
                            .headers()
                            .get("x-authenticated-user")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("(anonymous)")
                            .to_string();
                        Response::builder()
                            .status(200)
                            .header("x-instance", label)
                            .body(Full::new(Bytes::from(format!(
                                "hello {user} — served by {label}\n"
                            ))))
                    };
                    Ok::<_, Infallible>(resp.unwrap_or_else(|_| Response::new(Full::default())))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok(addr)
}

fn manifest_toml(
    digest: &str,
    i1: SocketAddr,
    i2: SocketAddr,
    i3: SocketAddr,
    proxy_addr: &str,
    admin_addr: &str,
) -> String {
    format!(
        r#"# Plecto production-shaped manifest — served by the real `plecto` binary:
#   cargo run -q -p plecto -- target/production-demo/manifest.toml {proxy_addr}
# Edit anything below and `kill -HUP <plecto pid>` to reload with zero downtime (ADR 000039).

[trust]
keys = ["trust.pem"]              # the roots allowed to sign filters (verify-then-load)

[state]
backend = "redb"                  # durable host state (ADR 000041): filter KV, counters and
path = "state/plecto.redb"        # rate-limit windows survive restarts. Fixed at construction —
                                  # changing it needs a restart, not a reload. Default: "memory".

[[filter]]
id = "apikey"
source = "filters/apikey"         # offline OCI image-layout, manifest-relative
digest = "{digest}"
isolation = "trusted"

[observability]
admin_addr = "{admin_addr}"     # /metrics /healthz /readyz — never on the data-plane port
access_log = true                 # one structured JSON event per request

[[upstream]]
name = "api"
addresses = ["{i1}", "{i2}", "{i3}"]
lb_algorithm = "least_request"    # P2C over per-instance in-flight counts (ADR 000035)
[upstream.health]
path = "/healthz"
interval_ms = 1000
[upstream.circuit_breaker]
max_requests = 64                 # shed load at the cap instead of queueing (ADR 000028)
[upstream.outlier_detection]
consecutive_gateway_failures = 5  # eject an instance misbehaving on live traffic (ADR 000032)

[[route]]
filters = ["apikey"]
upstream = "api"
strip_prefix = "/api"
[route.match]
path_prefix = "/api"
[route.rate_limit]
rate = 5                          # native per-client floor, before the chain (ADR 000033)
burst = 10
key = "client-ip"
"#
    )
}

fn print_banner(deploy: &Path, manifest: &Path, proxy_addr: &str, admin_addr: &str) {
    let deploy = deploy.display();
    println!("\n  Plecto demo — a production-shaped deployment (real binary + manifest.toml)\n");
    println!("  deploy dir : {deploy}");
    println!("  manifest   : {}", manifest.display());
    println!("  fleet      : 3 in-process `api` instances (this process keeps them alive)");
    println!(
        "\n  STEP 1 — done: the deploy dir is prepared (signed filter, trust root, manifest)."
    );
    println!("\n  STEP 2 — in a SECOND terminal, start the real gateway binary on it:\n");
    println!("    cargo run -q -p plecto -- {deploy}/manifest.toml {proxy_addr}");
    println!("\n  STEP 3 — exercise it like an operator (this terminal):\n");
    println!("    # auth: no key → the signed WASM filter short-circuits 401");
    println!("    curl -s -o /dev/null -w 'HTTP %{{http_code}}\\n' http://{proxy_addr}/api/data");
    println!("    # …a valid key → 200, identity stamped, least-request spread across api-1/2/3");
    println!(
        "    for i in $(seq 6); do curl -s -H 'x-api-key: alice-secret' http://{proxy_addr}/api/data; done"
    );
    println!();
    println!("    # native rate-limit floor (5 rps, burst 10, per client IP): burst through it");
    println!(
        "    for i in $(seq 14); do curl -s -o /dev/null -w '%{{http_code}} ' -H 'x-api-key: alice-secret' http://{proxy_addr}/api/data; done; echo"
    );
    println!("    #   → what's left of the burst passes (200), then 429s with");
    println!("    #     x-plecto-fault: rate-limited + retry-after (the floor counted the");
    println!("    #     auth curls above too — it runs before the chain, even on a 401)");
    println!();
    println!("    # the admin endpoint — RED metrics, filter spans, readiness (its own port):");
    println!("    curl -s http://{admin_addr}/metrics | grep -E '^plecto_' | head");
    println!(
        "    curl -s -o /dev/null -w 'readyz: HTTP %{{http_code}}\\n' http://{admin_addr}/readyz"
    );
    println!();
    println!("    # ops: edit {deploy}/manifest.toml (e.g. rate = 50), then reload / drain:");
    println!("    kill -HUP  <plecto pid>    # zero-downtime reload (fail-closed on a bad edit)");
    println!(
        "    kill -TERM <plecto pid>    # graceful shutdown: stop accepting, drain in-flight\n"
    );
    println!("  (Ctrl-C here stops the backend fleet.)\n");
}
