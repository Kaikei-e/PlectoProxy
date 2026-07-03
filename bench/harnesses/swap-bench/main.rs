//! Plecto benchmark harness — **endpoint-set swap under load** (ADR 000044). Isolates a different
//! axis than `load-balancing`'s health-based ejection: here the *configured address set itself*
//! changes (the shape a periodic-DNS re-resolution swap or an operator-driven reload both take),
//! exercising the per-pick `ArcSwap<Endpoints>` load this harness's sibling criterion bench
//! (`pick_under_swap_churn` in `crates/control/benches/fastpath.rs`) measures in isolation.
//!
//! Run it:  `cargo run --release -p plecto-server --features bench-harnesses --example swap-bench`
//!
//! Four upstream instances spin up (`a`, `b`, `c`, `d`); the manifest starts with the pool
//! `[a, b, c]` — `d` is a spare, idle until swapped in. A SIGHUP re-reads the on-disk manifest and
//! reconciles it (ADR 000008/000017), which is the same code path a resolved-address-set change
//! takes. `bench/perf/run-perf.sh`'s `swap` phase rewrites the manifest (dropping `c`, adding `d`)
//! and sends SIGHUP mid-load, then reads how quickly (and how cleanly) traffic follows.
//!
//! No filter / no TLS — this harness is only about the endpoint swap. Plain HTTP/1.1; everything
//! lives in a temp dir, cleaned up on exit.

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

const PROXY_ADDR: &str = "127.0.0.1:8087";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    let a = spawn_instance("a").await?;
    let b = spawn_instance("b").await?;
    let c = spawn_instance("c").await?;
    let d = spawn_instance("d").await?;

    let manifest_path = base.join("plecto.toml");
    std::fs::write(&manifest_path, manifest_toml(&[a, b, c]))?;

    // `from_manifest_path` remembers the path, so a SIGHUP-driven reload re-reads it in place —
    // the run-perf.sh `swap` phase overwrites this file (dropping c, adding d) mid-load.
    let control = Arc::new(Control::from_manifest_path(&manifest_path)?);

    #[cfg(unix)]
    {
        let control = control.clone();
        std::thread::spawn(move || match plecto_control::SignalReloadSource::sighup() {
            Ok(mut source) => plecto_control::serve_reloads(&control, &mut source),
            Err(e) => eprintln!("could not register SIGHUP handler: {e}"),
        });
    }
    #[cfg(not(unix))]
    eprintln!("note: SIGHUP reload is Unix-only; the endpoint set is static on this platform.");

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
        [("a", a), ("b", b), ("c", c), ("d", d)],
        &manifest_path,
    );
    tokio::signal::ctrl_c().await?;
    println!("\nshutting down — temp dir cleaned up.");
    Ok(())
}

/// One upstream instance: `GET /` stamps `x-instance: <label>` (so a load generator can bucket
/// served counts by instance, same convention as `load-balancing`'s `/toggle` demo); `/healthz`
/// always reports healthy (this harness's axis is the endpoint SET, not health state).
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
                            _ => Response::builder()
                                .status(200)
                                .header("x-instance", label)
                                .body(Full::new(Bytes::from(format!(
                                    "served by instance {label}\n"
                                )))),
                        };
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

fn manifest_toml(pool: &[SocketAddr]) -> String {
    let addresses = pool
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"# Plecto benchmark manifest (generated) — endpoint-set swap under load (ADR 000044).
# Edit `addresses` (e.g. drop one, add another) and `kill -HUP <pid>` to swap the pool live.
[[upstream]]
name = "pool"
addresses = [{addresses}]
[upstream.health]
path = "/healthz"
interval_ms = 500
timeout_ms = 300
healthy_threshold = 2
unhealthy_threshold = 2

[[route]]
upstream = "pool"
[route.match]
path_prefix = "/"
"#
    )
}

fn print_banner(proxy: SocketAddr, instances: [(&str, SocketAddr); 4], manifest: &std::path::Path) {
    let p = proxy.port();
    let pid = std::process::id();
    println!("\n  Plecto benchmark — endpoint-set swap under load (ADR 000044)\n");
    println!("  proxy    : http://localhost:{p}   (plain HTTP/1.1)");
    println!("  pid      : {pid}");
    println!("  manifest : {}", manifest.display());
    for (label, addr) in instances {
        println!("  inst  : {label} -> http://{addr}");
    }
    println!("\n  Initial pool: a, b, c (d is a spare, idle until swapped in).");
    println!("  Swap the pool live: rewrite `addresses` in the manifest above, then:");
    println!("    kill -HUP {pid}\n");
}
