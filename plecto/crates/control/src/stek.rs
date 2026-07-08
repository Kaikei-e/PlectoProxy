//! Shared session-ticket keys (STEK) with cert binding (ADR 000062): the opt-in `[resumption]`
//! replacement for the per-node ticketer (ADR 000052). N replicas pointed at one 64-byte random
//! key file derive identical ticket keys, so a ticket issued by any replica resumes on all —
//! restoring the resumption hit rate a round-robin LB destroys — while HKDF binds every derived
//! key to the cert set the deployment serves, so deployments with different certs cannot accept
//! each other's tickets even over a shared file (the USENIX'25 "STEK Sharing is Not Caring"
//! cross-listener class; nginx CVE-2025-23419 / Apache CVE-2025-23048 are the config-mistake
//! shape this kills structurally).
//!
//! Ticket construction is the RFC 5077 §4 general form with the MAC widened to the full record:
//! `key_name(16) ‖ IV(16) ‖ AES-256-CBC/PKCS#7 ciphertext ‖ HMAC-SHA-256(key_name ‖ IV ‖ ct)`,
//! encrypt-then-MAC. CBC+HMAC over an AES-GCM AEAD is deliberate, mirroring rustls' own move
//! (rustls#2023): the construction is key-committing — a ticket cannot be crafted to verify under
//! two candidate keys (no partitioning oracle even if an operator's key file is weaker than
//! required) — and random-IV CBC has no nonce-reuse cliff when many replicas seal high volumes
//! under one shared key. All primitives are aws-lc-rs (ADR 000051, one crypto backend).
//!
//! Key schedule (RFC 5869 extract-then-expand, all outputs from one file read):
//! `PRK = HKDF-Extract(salt = "plecto-stek-v1", IKM = file bytes)`, then
//! `HKDF-Expand(PRK, info = cert_binding ‖ purpose)` for the AES key, the MAC key, and the
//! public `key_name` — replicas agree on all three without coordination, and the `info` binding
//! (RFC 5869 §3.2) is what makes the keys cert-set-specific. Rotation is external (write a new
//! file); the ticketer re-reads lazily (throttled, on handshake traffic), keeps the previous
//! keys so mixed-fleet rotation stays seamless, and fail-closes to full handshakes — never to
//! plaintext, never to a stale-forever key — when the file exceeds `max_age_hours`, vanishes, or
//! loosens its permissions (availability is untouched; only resumption stops).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use aws_lc_rs::cipher::{
    AES_256, DecryptionContext, EncryptionContext, PaddedBlockDecryptingKey,
    PaddedBlockEncryptingKey, UnboundCipherKey,
};
use aws_lc_rs::iv::FixedLength;
use aws_lc_rs::{hkdf, hmac, rand};
use rustls::server::ProducesTickets;

use crate::error::ControlError;
use crate::manifest::Resumption;

/// The key file is exactly this many raw random bytes (`openssl rand 64 > stek.key`). A strict
/// length is the fail-closed guard the format affords: a config file, a PEM, or a truncated
/// write is rejected instead of silently becoming weak input keying material.
pub(crate) const STEK_FILE_LEN: usize = 64;

const KEY_NAME_LEN: usize = 16;
const IV_LEN: usize = 16;
const AES_KEY_LEN: usize = 32;
const TAG_LEN: usize = 32; // HMAC-SHA-256
/// PKCS#7 always pads, so the ciphertext is at least one block.
const MIN_TICKET_LEN: usize = KEY_NAME_LEN + IV_LEN + 16 + TAG_LEN;

/// Domain-separation salt for the key schedule; bump the suffix if the construction ever changes.
const HKDF_SALT: &[u8] = b"plecto-stek-v1";

/// How often at most the key file is re-examined. The check rides handshake traffic (no watcher
/// thread — control stays sync), so this bounds stat/read cost per replica, not reload latency:
/// an idle listener re-reads on its next handshake.
const RECHECK_INTERVAL: Duration = Duration::from_secs(10);

