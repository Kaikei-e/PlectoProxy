//! Phase B E2E: load a filter from a REAL offline OCI image-layout (no registry), proving the
//! ADR 000007 distribution path end-to-end — digest pin + bundled signature/SBOM layers +
//! chain dispatch — and that content pinning and blob integrity are fail-closed.

use plecto_control::oci::{OciLayoutStore, write_layout};
use plecto_control::{
    ChainOutcome, Control, ControlError, Host, HttpRequest, Manifest, ResolvedArtifact,
};
use plecto_host::Header;
use plecto_host::test_support::{TestSigner, bound_sbom, filter_hello_component};
use tempfile::tempdir;

fn req(headers: &[(&str, &str)]) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: "/".to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        headers: headers
            .iter()
            .map(|(n, v)| Header {
                name: (*n).to_string(),
                value: (*v).to_string(),
            })
            .collect(),
    }
}

fn signed_artifact() -> (TestSigner, ResolvedArtifact) {
    let component = filter_hello_component();
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(&component).unwrap();
    let sbom = bound_sbom(&component);
    let sbom_signature = signer.sign(&sbom).unwrap();
    (
        signer,
        ResolvedArtifact {
            component,
            component_signature,
            sbom,
            sbom_signature,
        },
    )
}

fn manifest_toml(digest: &str) -> String {
    format!(
        r#"
[[filter]]
id = "fh"
source = "fh"
digest = "{digest}"
isolation = "untrusted"

[chain]
filters = ["fh"]
"#
    )
}

#[test]
fn loads_and_runs_filter_from_offline_oci_layout() {
    let dir = tempdir().unwrap();
    let (signer, artifact) = signed_artifact();
    // wkg would produce this layout out-of-band; here we write it offline.
    let digest = write_layout(&dir.path().join("fh"), &artifact).unwrap();

    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let manifest = Manifest::from_toml(&manifest_toml(&digest)).unwrap();
    let control =
        Control::load(host, &manifest, Box::new(OciLayoutStore::new(dir.path()))).unwrap();

    // The filter, loaded entirely from the OCI layout (digest pin + bundled signature/SBOM),
    // actually runs through the chain.
    assert!(matches!(
        control.on_request(req(&[])),
        ChainOutcome::Forward(_)
    ));
    assert!(
        matches!(control.on_request(req(&[("x-plecto-block", "1")])), ChainOutcome::Respond(r) if r.status == 403)
    );
}

#[test]
fn oci_layout_wrong_pin_is_rejected() {
    let dir = tempdir().unwrap();
    let (signer, artifact) = signed_artifact();
    let _real = write_layout(&dir.path().join("fh"), &artifact).unwrap();

    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let wrong = format!("sha256:{}", "0".repeat(64));
    let manifest = Manifest::from_toml(&manifest_toml(&wrong)).unwrap();

    match Control::load(host, &manifest, Box::new(OciLayoutStore::new(dir.path()))) {
        Ok(_) => panic!("a wrong pinned digest must be rejected"),
        Err(e) => assert!(matches!(e, ControlError::DigestMismatch { .. }), "got {e}"),
    }
}

#[test]
fn oci_layout_blob_tampering_is_detected() {
    let dir = tempdir().unwrap();
    let (signer, artifact) = signed_artifact();
    let layout = dir.path().join("fh");
    let digest = write_layout(&layout, &artifact).unwrap();

    // Corrupt the image-manifest blob on disk after pinning. index.json still advertises the
    // original digest (the pin matches), but the blob's content no longer hashes to it.
    let (_, manifest_hex) = digest.split_once(':').unwrap();
    std::fs::write(
        layout.join("blobs").join("sha256").join(manifest_hex),
        b"tampered",
    )
    .unwrap();

    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let manifest = Manifest::from_toml(&manifest_toml(&digest)).unwrap();

    match Control::load(host, &manifest, Box::new(OciLayoutStore::new(dir.path()))) {
        Ok(_) => panic!("a tampered blob must be detected"),
        Err(e) => assert!(matches!(e, ControlError::Artifact { .. }), "got {e}"),
    }
}

#[test]
fn from_manifest_reads_trust_keys_and_loads_end_to_end() {
    // The full ops path (ADR 000007 / 000008): a single manifest names its trusted key and its
    // filters; `from_manifest` reads the PEM, builds the host + OCI store, and loads — no host
    // is injected. Everything is resolved relative to `base_dir`.
    let dir = tempdir().unwrap();
    let (signer, artifact) = signed_artifact();
    let digest = write_layout(&dir.path().join("fh"), &artifact).unwrap();
    std::fs::write(dir.path().join("cosign.pub"), signer.public_key_pem()).unwrap();

    let toml = format!(
        r#"
[trust]
keys = ["cosign.pub"]

[[filter]]
id = "fh"
source = "fh"
digest = "{digest}"
isolation = "untrusted"

[chain]
filters = ["fh"]
"#
    );
    let manifest = Manifest::from_toml(&toml).unwrap();
    let control = Control::from_manifest(&manifest, dir.path()).unwrap();

    assert!(
        matches!(control.on_request(req(&[("x-plecto-block", "1")])), ChainOutcome::Respond(r) if r.status == 403),
        "the manifest-built control plane loads and runs the filter"
    );
}

#[test]
fn oci_layout_with_unreadable_index_is_fail_closed() {
    // A corrupted / incomplete OCI layout (here: a garbage index.json — the layout's entry point)
    // must fail closed at resolve. Digest pinning and per-blob integrity are already covered; this
    // pins the OTHER supply-chain failure mode — a layout the host simply cannot parse must never
    // silently yield a filter.
    let dir = tempdir().unwrap();
    let (signer, artifact) = signed_artifact();
    let layout = dir.path().join("fh");
    let digest = write_layout(&layout, &artifact).unwrap();

    // clobber the layout's entry point after a valid write.
    std::fs::write(layout.join("index.json"), b"not an oci index at all").unwrap();

    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let manifest = Manifest::from_toml(&manifest_toml(&digest)).unwrap();
    match Control::load(host, &manifest, Box::new(OciLayoutStore::new(dir.path()))) {
        Ok(_) => panic!("a layout with an unreadable index.json must fail closed"),
        Err(e) => assert!(matches!(e, ControlError::Artifact { .. }), "got {e}"),
    }
}

#[test]
fn oci_layout_missing_source_directory_is_fail_closed() {
    // The manifest pins a `source` that resolves to no layout on disk. Resolve must fail closed
    // (the whole `build_active` aborts), never skip the filter and serve an unprotected chain.
    let dir = tempdir().unwrap();
    let (signer, _artifact) = signed_artifact();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    // a well-formed-looking pin, but `source = "fh"` (see `manifest_toml`) was never written here.
    let digest = format!("sha256:{}", "0".repeat(64));
    let manifest = Manifest::from_toml(&manifest_toml(&digest)).unwrap();
    match Control::load(host, &manifest, Box::new(OciLayoutStore::new(dir.path()))) {
        Ok(_) => panic!("a missing layout directory must fail closed"),
        Err(e) => assert!(matches!(e, ControlError::Artifact { .. }), "got {e}"),
    }
}
