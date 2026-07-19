//! CLI subcommand handlers for the `plecto` binary (Filter Dev Kit, ADR 000065): everything a
//! subcommand needs beyond "parse args and dispatch" lives here, so `main::run` stays a thin
//! dispatcher (bp-rust §2). Operator-facing output goes to stdout via `println!` by design —
//! these are commands, not data-plane logging.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

/// Probe deadline (connect, write, and read each). Localhost admin answers in microseconds;
/// this only bounds the hang case, and stays under Docker's 30 s healthcheck timeout while a
/// Kubernetes exec probe (default `timeoutSeconds: 1`) would kill a hanging probe first anyway.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// `plecto validate [--resolve] <manifest.toml>` — static manifest validation (the `nginx -t`
/// shape): strict parse + every fail-closed startup check that needs no artifact and mutates
/// nothing. `--resolve` additionally resolves each `[[filter]]`'s OCI layout and runs the
/// loader's provenance gate (digest pin + trusted signatures + SBOM binding) — the CI
/// pre-flight that pairs with `plecto package` (field report §3.5) — still with no serving,
/// no wasmtime, no state.
pub(crate) fn validate(rest: Vec<String>) -> anyhow::Result<()> {
    let resolve = rest.iter().any(|a| a == "--resolve");
    let path = rest
        .iter()
        .find(|a| a.as_str() != "--resolve")
        .ok_or_else(|| anyhow::anyhow!("usage: plecto validate [--resolve] <manifest.toml>"))?
        .clone();
    match plecto_control::validate_manifest_path(Path::new(&path)) {
        Ok(outcome) => {
            println!(
                "manifest OK: {path} (config version {})",
                outcome.config_version
            );
            for warning in &outcome.warnings {
                println!("warning {warning}");
            }
        }
        Err(e) => anyhow::bail!("manifest INVALID: {path}: {e}"),
    }
    if !resolve {
        return Ok(());
    }
    match plecto_control::resolve_manifest_path(Path::new(&path)) {
        Ok(checks) => {
            for check in &checks {
                println!(
                    "filter {} OK: artifact verified ({})",
                    check.id, check.digest
                );
            }
            Ok(())
        }
        Err(e) => anyhow::bail!("artifact INVALID: {path}: {e}"),
    }
}