/// One derivation epoch: everything HKDF yields from one (file contents, cert binding) input.
struct DerivedKeys {
    /// Public key-selector carried in each ticket (RFC 5077 §4 `key_name`) — derived, so all
    /// replicas on the same file + certs agree without coordination.
    key_name: [u8; KEY_NAME_LEN],
    enc: PaddedBlockEncryptingKey,
    dec: PaddedBlockDecryptingKey,
    mac: hmac::Key,
    /// SHA-256 of the file bytes these keys came from — rotation detection (content, not mtime,
    /// so filesystem timestamp granularity cannot mask a quick rotation).
    ikm_digest: [u8; 32],
    /// The file's mtime at derivation — drives the `max_age_hours` fail-close.
    mtime: SystemTime,
}

impl DerivedKeys {
    fn fresh(&self, max_age: Duration) -> bool {
        // An mtime in the future (clock skew during rotation) errors here; treat it as fresh —
        // the operator just rotated, which is the opposite of stale.
        SystemTime::now()
            .duration_since(self.mtime)
            .map(|age| age <= max_age)
            .unwrap_or(true)
    }
}

/// Current + previous derivation epochs. `current == None` is the fail-closed state (file
/// unusable): no tickets issued, none accepted, full handshakes only.
struct State {
    current: Option<DerivedKeys>,
    previous: Option<DerivedKeys>,
}

/// The shared-STEK `ProducesTickets` (ADR 000062). Built per config build; because every key is
/// a pure function of (file contents, cert set), a manifest reload with unchanged inputs derives
/// the identical keys — outstanding tickets survive reloads without any process-lifetime state
/// (determinism replaces ADR 000052's `OnceLock`). Only the previous-epoch memory is lost on a
/// rebuild, degrading mid-rotation tickets to a full handshake (safe).
pub(crate) struct SharedStekTicketer {
    path: PathBuf,
    max_age: Duration,
    /// The cert-set binding (ADR 000062 (a)): fed to every HKDF-Expand `info`.
    cert_binding: [u8; 32],
    /// Advertised `ticket_lifetime`: `max_age_hours`, so a ticket never claims to outlive the
    /// key that sealed it (the acceptance window is bounded by the same knob).
    lifetime_secs: u32,
    recheck: Duration,
    /// Seconds-since-epoch of the last file examination (throttle for the lazy reload).
    last_check: AtomicU64,
    state: parking_lot::RwLock<State>,
}

impl std::fmt::Debug for SharedStekTicketer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedStekTicketer")
            .field("path", &self.path)
            .field("max_age", &self.max_age)
            .finish_non_exhaustive()
    }
}

impl SharedStekTicketer {
    /// Build from the manifest's `[resumption]` (ADR 000062). Fail-closed at build: a missing /
    /// wrong-length / group-or-other-readable key file is a `ControlError` that aborts the load,
    /// exactly like a bad `[[tls]]` cert. A file that is merely OLDER than `max_age_hours` does
    /// NOT abort — per the ADR, key expiry stops resumption (full handshakes), not the proxy.
    pub(crate) fn from_manifest(
        resumption: &Resumption,
        base_dir: &Path,
        cert_binding: [u8; 32],
    ) -> Result<Arc<Self>, ControlError> {
        resumption.validate()?;
        Self::new(
            base_dir.join(&resumption.stek_file),
            Duration::from_secs(u64::from(resumption.max_age_hours) * 3600),
            cert_binding,
            RECHECK_INTERVAL,
        )
        .map(Arc::new)
    }

    /// `recheck` is injected so tests can force every call to re-examine the file.
    fn new(
        path: PathBuf,
        max_age: Duration,
        cert_binding: [u8; 32],
        recheck: Duration,
    ) -> Result<Self, ControlError> {
        let ticketer = Self {
            lifetime_secs: u32::try_from(max_age.as_secs()).unwrap_or(u32::MAX),
            path,
            max_age,
            cert_binding,
            recheck,
            last_check: AtomicU64::new(now_epoch_secs()),
            state: parking_lot::RwLock::new(State {
                current: None,
                previous: None,
            }),
        };
        let keys = read_and_derive(&ticketer.path, &ticketer.cert_binding).map_err(|reason| {
            ControlError::Stek {
                path: ticketer.path.display().to_string(),
                reason,
            }
        })?;
        if !keys.fresh(ticketer.max_age) {
            tracing::warn!(
                path = %ticketer.path.display(),
                "shared STEK file older than max_age_hours; resumption stays off until it rotates"
            );
        }
        ticketer.state.write().current = Some(keys);
        Ok(ticketer)
    }

