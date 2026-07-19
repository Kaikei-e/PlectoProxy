//! plecto — the fast-path binary (ADR 000013). Loads a declarative manifest, builds the control
//! plane (filters, routes, upstreams), and serves the fast path: HTTP/1.1 and HTTP/2 over TCP,
//! plus HTTP/3 over QUIC on the same port (UDP) when `[[tls]]` is configured. SIGHUP re-reads the
//! manifest and swaps it in without downtime (ADR 000008 / 000039); SIGTERM / SIGINT stop
//! accepting, drain in-flight connections, and exit cleanly (ADR 000039).
//!
//! Usage:
//! - `plecto <manifest.toml> [listen_addr]` — serve (listen defaults to `127.0.0.1:8080`)
//! - `plecto validate <manifest.toml>` — statically validate a manifest and exit (the `nginx -t`
//!   shape: strict parse + every fail-closed startup check that needs no artifact; for CI and
//!   pre-SIGHUP checks)
//! - `plecto conformance <component.wasm> [--json]` — Filter Dev Kit (ADR 000065): run the
//!   generic `plecto:filter` conformance battery against a component and exit non-zero unless
//!   every check passes. `--json` prints a machine-readable report instead of plain text.
//! - `plecto new-filter --lang rust <name>` — Filter Dev Kit (ADR 000065): scaffold a new
//!   `plecto:filter` guest crate + a dev manifest trusting your project's dev key.
//! - `plecto dev <filter-dir>` — Filter Dev Kit (ADR 000065): watch, componentize, gate on
//!   conformance, sign with the dev key, and reload in a loop (unix only — SIGHUP-based).
//! - `plecto healthz [--live] [--admin-addr <host:port>] [<manifest.toml>]` — probe the admin
//!   endpoint's `/readyz` (or `/healthz` with `--live`) and exit 0 on a 2xx response, 1
//!   otherwise. The self-probe a shell-less (distroless) image needs to drive a Docker/Compose
//!   `healthcheck:` (field report §3.6) — exit code 2 is never produced (Docker reserves it).
//! - `plecto schema` — print the manifest's JSON Schema (draft-07) for editor completion / CI
//! - `plecto --version` — print the version and exit

use std::path::Path;
use std::sync::Arc;

use plecto_control::Control;
use plecto_server::serve_with_shutdown;
use tokio::net::TcpListener;

mod cli;
mod dev_cmd;
mod dev_key;
mod new_filter;