/// `plecto conformance <component.wasm> [--json]` — run the generic `plecto:filter` conformance
/// battery against a component; non-zero exit unless every check passes.
pub(crate) fn conformance(rest: Vec<String>) -> anyhow::Result<()> {
    let json = rest.iter().any(|a| a == "--json");
    let path = rest
        .iter()
        .find(|a| a.as_str() != "--json")
        .ok_or_else(|| anyhow::anyhow!("usage: plecto conformance <component.wasm> [--json]"))?
        .clone();
    let bytes = std::fs::read(&path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
    let report = plecto_control::run_conformance(&bytes);
    if json {
        let checks: Vec<serde_json::Value> = report
            .checks
            .iter()
            .map(|c| serde_json::json!({"name": c.name, "passed": c.passed, "detail": c.detail}))
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

/// `plecto package <component.wasm> --key <pkcs8.pem> --out <layout-dir> [--sbom
/// <statement.json>]` — one-shot CI packaging (field report §3.1): conformance-gate a built
/// component, sign it and its SBOM with the operator's key, write the signed offline OCI
/// image-layout the loader requires, and print ONLY the pinned image-manifest digest to
/// stdout (machine output on stdout, diagnostics on stderr — `DIGEST=$(plecto package …)`).
/// Unlike `plecto dev` this touches no manifest, watches nothing, and never generates a key.
/// The default SBOM is the minimal bound in-toto statement (`bound_sbom`); `--sbom` supplies
/// a replacement statement, whose subject binding to THIS component stays the supplier's
/// responsibility — the loader (and `validate --resolve`) rejects an unbound one.
pub(crate) fn package(rest: Vec<String>) -> anyhow::Result<()> {
    let usage = || {
        anyhow::anyhow!(
            "usage: plecto package <component.wasm> --key <pkcs8.pem> --out <layout-dir> \
             [--sbom <statement.json>]"
        )
    };
    let mut component_path: Option<String> = None;
    let mut key_path: Option<String> = None;
    let mut out_dir: Option<String> = None;
    let mut sbom_path: Option<String> = None;
    let mut args = rest.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--key" => key_path = Some(args.next().ok_or_else(usage)?),
            "--out" => out_dir = Some(args.next().ok_or_else(usage)?),
            "--sbom" => sbom_path = Some(args.next().ok_or_else(usage)?),
            _ if component_path.is_none() && !arg.starts_with('-') => component_path = Some(arg),
            _ => return Err(usage()),
        }
    }
    let component_path = component_path.ok_or_else(usage)?;
    let key_path = key_path.ok_or_else(usage)?;
    let out_dir = out_dir.ok_or_else(usage)?;

    let component = std::fs::read(&component_path)
        .map_err(|e| anyhow::anyhow!("read {component_path}: {e}"))?;
    // Conformance gates packaging exactly as it gates `plecto dev`'s reloads: a non-conformant
    // component must never become a signed artifact. Check details go to stderr — stdout stays
    // reserved for the digest.
    let report = plecto_control::run_conformance(&component);
    if !report.is_conformant() {
        for check in report.checks.iter().filter(|c| !c.passed) {
            eprintln!("[FAIL] {} — {}", check.name, check.detail);
        }
        anyhow::bail!("{component_path} is not conformant with plecto:filter — nothing packaged");
    }

    let key_pem = zeroize::Zeroizing::new(
        std::fs::read(&key_path).map_err(|e| anyhow::anyhow!("read {key_path}: {e}"))?,
    );
    let signer = plecto_control::PemSigner::from_private_key_pem(&key_pem)
        .map_err(|e| anyhow::anyhow!("load signing key {key_path}: {e}"))?;
    let sbom = match &sbom_path {
        Some(path) => std::fs::read(path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?,
        None => plecto_control::bound_sbom(&component),
    };
    let component_signature = signer.sign(&component)?;
    let sbom_signature = signer.sign(&sbom)?;
    let digest = plecto_control::oci::write_layout(
        Path::new(&out_dir),
        &plecto_control::ResolvedArtifact {
            component,
            component_signature,
            sbom,
            sbom_signature,
        },
    )
    .map_err(|e| anyhow::anyhow!("write OCI layout {out_dir}: {e}"))?;
    println!("{digest}");
    Ok(())
}

/// `plecto healthz [--live] [--admin-addr <host:port>] [<manifest.toml>]` — probe the admin
/// endpoint and exit 0 on a 2xx response, 1 on anything else (refused, timeout, non-2xx).
/// Readiness (`/readyz`) by default — what a Compose `service_healthy` start gate means —
/// `--live` probes liveness (`/healthz`) for restart supervisors. The addr comes from
/// `--admin-addr` or the manifest's `[observability] admin_addr`, so a Compose `healthcheck:`
/// needs no second copy of the address. Exit code 2 is never produced (Docker reserves it);
/// `anyhow` errors from `main` exit 1.
pub(crate) fn healthz(rest: Vec<String>) -> anyhow::Result<()> {
    let usage = || {
        anyhow::anyhow!(
            "usage: plecto healthz [--live] [--admin-addr <host:port>] [<manifest.toml>]"
        )
    };
    let mut live = false;
    let mut admin_addr: Option<String> = None;
    let mut manifest: Option<String> = None;
    let mut args = rest.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--live" => live = true,
            "--admin-addr" => admin_addr = Some(args.next().ok_or_else(usage)?),
            _ if manifest.is_none() && !arg.starts_with('-') => manifest = Some(arg),
            _ => return Err(usage()),
        }
    }
    let addr = match admin_addr {
        Some(addr) => addr,
        None => {
            let path = manifest.ok_or_else(usage)?;
            let raw =
                std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
            let parsed = plecto_control::Manifest::from_toml(&raw)
                .map_err(|e| anyhow::anyhow!("parse {path}: {e}"))?;
            parsed.observability.admin_addr.ok_or_else(|| {
                anyhow::anyhow!(
                    "{path} sets no [observability] admin_addr — the admin endpoint is off, \
                     nothing to probe"
                )
            })?
        }
    };
    let probe_path = if live { "/healthz" } else { "/readyz" };
    let status = probe(&addr, probe_path)
        .map_err(|e| anyhow::anyhow!("unhealthy: GET {addr}{probe_path}: {e}"))?;
    // 2xx = healthy, mirroring the HTTP-probe success convention. One short line of output —
    // `docker inspect` captures the first 4 KiB of probe output into the health log.
    if (200..300).contains(&status) {
        println!("healthy: GET {probe_path} -> {status}");
        return Ok(());
    }
    anyhow::bail!("unhealthy: GET {probe_path} -> {status}")
}

/// One bounded HTTP/1.1 GET over a plain TCP stream, returning the response status code. A
/// full HTTP client buys nothing for a localhost status-line probe — the read stops at the
/// header terminator or the buffer cap, whichever first.
fn probe(addr: &str, path: &str) -> anyhow::Result<u16> {
    let sock = addr
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("resolve {addr}: {e}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("resolve {addr}: no address"))?;
    let mut stream = TcpStream::connect_timeout(&sock, PROBE_TIMEOUT)?;
    stream.set_read_timeout(Some(PROBE_TIMEOUT))?;
    stream.set_write_timeout(Some(PROBE_TIMEOUT))?;
    stream.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    let mut buf = [0u8; 512];
    let mut filled = 0;
    while filled < buf.len() {
        let n = stream.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
        if buf[..filled].windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }
    let head = str::from_utf8(&buf[..filled]).unwrap_or_default();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow::anyhow!("malformed response status line"))?;
    Ok(status)
}

/// `plecto new-filter --lang <lang> <name>` — scaffold a new `plecto:filter` guest crate + a dev
/// manifest trusting the project's dev key. `--lang`/`<name>` accepted in either order.
pub(crate) fn new_filter(rest: Vec<String>) -> anyhow::Result<()> {
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
    crate::new_filter::run(lang, name, &project_root)
}
