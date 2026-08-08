//! The `[chain]` section, parsed only so validation can reject it by name (ADR 000101).

use serde::{Deserialize, Serialize};

/// The pre-`[[route]]` global chain. Nothing runs it: only a route's inline `filters` reach the
/// dispatcher, so a non-empty `filters` here is a validation error. The section stays in the
/// schema for that one purpose — a bare unknown-field error could not name where the filters go.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Chain {
    #[serde(default)]
    pub filters: Vec<String>,
}
