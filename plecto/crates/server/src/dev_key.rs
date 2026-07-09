//! Project-local dev-key + `.gitignore` bookkeeping shared by `plecto new-filter` and
//! `plecto dev` (ADR 000065 decision 2). "Project-local" means relative to the directory the
//! command runs from (`project_root`) — one dev key per project, not per filter, not global
//! (see control/CONTEXT.md "Dev key" for why: a shared home-dir key widens the blast radius of
//! accidentally landing in a production manifest's `[trust]`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use plecto_control::DevSigner;

const DEV_KEY_RELATIVE_PATH: &str = ".plecto/dev-key";

pub(crate) fn dev_key_path(project_root: &Path) -> PathBuf {
    project_root.join(DEV_KEY_RELATIVE_PATH)
}

pub(crate) fn public_key_path(project_root: &Path) -> PathBuf {
    plecto_control::public_key_path_for(&dev_key_path(project_root))
}

/// Load or create the project's dev key, appending `.plecto/` to `<project_root>/.gitignore`
/// the first time this project gets one (idempotent — never appends a duplicate line).
pub(crate) fn load_or_create_dev_signer(project_root: &Path) -> Result<DevSigner> {
    let key_path = dev_key_path(project_root);
    let existed = key_path.exists();
    let signer = DevSigner::load_or_create(&key_path)
        .with_context(|| format!("load or create dev key at {}", key_path.display()))?;
    if !existed {
        ensure_gitignore_entry(project_root)?;
    }
    Ok(signer)
}

fn ensure_gitignore_entry(project_root: &Path) -> Result<()> {
    let gitignore_path = project_root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".plecto/") {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(".plecto/\n");
    std::fs::write(&gitignore_path, updated)
        .with_context(|| format!("write {}", gitignore_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_creates_the_key_and_appends_gitignore_once() {
        let dir = tempfile::tempdir().unwrap();
        load_or_create_dev_signer(dir.path()).unwrap();
        assert!(dev_key_path(dir.path()).exists());
        let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(
            gitignore.lines().filter(|l| l.trim() == ".plecto/").count(),
            1
        );

        // A second run reuses the same key and does not duplicate the .gitignore entry.
        let first_pub = std::fs::read_to_string(public_key_path(dir.path())).unwrap();
        load_or_create_dev_signer(dir.path()).unwrap();
        let second_pub = std::fs::read_to_string(public_key_path(dir.path())).unwrap();
        assert_eq!(first_pub, second_pub);
        let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(
            gitignore.lines().filter(|l| l.trim() == ".plecto/").count(),
            1
        );
    }

    #[test]
    fn appends_to_an_existing_gitignore_without_clobbering_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        load_or_create_dev_signer(dir.path()).unwrap();
        let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gitignore.contains("target/"));
        assert!(gitignore.contains(".plecto/"));
    }
}
