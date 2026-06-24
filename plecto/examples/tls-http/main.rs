//! Plecto demo — **TLS termination across HTTP/1.1, HTTP/2 and HTTP/3** (ADR 000014/15/16).
//!
//! Run it:  `cargo run -p plecto-server --example tls-http`
//!
//! With `[[tls]]` present, a single port carries **all three HTTP versions**: HTTP/1.1 and HTTP/2
//! over TCP (ALPN-negotiated — `h2` then `http/1.1`, no h2c), and HTTP/3 over QUIC on the *same
//! port number* (UDP). TCP responses advertise h3 via `Alt-Svc`, so an h3-capable client upgrades
//! itself on the next request. The fast path terminates TLS (rustls) and routes `/api/*` to the
//! upstream through a signed pass-through filter.
//!
//! It wires the **production load path** (nothing is faked): it signs the bundled `filter-hello`
//! component, packages it as an offline OCI image-layout, generates a self-signed cert, writes a
//! manifest, and builds the control plane via `Control::from_manifest_path` — the same entrypoint
//! the `plecto` binary uses. The throwaway test signer (fresh key per run) is a demo convenience,
//! not production provenance. Everything lives in a temp dir, cleaned up on exit.

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

/// The same port number is bound on TCP (HTTP/1.1 + HTTP/2) and UDP (HTTP/3 / QUIC).
const PROXY_ADDR: &str = "127.0.0.1:8443";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let base = dir.path();

    // trust root + signer, then sign filter-hello and bundle it as an offline OCI layout.
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

    // a fresh self-signed cert for `localhost`, shared by the TCP TLS terminator and QUIC listener.
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    std::fs::create_dir_all(base.join("tls"))?;
    std::fs::write(base.join("tls/cert.pem"), cert.cert.pem())?;
    std::fs::write(base.join("tls/key.pem"), cert.key_pair.serialize_pem())?;

    let upstream = spawn_upstream().await?;
    let manifest_path = base.join("plecto.toml");
    std::fs::write(&manifest_path, manifest_toml(&digest, upstream))?;

    // the real ops entrypoint: reads keys + OCI layout + certs, verifies signature + SBOM, loads
    // the filter, builds the TLS + QUIC configs — all fail-closed.
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

/// A tiny echo upstream: returns the path it received so the prefix strip is visible.
async fn echo(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    Ok(Response::builder()
        .status(200)
        .header("x-from", "tls-http-upstream")
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
        r#"# Plecto demo manifest (generated). With [[tls]] the fast path serves HTTP/1.1 + HTTP/2
# over TCP and HTTP/3 over QUIC on the same port; TCP responses advertise h3 via Alt-Svc.
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
path_prefix = "/api"
filters = ["hello"]
upstream = "app"
strip_prefix = "/api"

[[tls]]
cert_path = "tls/cert.pem"
key_path = "tls/key.pem"
"#
    )
}

fn print_banner(proxy: SocketAddr) {
    let p = proxy.port();
    println!("\n  Plecto demo — TLS termination across HTTP/1.1, HTTP/2 and HTTP/3\n");
    println!("  proxy : https://localhost:{p}   (self-signed cert — use curl -k)");
    println!("          HTTP/1.1 + HTTP/2 over TCP, HTTP/3 over QUIC on :{p}/udp (same port)");
    println!("\n  Try it (Ctrl-C to stop):\n");
    println!("    curl -k https://localhost:{p}/api/hello");
    println!("        -> HTTP/1.1 (or h2 if curl prefers it); /api stripped; forwarded");
    println!("    curl -k --http2 https://localhost:{p}/api/hello");
    println!("        -> ALPN negotiates h2 (the server advertises h2 then http/1.1; no h2c)");
    println!("    curl -k --http3 https://localhost:{p}/api/hello   # curl built with HTTP/3");
    println!("        -> same route over HTTP/3 (QUIC), 0-RTT disabled");
    println!("    curl -k -i https://localhost:{p}/api/hello | grep -i alt-svc");
    println!("        -> a TCP response advertises  alt-svc: h3=\":{p}\"; ma=86400");
    println!("    curl -k https://localhost:{p}/nope   # no matching route -> 404\n");
}
