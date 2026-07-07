//! Test / dev signing support — **NOT production provenance**. Generates a fresh ephemeral
//! ECDSA P-256 key (cosign's default scheme), signs blobs with it, and exposes the matching
//! public-key PEM so a test can build a `TrustPolicy` and drive the real verify path
//! end-to-end without the `cosign` CLI. The key is thrown away each time; this grants nothing
//! a caller could not already do with sigstore directly. `#[doc(hidden)]` — integration tests
//! need it `pub`, but it is not part of the supported surface.
use crate::TrustPolicy;
use anyhow::{Result, anyhow};
use sigstore::crypto::SigningScheme;
use sigstore::crypto::signing_key::SigStoreSigner;

/// A throwaway signer holding one ephemeral keypair, so the same key can sign both the
/// component and the SBOM (and a matching `TrustPolicy` trusts exactly that key).
pub struct TestSigner {
    signer: SigStoreSigner,
    public_key_pem: String,
}

impl TestSigner {
    pub fn new() -> Result<Self> {
        let signer = SigningScheme::ECDSA_P256_SHA256_ASN1
            .create_signer()
            .map_err(|e| anyhow!("create_signer: {e}"))?;
        let public_key_pem = signer
            .to_sigstore_keypair()
            .map_err(|e| anyhow!("to_sigstore_keypair: {e}"))?
            .public_key_to_pem()
            .map_err(|e| anyhow!("public_key_to_pem: {e}"))?;
        Ok(Self {
            signer,
            public_key_pem,
        })
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

/// The compiled `filter-hello` component bytes — the shared conformance fixture, built by
/// this crate's `build.rs`. Exposed so dependent crates (e.g. `plecto-control`) can load a
/// real `plecto:filter` component in their own tests.
pub fn filter_hello_component() -> Vec<u8> {
    std::fs::read(env!("FILTER_HELLO_COMPONENT")).expect("read filter-hello component")
}

/// The compiled `filter-apikey` component bytes — the real-world example filter (an API-key
/// auth gate), built by this crate's `build.rs`. Exposed so the server's `wasm-auth` example
/// can sign and load it through the production path.
pub fn filter_apikey_component() -> Vec<u8> {
    std::fs::read(env!("FILTER_APIKEY_COMPONENT")).expect("read filter-apikey component")
}

/// The compiled `filter-quickstart` component bytes — the minimal starter filter (stamps one
/// response header) behind the `quickstart` example, built by this crate's `build.rs`.
pub fn filter_quickstart_component() -> Vec<u8> {
    std::fs::read(env!("FILTER_QUICKSTART_COMPONENT")).expect("read filter-quickstart component")
}

/// The compiled `filter-noop` component bytes — the "pure WASM no-op" rung of the benchmark
/// cost ladder (no host-API calls), built by this crate's `build.rs`.
pub fn filter_noop_component() -> Vec<u8> {
    std::fs::read(env!("FILTER_NOOP_COMPONENT")).expect("read filter-noop component")
}

/// The compiled `filter-extauthz` component bytes — the outbound-HTTP example (an ext_authz-style
/// gate), built by this crate's `build.rs` when the `outbound-http` feature is on (ADR 000036).
#[cfg(feature = "outbound-http")]
pub fn filter_extauthz_component() -> Vec<u8> {
    std::fs::read(env!("FILTER_EXTAUTHZ_COMPONENT")).expect("read filter-extauthz component")
}

/// The compiled `filter-tcp-gate` component bytes — the outbound-TCP example (a raw-TCP consult
/// gate), built by this crate's `build.rs` when the `outbound-tcp` feature is on (ADR 000060).
#[cfg(feature = "outbound-tcp")]
pub fn filter_tcp_gate_component() -> Vec<u8> {
    std::fs::read(env!("FILTER_TCP_GATE_COMPONENT")).expect("read filter-tcp-gate component")
}

/// A minimal in-toto-style SBOM statement that binds `component`: its `subject` digest is
/// `sha256(component)`, satisfying the load gate's SBOM↔component binding (review f000003
/// #1). The predicate is empty (content policy is deferred). Test / dev helper — real
/// attestations come from `cosign attest`.
pub fn bound_sbom(component: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(component));
    format!(
        r#"{{"_type":"https://in-toto.io/Statement/v1","subject":[{{"name":"filter","digest":{{"sha256":"{digest}"}}}}],"predicateType":"https://cyclonedx.org/bom","predicate":{{}}}}"#
    )
    .into_bytes()
}
