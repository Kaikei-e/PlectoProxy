//! Plecto demo — **load balancing + active health checks** (ADR 000017).
//!
//! Run it:  `cargo run -p plecto-server --example load-balancing`
//!
//! One upstream named `pool` fans out over **three instances** (`a`, `b`, `c`). The fast path
//! round-robins each request across the *healthy* set, while a background supervisor probes every
//! instance's `/healthz` on an interval. Each instance also exposes a `/toggle` endpoint that flips
//! its own health, so you can watch an instance get **ejected** when it goes unhealthy and
//! **restored** when it recovers — and see the upstream **fail closed with 503** when none are left.
//!
//! No filter / no TLS here — this example is only about upstream LB, so it serves plain HTTP/1.1
//! (curl needs no `-k`). Everything lives in a temp dir, cleaned up on exit.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use plecto_control::Control;
use plecto_server::serve;

/// Plain HTTP/1.1 — the focus is upstream balancing, not TLS.
const PROXY_ADDR: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    // Three upstream instances, each labelling its responses and exposing /healthz + /toggle.
    let a = spawn_instance("a").await?;
    let b = spawn_instance("b").await?;
    let c = spawn_instance("c").await?;

    // The manifest: one upstream `pool` over the three instances, with a fast active health check
    // so toggling an instance reacts within ~1s. A filter-less `/` route forwards everything.
    let manifest_path = base.join("plecto.toml");
    std::fs::write(&manifest_path, manifest_toml(a, b, c))?;

    // No filters and no trust keys → the control plane builds with an empty trust policy; nothing
    // is loaded, so no signing/OCI is needed for this example.
    let control = Arc::new(Control::from_manifest_path(&manifest_path)?);

    let listener = TcpListener::bind(PROXY_ADDR).await?;
    let proxy = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(e) = serve(control, listener).await {
            eprintln!("proxy stopped: {e}");
        }
    });

    print_banner(proxy, [("a", a), ("b", b), ("c", c)]);
    tokio::signal::ctrl_c().await?;
    println!("\nshutting down — temp dir cleaned up.");
    Ok(())
}

/// One upstream instance. `GET /` (any non-special path) returns `x-instance: <label>` so the
/// round-robin is visible; `GET /healthz` reports the instance's current health (200 / 503); `GET
/// /toggle` flips that health, so you can drive an instance unhealthy and back by hand.
async fn spawn_instance(label: &'static str) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let healthy = Arc::new(AtomicBool::new(true));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let healthy = healthy.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let healthy = healthy.clone();
                    async move {
                        let resp = match req.uri().path() {
                            "/healthz" => {
                                let (status, msg) = if healthy.load(Ordering::Relaxed) {
                                    (200, "ok\n")
                                } else {
                                    (503, "unhealthy\n")
                                };
                                Response::builder()
                                    .status(status)
                                    .body(Full::new(Bytes::from_static(msg.as_bytes())))
                            }
                            "/toggle" => {
                                let now = !healthy.fetch_xor(true, Ordering::Relaxed);
                                Response::builder().status(200).body(Full::new(Bytes::from(
                                    format!("instance {label}: healthy={now}\n"),
                                )))
                            }
                            _ => Response::builder()
                                .status(200)
                                .header("x-instance", label)
                                .body(Full::new(Bytes::from(format!(
                                    "served by instance {label}\n"
                                )))),
                        };
                        // builder only errors on a malformed status/header, none of which occur here.
                        Ok::<_, Infallible>(resp.unwrap_or_else(|_| Response::new(Full::default())))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok(addr)
}

fn manifest_toml(a: SocketAddr, b: SocketAddr, c: SocketAddr) -> String {
    format!(
        r#"# Plecto demo manifest (generated) — one upstream over three instances, round-robin LB.
# Plain HTTP/1.1 (no [[tls]]). The active health check probes each instance's /healthz.
[[upstream]]
name = "pool"
addresses = ["{a}", "{b}", "{c}"]
[upstream.health]
path = "/healthz"
interval_ms = 500          # probe each instance twice a second
timeout_ms = 300
healthy_threshold = 2      # ~1s to (re)enter rotation after recovering
unhealthy_threshold = 2    # ~1s to eject after going unhealthy

# A filter-less route: forward everything under / to the pool (no chain, no prefix strip).
[[route]]
path_prefix = "/"
upstream = "pool"
"#
    )
}

fn print_banner(proxy: SocketAddr, instances: [(&str, SocketAddr); 3]) {
    let p = proxy.port();
    println!("\n  Plecto demo — load balancing + active health checks (ADR 000017)\n");
    println!("  proxy : http://localhost:{p}   (plain HTTP/1.1 — no -k needed)");
    for (label, addr) in instances {
        println!(
            "  inst  : {label} -> http://{addr}   (/healthz reports, /toggle flips its health)"
        );
    }
    println!("\n  Try it (Ctrl-C to stop):\n");
    println!("    # 1) round-robin: repeat and watch the instance cycle a -> b -> c");
    println!("    for i in $(seq 6); do curl -s http://localhost:{p}/; done");
    println!();
    println!("    # 2) drive instance b unhealthy, then re-run the loop above:");
    println!("    #    within ~1s b is ejected and traffic flows only to a and c.");
    println!("    curl -s http://{}/toggle", instances[1].1);
    println!();
    println!("    # 3) toggle b back; within ~1s it rejoins the rotation.");
    println!("    curl -s http://{}/toggle", instances[1].1);
    println!();
    println!("    # 4) toggle ALL three off -> the upstream has no healthy instance ->");
    println!("    #    the proxy fails closed with 503 (x-plecto-fault: no-healthy-upstream).");
    for (_, addr) in instances {
        println!("    curl -s http://{addr}/toggle >/dev/null");
    }
    println!(
        "    curl -s -i http://localhost:{p}/ | head -1   # HTTP/1.1 503 Service Unavailable\n"
    );
}
