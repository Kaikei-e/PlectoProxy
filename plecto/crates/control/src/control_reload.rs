//! `Control`'s reload surface (ADR 000007 / 000008): atomically swap to a new manifest, or
//! re-read the on-disk manifest and reload if its `config version` changed. Split out from
//! `lib.rs` since this is the crate's most correctness-sensitive method group (trust-change
//! rejection, all-or-nothing swap) — its own file matches "where's the thing that does X".

use std::sync::Arc;

use crate::error::ControlError;
use crate::manifest::Manifest;
use crate::reload::ReloadOutcome;
use crate::{Control, build_active, read_manifest};

impl Control {
    /// Atomically swap to a new manifest's filter set + chain (ADR 000007: build the new set
    /// fully, then switch in one store; the old set is drained as its `Arc` refs drop). If any
    /// filter fails to resolve / verify / load, the swap does **not** happen and the current
    /// set stays live — reload is all-or-nothing. The trust policy is fixed at construction.
    pub fn reload(&self, manifest: &Manifest) -> Result<(), ControlError> {
        let _gate = self.reload_gate.lock();
        self.ensure_trust_unchanged(manifest)?;
        self.ensure_state_unchanged(manifest)?;
        let active = build_active(
            &self.host,
            manifest,
            self.store.as_ref(),
            &self.base_dir,
            &self.upstreams,
        )?;
        self.active.store(Arc::new(active));
        Ok(())
    }

    /// Reject a reload whose manifest changes the `[trust]` section (f000004 #1). Trust roots
    /// are fixed for the life of the `Host` / epoch ticker; an operator rotates them by
    /// restarting with the new manifest, not by reloading — otherwise a trust-only edit would
    /// flip the content hash and be reported as a successful reload while having no effect.
    pub(crate) fn ensure_trust_unchanged(&self, manifest: &Manifest) -> Result<(), ControlError> {
        if manifest.trust != self.trust {
            return Err(ControlError::TrustChangeRequiresRestart);
        }
        Ok(())
    }

    /// Reject a reload whose manifest changes the `[state]` section (ADR 000041). The state
    /// backend is fixed for the life of the `Host` — same contract, same reasoning as
    /// `ensure_trust_unchanged`: a silently-dropped backend/path edit would read as a
    /// successful durability change that never happened.
    pub(crate) fn ensure_state_unchanged(&self, manifest: &Manifest) -> Result<(), ControlError> {
        if manifest.state != self.state {
            return Err(ControlError::StateChangeRequiresRestart);
        }
        Ok(())
    }

    /// Re-read the on-disk manifest and reload if its `config version` changed. The trigger
    /// (SIGHUP, `serve_reloads`) is content-free, so this is where the new config is actually
    /// read. Idempotent: an unchanged manifest (same semantic `content_hash`) is a no-op —
    /// no rebuild, no drain. A changed one is built fully and swapped atomically; on any
    /// build failure the running set is left untouched (fail-closed) and the error returned.
    ///
    /// Errors with `NoManifestPath` if this plane was not built from an on-disk manifest
    /// (`load` / `from_manifest`); use `from_manifest_path` / `load_at` for a reloadable plane.
    pub fn reload_from_disk(&self) -> Result<ReloadOutcome, ControlError> {
        let _gate = self.reload_gate.lock();
        let path = self
            .manifest_path
            .as_ref()
            .ok_or(ControlError::NoManifestPath)?;
        let manifest = read_manifest(path)?;
        // A [trust] or [state] change is rejected before anything else: it must never be
        // reported as a successful reload (f000004 #1 / ADR 000041), even though it would flip
        // the content hash below.
        self.ensure_trust_unchanged(&manifest)?;
        self.ensure_state_unchanged(&manifest)?;
        // Cheap idempotency gate: skip the rebuild + drain entirely when neither half of the
        // active config's identity moved (a comment-only edit, or a spurious trigger) — the
        // public `config version` and the never-logged reload fingerprint. A half that cannot be
        // computed (a referenced file momentarily unreadable, e.g. mid-rotation) falls through to
        // the full build instead of failing a possibly-idempotent SIGHUP outright — the build
        // re-reads and fails closed with the precise error if the problem persists.
        let active = self.active.load();
        match manifest.content_hash_at(Some(&self.base_dir)) {
            Ok(new_hash) if new_hash == active.hash => {
                match manifest.reload_fingerprint(&self.base_dir, &new_hash) {
                    // The fingerprint stays out of the event on purpose (see `ActiveConfig`).
                    Ok(fingerprint) if fingerprint == active.reload_fingerprint => {
                        tracing::info!(
                            config_version = %new_hash,
                            "reload: config version and referenced files unchanged — no rebuild"
                        );
                        return Ok(ReloadOutcome::Unchanged);
                    }
                    Ok(_) => {
                        tracing::info!(
                            config_version = %new_hash,
                            "reload: config version unchanged but a referenced file rotated — \
                             rebuilding"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "reload: referenced files unreadable — attempting the full rebuild"
                        );
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "reload: config version unavailable — attempting the full rebuild"
                );
            }
        }
        drop(active);
        // Build the new set fully before swapping; on failure the running set is untouched. The
        // outcome carries the hash the build computed (from the SAME CA read as its verifier),
        // not the gate's — the two could differ if the CA file changed in between.
        let active = build_active(
            &self.host,
            &manifest,
            self.store.as_ref(),
            &self.base_dir,
            &self.upstreams,
        )?;
        let hash = active.hash.clone();
        self.active.store(Arc::new(active));
        Ok(ReloadOutcome::Reloaded { hash })
    }

    /// The active config's `content_hash` (ADR 000008 `config version`): the audit identity of
    /// what is loaded right now, and the unit a future opt-in consensus layer would agree on.
    pub fn config_version(&self) -> String {
        self.active.load().hash.clone()
    }
}