    /// The lazy mtime/content watch (ADR 000062 (d)): at most once per `recheck`, re-read the
    /// file. Changed content shifts current → previous (the acceptance window across an external
    /// rotation); any anomaly — unreadable, wrong length, loosened permissions — drops ALL keys,
    /// fail-closing resumption until the file recovers (it re-heals on a later check).
    fn maybe_reload(&self) {
        let now = now_epoch_secs();
        let last = self.last_check.load(Ordering::Relaxed);
        if now.saturating_sub(last) < self.recheck.as_secs() {
            return;
        }
        // One thread wins the window; losers skip (the winner's result is imminent).
        if self
            .last_check
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        match read_and_derive(&self.path, &self.cert_binding) {
            Ok(keys) => {
                let mut state = self.state.write();
                match &state.current {
                    Some(current) if current.ikm_digest == keys.ikm_digest => {
                        // Same key material; refresh the mtime so `touch`-style rotation
                        // tooling does not spuriously age the keys out.
                        if let Some(current) = state.current.as_mut() {
                            current.mtime = keys.mtime;
                        }
                    }
                    Some(_) => {
                        tracing::info!(
                            path = %self.path.display(),
                            "shared STEK rotated; previous keys stay accepted until the next rotation or max_age"
                        );
                        state.previous = state.current.take();
                        state.current = Some(keys);
                    }
                    None => {
                        tracing::info!(
                            path = %self.path.display(),
                            "shared STEK file recovered; resumption re-enabled"
                        );
                        state.current = Some(keys);
                    }
                }
            }
            Err(reason) => {
                let mut state = self.state.write();
                if state.current.is_some() || state.previous.is_some() {
                    tracing::warn!(
                        path = %self.path.display(),
                        reason,
                        "shared STEK file unusable; resumption fail-closed to full handshakes"
                    );
                }
                state.current = None;
                state.previous = None;
            }
        }
    }
}

impl ProducesTickets for SharedStekTicketer {
    fn enabled(&self) -> bool {
        self.maybe_reload();
        self.state
            .read()
            .current
            .as_ref()
            .is_some_and(|keys| keys.fresh(self.max_age))
    }

    fn lifetime(&self) -> u32 {
        self.lifetime_secs
    }

    fn encrypt(&self, plain: &[u8]) -> Option<Vec<u8>> {
        self.maybe_reload();
        let state = self.state.read();
        let keys = state.current.as_ref().filter(|k| k.fresh(self.max_age))?;

        let mut iv = [0u8; IV_LEN];
        rand::fill(&mut iv).ok()?;
        let mut ct = plain.to_vec();
        keys.enc
            .less_safe_encrypt(&mut ct, EncryptionContext::Iv128(FixedLength::from(iv)))
            .ok()?;

        let mut out = Vec::with_capacity(KEY_NAME_LEN + IV_LEN + ct.len() + TAG_LEN);
        out.extend_from_slice(&keys.key_name);
        out.extend_from_slice(&iv);
        out.extend_from_slice(&ct);
        let tag = hmac::sign(&keys.mac, &out);
        out.extend_from_slice(tag.as_ref());
        Some(out)
    }

    fn decrypt(&self, cipher: &[u8]) -> Option<Vec<u8>> {
        self.maybe_reload();
        let state = self.state.read();
        // key_name selects the epoch (it is public data, plain comparison is fine); the MAC —
        // constant-time via `hmac::verify` — authenticates before any decryption touches CBC.
        [state.current.as_ref(), state.previous.as_ref()]
            .into_iter()
            .flatten()
            .filter(|keys| keys.fresh(self.max_age))
            .find(|keys| cipher.get(..KEY_NAME_LEN) == Some(&keys.key_name[..]))
            .and_then(|keys| open_ticket(keys, cipher))
    }
}

