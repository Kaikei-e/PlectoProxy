//! Opt-in shared session-ticket keys (`[resumption]`, ADR 000062).

use serde::{Deserialize, Serialize};

use crate::error::ControlError;

/// Opt-in shared STEK mode (`[resumption]`, ADR 000062). Absent = the default per-node,
/// process-lifetime ticket key (ADR 000052). Present = every replica pointed at the same
/// `stek_file` derives the same ticket keys, so a session ticket issued by one replica resumes on
/// any other behind the same LB — WITHOUT weakening the cross-listener story: keys are derived
/// per cert set (HKDF binding, `tls.rs`), so deployments serving different certs cannot accept
/// each other's tickets even when they share the file. Rotation is an external operator step
/// (write a fresh 64-byte random file in place); `max_age_hours` bounds how long stale key
/// material keeps resuming before the proxy fail-closes to full handshakes.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Resumption {
    /// Manifest-relative path to the shared key file: exactly 64 raw random bytes
    /// (`openssl rand 64 > stek.key`), owner-only permissions. Pure HKDF input keying material —
    /// the file carries no structure; key ids and AEAD/MAC keys are all derived.
    pub stek_file: String,
    /// Hours after the key file's mtime before resumption fail-closes to full handshakes
    /// (default 24). Also advertised as the ticket lifetime hint, so a ticket never claims to
    /// outlive the key that sealed it. Capped at 168 (RFC 8446 §4.6.1: 7 days).
    #[serde(default = "default_max_age_hours")]
    pub max_age_hours: u32,
}

fn default_max_age_hours() -> u32 {
    24
}

impl Resumption {
    /// The RFC 8446 §4.6.1 ceiling on `ticket_lifetime`: 604800 seconds.
    const MAX_AGE_HOURS_CAP: u32 = 168;

    /// Fail-closed range check (the file itself is checked where it is read, `stek.rs`).
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        if self.max_age_hours == 0 || self.max_age_hours > Self::MAX_AGE_HOURS_CAP {
            return Err(ControlError::Stek {
                path: self.stek_file.clone(),
                reason: format!(
                    "max_age_hours must be 1..={} (RFC 8446 caps ticket lifetime at 7 days), got {}",
                    Self::MAX_AGE_HOURS_CAP,
                    self.max_age_hours
                ),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::manifest::Manifest;

    #[test]
    fn resumption_parses_with_default_max_age() {
        let m = Manifest::from_toml("[resumption]\nstek_file = \"tls/stek.key\"\n").unwrap();
        let r = m.resumption.expect("[resumption] parses");
        assert_eq!(r.stek_file, "tls/stek.key");
        assert_eq!(r.max_age_hours, 24, "max_age_hours defaults to 24");

        // Absent section stays None (per-node default, ADR 000052 unchanged).
        assert!(Manifest::from_toml("").unwrap().resumption.is_none());

        // deny_unknown_fields holds inside the section.
        assert!(
            Manifest::from_toml("[resumption]\nstek_files = \"x\"\n").is_err(),
            "a typo inside [resumption] is rejected"
        );
    }

    #[test]
    fn max_age_hours_is_range_checked() {
        for (hours, ok) in [(0u32, false), (1, true), (168, true), (169, false)] {
            let toml = format!("[resumption]\nstek_file = \"stek.key\"\nmax_age_hours = {hours}\n");
            let r = Manifest::from_toml(&toml).unwrap().resumption.unwrap();
            assert_eq!(
                r.validate().is_ok(),
                ok,
                "max_age_hours = {hours} should be ok={ok}"
            );
        }
    }

    #[test]
    fn resumption_rides_the_content_hash() {
        let absent = Manifest::from_toml("").unwrap();
        let shared = Manifest::from_toml("[resumption]\nstek_file = \"stek.key\"\n").unwrap();
        assert_ne!(
            absent.content_hash().unwrap(),
            shared.content_hash().unwrap(),
            "opting into shared STEK is a real config change — the hash must flip"
        );
    }

    /// ADR 000062 (b), follow-through of the pre-mTLS canary: the fail-closed crossing rule
    /// (`[resumption]` shared STEK × `[listen.client_auth]` → build error) is now implemented —
    /// pinned by `tls::tests::client_auth_with_shared_stek_fails_closed` and the load-level E2E
    /// in `plecto-server/tests/tls.rs` (resume-without-reverify amplified across replicas:
    /// nginx CVE-2025-23419, Apache CVE-2025-23048, Cloudflare's mTLS resumption incident). This
    /// test keeps guarding the SCHEMA: any NEW client-auth flavoured field may only ship after its
    /// shared-STEK crossing semantics are decided (ADR 000078), then join the sanctioned list.
    #[test]
    fn client_auth_schema_fields_stay_sanctioned() {
        // Property NAMES only — doc comments (schema descriptions) may mention mTLS freely.
        fn property_names(value: &serde_json::Value, out: &mut Vec<String>) {
            if let Some(object) = value.as_object() {
                if let Some(properties) = object.get("properties").and_then(|p| p.as_object()) {
                    out.extend(properties.keys().cloned());
                }
                for nested in object.values() {
                    property_names(nested, out);
                }
            } else if let Some(array) = value.as_array() {
                for nested in array {
                    property_names(nested, out);
                }
            }
        }
        // Each entry records WHY it is allowed to coexist with the schema's STEK fields.
        let sanctioned = [
            "client_auth",      // [listen.client_auth]: crossing rule enforced fail-closed
            "client_cert_path", // [upstream.tls]: presented identity, no ticket interaction
            "client_key_path",  // ditto
        ];
        let schema: serde_json::Value =
            serde_json::from_str(&crate::manifest_json_schema().unwrap()).unwrap();
        let mut names = Vec::new();
        property_names(&schema, &mut names);
        for name in &names {
            let lower = name.to_lowercase();
            if ["client_ca", "client_auth", "client_cert", "mtls"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                assert!(
                    sanctioned.contains(&name.as_str()),
                    "manifest schema grew an unsanctioned client-auth field {name:?} — decide \
                     its shared-STEK crossing semantics (ADR 000062 (b) / 000078) and add it to \
                     the sanctioned list"
                );
            }
        }
    }
}
