//! `DevSigner`: a persistent, project-local ECDSA P-256 signer for the `plecto dev` inner loop
//! (ADR 000065). In the lineage of `test_support::TestSigner` — same signing scheme, same
//! `TrustPolicy` interop — but the key survives across process invocations instead of being
//! thrown away each call. Unlike `TestSigner`, this is production code: `plecto dev` and
//! `plecto new-filter` link it directly, not behind the `test-support` feature.
//!
//! The verification path this key exercises is byte-for-byte the same code a production
//! deploy uses (ADR 000006 P5): a dev key changes *which* key the manifest's `[trust]` names,
//! never how a signature is checked. See host/CONTEXT.md "Conformant (component)" and
//! control/CONTEXT.md "Dev key" for the surrounding vocabulary.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use sigstore::crypto::signing_key::SigStoreSigner;
use sigstore::crypto::signing_key::ecdsa::{ECDSAKeys, EllipticCurve};
use zeroize::Zeroizing;

use crate::TrustPolicy;

/// Prefixed onto a persisted dev public-key file, before the PEM block. Plain text ahead of
/// `-----BEGIN...` is not part of the PEM grammar (parsers skip straight to the marker), so
/// this survives being read back as a normal SPKI PEM while staying grep-able — the hook
/// `plecto validate` uses to warn when a dev key ends up in a production manifest's `[trust]`
/// (ADR 000065 decision 5).
pub const DEV_KEY_MARKER: &str = "# plecto-dev-key -- DO NOT reference from a production manifest";

/// A persistent ECDSA P-256 / cosign-scheme signer. Holds one key pair; the same key signs
/// both a filter component and its SBOM, matching a `TrustPolicy` that trusts exactly that key.
pub struct DevSigner {
    signer: SigStoreSigner,
    public_key_pem: String,
}

impl DevSigner {
    /// Generate a fresh key pair. Returns the ready-to-use signer plus the PKCS8 PEM private
    /// key, so the caller decides whether to persist it (`load_or_create`) or sign once and
    /// drop it (an ephemeral self-signed conformance check, no file ever written).
    pub fn generate() -> Result<(Self, Zeroizing<String>)> {
        let keys =
            ECDSAKeys::new(EllipticCurve::P256).map_err(|e| anyhow!("generate dev key: {e}"))?;
        let private_key_pem = keys
            .as_inner()
            .private_key_to_pem()
            .map_err(|e| anyhow!("export dev private key: {e}"))?;
        let signer = Self::from_keys(keys)?;
        Ok((signer, private_key_pem))
    }

    /// Rebuild from a previously persisted PKCS8 PEM private key. The elliptic curve is
    /// detected from the key's own OID (sigstore-rs), so this works for any dev key this
    /// module has ever generated.
    pub fn from_private_key_pem(pem: &[u8]) -> Result<Self> {
        let keys = ECDSAKeys::from_pem(pem).map_err(|e| anyhow!("load dev key: {e}"))?;
        Self::from_keys(keys)
    }

    fn from_keys(keys: ECDSAKeys) -> Result<Self> {
        let public_key_pem = keys
            .as_inner()
            .public_key_to_pem()
            .map_err(|e| anyhow!("export dev public key: {e}"))?;
        let signer = keys
            .to_sigstore_signer()
            .map_err(|e| anyhow!("build dev signer: {e}"))?;
        Ok(Self {
            signer,
            public_key_pem,
        })
    }

    /// Load the key at `private_key_path`, generating and persisting a fresh one on first use.
    /// The private key is written `0600` (owner read/write only); the matching public key is
    /// written alongside at `<private_key_path>.pub`, prefixed with [`DEV_KEY_MARKER`].
    /// Does NOT touch `.gitignore` — that is the CLI caller's job (it knows the project root;
    /// this function only knows the one key path it was given).
    pub fn load_or_create(private_key_path: &Path) -> Result<Self> {
        match fs::read(private_key_path) {
            Ok(pem) => Self::from_private_key_pem(&pem),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let (signer, private_key_pem) = Self::generate()?;
                write_dev_key_files(
                    private_key_path,
                    private_key_pem.as_bytes(),
                    signer.public_key_pem(),
                )?;
                Ok(signer)
            }
            Err(e) => {
                Err(e).with_context(|| format!("read dev key {}", private_key_path.display()))
            }
        }
    }

    /// Raw DER ECDSA signature over `msg` (the shape `SignedArtifact` expects).
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        self.signer.sign(msg).map_err(|e| anyhow!("sign: {e}"))
    }

    pub fn public_key_pem(&self) -> &str {
        &self.public_key_pem
    }

    /// A `TrustPolicy` that trusts exactly this signer's key.
    pub fn trust_policy(&self) -> Result<TrustPolicy> {
        TrustPolicy::from_pem_keys([self.public_key_pem.as_bytes()])
    }
}