/// Verify-then-decrypt one ticket under one epoch's keys. Attacker-controlled input: every parse
/// is bounds-checked, every failure is `None` (a full handshake), nothing panics.
fn open_ticket(keys: &DerivedKeys, cipher: &[u8]) -> Option<Vec<u8>> {
    if cipher.len() < MIN_TICKET_LEN {
        return None;
    }
    // checked split (bp-rust: no panic on the data plane) — already implied by the length guard
    // above, but the decrypt path must not rely on a distant invariant to stay panic-free.
    let (body, tag) = cipher
        .len()
        .checked_sub(TAG_LEN)
        .and_then(|at| cipher.split_at_checked(at))?;
    hmac::verify(&keys.mac, body, tag).ok()?;
    let iv: [u8; IV_LEN] = body
        .get(KEY_NAME_LEN..KEY_NAME_LEN + IV_LEN)?
        .try_into()
        .ok()?;
    let ct = body.get(KEY_NAME_LEN + IV_LEN..)?;
    let mut buf = ct.to_vec();
    let plain_len = keys
        .dec
        .decrypt(&mut buf, DecryptionContext::Iv128(FixedLength::from(iv)))
        .ok()?
        .len();
    buf.truncate(plain_len);
    Some(buf)
}

/// Read + validate the key file and run the key schedule. `Err` carries the operator-facing
/// reason; the caller decides whether it aborts a build (startup) or fail-closes (runtime).
fn read_and_derive(path: &Path, cert_binding: &[u8; 32]) -> Result<DerivedKeys, String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("stat failed: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "permissions {:03o} allow group/other access — chmod 600 (fail-closed, key material)",
                mode & 0o777
            ));
        }
    }
    let mtime = metadata
        .modified()
        .map_err(|e| format!("mtime unavailable: {e}"))?;
    let bytes = std::fs::read(path).map_err(|e| format!("read failed: {e}"))?;
    if bytes.len() != STEK_FILE_LEN {
        return Err(format!(
            "must be exactly {STEK_FILE_LEN} raw random bytes (openssl rand {STEK_FILE_LEN}), got {}",
            bytes.len()
        ));
    }
    derive(&bytes, cert_binding, mtime)
}

/// The key schedule: one HKDF-Extract, three labelled Expands. Every output is bound to the cert
/// set through `info` (RFC 5869 §3.2) — that binding, not configuration, is what stops a ticket
/// sealed for one cert set from opening under another.
fn derive(ikm: &[u8], cert_binding: &[u8; 32], mtime: SystemTime) -> Result<DerivedKeys, String> {
    let ikm_digest: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(ikm).into();

    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, HKDF_SALT).extract(ikm);
    let mut aes_key = [0u8; AES_KEY_LEN];
    expand(&prk, cert_binding, b"aes-256-cbc-key", &mut aes_key)?;
    let mut mac_key = [0u8; 32];
    expand(&prk, cert_binding, b"hmac-sha-256-key", &mut mac_key)?;
    let mut key_name = [0u8; KEY_NAME_LEN];
    expand(&prk, cert_binding, b"key-name", &mut key_name)?;

    let enc = UnboundCipherKey::new(&AES_256, &aes_key)
        .and_then(PaddedBlockEncryptingKey::cbc_pkcs7)
        .map_err(|_| "AES encrypt key init failed".to_string())?;
    let dec = UnboundCipherKey::new(&AES_256, &aes_key)
        .and_then(PaddedBlockDecryptingKey::cbc_pkcs7)
        .map_err(|_| "AES decrypt key init failed".to_string())?;
    let mac = hmac::Key::new(hmac::HMAC_SHA256, &mac_key);
    Ok(DerivedKeys {
        key_name,
        enc,
        dec,
        mac,
        ikm_digest,
        mtime,
    })
}

