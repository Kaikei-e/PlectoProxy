//! Trust roots (`[trust]`, ADR 000006).

use serde::{Deserialize, Serialize};

/// Trust roots: paths (manifest-relative) to trusted signer public keys, PEM (ADR 000006).
/// `PartialEq` lets `reload` detect a trust-section change (which it rejects — trust is fixed
/// at construction, f000004 #1).
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Trust {
    #[serde(default)]
    pub keys: Vec<String>,
}
