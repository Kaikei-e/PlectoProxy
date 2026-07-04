//! Provenance verification (ADR 000006): the set of keys a `Host` trusts to sign filters, and
//! the signed artifact material [`Host::load`] verifies before ever touching component bytes.

use anyhow::Result;
use sigstore::crypto::{CosignVerificationKey, Signature};

/// The set of public keys the operator trusts to sign filters (ADR 000006 provenance). A
/// filter loads only if a trusted key verifies BOTH its component signature and its SBOM
/// signature (keyed cosign, offline — no Fulcio / Rekor / network). An **empty** policy
/// trusts no one, so nothing loads: deny-by-default / fail-closed, with no "allow unsigned"
/// escape hatch in the production API. The keys live on the `Host`, not on each `load` call,
/// so the operator manages one trust root.
///
/// This gates *whether a filter may load at all*. It deliberately does NOT pick the filter's
/// `Isolation` (trusted/untrusted lifecycle) — a valid signature from a third party's key is
/// still untrusted code. Mapping signer identity to isolation is left to the declarative
/// manifest (ADR 000007); here, isolation stays the caller's explicit `LoadOptions` choice.
pub struct TrustPolicy {
    keys: Vec<CosignVerificationKey>,
}

impl TrustPolicy {
    /// Trust the given public keys (SPKI PEM). The key type is auto-detected — cosign's
    /// default is ECDSA P-256; P-256 / Ed25519 / RSA cosign keys are all accepted.
    pub fn from_pem_keys<I, B>(pems: I) -> Result<Self>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        let keys = pems
            .into_iter()
            .map(|pem| {
                CosignVerificationKey::try_from_pem(pem.as_ref())
                    .map_err(|e| anyhow::anyhow!("invalid trusted public key (PEM): {e}"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { keys })
    }

    /// An explicitly empty policy — trusts no one, so every load fails closed. Useful to
    /// assert the fail-closed default.
    pub fn empty() -> Self {
        Self { keys: Vec::new() }
    }

    /// Does ANY trusted key verify this raw (DER) signature over `msg`? cosign ECDSA
    /// signatures are ASN.1 DER; verification hashes `msg` internally (do not pre-hash).
    pub(crate) fn verifies(&self, signature_der: &[u8], msg: &[u8]) -> bool {
        self.keys.iter().any(|k| {
            k.verify_signature(Signature::Raw(signature_der), msg)
                .is_ok()
        })
    }
}

/// The material the host verifies before instantiating a filter (ADR 000006). The component
/// bytes plus a keyed cosign signature over them, and a **mandatory** SBOM with its own
/// signature. Signatures are RAW DER ECDSA bytes: decoding cosign's base64 `.sig` and
/// fetching the artifact from an OCI registry is the ADR 000007 / `wkg` boundary, kept out
/// of the host so ADR 000006 (verify) and ADR 000007 (distribute) stay decoupled.
pub struct SignedArtifact<'a> {
    /// The WASM component bytes.
    pub component_bytes: &'a [u8],
    /// Raw DER signature over `component_bytes` (cosign `sign-blob`).
    pub component_signature: &'a [u8],
    /// The SBOM as an in-toto-style statement whose `subject[].digest.sha256` binds it to
    /// `component_bytes` (verified at load, review f000003 #1). The predicate (the SBOM body)
    /// stays opaque in v0.1 — content policy (CVE / license scanning) is deferred.
    pub sbom: &'a [u8],
    /// Raw DER signature over `sbom`.
    pub sbom_signature: &'a [u8],
}
