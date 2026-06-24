//! Plecto demo — **a real WASM filter doing real work: API-key authentication** (the whole point).
//!
//! Run it:  `cargo run -p plecto-server --example wasm-auth`
//!
//! This is Plecto's thesis in one runnable file: the per-request *decision* — here, authentication —
//! is a **sandboxed WASM Component Model filter** (`crates/filter-apikey`, compiled to a
//! `plecto:filter` component), not native proxy code. The fast path stays native; the policy is a
//! component you can write in any language, hot-swap with zero downtime, and that can touch **only**
//! the host-API it was lent (here `host-kv` / `host-counter` / `host-log` — no network, no FS).
//!
//! What the filter does (`crates/filter-apikey/src/lib.rs`):
//!   * `init` seeds a demo key→user map into host KV (filters are stateless; state lives in the host);
//!   * `on-request` reads `x-api-key`, looks it up in KV, and returns a typed `decision`:
//!       - missing/invalid key  -> `short-circuit` 401 (the upstream is never reached);
//!       - valid key            -> `modified` (stamp `x-authenticated-user`) + bump a per-user counter, then continue.
//!
//! It wires the production load path (sign + OCI layout + verify + load, all fail-closed) and serves
//! plain HTTP/1.1 so the focus is the filter. Temp dir, cleaned up on exit.

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

use plecto_control::oci::write_layout;
use plecto_control::{Control, ResolvedArtifact};
use plecto_host::test_support::{TestSigner, bound_sbom, filter_apikey_component};
use plecto_server::serve;

const PROXY_ADDR: &str = "127.0.0.1:8084";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    // trust root + signer, then sign the REAL example filter (filter-apikey) and bundle it as an
    // offline OCI image-layout — exactly how an operator would distribute a signed filter.
    let signer = TestSigner::new()?;
    std::fs::write(base.join("trust.pem"), signer.public_key_pem())?;
    let component = filter_apikey_component();
    let component_signature = signer.sign(&component)?;
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom)?;
    let digest = write_layout(
        &base.join("filters/apikey"),
        &ResolvedArtifact {
            component,
            component_signature,
            sbom,
            sbom_signature,
        },
    )?;

    let upstream = spawn_upstream().await?;
    let manifest_path = base.join("plecto.toml");
    std::fs::write(&manifest_path, manifest_toml(&digest, upstream))?;

    // the real ops entrypoint: verifies the signature + SBOM and loads the filter, fail-closed.
    let control = Arc::new(Control::from_manifest_path(&manifest_path)?);

    let listener = TcpListener::bind(PROXY_ADDR).await?;
    let proxy = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(e) = serve(control, listener).await {
            eprintln!("proxy stopped: {e}");
        }
    });

    print_banner(proxy);
    tokio::signal::ctrl_c().await?;
    println!("\nshutting down — temp dir cleaned up.");
    Ok(())
}

/// The protected upstream: echoes the `x-authenticated-user` the filter stamped, so a successful
/// auth is visible. It only ever sees requests the filter let through (a 401 never reaches here).
async fn echo(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let user = req
        .headers()
        .get("x-authenticated-user")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(anonymous)")
        .to_string();
    Ok(Response::builder()
        .status(200)
        .header("x-from", "protected-upstream")
        .body(Full::new(Bytes::from(format!(
            "hello {user} — you reached the protected upstream\n"
        ))))
        .unwrap())
}

async fn spawn_upstream() -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(echo))
                    .await;
            });
        }
    });
    Ok(addr)
}

fn manifest_toml(digest: &str, upstream: SocketAddr) -> String {
    format!(
        r#"# Plecto demo manifest (generated) — a signed WASM auth filter gates a /api route.
[trust]
keys = ["trust.pem"]

[[filter]]
id = "apikey"
source = "filters/apikey"
digest = "{digest}"
isolation = "trusted"

[[upstream]]
name = "protected"
addresses = ["{upstream}"]
[upstream.health]
path = "/"

[[route]]
path_prefix = "/api"
filters = ["apikey"]
upstream = "protected"
strip_prefix = "/api"
"#
    )
}

fn print_banner(proxy: SocketAddr) {
    let p = proxy.port();
    println!("\n  Plecto demo — a real WASM filter: API-key authentication\n");
    println!("  proxy : http://localhost:{p}   (plain HTTP/1.1)");
    println!("  filter: crates/filter-apikey (a signed plecto:filter component)");
    println!("  keys  : alice-secret -> alice,  bob-secret -> bob   (seeded into host KV at init)");
    println!("\n  Try it (Ctrl-C to stop):\n");
    println!("    # no key -> the filter short-circuits 401; the upstream is never reached:");
    println!("    curl -i http://localhost:{p}/api/data");
    println!();
    println!("    # an unknown key -> 401 as well:");
    println!("    curl -i -H 'x-api-key: nope' http://localhost:{p}/api/data");
    println!();
    println!("    # a valid key -> the filter stamps x-authenticated-user and continues;");
    println!("    # the upstream greets the authenticated caller by name:");
    println!("    curl -s -H 'x-api-key: alice-secret' http://localhost:{p}/api/data");
    println!("    curl -s -H 'x-api-key: bob-secret'   http://localhost:{p}/api/data");
    println!();
    println!("    # a spoofed identity header is overwritten by the filter (set replaces), so the");
    println!("    # upstream still sees the key's real user, never the client's claim:");
    println!("    curl -s -H 'x-api-key: alice-secret' -H 'x-authenticated-user: admin' \\");
    println!("         http://localhost:{p}/api/data        # -> hello alice (not admin)\n");
}
