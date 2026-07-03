//! E2E (tdd-workflow Phase 0) for the binary's operator CLI: `plecto --version` and
//! `plecto validate <manifest>` (the `nginx -t` shape — validate a manifest in CI / before a
//! SIGHUP without serving). Drives the real compiled binary (`CARGO_BIN_EXE_plecto`).
//!
//! `validate` is a STATIC config check: parse (strict, `deny_unknown_fields`), reference and
//! range validation (filters / chain / routes / upstreams), and the fail-closed file loads the
//! server would do at startup (trust keys, TLS certs, upstream CA). It must not serve, must not
//! load WASM artifacts (they may not exist where CI runs), and must not create state files.

use std::path::Path;
use std::process::Command;

fn run(args: &[&str], dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_plecto"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

const VALID_MANIFEST: &str = r#"
[[upstream]]
name = "app"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"

[[route]]
upstream = "app"
[route.match]
path_prefix = "/api"
"#;

#[test]
fn version_flag_prints_the_package_version_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    for flag in ["--version", "-V"] {
        let out = run(&[flag], dir.path());
        assert!(out.status.success(), "{flag} exits 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(env!("CARGO_PKG_VERSION")),
            "{flag} prints the package version, got: {stdout:?}"
        );
    }
}

#[test]
fn validate_accepts_a_good_manifest_and_exits_without_serving() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("plecto.toml"), VALID_MANIFEST).unwrap();

    let out = run(&["validate", "plecto.toml"], dir.path());

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a valid manifest validates, stderr: {stderr:?}"
    );
    // The config version is the operator's audit handle (ADR 000008) — surface it.
    assert!(
        stdout.contains("sha256:"),
        "validate reports the config version, got: {stdout:?}"
    );
    // The process EXITED (output() already proves it did not stay up serving).
}

#[test]
fn validate_rejects_an_unknown_field_with_a_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plecto.toml"),
        VALID_MANIFEST.replace("path = \"/healthz\"", "path = \"/healthz\"\ntypo_field = 1"),
    )
    .unwrap();

    let out = run(&["validate", "plecto.toml"], dir.path());

    assert!(!out.status.success(), "an unknown field must fail validate");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("typo_field"),
        "the error names the offending field, got: {stderr:?}"
    );
}

#[test]
fn validate_rejects_a_route_referencing_an_unknown_upstream() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plecto.toml"),
        VALID_MANIFEST.replace("upstream = \"app\"", "upstream = \"nonexistent\""),
    )
    .unwrap();

    let out = run(&["validate", "plecto.toml"], dir.path());

    assert!(
        !out.status.success(),
        "a dangling upstream reference must fail validate"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nonexistent"),
        "the error names the unknown upstream, got: {stderr:?}"
    );
}

#[test]
fn validate_never_creates_the_redb_state_file() {
    // `[state] backend = "redb"` opens (and CREATES) the database at startup; validate must only
    // check the section's coherence, never touch the filesystem — a CI validate run should leave
    // no state files behind.
    let dir = tempfile::tempdir().unwrap();
    let manifest = format!("[state]\nbackend = \"redb\"\npath = \"kv.redb\"\n{VALID_MANIFEST}");
    std::fs::write(dir.path().join("plecto.toml"), manifest).unwrap();

    let out = run(&["validate", "plecto.toml"], dir.path());

    assert!(
        out.status.success(),
        "a coherent [state] section validates, stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !dir.path().join("kv.redb").exists(),
        "validate must not create the redb state file"
    );
}

#[test]
fn validate_rejects_a_bad_upstream_ca_path() {
    // The fail-closed file loads the server would do at startup run under validate too — a
    // missing [upstream.tls] CA is exactly the class of mistake `validate` exists to catch
    // before a deploy (ADR 000042).
    let dir = tempfile::tempdir().unwrap();
    let manifest = VALID_MANIFEST.replace(
        "[upstream.health]",
        "[upstream.tls]\nca_path = \"missing-ca.pem\"\n[upstream.health]",
    );
    std::fs::write(dir.path().join("plecto.toml"), manifest).unwrap();

    let out = run(&["validate", "plecto.toml"], dir.path());

    assert!(
        !out.status.success(),
        "a missing CA file must fail validate"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("missing-ca.pem"),
        "the error names the missing CA path, got: {stderr:?}"
    );
}
