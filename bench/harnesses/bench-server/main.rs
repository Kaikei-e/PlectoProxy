//! Plecto benchmark harness — the canonical bench server. Merges the former `wasm-bench` (the
//! WASM extension-plane cost ladder) and `edge-bench` (host rate-limit + request-body hook)
//! harnesses onto **one process, one `/baseline` route**, so the plain-HTTP/1.1 ceiling is
//! measured exactly once and every other phase (`tls`, `wasm`, formerly `churn`) references that
//! same number instead of re-measuring it on a differently-configured server.
//!
//! Run it:  `cargo run --release -p plecto-server --features bench-harnesses --example bench-server`
//!
//! Routes, all forwarding to the **same** `backend` upstream unless noted (a cost ladder: each
//! adjacent delta isolates one cost):
//!
//!   * `/baseline/*`        — no filter at all (native fast path only)         → the control
//!   * `/noop-pooled/*`     — a **pure no-op** WASM filter, pooled             → dispatch floor
//!   * `/noop-fresh/*`      — the same no-op, fresh instance per request       → + instantiation
//!   * `/trusted/*`         — the signed `filter-apikey` component, pooled     → + a real filter's work
//!   * `/ondemand/*`        — the apikey filter, fresh instance per request
//!   * `/ratelimit/*`       — `filter-hello` consults the host-native token bucket (`x-plecto-ratelimit`
//!     selects the key)
//!   * `/body/*`            — `filter-hello`'s `on-request-body` buffers + uppercases the POST body
//!   * `/body-untrusted/*`  — the same hello filter, untrusted (fresh instance per request)
//!   * `/body-noop/*`       — the SAME pooled no-op filter as `/noop-pooled`, but its `on-request-body`
//!     returns the body untouched (a copy round-trip without a transform)
//!   * `/body-headeronly/*` — `filter-quickstart` (header-only, no `on-request-body` export) — the
//!     ADR 000038 zero-copy bypass control: a POST here must not buffer the body
//!   * `/ws`                — an Upgrade-tunneled route (ADR 000048) to a dedicated WebSocket-echo
//!     upstream; `[route.upgrade]` allowlists the `websocket` token
//!
//! The API-key filter reads `x-api-key`; a valid key (`alice-secret`/`bob-secret`) is stamped +
//! forwarded (200), anything else is short-circuited 401 without touching the upstream. Plain
//! HTTP/1.1; temp dir cleaned up on exit.

// jemalloc (feature `jemalloc`): swap the global allocator to isolate the memory matrix's
// allocator hypothesis (glibc arena retention). Off by default; the mem-matrix allocator sweep
// builds this variant to compare against glibc / MALLOC_ARENA_MAX=1.
#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod ws;

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
use plecto_host::test_support::{
    TestSigner, bound_sbom, filter_apikey_component, filter_hello_component, filter_noop_component,
    filter_quickstart_component,
};
use plecto_server::serve;

