//! Plecto quickstart — **the 5-minute hello**: run a proxy whose per-request logic is a sandboxed
//! WASM filter, and see it act with one `curl`.
//!
//! Run it:  `cargo run -p plecto-server --example quickstart`
//!
//! The proxy forwards `/` to a tiny in-process upstream. In between, the `filter-quickstart`
//! component (`examples/filters/filter-quickstart`) stamps `x-plecto: hello-from-wasm` on the
//! response — so a single request proves a WASM Component Model filter, loaded through the
//! production verify-then-load path (cosign signature + SBOM), touched your traffic:
//!
//! ```text
//! curl -i http://localhost:8080/
//!   HTTP/1.1 200 OK
//!   x-plecto: hello-from-wasm      <-- added by the sandboxed filter
//!   ...
//! ```
//!
//! Next: read `wasm-auth` (a real filter doing API-key auth), then scaffold your own from
//! `examples/filters/filter-template`. Plain HTTP/1.1; temp dir cleaned up on exit.

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
use plecto_host::test_support::{TestSigner, bound_sbom, filter_quickstart_component};
use plecto_server::serve;

const PROXY_ADDR: &str = "127.0.0.1:8080";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    // Sign the filter and bundle it as an offline OCI layout — the same provenance path production
    // uses (a filter is loaded only after its signature + SBOM verify, fail-closed).
    let signer = TestSigner::new()?;
    std::fs::write(base.join("trust.pem"), signer.public_key_pem())?;
    let component = filter_quickstart_component();
    let component_signature = signer.sign(&component)?;
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom)?;
    let artifact = ResolvedArtifact {
        component,
        component_signature,
        sbom,
        sbom_signature,
    };
    let digest = write_layout(&base.join("filters/quickstart"), &artifact)?;

    let upstream = spawn_upstream().await?;
    let manifest_path = base.join("plecto.toml");
    std::fs::write(&manifest_path, manifest_toml(&digest, upstream))?;

    let control = Arc::new(Control::from_manifest_path(&manifest_path)?);

    let proxy_addr = std::env::var("PLECTO_PROXY_ADDR").unwrap_or_else(|_| PROXY_ADDR.to_string());
    let listener = TcpListener::bind(&proxy_addr).await?;
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

/// A tiny in-process upstream: returns a small JSON body so a successful proxy hop is observable.
async fn spawn_upstream() -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let svc = service_fn(|_req: Request<Incoming>| async move {
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(Full::new(Bytes::from("{\"upstream\":\"hello\"}\n")))
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
    format!(
        r#"# Plecto quickstart manifest (generated) — one filter, one route.
[trust]
keys = ["trust.pem"]

[[filter]]
id = "quickstart"
source = "filters/quickstart"
digest = "{digest}"
isolation = "trusted"

[[upstream]]
name = "backend"
addresses = ["{upstream}"]
[upstream.health]
path = "/"

[[route]]
filters = ["quickstart"]
upstream = "backend"
[route.match]
path_prefix = "/"
"#
    )
}

fn print_banner(proxy: SocketAddr) {
    let p = proxy.port();
    println!("\n  Plecto quickstart — a WASM filter on the request path\n");
    println!("  proxy  : http://localhost:{p}   (plain HTTP/1.1)");
    println!("  filter : filter-quickstart (signed + loaded through the production path)\n");
    println!("  Try it (Ctrl-C to stop):\n");
    println!("    curl -i http://localhost:{p}/");
    println!("      -> look for  x-plecto: hello-from-wasm  (added by the sandboxed filter)\n");
    println!(
        "  Next: `--example wasm-auth` (real API-key auth), then examples/filters/filter-template.\n"
    );
}
