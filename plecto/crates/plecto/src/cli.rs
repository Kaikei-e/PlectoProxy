//! CLI subcommand handlers for the `plecto` binary (Filter Dev Kit, ADR 000065): everything a
//! subcommand needs beyond "parse args and dispatch" lives here, so `main::run` stays a thin
//! dispatcher (bp-rust §2). Operator-facing output goes to stdout via `println!` by design —
//! these are commands, not data-plane logging.

use std::path::Path;

/// `plecto validate <manifest.toml>` — static manifest validation (the `nginx -t` shape):
/// strict parse + every fail-closed startup check that needs no artifact and mutates nothing.
pub(crate) fn validate(path: &str) -> anyhow::Result<()> {
    match plecto_control::validate_manifest_path(Path::new(path)) {
        Ok(outcome) => {
            println!(
                "manifest OK: {path} (config version {})",
                outcome.config_version
            );
            for warning in &outcome.warnings {
                println!("warning {warning}");
            }
            Ok(())
        }
        Err(e) => anyhow::bail!("manifest INVALID: {path}: {e}"),
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
