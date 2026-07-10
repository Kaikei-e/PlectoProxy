//! `plecto new-filter --lang <lang> <name>` (ADR 000065 decision 4): scaffold a new
//! `plecto:filter` guest crate. Rust is the only implemented language this increment (the ADR's
//! own 90-day-plan phasing) — `go`/`moonbit`/`c`/`js` return a clear "not yet" error rather than
//! silently doing nothing, so the CLI's surface honestly reflects what is built.
//!
//! The Rust template's `Cargo.toml` / `src/lib.rs` are embedded at COMPILE time via
//! `include_str!` from `examples/filters/filter-template/` — the same file `just
//! sync-template-wit` keeps current — so a released `plecto` binary ships a working scaffold
//! without needing this repo checked out at runtime. The `plecto:filter` WIT contract itself is
//! NOT embedded: it is fetched live via `wkg` (ADR 000064's distribution channel), which is the
//! whole point of publishing it — an external filter author never clones Plecto.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::dev_key;

const TEMPLATE_CARGO_TOML: &str =
    include_str!("../../../examples/filters/filter-template/Cargo.toml");
const TEMPLATE_LIB_RS: &str = include_str!("../../../examples/filters/filter-template/src/lib.rs");

/// wkg's namespace→registry mapping for `plecto:filter` (ADR 000064). Embedded the same way as
/// the Rust template: a released binary needs this to resolve `ghcr.io/kaikei-e/wit/plecto:filter`
/// without the caller having a copy of this repo's `wkg-registry.toml` on disk.
const WKG_REGISTRY_CONFIG: &str = include_str!("../../../wkg-registry.toml");

pub(crate) fn run(lang: &str, name: &str, project_root: &Path) -> Result<()> {
    if lang != "rust" {
        bail!(
            "plecto new-filter --lang {lang} is not implemented yet (only \"rust\" is, in this \
             release) — go/moonbit/c/js scaffolds are tracked follow-up work, not silently \
             skipped. See docs/ADR/000065.md and examples/filters/filter-hello-{lang}/ for the \
             hand-written example in the meantime."
        );
    }
    validate_name(name)?;

    let dest = project_root.join(name);
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }

    // The wkg fetch (network) or dev-key step can fail after the directory already has files in
    // it (e.g. `wkg` not installed yet) — clean up on any failure so a retry, after the operator
    // fixes the underlying problem, does not immediately trip the `dest.exists()` check above.
    match scaffold(name, project_root, &dest) {
        Ok(signer) => {
            println!("created {}/", dest.display());
            println!("  Cargo.toml, src/lib.rs  — your filter (edit on_request in src/lib.rs)");
            println!("  wit/world.wit           — the plecto:filter contract, fetched via wkg");
            println!(
                "  manifest.toml           — a dev manifest, trusting your project's dev key ({})",
                signer.public_key_pem().lines().next().unwrap_or_default()
            );
            println!();
            println!("next: cd {name} && plecto dev .");
            Ok(())
        }
        Err(e) => {
            if let Err(cleanup) = std::fs::remove_dir_all(&dest) {
                tracing::warn!(error = %cleanup, path = %dest.display(),
                    "could not roll back the partial scaffold");
            }
            Err(e)
        }
    }
}

fn scaffold(name: &str, project_root: &Path, dest: &Path) -> Result<plecto_control::DevSigner> {
    let pascal_name = pascal_case(name);
    std::fs::create_dir_all(dest.join("src"))
        .with_context(|| format!("create {}", dest.display()))?;

    let cargo_toml =
        TEMPLATE_CARGO_TOML.replacen("name = \"my-filter\"", &format!("name = \"{name}\""), 1);
    std::fs::write(dest.join("Cargo.toml"), cargo_toml)
        .with_context(|| format!("write {}/Cargo.toml", dest.display()))?;

    let lib_rs = TEMPLATE_LIB_RS
        .replace("MyFilter", &pascal_name)
        .replace("my-filter: on-request", &format!("{name}: on-request"))
        .replace("blocked by my-filter", &format!("blocked by {name}"));
    std::fs::write(dest.join("src/lib.rs"), lib_rs)
        .with_context(|| format!("write {}/src/lib.rs", dest.display()))?;

    fetch_wit(&dest.join("wit"))?;

    let signer = dev_key::load_or_create_dev_signer(project_root)?;
    // `dest` is always exactly `project_root.join(name)` (checked by the caller), one level
    // below `project_root` — so the relative path from `dest` back to the project's `.plecto/`
    // is always `..`, never a general path-diff problem.
    let dev_key_rel = Path::new("..").join(dev_key::public_key_path(Path::new("")));
    std::fs::write(
        dest.join("manifest.toml"),
        dev_manifest_toml(name, &dev_key_rel),
    )
    .with_context(|| format!("write {}/manifest.toml", dest.display()))?;

    Ok(signer)
}

