//! `plecto dev <filter-dir>` (ADR 000065 decision 2): the Filter Dev Kit inner loop — watch,
//! componentize, gate on conformance, sign with the project's persistent dev key, and reload —
//! running the SAME server process and the SAME `ReloadSource`/SIGHUP plumbing `plecto serve`
//! uses. The signature-verification code path is never weakened (P5): only the trust root
//! (a dev key instead of a production one) differs between `plecto dev` and a real deploy.
//!
//! Conformance gates the reload, not just the build: a non-conformant rebuild is reported and
//! discarded WITHOUT touching the manifest or the OCI layout, so the running gateway keeps
//! serving the last good build and a real (operator-sent) SIGHUP can never pick up a broken one.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use notify_debouncer_mini::{DebounceEventResult, new_debouncer, notify::RecursiveMode};
use plecto_control::{Control, DevSigner, ResolvedArtifact};
use plecto_server::serve_with_shutdown;
use tokio::net::TcpListener;

use crate::dev_key;

/// How long to wait after the last filesystem event before rebuilding (ADR 000065 research:
/// notify-debouncer-mini's own recommended shape for a single-directory dev watch).
const DEBOUNCE: Duration = Duration::from_millis(300);

pub(crate) async fn run(filter_dir: &Path, project_root: &Path) -> Result<()> {
    let filter_dir = filter_dir
        .canonicalize()
        .with_context(|| format!("resolve {}", filter_dir.display()))?;
    let manifest_path = filter_dir.join("manifest.toml");
    if !manifest_path.exists() {
        bail!(
            "{} not found — `plecto dev` expects a filter directory with a manifest.toml \
             (run `plecto new-filter` first, or point it at one you wrote by hand)",
            manifest_path.display()
        );
    }
    let filter_id = read_first_filter_id(&manifest_path)?;
    let signer = dev_key::load_or_create_dev_signer(project_root)?;

    println!(
        "plecto dev: building {} ({filter_id})…",
        filter_dir.display()
    );
    build_sign_and_gate(&filter_dir, &manifest_path, &filter_id, &signer)?;
    println!("plecto dev: initial build is conformant");

    let control = Arc::new(
        Control::from_manifest_path(&manifest_path)
            .map_err(|e| anyhow!(plecto_control::diagnosed_message(&e)))?,
    );

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

    spawn_watch_loop(filter_dir.clone(), manifest_path.clone(), filter_id, signer)?;

    let listen = control
        .listen_addr()
        .map(str::to_string)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let listener = TcpListener::bind(&listen).await?;
    println!(
        "plecto dev: serving {listen}, watching {}/src",
        filter_dir.display()
    );
    serve_with_shutdown(control, listener, crate::shutdown_signal()).await?;
    Ok(())
}

/// The watch → rebuild → gate → self-SIGHUP loop, on its own thread (mirrors `serve_reloads`'s
/// own blocking-loop-on-a-thread shape — the control plane stays sync/no-tokio by design).
/// Watches ONLY `<filter-dir>/src` (recursive) — never `Cargo.toml`/`Cargo.lock`/`filter_dir`
/// itself. Manual testing found that with a `notify` watch on the single file `Cargo.toml`,
/// events for OTHER files in that same directory (this loop's own
/// `target/`/`artifacts/`/`manifest.toml` writes) were reported as "Cargo.toml changed" — a
/// self-sustaining rebuild loop that no path-based filtering on the reported path could fix
/// (the report itself was wrong). Upstream documents single-file watches as fragile territory
/// (an inode watch breaks under rename-replace saves; the recommended robust pattern is
/// watching a directory), so rather than depend on exact single-file semantics we watch no
/// file outside `src/` at all. `src/` never contains build output — everything this loop
/// writes lives at `filter_dir`'s top level, a sibling of `src/`, so watching only `src/`
/// sidesteps the whole class of bug. A `Cargo.toml` dependency edit needs a restart of
/// `plecto dev` — a deliberate, documented scope cut, not an oversight.
fn spawn_watch_loop(
    filter_dir: PathBuf,
    manifest_path: PathBuf,
    filter_id: String,
    signer: DevSigner,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(DEBOUNCE, tx).context("start the file watcher")?;
    debouncer
        .watcher()
        .watch(&filter_dir.join("src"), RecursiveMode::Recursive)
        .with_context(|| format!("watch {}/src", filter_dir.display()))?;

    std::thread::spawn(move || {
        // Owned by this thread for its lifetime — dropping it would stop the watch.
        let _debouncer = debouncer;
        for result in rx {
            let changed = match result {
                Ok(events) => events
                    .iter()
                    .any(|e| path_is_relevant(&e.path, &filter_dir)),
                Err(e) => {
                    tracing::warn!(error = %e, "file watch error");
                    false
                }
            };
            if !changed {
                continue;
            }
            println!("plecto dev: change detected, rebuilding {filter_id}…");
            match build_sign_and_gate(&filter_dir, &manifest_path, &filter_id, &signer) {
                Ok(()) => {
                    // SAFETY: `raise` sends SIGHUP to this same process — no pointers, no
                    // preconditions beyond a valid signal number (SIGHUP is always valid).
                    let raised = unsafe { libc::raise(libc::SIGHUP) };
                    if raised == 0 {
                        println!("plecto dev: conformant — reloaded");
                    } else {
                        tracing::error!("raise(SIGHUP) failed; the new build was not reloaded");
                    }
                }
                Err(e) => {
                    eprintln!("plecto dev: {e:#}");
                    println!(
                        "plecto dev: reload blocked — the running gateway keeps the last good build"
                    );
                }
            }
        }
    });
    Ok(())
}

