//! Plecto demo — **canary release: weighted traffic split + header-match routing** (ADR 000034).
//!
//! Run it:  `cargo run -p plecto-server --example canary`
//!
//! The scenario: `checkout` v2 is rolling out. One route splits public traffic **90/10** between
//! `checkout-v1` and `checkout-v2` (`[[route.backends]]` weights, deterministic error-diffusion
//! apportionment — a 90/10 split emits v2 exactly every 10th request, not in bursts). A second,
//! more specific route sends anyone with `x-canary: always` straight to v2 — header matches beat a
//! plain path match under the specificity rule (host > longest prefix > method > headers > query) —
//! so internal testers exercise v2 at will while the public keeps its 90/10 odds.
//!
//! The manifest lives on disk and the SIGHUP reload loop runs (ADR 000008 / 000039), so the rollout
//! is *operable*: edit the v2 weight to `0` and `kill -HUP` to **drain the canary instantly** (bad
//! rollout), or to `100`/drop v1 to promote it — zero downtime either way. The tester route keeps
//! working while the public split is drained: weight 0 only empties the *split*, not the explicit
//! header route.
//!
//! No filter / no TLS — this example is only about native routing, so it serves plain HTTP/1.1.
//! Everything lives in a temp dir, cleaned up on exit.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use plecto_control::Control;
use plecto_server::serve;

const PROXY_ADDR: &str = "127.0.0.1:8083";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    let v1 = spawn_version("v1").await?;
    let v2 = spawn_version("v2").await?;

    let manifest_path = base.join("plecto.toml");
    std::fs::write(&manifest_path, manifest_toml(v1, v2))?;

    // `from_manifest_path` remembers the path, so a SIGHUP re-reads the edited weights live.
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
    eprintln!("note: SIGHUP reload is Unix-only; this platform serves a static split.");

    // Overridable (PLECTO_PROXY_ADDR) so a host whose default port is taken can move it.
    let proxy_addr = std::env::var("PLECTO_PROXY_ADDR").unwrap_or_else(|_| PROXY_ADDR.to_string());
    let listener = TcpListener::bind(&proxy_addr).await?;
    let proxy = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(e) = serve(control, listener).await {
            eprintln!("proxy stopped: {e}");
        }
    });

    print_banner(proxy, &manifest_path);
    tokio::signal::ctrl_c().await?;
    println!("\nshutting down — temp dir cleaned up.");
    Ok(())
}

/// One version of the checkout service. Responses carry `x-app-version` and name the version in the
/// body, so the split ratio is countable from the outside.
async fn spawn_version(version: &'static str) -> anyhow::Result<SocketAddr> {
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
                        Response::builder()
                            .status(200)
                            .header("x-app-version", version)
                            .body(Full::new(Bytes::from(format!(
                                "checkout {version} handled {}\n",
                                req.uri().path()
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

fn manifest_toml(v1: SocketAddr, v2: SocketAddr) -> String {
    format!(
        r#"# Plecto demo manifest (generated) — canary rollout for the checkout service.
# Edit a weight below, then `kill -HUP <pid>` to apply it with zero downtime:
#   * drain the canary  : weight = 10  ->  weight = 0     (v2 gets no public traffic)
#   * promote the canary: v1 weight 90 -> 0, v2 weight 10 -> 100
[[upstream]]
name = "checkout-v1"
addresses = ["{v1}"]
[upstream.health]
path = "/healthz"
interval_ms = 500
timeout_ms = 300

[[upstream]]
name = "checkout-v2"
addresses = ["{v2}"]
[upstream.health]
path = "/healthz"
interval_ms = 500
timeout_ms = 300

# The public route: a weighted 90/10 split (Gateway-API semantics, weight / sum-of-weights).
[[route]]
[route.match]
path_prefix = "/"
[[route.backends]]
upstream = "checkout-v1"
weight = 90
[[route.backends]]
upstream = "checkout-v2"
weight = 10

# The tester route: `x-canary: always` goes straight to v2. A header match makes this route more
# specific than the split above (same prefix), so it wins whenever the header is present.
[[route]]
upstream = "checkout-v2"
[route.match]
path_prefix = "/"
[route.match.headers]
x-canary = "always"
"#
    )
}

fn print_banner(proxy: SocketAddr, manifest: &std::path::Path) {
    let p = proxy.port();
    let pid = std::process::id();
    println!(
        "\n  Plecto demo — canary release: weighted split + header-match routing (ADR 000034)\n"
    );
    println!("  proxy    : http://localhost:{p}   (plain HTTP/1.1)");
    println!("  pid      : {pid}");
    println!("  manifest : {}", manifest.display());
    println!("  split    : checkout-v1 90  /  checkout-v2 10   (public traffic)");
    println!("\n  Try it (Ctrl-C to stop):\n");
    println!("    # 1) the public split: 20 requests land exactly 18 on v1, 2 on v2 — the");
    println!("    #    apportionment schedule interleaves evenly (every 10th request is v2):");
    println!(
        "    for i in $(seq 20); do curl -s http://localhost:{p}/checkout; done | sort | uniq -c"
    );
    println!();
    println!("    # 2) an internal tester forces the canary — the header-match route wins:");
    println!("    curl -s -H 'x-canary: always' http://localhost:{p}/checkout   # always v2");
    println!();
    println!("    # 3) the rollout looks bad — DRAIN the canary with zero downtime:");
    println!("    #    edit the manifest (weight = 10 -> weight = 0), then reload:");
    println!("    kill -HUP {pid}");
    println!("    #    the public split now sends v2 nothing… but the tester route still");
    println!("    #    reaches v2, so you can debug the bad canary while users are safe.");
    println!();
    println!("    # 4) or promote it: v1 weight -> 0, v2 weight -> 100, SIGHUP again.\n");
}