/// `wkg get plecto:filter@0.1.0 -o <dest> --format wit` (ADR 000064's published channel — the
/// same command `wkg-registry.toml`'s own doc comment tells a filter author to run). Shelled
/// out, not embedded via `wasm-pkg-core`: the research behind ADR 000065 could not confirm that
/// library crate's stable public API, so the CLI subprocess (already proven in
/// `examples/filters/filter-hello-go/build.sh` and the release workflow) is the lower-risk path.
fn fetch_wit(dest: &Path) -> Result<()> {
    let registry_config =
        std::env::temp_dir().join(format!("plecto-wkg-registry-{}.toml", std::process::id()));
    std::fs::write(&registry_config, WKG_REGISTRY_CONFIG)
        .with_context(|| format!("write {}", registry_config.display()))?;
    let result = Command::new("wkg")
        .args(["get", "plecto:filter@0.1.0", "--config"])
        .arg(&registry_config)
        .arg("-o")
        .arg(dest)
        .args(["--format", "wit"])
        .status();
    if let Err(cleanup) = std::fs::remove_file(&registry_config) {
        tracing::warn!(error = %cleanup, path = %registry_config.display(),
            "could not remove the temporary wkg registry config");
    }
    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => bail!(
            "wkg get plecto:filter@0.1.0 exited with {status}. Check your network connection to \
             ghcr.io — the plecto:filter WIT contract is published there (ADR 000064)."
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "`wkg` is not on PATH. Install it from \
             https://github.com/bytecodealliance/wasm-pkg-tools/releases (wasm-pkg-tools) — \
             `plecto new-filter` fetches the plecto:filter WIT contract with it (ADR 000064)."
        ),
        Err(e) => Err(anyhow!("run wkg: {e}")),
    }
}

fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if valid {
        Ok(())
    } else {
        bail!(
            "\"{name}\" is not a valid filter name — use lowercase ascii letters, digits, and \
             hyphens, not starting or ending with a hyphen (it becomes both a Cargo package name \
             and a manifest filter id)"
        )
    }
}

fn pascal_case(kebab: &str) -> String {
    let mut out = String::with_capacity(kebab.len() + 6);
    for word in kebab.split('-') {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    // Don't double up when the name already ends in "filter" (e.g. "my-auth-filter") — only
    // append the suffix when it would actually add information.
    if !out.ends_with("Filter") {
        out.push_str("Filter");
    }
    out
}

fn dev_manifest_toml(name: &str, dev_key_rel: &Path) -> String {
    format!(
        r#"# Generated by `plecto new-filter` (ADR 000065): a dev manifest trusting your project's
# .plecto/dev-key. `plecto dev` rewrites [[filter]].digest below on every rebuild — the rest is
# yours to edit. See docs/writing-a-filter.md for the full field reference.

[trust]
keys = ["{dev_key_rel}"]

[[filter]]
id = "{name}"
source = "artifacts/{name}"   # plecto dev writes the built component's OCI layout here
digest = "sha256:PENDING"     # plecto dev fills this in after the first successful build
isolation = "untrusted"

[[upstream]]
name = "app"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"

[[route]]
filters = ["{name}"]
upstream = "app"
[route.match]
path_prefix = "/"
"#,
        name = name,
        dev_key_rel = dev_key_rel.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pascal_case_joins_hyphenated_words_and_suffixes_filter() {
        assert_eq!(pascal_case("my-auth-filter"), "MyAuthFilter");
        assert_eq!(pascal_case("hello"), "HelloFilter");
    }

    #[test]
    fn validate_name_rejects_uppercase_and_leading_hyphen() {
        assert!(validate_name("my-filter").is_ok());
        assert!(validate_name("My-Filter").is_err());
        assert!(validate_name("-leading").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn unsupported_lang_fails_clearly_instead_of_silently_succeeding() {
        let dir = tempfile::tempdir().unwrap();
        let err = run("go", "my-filter", dir.path()).unwrap_err();
        assert!(err.to_string().contains("not implemented yet"));
        assert!(!dir.path().join("my-filter").exists());
    }

    #[test]
    fn a_failed_wkg_fetch_leaves_no_partial_directory_for_a_retry() {
        // Only meaningful where `wkg` is absent (this sandbox; CI's polyglot-guest-go job
        // installs it, so skip there rather than attempt a flaky real network call). Where it
        // does run, this exercises the real "wkg missing" failure path, not a mock:
        // Cargo.toml/src/lib.rs get written, then `fetch_wit` fails, and `run` must roll the
        // whole directory back so a retry (after `wkg` is installed) does not immediately trip
        // the `dest.exists()` guard.
        if Command::new("wkg").arg("--version").output().is_ok() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let err = run("rust", "my-filter", dir.path()).unwrap_err();
        assert!(err.to_string().contains("wkg"), "got: {err}");
        assert!(
            !dir.path().join("my-filter").exists(),
            "a failed scaffold must not leave a partial directory behind"
        );
    }
}