const PROXY_ADDR: &str = "127.0.0.1:8085";

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Sign a component + its bound SBOM with `signer` and write the OCI layout under `dir`, returning
/// the component digest the manifest references.
fn sign_and_write(
    signer: &TestSigner,
    component: Vec<u8>,
    dir: &std::path::Path,
) -> anyhow::Result<String> {
    let component_signature = signer.sign(&component)?;
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom)?;
    let artifact = ResolvedArtifact {
        component,
        component_signature,
        sbom,
        sbom_signature,
    };
    Ok(write_layout(dir, &artifact)?)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    let signer = TestSigner::new()?;
    std::fs::write(base.join("trust.pem"), signer.public_key_pem())?;

    // apikey / apikey_od: the SAME signed component under two isolation modes (pooled vs
    // fresh-per-request) — the WASM ladder's "a real filter's work" rungs.
    let apikey_digest = sign_and_write(
        &signer,
        filter_apikey_component(),
        &base.join("filters/apikey"),
    )?;
    // noop / noop_od: a filter that makes no host-API calls, signed the same way. baseline->noop
    // isolates the irreducible dispatch tax; the pooled "noop" id is reused verbatim by
    // /body-noop (the on-request-body pass-through-cost control), since it is the identical
    // component under the identical (trusted) isolation.
    let noop_digest = sign_and_write(&signer, filter_noop_component(), &base.join("filters/noop"))?;
    // hello / hello-u: on-request-body buffer+uppercase, pooled vs fresh; also the rate-limit
    // filter (the bucket spec is host-configured on the filter, ADR 000026).
    let hello_digest = sign_and_write(
        &signer,
        filter_hello_component(),
        &base.join("filters/hello"),
    )?;
    // quickstart: header-only (no on-request-body export) — the ADR 000038 zero-copy control.
    let quickstart_digest = sign_and_write(
        &signer,
        filter_quickstart_component(),
        &base.join("filters/quickstart"),
    )?;

    let capacity = env_u64("RL_CAPACITY", 1_000_000_000);
    let refill_tokens = env_u64("RL_REFILL_TOKENS", 1_000_000_000);
    let refill_interval_ms = env_u64("RL_REFILL_INTERVAL_MS", 1000);
    let resp_bytes = env_u64("RESP_BYTES", 16) as usize;
    let latency_ms = env_u64("BACKEND_LATENCY_MS", 0);
    let ws_idle_timeout_ms = env_u64("WS_IDLE_TIMEOUT_MS", 300_000);

    // Upstream: an EXTERNAL process when `UPSTREAM_ADDR` is set (the mem-matrix does this so the
    // proxy's RSS is measured alone), otherwise an in-process one.
    let upstream = match std::env::var("UPSTREAM_ADDR") {
        Ok(addr) => addr.parse::<SocketAddr>()?,
        Err(_) => spawn_upstream(latency_ms, resp_bytes).await?,
    };
    let ws_upstream = ws::spawn_echo_upstream().await?;

    let digests = Digests {
        apikey: &apikey_digest,
        noop: &noop_digest,
        hello: &hello_digest,
        quickstart: &quickstart_digest,
    };
    let ratelimit = RateLimitSpec {
        capacity,
        refill_tokens,
        refill_interval_ms,
    };
    let manifest_path = base.join("plecto.toml");
    std::fs::write(
        &manifest_path,
        manifest_toml(
            &digests,
            upstream,
            ws_upstream,
            &ratelimit,
            ws_idle_timeout_ms,
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

/// The shared JSON-ish upstream: drains the request body (so POST bodies + keep-alive work),
/// sleeps `latency_ms`, then returns a `resp_bytes`-sized body. Echoes `x-authenticated-user` (set
/// by the apikey filter) back as a response header, so a curl exploration can see the stamped
/// identity even though the body itself is a fixed filler payload.
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
                        let user = req.headers().get("x-authenticated-user").cloned();
                        let _ = req.into_body().collect().await;
                        if latency_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(latency_ms)).await;
                        }
                        let mut builder =
                            Response::builder().status(200).header("x-from", "backend");
                        if let Some(u) = user {
                            builder = builder.header("x-authenticated-user", u);
                        }
                        Ok::<_, Infallible>(
                            builder
                                .body(Full::new(body))
                                .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()))),
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

/// The four filters' component digests (all four are the same three components, `apikey` and
/// `noop` each reused under two isolation modes via a separate manifest `[[filter]]` id).
struct Digests<'a> {
    apikey: &'a str,
    noop: &'a str,
    hello: &'a str,
    quickstart: &'a str,
}

/// The host-native token bucket spec (ADR 000026), operator-configured on the filter entry.
struct RateLimitSpec {
    capacity: u64,
    refill_tokens: u64,
    refill_interval_ms: u64,
}

