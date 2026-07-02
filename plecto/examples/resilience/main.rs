//! Plecto demo — **the resilience axes: retry, timeouts, circuit breaker, outlier detection**
//! (ADR 000023 / 000030 / 000031 / 000028 / 000032).
//!
//! Run it:  `cargo run -p plecto-server --example resilience`
//!
//! One upstream (`orders`) over three instances whose failure mode you flip at runtime
//! (`/mode/ok` / `/mode/slow` / `/mode/fail` on each instance). Every axis is visible from curl:
//!
//!   * **retry + per-try timeout** — a slow instance times out after 500ms and the request is
//!     re-sent to a DIFFERENT healthy instance (GET is idempotent, ADR 000023): the client still
//!     gets a 200, just ~0.5s late.
//!   * **overall timeout** — when EVERY instance is slow, retrying can't help; the whole
//!     transaction is bounded at 800ms and fails closed 504 (`x-plecto-fault: request-timeout`).
//!   * **circuit breaker** — at most 2 requests are in flight to the upstream at once (ADR 000028);
//!     excess concurrent requests shed instantly with 503 (`x-plecto-fault: circuit-open`) instead
//!     of queueing onto a saturated backend.
//!   * **outlier detection** — an instance answering 503 is retried around (clients keep seeing
//!     200!) while each 503 feeds its failure streak; after 3 consecutive it is silently ejected
//!     from rotation for 5s (ADR 000032). Its `/stats` hit counter proves no request touches it —
//!     even though `/healthz` stayed green the whole time (a different axis than active health).
//!
//! No filter / no TLS — this example is only about native fast-path resilience. Everything lives
//! in a temp dir, cleaned up on exit.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

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

/// An instance's failure mode, flipped at runtime via its `/mode/*` control endpoints.
const MODE_OK: u8 = 0;
const MODE_SLOW: u8 = 1; // sleep 1500ms, then 200 — beyond every proxy deadline
const MODE_FAIL: u8 = 2; // 503 — the gateway-class an unhealthy service instance returns

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    let a = spawn_instance("a").await?;
    let b = spawn_instance("b").await?;
    let c = spawn_instance("c").await?;

    let manifest_path = base.join("plecto.toml");
    std::fs::write(&manifest_path, manifest_toml(a, b, c))?;

    let control = Arc::new(Control::from_manifest_path(&manifest_path)?);

    // Overridable (PLECTO_PROXY_ADDR) so a host whose default port is taken can move it.
    let proxy_addr = std::env::var("PLECTO_PROXY_ADDR").unwrap_or_else(|_| PROXY_ADDR.to_string());
    let listener = TcpListener::bind(&proxy_addr).await?;
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

