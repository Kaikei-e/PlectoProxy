//! One TLS server certificate (`[[tls]]`, ADR 000014).

use serde::{Deserialize, Serialize};

/// One TLS server certificate (ADR 000014). The fast path terminates TLS with rustls and selects
/// a cert by SNI: `host` names the SNI this cert serves (case-insensitive); `None` is the default
/// cert presented when no SNI matches. `cert_path` / `key_path` are manifest-relative PEM files
/// (a cert chain and its private key). Both the paths and the referenced files' BYTES ride the
/// reload gate, so an in-place renewal (a certbot deploy hook) reloads under a bare SIGHUP: the
/// cert digest rides the public config version, the key digest the unlogged reload fingerprint
/// (`manifest::content_hash`).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsCert {
    /// SNI host this cert serves (case-insensitive). `None` = the default cert.
    #[serde(default)]
    pub host: Option<String>,
    /// Manifest-relative path to the PEM cert chain.
    pub cert_path: String,
    /// Manifest-relative path to the PEM private key.
    pub key_path: String,
}