fn main() -> anyhow::Result<()> {
    // Cap glibc malloc arenas BEFORE the runtime spawns worker threads (M_ARENA_MAX only gates new
    // arenas, so it must precede them) — a manual runtime build instead of `#[tokio::main]` is what
    // gives us that ordering. Bounds RSS on many-core hosts (docs/servey body-tax).
    plecto_server::cap_malloc_arenas();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    // JSON structured logging for the binary (Stage A observability, ADR 000009): the access log
    // (`plecto::access`) and the host diagnostics render as machine-parseable lines. `try_init` is
    // idempotent — a failure means a global subscriber is already installed (e.g. a test harness),
    // which we intentionally keep.
    let _logging = tracing_subscriber::fmt()
        .json()
        .with_target(true)
        .try_init();

    const USAGE: &str = "usage: plecto <manifest.toml> [listen_addr] | plecto validate [--resolve] <manifest.toml> | plecto conformance <component.wasm> [--json] | plecto package <component.wasm> --key <pkcs8.pem> --out <layout-dir> [--sbom <statement.json>] | plecto new-filter --lang rust <name> | plecto dev <filter-dir> | plecto healthz [--live] [--admin-addr <host:port>] [<manifest.toml>] | plecto schema | plecto --version";
    let mut args = std::env::args().skip(1);
    let manifest = match args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?.as_str() {
        "--version" | "-V" => {
            println!(
                "plecto {} (profile: {})",
                env!("CARGO_PKG_VERSION"),
                capability_profile()
            );
            return Ok(());
        }
        // The manifest's JSON Schema (ADR 000049), derived from the same serde model `validate`
        // parses with — pipe to a file and point taplo / Even Better TOML at it (`#:schema`).
        "schema" => {
            println!("{}", plecto_control::manifest_json_schema()?);
            return Ok(());
        }
        // Static manifest validation (the `nginx -t` shape): strict parse + every fail-closed
        // startup check that needs no artifact and mutates nothing, then exit. `--resolve`
        // additionally resolves each filter's OCI layout and runs the loader's provenance gate
        // (digest pin + signatures + SBOM binding, field report §3.5) — still without serving,
        // without wasmtime, without state. Plain (non-JSON) output — this is an operator/CI
        // command, not the serving process.
        "validate" => return cli::validate(args.collect()),
        // Filter Dev Kit conformance CLI (ADR 000065 decision 3): the CLI surface over the same
        // generic-property battery `plecto dev` runs before every reload. Self-signs with a
        // throwaway key (never `.plecto/dev-key`), so this needs no manifest, no trust setup —
        // just a component.
        "conformance" => return cli::conformance(args.collect()),
        // Self-probe for shell-less containers (field report §3.6): a Compose `healthcheck:`
        // exec-array can point at the binary already in the image instead of a curl that a
        // distroless image does not have. Readiness by default — `service_healthy` is a start
        // gate, which is readiness semantics; `--live` for restart-supervisor liveness.
        "healthz" => return cli::healthz(args.collect()),
        // One-shot CI packaging (field report §3.1): componentize is the builder's job, this
        // takes a built component + an operator key and emits the signed offline OCI
        // image-layout the loader requires, printing ONLY the pinned digest to stdout. The
        // production-shaped sibling of `plecto dev`'s inner loop — no watch, no dev key, no
        // manifest rewrite.
        "package" => return cli::package(args.collect()),
        // Filter Dev Kit scaffold CLI (ADR 000065 decision 4): `plecto new-filter --lang rust
        // <name>` — the `filter-template` directory, CLI-ified, plus the project's dev key and
        // a ready-to-run dev manifest. `--lang`/`<name>` accepted in either order.
        "new-filter" => return cli::new_filter(args.collect()),
        // Filter Dev Kit inner loop (ADR 000065 decision 2): watch → componentize → conformance
        // gate → dev-key sign → reload, in-process, reusing the exact SIGHUP reload plumbing
        // `plecto serve` uses. unix-only, like the rest of the SIGHUP reload mechanism.
        #[cfg(unix)]
        "dev" => {
            let filter_dir = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("usage: plecto dev <filter-dir>"))?;
            let project_root = std::env::current_dir()
                .map_err(|e| anyhow::anyhow!("resolve the current directory: {e}"))?;
            dev_cmd::run(Path::new(&filter_dir), &project_root).await?;
            return Ok(());
        }
        #[cfg(not(unix))]
        "dev" => {
            anyhow::bail!("plecto dev requires unix (it reloads via SIGHUP, like plecto serve)")
        }
        manifest => manifest.to_string(),
    };
    let listen_arg = args.next();

    // A load failure at startup is where a newcomer first meets the signature gate — render
    // the registered PLECTO-E diagnostic (four-part: code/cause/suggestion/docs, ADR 000065
    // decision 5) alongside the error instead of the bare thiserror message.
    let control = Arc::new(
        Control::from_manifest_path(Path::new(&manifest))
            .map_err(|e| anyhow::anyhow!(plecto_control::diagnosed_message(&e)))?,
    );

    // Bind precedence: the explicit CLI arg (operator override) > the manifest's `[listen] addr`
    // (the static single source, field report §3.2) > the loopback default.
    let listen = listen_arg
        .or_else(|| control.listen_addr().map(str::to_string))
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());

    // The SIGHUP reload loop (ADR 000008 / 000039): `from_manifest_path` remembers the path, so
    // each SIGHUP re-reads the on-disk manifest and swaps it in atomically, fail-closed (a bad
    // edit keeps the current set live). `serve_reloads` is a blocking loop, so it runs on its own
    // thread beside the async data plane. Signals are a unix concept; elsewhere the config is
    // static for the process lifetime.
    #[cfg(unix)]
    spawn_sighup_reload(control.clone());

    // Raise the soft RLIMIT_NOFILE to the hard limit (docs/servey production hardening) — only on
    // the actual serve path, not the utility subcommands above (`schema` prints bare JSON to
    // stdout; a stray log line here would corrupt it). Unlike `cap_malloc_arenas` this has no
    // thread-creation ordering requirement, but it does need the subscriber above so its warning
    // (if the hard limit is itself too low) is actually visible.
    plecto_server::raise_nofile_limit();

    let listener = TcpListener::bind(&listen).await?;
    tracing::info!(%listen, version = %control.config_version(), "plecto fast path listening");
    serve_with_shutdown(control, listener, shutdown_signal()).await?;
    tracing::info!("plecto fast path stopped");
    Ok(())
}

/// The named runtime capability profile this binary was compiled as (ADR 000079). The two
/// shipped feature sets get their release-profile name; any other set is reported feature by
/// feature, so a source build with a partial set never masquerades as a named profile.
/// Compile-time inclusion is not a runtime grant — capabilities are still lent per filter by
/// the manifest (deny-by-default, ADR 000036 / 000060).
fn capability_profile() -> String {
    let compiled: Vec<&str> = [
        (cfg!(feature = "outbound-http"), "outbound-http"),
        (cfg!(feature = "outbound-tcp"), "outbound-tcp"),
        (cfg!(feature = "fat-guest"), "fat-guest"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    match compiled.as_slice() {
        [] => "minimal".to_owned(),
        ["outbound-http", "outbound-tcp", "fat-guest"] => {
            "capabilities (outbound-http, outbound-tcp, fat-guest)".to_owned()
        }
        partial => format!("custom ({})", partial.join(", ")),
    }
}

/// Spawn the SIGHUP reload loop on its own thread (ADR 000008 / 000039): `serve_reloads` is a
/// blocking loop, so it runs beside the async data plane. Shared by `plecto serve` and
/// `plecto dev` (which reuses the exact same reload plumbing).
#[cfg(unix)]
pub(crate) fn spawn_sighup_reload(control: Arc<plecto_control::Control>) {
    std::thread::spawn(move || match plecto_control::SignalReloadSource::sighup() {
        Ok(mut source) => plecto_control::serve_reloads(&control, &mut source),
        Err(e) => {
            tracing::error!(error = %e, "cannot register SIGHUP handler; hot reload disabled")
        }
    });
}

/// Resolves on the operator's "stop serving" signal — SIGTERM (process supervisors) or SIGINT
/// (ctrl-c) — triggering graceful shutdown (ADR 000039 / 000059): `/readyz` flips not-ready,
/// the readiness grace elapses, accept stops, in-flight connections drain up to the drain
/// window (`[listen.drain]`, default 30 s), then the process exits 0.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot register SIGTERM handler; ctrl-c only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