fn expand(
    prk: &hkdf::Prk,
    cert_binding: &[u8],
    label: &[u8],
    out: &mut [u8],
) -> Result<(), String> {
    struct OkmLen(usize);
    impl hkdf::KeyType for OkmLen {
        fn len(&self) -> usize {
            self.0
        }
    }
    prk.expand(&[cert_binding, label], OkmLen(out.len()))
        .and_then(|okm| okm.fill(out))
        .map_err(|_| format!("HKDF expand failed for {}", String::from_utf8_lossy(label)))
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh 64-byte key file with owner-only permissions in a temp dir.
    fn stek_file(dir: &Path, bytes: &[u8]) -> PathBuf {
        let path = dir.join("stek.key");
        write_key(&path, bytes);
        path
    }

    fn write_key(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn ticketer(path: PathBuf, binding: [u8; 32]) -> SharedStekTicketer {
        // recheck = 0: every call re-examines the file, so rotation tests need no sleeping.
        SharedStekTicketer::new(path, Duration::from_secs(3600), binding, Duration::ZERO).unwrap()
    }

    #[test]
    fn two_replicas_on_one_file_derive_interchangeable_keys() {
        // The whole point of ADR 000062: independently-built ticketers (two replicas, or one
        // replica across a manifest reload) seal/open each other's tickets.
        let dir = tempfile::tempdir().unwrap();
        let path = stek_file(dir.path(), &[7u8; STEK_FILE_LEN]);
        let a = ticketer(path.clone(), [1; 32]);
        let b = ticketer(path, [1; 32]);
        let ticket = a.encrypt(b"session-state").unwrap();
        assert_eq!(
            b.decrypt(&ticket).as_deref(),
            Some(&b"session-state"[..]),
            "a ticket sealed by one replica opens on another (same file, same certs)"
        );
    }

    #[test]
    fn cert_binding_isolates_deployments_sharing_a_file() {
        // ADR 000062 (a): the SAME key file with a DIFFERENT cert set derives disjoint keys —
        // the CVE-2025-23419 / 23048 crossing shape dies in the key schedule, not in config.
        let dir = tempfile::tempdir().unwrap();
        let path = stek_file(dir.path(), &[7u8; STEK_FILE_LEN]);
        let a = ticketer(path.clone(), [1; 32]);
        let b = ticketer(path, [2; 32]);
        let ticket = a.encrypt(b"session-state").unwrap();
        assert_eq!(
            b.decrypt(&ticket),
            None,
            "a different cert set must not accept the ticket"
        );
        // Even the public key_name differs — the ticket is not merely rejected at the MAC, it
        // never selects a key (silent mismatch, no oracle).
        assert_ne!(
            ticket[..KEY_NAME_LEN],
            b.encrypt(b"x").unwrap()[..KEY_NAME_LEN],
            "different cert sets derive different key_names"
        );
    }

    #[test]
    fn rotation_keeps_previous_keys_accepted_and_expires_them_next_round() {
        let dir = tempfile::tempdir().unwrap();
        let path = stek_file(dir.path(), &[7u8; STEK_FILE_LEN]);
        let t = ticketer(path.clone(), [1; 32]);
        let old_ticket = t.encrypt(b"old").unwrap();

        // External rotation #1: previous stays accepted (mixed-fleet grace window).
        write_key(&path, &[8u8; STEK_FILE_LEN]);
        let new_ticket = t.encrypt(b"new").unwrap();
        assert_eq!(
            t.decrypt(&old_ticket).as_deref(),
            Some(&b"old"[..]),
            "a pre-rotation ticket still opens (previous keys held)"
        );
        assert_ne!(
            old_ticket[..KEY_NAME_LEN],
            new_ticket[..KEY_NAME_LEN],
            "new tickets carry the new epoch's key_name"
        );

        // Rotation #2: the original epoch falls off the window.
        write_key(&path, &[9u8; STEK_FILE_LEN]);
        assert_eq!(
            t.decrypt(&old_ticket),
            None,
            "two rotations later the original epoch is gone"
        );
        assert_eq!(t.decrypt(&new_ticket).as_deref(), Some(&b"new"[..]));
    }

    #[test]
    fn vanished_or_invalid_file_fail_closes_then_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = stek_file(dir.path(), &[7u8; STEK_FILE_LEN]);
        let t = ticketer(path.clone(), [1; 32]);
        let ticket = t.encrypt(b"s").unwrap();

        std::fs::remove_file(&path).unwrap();
        assert!(!t.enabled(), "a vanished key file disables resumption");
        assert_eq!(t.encrypt(b"s"), None, "no tickets issued while fail-closed");
        assert_eq!(t.decrypt(&ticket), None, "none accepted either");

        write_key(&path, &[7u8; STEK_FILE_LEN]);
        assert!(t.enabled(), "the file recovering re-enables resumption");
        assert_eq!(
            t.decrypt(&ticket).as_deref(),
            Some(&b"s"[..]),
            "same key material derives the same keys — the old ticket opens again"
        );
    }

    #[cfg(unix)]
    #[test]
    fn group_readable_file_is_rejected_at_build() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = stek_file(dir.path(), &[7u8; STEK_FILE_LEN]);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let err = SharedStekTicketer::new(path, Duration::from_secs(3600), [1; 32], Duration::ZERO)
            .expect_err("group-readable key material must fail the build");
        assert!(matches!(err, ControlError::Stek { .. }));
    }

    #[test]
    fn wrong_length_file_is_rejected_at_build() {
        let dir = tempfile::tempdir().unwrap();
        let path = stek_file(dir.path(), &[7u8; 48]); // nginx-sized, not ours
        let err = SharedStekTicketer::new(path, Duration::from_secs(3600), [1; 32], Duration::ZERO)
            .expect_err("a non-64-byte file must fail the build");
        match err {
            ControlError::Stek { reason, .. } => {
                assert!(reason.contains("64"), "reason names the expected length")
            }
            other => panic!("expected Stek, got {other}"),
        }
    }

    #[test]
    fn expired_key_material_stops_resumption_but_not_construction() {
        // ADR 000062 (d): max_age exceeded → resumption stops (full handshakes), availability
        // stays. Build with max_age = 0h is invalid at the manifest layer, so age the file
        // instead: set mtime a day back with a 1h max_age.
        let dir = tempfile::tempdir().unwrap();
        let path = stek_file(dir.path(), &[7u8; STEK_FILE_LEN]);
        let old = SystemTime::now() - Duration::from_secs(24 * 3600);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(old).unwrap();
        drop(file);

        let t = ticketer(path, [1; 32]);
        assert!(!t.enabled(), "expired key material means no resumption");
        assert_eq!(t.encrypt(b"s"), None);
    }

    #[test]
    fn tampering_any_byte_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = stek_file(dir.path(), &[7u8; STEK_FILE_LEN]);
        let t = ticketer(path, [1; 32]);
        let ticket = t.encrypt(b"session-state").unwrap();
        for i in 0..ticket.len() {
            let mut forged = ticket.clone();
            forged[i] ^= 0x01;
            assert_eq!(
                t.decrypt(&forged),
                None,
                "flipping byte {i} must fail authentication"
            );
        }
    }

    #[test]
    fn lifetime_advertises_max_age() {
        let dir = tempfile::tempdir().unwrap();
        let path = stek_file(dir.path(), &[7u8; STEK_FILE_LEN]);
        let t = SharedStekTicketer::new(
            path,
            Duration::from_secs(24 * 3600),
            [1; 32],
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(t.lifetime(), 24 * 3600, "ticket_lifetime hint = max_age");
    }

    proptest::proptest! {
        /// Roundtrip over arbitrary plaintext sizes (rustls session values vary with SNI,
        /// ALPN, cert chains): seal-then-open is the identity.
        #[test]
        fn seal_open_roundtrip(plain in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..2048)) {
            let dir = tempfile::tempdir().unwrap();
            let path = stek_file(dir.path(), &[7u8; STEK_FILE_LEN]);
            let t = ticketer(path, [1; 32]);
            let ticket = t.encrypt(&plain).unwrap();
            proptest::prop_assert_eq!(t.decrypt(&ticket), Some(plain));
        }

        /// `decrypt` is the attacker-facing surface (rustls: "fully attacker controlled ...
        /// panic-proof"): arbitrary bytes never panic and never authenticate.
        #[test]
        fn arbitrary_bytes_never_decrypt_or_panic(garbage in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)) {
            let dir = tempfile::tempdir().unwrap();
            let path = stek_file(dir.path(), &[7u8; STEK_FILE_LEN]);
            let t = ticketer(path, [1; 32]);
            proptest::prop_assert_eq!(t.decrypt(&garbage), None);
        }

        /// The key schedule is a pure function of (file, cert binding): same inputs → same
        /// keys; different binding → different key_name (the schedule, pinned).
        #[test]
        fn key_schedule_is_deterministic_and_binding_sensitive(
            ikm in proptest::collection::vec(proptest::prelude::any::<u8>(), STEK_FILE_LEN..=STEK_FILE_LEN),
            binding_a in proptest::prelude::any::<[u8; 32]>(),
            binding_b in proptest::prelude::any::<[u8; 32]>(),
        ) {
            let now = SystemTime::now();
            let a1 = derive(&ikm, &binding_a, now).unwrap();
            let a2 = derive(&ikm, &binding_a, now).unwrap();
            proptest::prop_assert_eq!(a1.key_name, a2.key_name, "deterministic");
            if binding_a != binding_b {
                let b = derive(&ikm, &binding_b, now).unwrap();
                proptest::prop_assert_ne!(a1.key_name, b.key_name, "binding-sensitive");
            }
        }
    }
}
