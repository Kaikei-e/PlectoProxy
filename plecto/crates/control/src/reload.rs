//! The reload-trigger seam (ADR 000008: static declarative config + hot reload). `Control`
//! already swaps the active set atomically; this module is *what tells it when to*. The
//! trigger is deliberately content-free: it says "re-read the manifest from disk", never
//! "here is the new config". That keeps Plecto on the static-manifest side of ADR 000008
//! (the operator edits the on-disk manifest and pushes a reload; there is no xDS-style
//! dynamic config push).
//!
//! `ReloadSource` is the seam (cf. `ArtifactStore`): the unix SIGHUP adapter is the real
//! one, and a test injects a fake so the loop is exercised without process-global signals.

use crate::Control;

/// Outcome of a single disk reload. A *failed* reload is not represented here — it is the
/// `Err` arm of `reload_from_disk`, and leaves the running set untouched (all-or-nothing,
/// fail-closed). The success arm distinguishes "nothing to do" from "swapped".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// The on-disk manifest is byte-for-meaning identical to the running one (same
    /// `content_hash`); no rebuild, no drain, no swap happened.
    Unchanged,
    /// A new filter set + chain was built and swapped in atomically. Carries the new
    /// `config version` (the manifest content hash) for logging / audit.
    Reloaded { hash: String },
}

/// A source of reload triggers. `recv` blocks until the next trigger fires, or returns `None`
/// when the source is exhausted / shut down (the loop then ends). Implementors carry no
/// config: a trigger only ever means "re-read the manifest now".
pub trait ReloadSource {
    /// Block until the next reload trigger; `None` ends the reload loop.
    fn recv(&mut self) -> Option<()>;
}

/// Drive `control` from a `ReloadSource`: on every trigger, re-read the on-disk manifest and
/// reload. Returns when the source is exhausted. A reload failure (parse / resolve / verify /
/// load) is logged and the **current set stays live** (fail-closed) — a bad edit never takes
/// the proxy down. An unchanged manifest is a cheap no-op.
///
/// This is a blocking loop; an embedder runs it on a dedicated thread alongside the data
/// plane. The trait makes it testable without sending real signals.
pub fn serve_reloads(control: &Control, source: &mut dyn ReloadSource) {
    while source.recv().is_some() {
        match control.reload_from_disk() {
            Ok(ReloadOutcome::Unchanged) => {
                tracing::debug!("reload trigger: manifest unchanged, keeping current set");
            }
            Ok(ReloadOutcome::Reloaded { hash }) => {
                tracing::info!(config_version = %hash, "reload: swapped to new manifest");
            }
            Err(err) => {
                // Fail-closed: keep serving the old set; surface the error, do not crash.
                tracing::error!(error = %err, "reload failed; keeping current set");
            }
        }
    }
}

/// The real trigger: SIGHUP, the operator's "I edited the manifest, pick it up" signal. Unix
/// only — signals are a unix concept. Construct once, then hand to `serve_reloads`.
#[cfg(unix)]
pub struct SignalReloadSource {
    signals: signal_hook::iterator::Signals,
}

#[cfg(unix)]
impl SignalReloadSource {
    /// Register a SIGHUP handler. Fails if the handler cannot be installed (e.g. the signal is
    /// already taken by another consumer in-process).
    pub fn sighup() -> std::io::Result<Self> {
        let signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGHUP])?;
        Ok(Self { signals })
    }
}

#[cfg(unix)]
impl ReloadSource for SignalReloadSource {
    fn recv(&mut self) -> Option<()> {
        // `forever()` blocks for the next delivered signal; pending signals are buffered in
        // `signals`, not the iterator, so recreating it per call loses nothing.
        self.signals.forever().next().map(|_| ())
    }
}
