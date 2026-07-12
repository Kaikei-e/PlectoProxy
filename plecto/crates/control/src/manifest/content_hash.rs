//! The manifest's semantic content hash (ADR 000008 `config version`).

use std::path::Path;

use sha2::{Digest, Sha256};

use super::Manifest;
use crate::error::ControlError;

impl Manifest {
    /// The **semantic** content hash of this manifest — `sha256:<hex>` over a canonical
    /// serialisation, not over the raw TOML. Two manifests that mean the same thing (differing
    /// only in comments, whitespace, key order, or an explicit default written vs. omitted)
    /// hash identically; any meaningful change flips the hash.
    ///
    /// This is the manifest's `config version`: the unit the reload gate compares for
    /// idempotency (via [`content_hash_at`]), the value an operator audits, and the value a
    /// future opt-in consensus layer (ADR 000008 openraft) would agree on. Canonical form is
    /// `serde_json` over the derived `Serialize` — deterministic because the struct field order
    /// is fixed and the manifest holds no maps (only ordered `Vec`s).
    ///
    /// Does **not** read referenced files. The load/reload path uses [`content_hash_at`] /
    /// [`content_hash_with_ca`] so an in-place client-auth CA renewal flips the version.
    pub fn content_hash(&self) -> Result<String, ControlError> {
        self.content_hash_with_ca(None)
    }

    /// [`content_hash`] with referenced files resolved against `base_dir` (when `Some`): reads
    /// `[listen.client_auth].ca_path` and mixes its digest in. Same path + different bytes must
    /// flip the config version; otherwise SIGHUP reports `Unchanged` and the new trust roots
    /// never load (fail-closed: an unreadable CA is an error, not a silently CA-less hash).
    pub fn content_hash_at(&self, base_dir: Option<&Path>) -> Result<String, ControlError> {
        let ca = match base_dir {
            Some(base) => self.read_client_auth_ca(base)?,
            None => None,
        };
        self.content_hash_with_ca(ca.as_deref())
    }

    /// Read `[listen.client_auth].ca_path`, or `None` when no client auth is configured. The
    /// ONE read a build shares between the config version and the client verifier
    /// ([`content_hash_with_ca`] + `tls::build_server_configs`), so the recorded version always
    /// describes the trust roots the verifier was actually built from.
    pub fn read_client_auth_ca(&self, base_dir: &Path) -> Result<Option<Vec<u8>>, ControlError> {
        let Some(auth) = &self.listen.client_auth else {
            return Ok(None);
        };
        std::fs::read(base_dir.join(&auth.ca_path))
            .map(Some)
            .map_err(|e| ControlError::ClientAuthCa {
                path: auth.ca_path.clone(),
                reason: format!("read failed: {e}"),
            })
    }

    /// The semantic hash, with the client-auth CA bundle's digest mixed in when supplied
    /// (callers obtain the bytes from [`read_client_auth_ca`]).
    pub fn content_hash_with_ca(
        &self,
        client_auth_ca: Option<&[u8]>,
    ) -> Result<String, ControlError> {
        let mut hasher = Sha256::new();
        hasher.update(serde_json::to_vec(self)?);
        if let Some(bytes) = client_auth_ca {
            hasher.update(b"\0listen.client_auth.ca\0");
            hasher.update(Sha256::digest(bytes));
        }
        Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_semantic_not_textual() {
        // Representation noise that does NOT change meaning must NOT change the hash:
        // comments, whitespace, key order, and an explicit default (`isolation = "untrusted"`)
        // written vs. omitted all canonicalise away.
        let terse = Manifest::from_toml(
            r#"
[[filter]]
id = "auth"
source = "artifacts/auth"
digest = "sha256:abc"

[chain]
filters = ["auth"]
"#,
        )
        .unwrap();

        let noisy = Manifest::from_toml(
            r#"
# a leading comment
[chain]
filters = ["auth"]   # chain first, with trailing comment

[[filter]]
digest   = "sha256:abc"
source   = "artifacts/auth"
id       = "auth"
isolation = "untrusted"   # the default, written explicitly
"#,
        )
        .unwrap();

        assert_eq!(
            terse.content_hash().unwrap(),
            noisy.content_hash().unwrap(),
            "semantically identical manifests must share a content hash"
        );
        assert!(terse.content_hash().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn client_auth_rides_the_content_hash_but_startup_fixed_listen_fields_do_not() {
        // Regression (large-review finding): `build_active` consumes `listen.client_auth` on
        // every reload, so a client_auth-only edit MUST flip the config version — otherwise
        // SIGHUP reports `Unchanged` and the mTLS change is silently not applied. The
        // startup-fixed `[listen]` fields (addr etc.) stay hash-exempt as before.
        let base = Manifest::from_toml("").unwrap();
        let with_addr = Manifest::from_toml("[listen]\naddr = \"0.0.0.0:8443\"\n").unwrap();
        assert_eq!(
            base.content_hash().unwrap(),
            with_addr.content_hash().unwrap(),
            "a bind-address edit is startup-fixed and must not flip the config version"
        );

        let with_client_auth =
            Manifest::from_toml("[listen.client_auth]\nca_path = \"ca.pem\"\n").unwrap();
        assert_ne!(
            base.content_hash().unwrap(),
            with_client_auth.content_hash().unwrap(),
            "adding [listen.client_auth] must flip the config version"
        );

        let other_ca =
            Manifest::from_toml("[listen.client_auth]\nca_path = \"other.pem\"\n").unwrap();
        assert_ne!(
            with_client_auth.content_hash().unwrap(),
            other_ca.content_hash().unwrap(),
            "changing the client-auth CA must flip the config version"
        );
    }

    #[test]
    fn client_auth_ca_file_bytes_ride_content_hash_at() {
        // Same ca_path, different file bytes must flip the reload version (in-place CA rotation).
        let dir = tempfile::tempdir().unwrap();
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(
            &ca_path,
            b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let m = Manifest::from_toml("[listen.client_auth]\nca_path = \"ca.pem\"\n").unwrap();
        let h1 = m.content_hash_at(Some(dir.path())).unwrap();
        std::fs::write(
            &ca_path,
            b"-----BEGIN CERTIFICATE-----\nBBBB\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let h2 = m.content_hash_at(Some(dir.path())).unwrap();
        assert_ne!(h1, h2, "in-place CA overwrite must flip content_hash_at");
        assert_eq!(
            m.content_hash().unwrap(),
            m.content_hash().unwrap(),
            "path-only content_hash stays stable (no file read)"
        );
    }

    #[test]
    fn content_hash_changes_on_meaningful_edit() {
        let v1 = Manifest::from_toml(
            r#"
[[filter]]
id = "auth"
source = "artifacts/auth"
digest = "sha256:abc"

[chain]
filters = ["auth"]
"#,
        )
        .unwrap();
        // Same filter, different chain (drops it) — a real config change.
        let v2 = Manifest::from_toml(
            r#"
[[filter]]
id = "auth"
source = "artifacts/auth"
digest = "sha256:abc"

[chain]
filters = []
"#,
        )
        .unwrap();

        assert_ne!(
            v1.content_hash().unwrap(),
            v2.content_hash().unwrap(),
            "a chain change must flip the content hash"
        );
    }
}