/// Is `path` under `<filter-dir>/src` — as opposed to build/reload OUTPUT (`target/`,
/// `artifacts/`, `manifest.toml`, `Cargo.lock`) a coarser or misattributing watch might also
/// report? See `spawn_watch_loop`'s doc comment for why this check exists at all: it is the
/// actual fix, not the `.watch()` call scoping alone.
fn path_is_relevant(path: &Path, filter_dir: &Path) -> bool {
    path.starts_with(filter_dir.join("src"))
}

/// Build → conformance-gate → (only if conformant) sign with the project's persistent dev key,
/// write the OCI layout, and rewrite the manifest's pinned digest. Conformance runs BEFORE any
/// of the signing/writing/manifest steps: a non-conformant build must never touch
/// `manifest.toml`, or a later real SIGHUP (an operator's, unrelated to this loop) could load it.
fn build_sign_and_gate(
    filter_dir: &Path,
    manifest_path: &Path,
    filter_id: &str,
    signer: &DevSigner,
) -> Result<()> {
    let component = componentize(filter_dir)?;

    let report = plecto_control::run_conformance(&component);
    for check in &report.checks {
        let mark = if check.passed { "PASS" } else { "FAIL" };
        println!("  [{mark}] {} — {}", check.name, check.detail);
    }
    if !report.is_conformant() {
        bail!("{filter_id} is not conformant with plecto:filter");
    }

    let component_signature = signer.sign(&component)?;
    let sbom = plecto_control::bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom)?;
    let artifact = ResolvedArtifact {
        component,
        component_signature,
        sbom,
        sbom_signature,
    };
    let layout_dir = filter_dir.join("artifacts").join(filter_id);
    let digest = plecto_control::oci::write_layout(&layout_dir, &artifact)
        .map_err(|e| anyhow!("write OCI layout: {e}"))?;
    update_manifest_digest(manifest_path, filter_id, &digest)
}

/// `cargo build --target wasm32-unknown-unknown --release` + the same `ComponentEncoder` recipe
/// `crates/host/build.rs` uses at Plecto's own build time (ADR 000010) — here at CLI runtime,
/// once per rebuild.
fn componentize(filter_dir: &Path) -> Result<Vec<u8>> {
    let target_dir = filter_dir.join("target");
    let status = Command::new("cargo")
        .current_dir(filter_dir)
        .args([
            "build",
            "--target",
            "wasm32-unknown-unknown",
            "--release",
            "--target-dir",
        ])
        .arg(&target_dir)
        .status()
        .with_context(|| format!("spawn cargo build in {}", filter_dir.display()))?;
    if !status.success() {
        bail!(
            "cargo build --target wasm32-unknown-unknown --release failed in {} (exit {status})",
            filter_dir.display()
        );
    }

    let package_name = read_cargo_package_name(&filter_dir.join("Cargo.toml"))?;
    let stem = package_name.replace('-', "_");
    let core_path = target_dir.join(format!("wasm32-unknown-unknown/release/{stem}.wasm"));
    let core_bytes = std::fs::read(&core_path)
        .with_context(|| format!("read built guest module {}", core_path.display()))?;

    wit_component::ComponentEncoder::default()
        .module(&core_bytes)
        .context("ComponentEncoder::module")?
        .validate(true)
        .encode()
        .context("wrap the guest module into a Component (wit-component)")
}

fn read_cargo_package_name(cargo_toml: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(cargo_toml)
        .with_context(|| format!("read {}", cargo_toml.display()))?;
    let doc = raw
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parse {}", cargo_toml.display()))?;
    doc["package"]["name"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} has no [package].name", cargo_toml.display()))
}

fn read_first_filter_id(manifest_path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let doc = raw
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parse {}", manifest_path.display()))?;
    doc.get("filter")
        .and_then(|item| item.as_array_of_tables())
        .and_then(|filters| filters.iter().next())
        .and_then(|table| table.get("id"))
        .and_then(|id| id.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} has no [[filter]] with an id", manifest_path.display()))
}

