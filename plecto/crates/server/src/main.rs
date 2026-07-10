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
//! - `plecto schema` — print the manifest's JSON Schema (draft-07) for editor completion / CI
//! - `plecto --version` — print the version and exit

use std::path::Path;
use std::sync::Arc;

use plecto_control::Control;
use plecto_server::serve_with_shutdown;
use tokio::net::TcpListener;

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

    const USAGE: &str = "usage: plecto <manifest.toml> [listen_addr] | plecto validate <manifest.toml> | plecto conformance <component.wasm> [--json] | plecto new-filter --lang rust <name> | plecto dev <filter-dir> | plecto schema | plecto --version";
    let mut args = std::env::args().skip(1);
    let manifest = match args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?.as_str() {
        "--version" | "-V" => {
            println!("plecto {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        // The manifest's JSON Schema (ADR 000049), derived from the same serde model `validate`
        // parses with — pipe to a file and point taplo / Even Better TOML at it (`#:schema`).
        "schema" => {
            println!("{}", plecto_control::manifest_json_schema());
            return Ok(());
        }
        // Static manifest validation (the `nginx -t` shape): strict parse + every fail-closed
        // startup check that needs no artifact and mutates nothing, then exit. Plain (non-JSON)
        // output — this is an operator/CI command, not the serving process.
        "validate" => {
            let path = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("usage: plecto validate <manifest.toml>"))?;
            match plecto_control::validate_manifest_path(Path::new(&path)) {
                Ok(outcome) => {
                    println!(
                        "manifest OK: {path} (config version {})",
                        outcome.config_version
                    );
                    for warning in &outcome.warnings {
                        println!("warning {warning}");
                    }
                    return Ok(());
                }
                Err(e) => anyhow::bail!("manifest INVALID: {path}: {e}"),
            }
        }
        // Filter Dev Kit conformance CLI (ADR 000065 decision 3): the CLI surface over the same
        // generic-property battery `plecto dev` runs before every reload. Self-signs with a
        // throwaway key (never `.plecto/dev-key`), so this needs no manifest, no trust setup —
        // just a component.
        "conformance" => {
            let rest: Vec<String> = args.collect();
            let json = rest.iter().any(|a| a == "--json");
            let path = rest
                .iter()
                .find(|a| a.as_str() != "--json")
                .ok_or_else(|| {
                    anyhow::anyhow!("usage: plecto conformance <component.wasm> [--json]")
                })?
                .clone();
            let bytes = std::fs::read(&path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
            let report = plecto_control::run_conformance(&bytes);
            if json {
                let checks: Vec<serde_json::Value> = report
                    .checks
                    .iter()
                    .map(|c| {
                        serde_json::json!({"name": c.name, "passed": c.passed, "detail": c.detail})
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({"conformant": report.is_conformant(), "checks": checks})
                );
            } else {
                for check in &report.checks {
                    let mark = if check.passed { "PASS" } else { "FAIL" };
                    println!("[{mark}] {} — {}", check.name, check.detail);
                }
            }
            if report.is_conformant() {
                return Ok(());
            }
            anyhow::bail!("{path} is not conformant with plecto:filter");
        }
        // Filter Dev Kit scaffold CLI (ADR 000065 decision 4): `plecto new-filter --lang rust
        // <name>` — the `filter-template` directory, CLI-ified, plus the project's dev key and
        // a ready-to-run dev manifest. `--lang`/`<name>` accepted in either order.
        "new-filter" => {
            let rest: Vec<String> = args.collect();
            let usage = || anyhow::anyhow!("usage: plecto new-filter --lang <lang> <name>");
            let lang_idx = rest.iter().position(|a| a == "--lang");
            let lang = lang_idx.and_then(|i| rest.get(i + 1)).ok_or_else(usage)?;
            let lang_value_idx = lang_idx.map(|i| i + 1);
            let name = rest
                .iter()
                .enumerate()
                .find(|(i, a)| a.as_str() != "--lang" && Some(*i) != lang_value_idx)
                .map(|(_, a)| a.as_str())
                .ok_or_else(usage)?;
            let project_root = std::env::current_dir()
                .map_err(|e| anyhow::anyhow!("resolve the current directory: {e}"))?;
            new_filter::run(lang, name, &project_root)?;
            return Ok(());
        }
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
    {
        let control = control.clone();
        std::thread::spawn(move || match plecto_control::SignalReloadSource::sighup() {
            Ok(mut source) => plecto_control::serve_reloads(&control, &mut source),
            Err(e) => {
                tracing::error!(error = %e, "cannot register SIGHUP handler; hot reload disabled")
            }
        });
    }

    let listener = TcpListener::bind(&listen).await?;
    tracing::info!(%listen, version = %control.config_version(), "plecto fast path listening");
    serve_with_shutdown(control, listener, shutdown_signal()).await?;
    tracing::info!("plecto fast path stopped");
    Ok(())
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
