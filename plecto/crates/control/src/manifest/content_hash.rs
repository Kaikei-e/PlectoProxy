//! The manifest's semantic content hash (ADR 000008 `config version`).

use sha2::{Digest, Sha256};

use super::Manifest;
use crate::error::ControlError;

impl Manifest {
    /// The **semantic** content hash of this manifest — `sha256:<hex>` over a canonical
    /// serialisation, not over the raw TOML. Two manifests that mean the same thing (differing
    /// only in comments, whitespace, key order, or an explicit default written vs. omitted)
    /// hash identically; any meaningful change flips the hash.
    ///
    /// This is the manifest's `config version`: the unit `reload_from_disk` compares for
    /// idempotency, the value an operator audits, and the value a future opt-in consensus
    /// layer (ADR 000008 openraft) would agree on. Canonical form is `serde_json` over the
    /// derived `Serialize` — deterministic because the struct field order is fixed and the
    /// manifest holds no maps (only ordered `Vec`s).
    pub fn content_hash(&self) -> Result<String, ControlError> {
        let bytes = serde_json::to_vec(self)?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(&bytes))))
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
