//! The host state backend (`[state]`, ADR 000041).

use serde::{Deserialize, Serialize};

/// The host state backend (`[state]`, ADR 000041): the single knob choosing where the three
/// state capabilities (`host-kv` / `host-counter` / `host-ratelimit`) keep their bytes.
/// `memory` (the default) keeps zero-config startup — state dies with the process; `redb`
/// makes it durable at `path`, so counters and rate-limit windows survive a restart
/// (fail-closed direction, ADR 000004). One backend serves all three capabilities; fixed at
/// construction like `[trust]` (`PartialEq` backs the reload rejection — restart to apply).
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct State {
    #[serde(default)]
    pub backend: StateBackendKind,
    /// Manifest-relative path of the redb database file. Required iff `backend = "redb"`;
    /// the parent directory must already exist (operator responsibility).
    #[serde(default)]
    pub path: Option<String>,
}

/// Manifest spelling of the state backend (ADR 000041). Defaults to `memory`.
#[derive(
    Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema, Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "lowercase")]
pub enum StateBackendKind {
    #[default]
    Memory,
    Redb,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    // ----- ADR 000041: [state] — host state backend selection -----

    #[test]
    fn state_defaults_memory_and_parses_redb() {
        // Absent [state] → memory, no path (zero-config startup keeps working).
        let bare = Manifest::from_toml("").unwrap();
        assert_eq!(bare.state.backend, StateBackendKind::Memory);
        assert_eq!(bare.state.path, None);

        // Explicit redb reads the knobs.
        let m = Manifest::from_toml(
            r#"
[state]
backend = "redb"
path = "state/plecto.redb"
"#,
        )
        .unwrap();
        assert_eq!(m.state.backend, StateBackendKind::Redb);
        assert_eq!(m.state.path.as_deref(), Some("state/plecto.redb"));

        // deny_unknown_fields holds inside the section.
        assert!(
            Manifest::from_toml("[state]\nbackedn = \"redb\"\n").is_err(),
            "a typo inside [state] is rejected"
        );
    }

    #[test]
    fn state_rides_the_content_hash_and_the_default_is_canonical() {
        // An explicit default ([state] backend = "memory") and an absent section are the same
        // config → same hash (determinism invariant, like an explicit isolation = "untrusted").
        let absent = Manifest::from_toml("").unwrap();
        let explicit = Manifest::from_toml("[state]\nbackend = \"memory\"\n").unwrap();
        assert_eq!(
            absent.content_hash().unwrap(),
            explicit.content_hash().unwrap(),
            "an explicit default [state] must not change the content hash"
        );

        // Choosing redb is a real config change → the hash flips. (Like [trust], the reload
        // path rejects the change before hashing; the hash still reflects config identity.)
        let redb = Manifest::from_toml("[state]\nbackend = \"redb\"\npath = \"s.redb\"\n").unwrap();
        assert_ne!(
            absent.content_hash().unwrap(),
            redb.content_hash().unwrap(),
            "a state-backend change must flip the content hash"
        );
    }
}