/// The public-key sibling path `load_or_create` writes next to a private-key path
/// (`<path>.pub`). Exposed so a caller (e.g. `plecto new-filter` writing a dev manifest's
/// `[trust]`) can name it without re-deriving the convention.
pub fn public_key_path_for(private_key_path: &Path) -> PathBuf {
    let mut name = private_key_path.as_os_str().to_owned();
    name.push(".pub");
    PathBuf::from(name)
}

fn write_dev_key_files(
    private_key_path: &Path,
    private_key_pem: &[u8],
    public_key_pem: &str,
) -> Result<()> {
    if let Some(parent) = private_key_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(private_key_path, private_key_pem)
        .with_context(|| format!("write dev key {}", private_key_path.display()))?;
    let mut perms = fs::metadata(private_key_path)
        .with_context(|| format!("stat dev key {}", private_key_path.display()))?
        .permissions();
    perms.set_mode(0o600);
    fs::set_permissions(private_key_path, perms)
        .with_context(|| format!("chmod 0600 {}", private_key_path.display()))?;

    let public_key_path = public_key_path_for(private_key_path);
    let marked = format!("{DEV_KEY_MARKER}\n{public_key_pem}");
    fs::write(&public_key_path, marked)
        .with_context(|| format!("write dev public key {}", public_key_path.display()))?;
    Ok(())
}

/// A minimal in-toto-style SBOM statement that binds `component`: its `subject` digest is
/// `sha256(component)`, satisfying the load gate's SBOM↔component binding (review f000003 #1).
/// The predicate is empty (content policy is deferred). Production helper — this crate's own
/// `Host::load` verifies the binding it produces; a real supply chain gets its attestations
/// from `cosign attest`. Also re-exported (unchanged) from `test_support` for the existing
/// test suites.
pub fn bound_sbom(component: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(component));
    format!(
        r#"{{"_type":"https://in-toto.io/Statement/v1","subject":[{{"name":"filter","digest":{{"sha256":"{digest}"}}}}],"predicateType":"https://cyclonedx.org/bom","predicate":{{}}}}"#
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_signer_is_trusted_by_its_own_policy() {
        let (signer, _private_pem) = DevSigner::generate().unwrap();
        let policy = signer.trust_policy().unwrap();
        let sig = signer.sign(b"hello").unwrap();
        assert!(policy.verifies(sig.as_slice(), b"hello"));
    }

    #[test]
    fn load_or_create_persists_and_reloads_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join(".plecto").join("dev-key");

        let first = DevSigner::load_or_create(&key_path).unwrap();
        assert!(key_path.exists());
        assert!(public_key_path_for(&key_path).exists());

        let second = DevSigner::load_or_create(&key_path).unwrap();
        assert_eq!(first.public_key_pem(), second.public_key_pem());

        // A signature from the reloaded signer must verify under a trust policy built from
        // the FIRST run's public key -- proof the reload is the exact same key, not a
        // same-shaped new one.
        let policy = first.trust_policy().unwrap();
        let sig = second.sign(b"round-trip").unwrap();
        assert!(policy.verifies(sig.as_slice(), b"round-trip"));
    }

    #[test]
    fn persisted_private_key_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("dev-key");
        DevSigner::load_or_create(&key_path).unwrap();

        let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn persisted_public_key_file_carries_the_dev_marker() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("dev-key");
        DevSigner::load_or_create(&key_path).unwrap();

        let pub_contents = fs::read_to_string(public_key_path_for(&key_path)).unwrap();
        assert!(pub_contents.starts_with(DEV_KEY_MARKER));
        assert!(pub_contents.contains("BEGIN PUBLIC KEY"));
    }

    #[test]
    fn bound_sbom_subject_digest_matches_component_sha256() {
        use sha2::{Digest, Sha256};
        let component = b"pretend-component-bytes";
        let sbom = bound_sbom(component);
        let want = hex::encode(Sha256::digest(component));
        assert!(String::from_utf8(sbom).unwrap().contains(&want));
    }
}
