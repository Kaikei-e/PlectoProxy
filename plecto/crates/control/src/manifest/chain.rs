//! The default `[chain]` (single ordered chain, v0.1).

use serde::{Deserialize, Serialize};

/// The single ordered chain for v0.1 (named chains / route matching are deferred to the
/// fast-path server).
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Chain {
    #[serde(default)]
    pub filters: Vec<String>,
}