/// One `orders` instance. Any regular path serves per the current mode and counts a hit;
/// `/mode/{ok,slow,fail}` flips the mode; `/stats` reports the hit count (so an outlier ejection
/// is provable from outside); `/healthz` is ALWAYS 200 — active health stays green on purpose, so
/// what you watch is retry/outlier behaviour, not health-probe ejection.
async fn spawn_instance(label: &'static str) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mode = Arc::new(AtomicU8::new(MODE_OK));
    let hits = Arc::new(AtomicU64::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let mode = mode.clone();
            let hits = hits.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let mode = mode.clone();
                    let hits = hits.clone();
                    async move {
                        let resp = match req.uri().path() {
                            "/healthz" => Response::builder()
                                .status(200)
                                .body(Full::new(Bytes::from_static(b"ok\n"))),
                            "/stats" => Response::builder().status(200).body(Full::new(
                                Bytes::from(format!(
                                    "instance {label}: {} hits\n",
                                    hits.load(Ordering::Relaxed)
                                )),
                            )),
                            "/mode/ok" | "/mode/slow" | "/mode/fail" => {
                                let m = match req.uri().path() {
                                    "/mode/slow" => MODE_SLOW,
                                    "/mode/fail" => MODE_FAIL,
                                    _ => MODE_OK,
                                };
                                mode.store(m, Ordering::Relaxed);
                                Response::builder().status(200).body(Full::new(Bytes::from(
                                    format!("instance {label}: mode set\n"),
                                )))
                            }
                            path => {
                                hits.fetch_add(1, Ordering::Relaxed);
                                match mode.load(Ordering::Relaxed) {
                                    MODE_FAIL => Response::builder().status(503).body(Full::new(
                                        Bytes::from(format!("instance {label}: dependency down\n")),
                                    )),
                                    m => {
                                        if m == MODE_SLOW {
                                            tokio::time::sleep(Duration::from_millis(1500)).await;
                                        }
                                        Response::builder()
                                            .status(200)
                                            .header("x-instance", label)
                                            .body(Full::new(Bytes::from(format!(
                                                "instance {label} served {path}\n"
                                            ))))
                                    }
                                }
                            }
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

fn manifest_toml(a: SocketAddr, b: SocketAddr, c: SocketAddr) -> String {
    format!(
        r#"# Plecto demo manifest (generated) — every resilience knob on one upstream.
[[upstream]]
name = "orders"
addresses = ["{a}", "{b}", "{c}"]
request_timeout_ms = 500      # per-try: one attempt gets 500ms to produce response headers
overall_timeout_ms = 800      # the WHOLE transaction (attempts + backoff) is bounded at 800ms
max_retries = 1               # one re-send, to a DIFFERENT healthy instance
[upstream.health]
path = "/healthz"             # always 200 in this demo — health is deliberately a separate axis
interval_ms = 500
timeout_ms = 300
[upstream.circuit_breaker]
max_requests = 2              # at most 2 in-flight requests; excess sheds 503 circuit-open
[upstream.outlier_detection]
consecutive_gateway_failures = 3   # three consecutive 502/503/504 eject the instance…
base_ejection_time_ms = 5000       # …for 5s (doubling per consecutive ejection)
max_ejection_percent = 34          # but never eject more than 1 of the 3 (no self-DoS)

[[route]]
upstream = "orders"
[route.match]
path_prefix = "/"
"#
    )
}

fn print_banner(proxy: SocketAddr, instances: [(&str, SocketAddr); 3]) {
    let p = proxy.port();
    println!("\n  Plecto demo — resilience: retry, timeouts, circuit breaker, outlier detection\n");
    println!("  proxy : http://localhost:{p}   (plain HTTP/1.1)");
    for (label, addr) in instances {
        println!("  inst  : {label} -> http://{addr}   (/mode/ok /mode/slow /mode/fail, /stats)");
    }
    let a = instances[0].1;
    println!("\n  Try it (Ctrl-C to stop):\n");
    println!("    # 1) baseline: round-robin over three healthy instances");
    println!("    for i in $(seq 6); do curl -s http://localhost:{p}/orders; done");
    println!();
    println!("    # 2) retry rescues a slow instance: `a` now times out per-try (500ms), and the");
    println!("    #    request is re-sent to a healthy instance — still 200, ~0.5s late:");
    println!("    curl -s http://{a}/mode/slow");
    println!(
        "    for i in $(seq 3); do curl -s -w '  (%{{time_total}}s)\\n' -o /dev/stdout http://localhost:{p}/orders; done"
    );
    println!();
    println!("    # 3) ALL instances slow → retrying can't help; the overall deadline (800ms)");
    println!("    #    fails the whole transaction closed with 504:");
    for (_, addr) in instances {
        println!("    curl -s http://{addr}/mode/slow >/dev/null");
    }
    println!("    curl -s -i http://localhost:{p}/orders | grep -E 'HTTP/|x-plecto-fault'");
    println!();
    println!("    # 4) circuit breaker: with the upstream saturated (still all-slow), fire 4");
    println!("    #    concurrent requests — 2 occupy the in-flight cap, the rest shed 503");
    println!("    #    circuit-open INSTANTLY instead of queueing:");
    println!(
        "    for i in 1 2 3 4; do curl -s -o /dev/null -w '%{{http_code}} %{{time_total}}s\\n' http://localhost:{p}/orders & done; wait"
    );
    println!();
    println!("    # 5) outlier detection: reset everything, then make `a` answer 503. Clients");
    println!("    #    keep seeing 200 (each 503 is retried around) while a's failure streak");
    println!("    #    builds — after 3 consecutive, `a` is ejected for 5s. Its /stats counter");
    println!("    #    freezes even though its /healthz never went red:");
    for (_, addr) in instances {
        println!("    curl -s http://{addr}/mode/ok >/dev/null");
    }
    println!("    curl -s http://{a}/mode/fail");
    println!(
        "    for i in $(seq 9); do curl -s -o /dev/null -w '%{{http_code}} ' http://localhost:{p}/orders; done; echo"
    );
    println!("    curl -s http://{a}/stats     # note the count…");
    println!(
        "    for i in $(seq 9); do curl -s -o /dev/null -w '%{{http_code}} ' http://localhost:{p}/orders; done; echo"
    );
    println!("    curl -s http://{a}/stats     # …frozen: a is out of rotation, silently\n");
}
