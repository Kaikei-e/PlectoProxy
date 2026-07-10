//! `plecto new-filter --lang <lang> <name>` (ADR 000065 decision 4): scaffold a new
//! `plecto:filter` guest crate. Rust is the only implemented language this increment (the ADR's
//! own 90-day-plan phasing) — `go`/`moonbit`/`c`/`js` return a clear "not yet" error rather than
//! silently doing nothing, so the CLI's surface honestly reflects what is built.
//!
//! The Rust template's `Cargo.toml` / `src/lib.rs`, and the `plecto:filter` WIT contract itself,
//! are all embedded at COMPILE time via `include_str!` from this same source tree
//! (`examples/filters/filter-template/` and `wit/world.wit` respectively) — so a released
//! `plecto` binary ships a working scaffold, on the exact contract version that binary's own
//! host runs, without needing this repo checked out at runtime or a network round-trip to a WIT
//! registry (self-vendoring, ADR 000072). The WIT text `new-filter` writes is the same file
//! `plecto-host`'s bindgen resolves, so the CLI can no longer scaffold a different *package
//! version* than the host loads. The guest Rust template (`lib.rs`) is a separate embed and must
//! stay API-compatible with that WIT — a compile smoke test guards that coupling. `wkg`
//! (ADR 000064) remains the distribution channel for filter authors who do NOT use this CLI
//! (polyglot / out-of-tree).

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::dev_key;

const TEMPLATE_CARGO_TOML: &str =
    include_str!("../../../examples/filters/filter-template/Cargo.toml");
const TEMPLATE_LIB_RS: &str = include_str!("../../../examples/filters/filter-template/src/lib.rs");

/// The canonical `plecto:filter` contract text — the same file `plecto-host`'s
/// `wasmtime::component::bindgen!({ path: "../../wit", .. })` resolves.
const FILTER_WIT: &str = include_str!("../../../wit/world.wit");

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

    // The dev-key step can fail after the directory already has files in it (Cargo.toml / lib.rs
    // / wit/) — clean up on any failure so a retry, after the operator fixes the underlying
    // problem, does not immediately trip the `dest.exists()` check above.
    match scaffold(name, project_root, &dest) {
        Ok(signer) => {
            println!("created {}/", dest.display());
            println!("  Cargo.toml, src/lib.rs  — your filter (edit on_request in src/lib.rs)");
            println!(
                "  wit/world.wit           — the plecto:filter contract, vendored into this binary"
            );
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

    write_wit(&dest.join("wit"))?;

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

/// Write the vendored `plecto:filter` contract into the scaffold (self-vendoring, ADR 000072).
/// No network, no subprocess: `FILTER_WIT` is compiled into this binary, so this can never
/// scaffold a contract version other than the one the binary's own host runs.
fn write_wit(dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    std::fs::write(dest.join("world.wit"), FILTER_WIT)
        .with_context(|| format!("write {}/world.wit", dest.display()))
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
    fn a_failed_dev_key_step_leaves_no_partial_directory_for_a_retry() {
        // No network dependency left to fail post-000072, so force the OTHER fallible step
        // (`dev_key::load_or_create_dev_signer`, which runs after Cargo.toml/src/lib.rs/wit/ are
        // already written) by pre-creating its target path as a directory: `fs::read` on it then
        // fails with something other than NotFound, instead of the "generate a fresh key"
        // branch. This exercises the real rollback path, not a mock.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".plecto/dev-key")).unwrap();
        let err = run("rust", "my-filter", dir.path()).unwrap_err();
        assert!(err.to_string().contains("dev key"), "got: {err}");
        assert!(
            !dir.path().join("my-filter").exists(),
            "a failed scaffold must not leave a partial directory behind"
        );
    }

    #[test]
    fn scaffolded_wit_matches_the_canonical_current_contract() {
        // Regression test for the exact defect ADR 000072 fixes: a scaffolded filter must never
        // target a contract version other than the one this binary's own host runs.
        let dir = tempfile::tempdir().unwrap();
        run("rust", "my-filter", dir.path()).unwrap();
        let scaffolded =
            std::fs::read_to_string(dir.path().join("my-filter/wit/world.wit")).unwrap();
        assert_eq!(scaffolded, FILTER_WIT);
        assert!(
            scaffolded.contains("package plecto:filter@0.2.0;"),
            "got: {scaffolded}"
        );
        assert!(scaffolded.contains("value: list<u8>"), "got: {scaffolded}");
    }

    #[test]
    fn scaffolded_rust_filter_compiles_against_vendored_wit() {
        // Guards the residual coupling ADR 000072 leaves explicit: WIT is self-vendored from the
        // host's contract file, but `src/lib.rs` is a separate embed. A contract-shaped breaking
        // change that updates `world.wit` without the guest template must fail here, not at the
        // operator's first `plecto new-filter` + build.
        let dir = tempfile::tempdir().unwrap();
        run("rust", "my-filter", dir.path()).unwrap();
        let filter_dir = dir.path().join("my-filter");
        let target_dir = dir.path().join("cargo-target");
        let output = std::process::Command::new("cargo")
            .current_dir(&filter_dir)
            .args([
                "build",
                "--target",
                "wasm32-unknown-unknown",
                "--target-dir",
            ])
            .arg(&target_dir)
            .output()
            .expect("spawn cargo build for the scaffolded filter");
        assert!(
            output.status.success(),
            "scaffolded filter failed to compile against the vendored WIT\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