fn manifest_toml(
    digests: &Digests<'_>,
    upstream: SocketAddr,
    ws_upstream: SocketAddr,
    ratelimit: &RateLimitSpec,
    ws_idle_timeout_ms: u64,
) -> String {
    let Digests {
        apikey: apikey_digest,
        noop: noop_digest,
        hello: hello_digest,
        quickstart: quickstart_digest,
    } = *digests;
    let RateLimitSpec {
        capacity,
        refill_tokens,
        refill_interval_ms,
    } = *ratelimit;
    // request_deadline_ms is generous throughout: this is a cost measurement, not an SLA test —
    // the default 100ms could false-fail a cold on-demand init or a buffered body decision.
    format!(
        r#"# Plecto benchmark manifest (generated) — the canonical bench server: WASM cost ladder +
# host rate-limit (ADR 000026) + request-body hook (ADR 000025/000038) + WS upgrade (ADR 000048).
[trust]
keys = ["trust.pem"]

[[filter]]
id = "apikey"
source = "filters/apikey"
digest = "{apikey_digest}"
isolation = "trusted"
request_deadline_ms = 1000

[[filter]]
id = "apikey_od"
source = "filters/apikey"
digest = "{apikey_digest}"
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
source = "filters/noop"
digest = "{noop_digest}"
isolation = "untrusted"
request_deadline_ms = 1000

[[filter]]
id = "hello"
source = "filters/hello"
digest = "{hello_digest}"
isolation = "trusted"
request_deadline_ms = 1000
ratelimit = {{ capacity = {capacity}, refill_tokens = {refill_tokens}, refill_interval_ms = {refill_interval_ms} }}

[[filter]]
id = "hello_od"
source = "filters/hello"
digest = "{hello_digest}"
isolation = "untrusted"
request_deadline_ms = 1000
ratelimit = {{ capacity = {capacity}, refill_tokens = {refill_tokens}, refill_interval_ms = {refill_interval_ms} }}

[[filter]]
id = "quickstart"
source = "filters/quickstart"
digest = "{quickstart_digest}"
isolation = "trusted"
request_deadline_ms = 1000

[[upstream]]
name = "backend"
addresses = ["{upstream}"]
[upstream.health]
path = "/"

[[upstream]]
name = "wsback"
addresses = ["{ws_upstream}"]
[upstream.health]
path = "/healthz"

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

[[route]]
filters = ["hello"]
upstream = "backend"
strip_prefix = "/ratelimit"
[route.match]
path_prefix = "/ratelimit"

[[route]]
filters = ["hello"]
upstream = "backend"
strip_prefix = "/body"
[route.match]
path_prefix = "/body"

[[route]]
filters = ["hello_od"]
upstream = "backend"
strip_prefix = "/body-untrusted"
[route.match]
path_prefix = "/body-untrusted"

[[route]]
filters = ["noop"]
upstream = "backend"
strip_prefix = "/body-noop"
[route.match]
path_prefix = "/body-noop"

[[route]]
filters = ["quickstart"]
upstream = "backend"
strip_prefix = "/body-headeronly"
[route.match]
path_prefix = "/body-headeronly"

[[route]]
upstream = "wsback"
[route.match]
path_prefix = "/ws"
[route.upgrade]
protocols = ["websocket"]
idle_timeout_ms = {ws_idle_timeout_ms}
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
    println!("\n  Plecto benchmark — the canonical bench server\n");
    println!("  proxy  : http://localhost:{p}   (plain HTTP/1.1)");
    println!("  backend: in-process, sleeps {latency_ms} ms, returns {resp_bytes}-byte body");
    println!(
        "  bucket : capacity={capacity} refill={refill_tokens}/{refill_interval_ms}ms  (host-set, per key)\n"
    );
    println!("  cost ladder (same backend, only the decision path differs):");
    println!("    /baseline/*        no filter          curl http://localhost:{p}/baseline/x");
    println!("    /noop-pooled/*     wasm no-op, pooled  curl http://localhost:{p}/noop-pooled/x");
    println!("    /noop-fresh/*      wasm no-op, fresh   curl http://localhost:{p}/noop-fresh/x");
    println!(
        "    /trusted/*         apikey, pooled      curl -H 'x-api-key: alice-secret' http://localhost:{p}/trusted/x"
    );
    println!(
        "    /ondemand/*        apikey, fresh/req    curl -H 'x-api-key: alice-secret' http://localhost:{p}/ondemand/x"
    );
    println!("  host-API paths:");
    println!(
        "    /ratelimit/*       host token bucket   curl -H 'x-plecto-ratelimit: alice' http://localhost:{p}/ratelimit/x"
    );
    println!(
        "    /body/*            on-request-body     curl -X POST --data hello http://localhost:{p}/body/x   (-> HELLO)"
    );
    println!("    /body-untrusted/*  same, fresh-per-request instance (untrusted isolation)");
    println!("    /body-noop/*       on-request-body, body returned untouched (no transform)");
    println!("    /body-headeronly/* header-only filter — body streams through (ADR 000038)");
    println!("    /ws                WebSocket upgrade tunnel (ADR 000048)");
    println!(
        "    (no/invalid key on a filtered route -> 401; over-limit -> 429; deny-body -> 403)\n"
    );
}