/// Format-preserving rewrite of exactly `[[filter]].digest` for the entry whose `id ==
/// filter_id` — a plain parse+reserialize (via `toml`/serde) would blow away this file's
/// comments and layout, and it stays a file an operator may also hand-edit (routes, upstream).
fn update_manifest_digest(manifest_path: &Path, filter_id: &str, digest: &str) -> Result<()> {
    let raw = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parse {}", manifest_path.display()))?;
    let filters = doc
        .get_mut("filter")
        .and_then(|item| item.as_array_of_tables_mut())
        .ok_or_else(|| anyhow!("{} has no [[filter]] tables", manifest_path.display()))?;
    let mut found = false;
    for table in filters.iter_mut() {
        if table.get("id").and_then(|v| v.as_str()) == Some(filter_id) {
            table["digest"] = toml_edit::value(digest);
            found = true;
        }
    }
    if !found {
        bail!(
            "{} has no [[filter]] with id = {filter_id:?}",
            manifest_path.display()
        );
    }
    std::fs::write(manifest_path, doc.to_string())
        .with_context(|| format!("write {}", manifest_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn update_manifest_digest_rewrites_only_the_matching_filters_digest() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.toml");
        write(
            &manifest_path,
            "# a comment the rewrite must preserve\n\
             [trust]\n\
             keys = [\"k.pub\"]\n\n\
             [[filter]]\n\
             id = \"a\"\n\
             source = \"artifacts/a\"\n\
             digest = \"sha256:OLD\"\n\n\
             [[filter]]\n\
             id = \"b\"\n\
             source = \"artifacts/b\"\n\
             digest = \"sha256:UNRELATED\"\n",
        );

        update_manifest_digest(&manifest_path, "a", "sha256:NEW").unwrap();

        let rewritten = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(rewritten.contains("# a comment the rewrite must preserve"));
        assert!(rewritten.contains("digest = \"sha256:NEW\""));
        assert!(rewritten.contains("digest = \"sha256:UNRELATED\""));
        assert!(!rewritten.contains("sha256:OLD"));
    }

    #[test]
    fn update_manifest_digest_errors_on_an_unknown_filter_id() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.toml");
        write(
            &manifest_path,
            "[[filter]]\nid = \"a\"\nsource = \"x\"\ndigest = \"sha256:OLD\"\n",
        );
        let err = update_manifest_digest(&manifest_path, "ghost", "sha256:NEW").unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn read_first_filter_id_reads_the_first_entry() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.toml");
        write(
            &manifest_path,
            "[[filter]]\nid = \"my-auth-filter\"\nsource = \"x\"\ndigest = \"sha256:PENDING\"\n",
        );
        assert_eq!(
            read_first_filter_id(&manifest_path).unwrap(),
            "my-auth-filter"
        );
    }

    #[test]
    fn read_cargo_package_name_reads_the_package_table() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_toml = dir.path().join("Cargo.toml");
        write(
            &cargo_toml,
            "[package]\nname = \"my-auth-filter\"\nversion = \"0.1.0\"\n",
        );
        assert_eq!(
            read_cargo_package_name(&cargo_toml).unwrap(),
            "my-auth-filter"
        );
    }

    #[test]
    fn path_is_relevant_accepts_only_paths_under_src() {
        let filter_dir = Path::new("/proj/my-filter");
        assert!(path_is_relevant(&filter_dir.join("src/lib.rs"), filter_dir));
        assert!(path_is_relevant(
            &filter_dir.join("src/nested/module.rs"),
            filter_dir
        ));
    }

    #[test]
    fn path_is_relevant_rejects_the_loops_own_build_output_and_cargo_files() {
        // The regression this guards: a `notify` watch on the single file `Cargo.toml`
        // misattributed OTHER files' events in the same directory to that filename (confirmed
        // by manual testing on the inotify backend) — including `plecto dev`'s own rebuild
        // writes to `target/`/`artifacts/`/`manifest.toml`, a self-sustaining rebuild loop no
        // path filter on the (wrongly reported) path could fix. The actual fix was to stop
        // watching `Cargo.toml` at all — `path_is_relevant` now only ever sees genuine `src/`
        // events, but stays as a second line of defense against anything outside `src/`.
        let filter_dir = Path::new("/proj/my-filter");
        assert!(!path_is_relevant(
            &filter_dir.join("manifest.toml"),
            filter_dir
        ));
        assert!(!path_is_relevant(
            &filter_dir.join("target/wasm32-unknown-unknown/release/my_filter.wasm"),
            filter_dir
        ));
        assert!(!path_is_relevant(
            &filter_dir.join("artifacts/my-filter/index.json"),
            filter_dir
        ));
        assert!(!path_is_relevant(
            &filter_dir.join("Cargo.lock"),
            filter_dir
        ));
        assert!(!path_is_relevant(
            &filter_dir.join("Cargo.toml"),
            filter_dir
        ));
    }
}
