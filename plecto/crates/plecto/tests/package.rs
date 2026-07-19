//! E2E (tdd-workflow Phase 0) for `plecto package` + `plecto validate --resolve` (field report
//! §3.1 / §3.5): the one-shot CI packaging pipeline. `package` turns a built component + an
//! operator key into the signed offline OCI image-layout the loader requires and prints the
//! pinned image-manifest digest — nothing else — to stdout, so `DIGEST=$(plecto package …)`
//! composes. `validate --resolve` then proves, without serving, that a manifest + its layouts
//! would pass the load-time gates: digest pin, trusted signatures, SBOM binding.
//!
//! Drives the real compiled binary (`CARGO_BIN_EXE_plecto`).

use std::path::Path;
use std::process::Command;

use plecto_host::DevSigner;
use plecto_host::test_support::filter_hello_component;

fn run(args: &[&str], dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_plecto"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

/// Write a fresh ECDSA P-256 key pair: the PKCS8 private key for `--key`, the SPKI public key
/// for the manifest's `[trust]`.
fn write_key_pair(dir: &Path) {
    let (signer, private_pem) = DevSigner::generate().unwrap();
    std::fs::write(dir.join("key.pem"), private_pem.as_bytes()).unwrap();
    std::fs::write(dir.join("trust.pem"), signer.public_key_pem()).unwrap();
}

fn write_component(dir: &Path) {
    std::fs::write(dir.join("filter.wasm"), filter_hello_component()).unwrap();
}

fn manifest_pinning(digest: &str) -> String {
    format!(
        r#"[trust]
keys = ["trust.pem"]

[[filter]]
id = "hello"
source = "layout"
digest = "{digest}"
isolation = "trusted"

[[upstream]]
name = "app"
addresses = ["127.0.0.1:9000"]
[upstream.health]
path = "/healthz"

[[route]]
filters = ["hello"]
upstream = "app"
[route.match]
path_prefix = "/"
"#
    )
}

fn package(dir: &Path) -> std::process::Output {
    run(
        &[
            "package",
            "filter.wasm",
            "--key",
            "key.pem",
            "--out",
            "layout",
        ],
        dir,
    )
}

#[test]
fn package_prints_only_the_pinned_digest_and_the_layout_passes_resolve() {
    let dir = tempfile::tempdir().unwrap();
    write_key_pair(dir.path());
    write_component(dir.path());

    let out = package(dir.path());
    assert!(
        out.status.success(),
        "package exits 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // stdout is the pinned digest and nothing else — `DIGEST=$(plecto package …)` in CI.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let digest = stdout.trim_end();
    assert!(
        digest.starts_with("sha256:") && digest.len() == "sha256:".len() + 64,
        "stdout is exactly one sha256:<hex> digest, got: {stdout:?}"
    );
    assert_eq!(
        stdout.lines().count(),
        1,
        "no extra stdout lines: {stdout:?}"
    );

    // The digest pins the layout in a manifest, and `validate --resolve` proves the load-time
    // gates (digest pin + signature + SBOM binding) pass — the closed CI pipeline of §3.1/§3.5.
    std::fs::write(dir.path().join("plecto.toml"), manifest_pinning(digest)).unwrap();
    let resolve = run(&["validate", "--resolve", "plecto.toml"], dir.path());
    assert!(
        resolve.status.success(),
        "validate --resolve accepts the packaged layout, stderr: {}",
        String::from_utf8_lossy(&resolve.stderr)
    );
}

#[test]
fn package_gates_on_conformance_and_writes_nothing_for_junk() {
    let dir = tempfile::tempdir().unwrap();
    write_key_pair(dir.path());
    std::fs::write(dir.path().join("filter.wasm"), b"not a component").unwrap();

    let out = package(dir.path());
    assert!(!out.status.success(), "junk input is rejected");
    assert!(
        out.stdout.is_empty(),
        "no digest on stdout for a rejected component: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !dir.path().join("layout").exists(),
        "a non-conformant component never produces a layout"
    );
}

#[test]
fn package_accepts_a_supplied_bound_sbom() {
    let dir = tempfile::tempdir().unwrap();
    write_key_pair(dir.path());
    write_component(dir.path());
    // A supplier-provided statement, still bound to this component (subject digest = its
    // sha256) — binding stays the supplier's responsibility and the loader still verifies it.
    let component = filter_hello_component();
    let default_statement = plecto_host::bound_sbom(&component);
    let mut statement: serde_json::Value = serde_json::from_slice(&default_statement).unwrap();
    statement["predicate"] = serde_json::json!({"supplier": "moka-1"});
    std::fs::write(
        dir.path().join("sbom.json"),
        serde_json::to_vec(&statement).unwrap(),
    )
    .unwrap();

    let out = run(
        &[
            "package",
            "filter.wasm",
            "--key",
            "key.pem",
            "--out",
            "layout",
            "--sbom",
            "sbom.json",
        ],
        dir.path(),
    );
    assert!(
        out.status.success(),
        "package --sbom exits 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let digest = String::from_utf8_lossy(&out.stdout).trim_end().to_string();

    // The supplied statement rides in the layout and the full gate still passes.
    std::fs::write(dir.path().join("plecto.toml"), manifest_pinning(&digest)).unwrap();
    let resolve = run(&["validate", "--resolve", "plecto.toml"], dir.path());
    assert!(
        resolve.status.success(),
        "resolve accepts the supplier SBOM, stderr: {}",
        String::from_utf8_lossy(&resolve.stderr)
    );
}

#[test]
fn resolve_fails_closed_on_an_untrusted_signature() {
    let dir = tempfile::tempdir().unwrap();
    write_key_pair(dir.path());
    write_component(dir.path());
    let out = package(dir.path());
    assert!(out.status.success());
    let digest = String::from_utf8_lossy(&out.stdout).trim_end().to_string();

    // Swap the trust root for a key that never signed this layout: static validation still
    // passes (the PEM is well-formed), but --resolve must reject — same gate as the loader.
    let (stranger, _) = DevSigner::generate().unwrap();
    std::fs::write(dir.path().join("trust.pem"), stranger.public_key_pem()).unwrap();
    std::fs::write(dir.path().join("plecto.toml"), manifest_pinning(&digest)).unwrap();

    let stat = run(&["validate", "plecto.toml"], dir.path());
    assert!(
        stat.status.success(),
        "static validate does not resolve artifacts"
    );
    let resolve = run(&["validate", "--resolve", "plecto.toml"], dir.path());
    assert!(
        !resolve.status.success(),
        "--resolve rejects a layout no trusted key signed"
    );
}

#[test]
fn resolve_fails_closed_on_a_tampered_blob() {
    let dir = tempfile::tempdir().unwrap();
    write_key_pair(dir.path());
    write_component(dir.path());
    let out = package(dir.path());
    assert!(out.status.success());
    let digest = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    std::fs::write(dir.path().join("plecto.toml"), manifest_pinning(&digest)).unwrap();

    // Flip a byte in the largest blob (the component layer): the content no longer matches
    // its digest-addressed filename, so resolution fails before any signature check.
    let blobs = dir.path().join("layout/blobs/sha256");
    let component_blob = std::fs::read_dir(&blobs)
        .unwrap()
        .map(|e| e.unwrap().path())
        .max_by_key(|p| std::fs::metadata(p).unwrap().len())
        .unwrap();
    let mut bytes = std::fs::read(&component_blob).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&component_blob, bytes).unwrap();

    let resolve = run(&["validate", "--resolve", "plecto.toml"], dir.path());
    assert!(
        !resolve.status.success(),
        "--resolve rejects a blob whose content diverges from its digest"
    );
}
