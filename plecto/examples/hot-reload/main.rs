//! Plecto demo — **zero-downtime reload** (ADR 000008): edit the manifest, send SIGHUP, watch the
//! change take effect without dropping connections.
//!
//! Run it:  `cargo run -p plecto-server --example hot-reload`
//!
//! The proxy serves a `/api/*` route whose upstream echoes the path it received. A background
//! thread runs `serve_reloads`, which re-reads the on-disk manifest on every **SIGHUP** and swaps
//! the active config **atomically** (ArcSwap): new requests see the new config, in-flight ones keep
//! the old. A reload is reconciled by the manifest's semantic content hash — an unchanged edit is a
//! no-op, and a *broken* edit is **fail-closed** (the running config stays live, the proxy never
//! goes down). Edit `strip_prefix` in the printed manifest and SIGHUP to see the upstream's echoed
//! path change live.
//!
//! Plain HTTP/1.1; the production load path (sign + OCI + verify) still runs. Temp dir, cleaned up
//! on exit. SIGHUP is a Unix concept — the reload trigger is wired only there.

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
use plecto_host::test_support::{TestSigner, bound_sbom, filter_hello_component};
use plecto_server::serve;

const PROXY_ADDR: &str = "127.0.0.1:8082";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    let signer = TestSigner::new()?;
    std::fs::write(base.join("trust.pem"), signer.public_key_pem())?;
    let component = filter_hello_component();
    let component_signature = signer.sign(&component)?;
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom)?;
    let digest = write_layout(
        &base.join("filters/hello"),
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

    // `from_manifest_path` REMEMBERS the path, so `reload_from_disk` (driven by SIGHUP) re-reads it.
    let control = Arc::new(Control::from_manifest_path(&manifest_path)?);

    // The SIGHUP reload loop (Unix). It is a blocking loop, so it runs on its own thread next to the
    // async data plane; on each SIGHUP it re-reads the manifest and reloads fail-closed.
    #[cfg(unix)]
    {
        let control = control.clone();
        std::thread::spawn(move || match plecto_control::SignalReloadSource::sighup() {
            Ok(mut source) => plecto_control::serve_reloads(&control, &mut source),
            Err(e) => eprintln!("could not register SIGHUP handler: {e}"),
        });
    }
    #[cfg(not(unix))]
    eprintln!("note: SIGHUP reload is Unix-only; this platform serves a static config.");

    let listener = TcpListener::bind(PROXY_ADDR).await?;
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

/// The echo upstream: returns the path it received, so a reloaded `strip_prefix` is observable.
async fn echo(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    Ok(Response::builder()
        .status(200)
        .header("x-from", "hot-reload-upstream")
        .body(Full::new(Bytes::from(format!(
            "upstream received: {path}\n"
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
        r#"# Plecto demo manifest (generated). Edit me, then `kill -HUP <pid>` to reload live.
[trust]
keys = ["trust.pem"]

[[filter]]
id = "hello"
source = "filters/hello"
digest = "{digest}"
isolation = "trusted"

[[upstream]]
name = "app"
addresses = ["{upstream}"]
[upstream.health]
path = "/"

[[route]]
filters = ["hello"]
upstream = "app"
strip_prefix = "/api"      # <- try changing this to "/" and SIGHUP; watch the echoed path change
[route.match]
path_prefix = "/api"
"#
    )
}

fn print_banner(proxy: SocketAddr, manifest: &std::path::Path) {
    let p = proxy.port();
    let pid = std::process::id();
    println!("\n  Plecto demo — zero-downtime reload (SIGHUP)\n");
    println!("  proxy    : http://localhost:{p}   (plain HTTP/1.1)");
    println!("  pid      : {pid}");
    println!("  manifest : {}", manifest.display());
    println!("\n  Try it (Ctrl-C to stop):\n");
    println!("    # 1) baseline — /api is stripped, so the upstream sees /hello:");
    println!("    curl -s http://localhost:{p}/api/hello        # upstream received: /hello");
    println!();
    println!(
        "    # 2) edit the manifest: change  strip_prefix = \"/api\"  to  strip_prefix = \"/\""
    );
    println!("    #    (any editor; the path is printed above), then reload with SIGHUP:");
    println!("    kill -HUP {pid}");
    println!();
    println!("    # 3) same request — the upstream now sees the full path, live, no restart:");
    println!("    curl -s http://localhost:{p}/api/hello        # upstream received: /api/hello");
    println!();
    println!("    # A broken edit (bad TOML / unknown filter) is fail-closed: the reload is");
    println!("    # rejected and the running config keeps serving. The proxy never goes down.\n");
}
