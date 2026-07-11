//! Error types surfaced by the host: [`RunError`] (a per-request filter-call failure) and
//! [`LoadError`] (why `Host::load` rejected a filter), plus the SBOM↔component binding check.

use sha2::{Digest, Sha256};

use crate::{Header, HttpResponse};

/// Why a per-request filter call did not produce a `decision`. Kept deliberately distinct
/// from `RequestDecision`/`ResponseDecision` — those are the filter's *intentional* typed
/// output; a `RunError` is the filter *failing*. The fast path MUST fail-closed on it:
/// synthesise an error response and never forward to upstream (CLAUDE.md — no fail-open).
/// Keeping the two apart also makes "deadline" vs "trap" an observable health signal.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The filter ran past its epoch deadline (ADR 000006 metering) and was interrupted.
    /// Fail-closed mapping: 504.
    #[error("filter exceeded its epoch deadline")]
    Deadline,
    /// The filter trapped (`unreachable`, a guest panic, or an allocation past the Store
    /// memory limit that aborted the guest). Fail-closed mapping: 502.
    #[error("filter trapped: {0}")]
    Trap(anyhow::Error),
    /// A fresh instance could not be created — untrusted per-request instantiation, or the
    /// rebuild of a trusted instance after a prior trap. Fail-closed mapping: 502.
    #[error("filter instantiation failed: {0}")]
    Instantiate(anyhow::Error),
    /// A trusted filter trapped on several consecutive requests, so the host is in a short
    /// trap-cooldown: it returns this cheap fail-closed response instead of re-instantiating +
    /// re-init'ing every request (circuit-breaker, review f000003 #5). Fail-closed mapping: 503.
    #[error("filter is in trap-cooldown (circuit open)")]
    Unavailable,
    /// The guest returned cleanly but its output violates contract rules (CRLF / CTL / non-tchar
    /// header, oversize header, or oversize synthesised body — ADR 000071 / 000073). Distinct
    /// from `Trap` so operators can tell a misbehaving-but-alive filter from a crashing one;
    /// fail-closed mapping is 502.
    #[error("filter returned invalid output (malformed header or oversize synthesised body)")]
    InvalidOutput,
}

/// Marker the runtime wraps in a `wasmtime::Error` when guest output fails validation, so
/// [`RunError::from_call`] can classify it as [`RunError::InvalidOutput`] instead of a trap.
#[derive(Debug, thiserror::Error)]
#[error("filter returned invalid output; failing closed")]
pub(crate) struct InvalidGuestOutput;

impl RunError {
    /// Classify the error from a guest call: an epoch interrupt is a `Deadline`, a validation
    /// marker is `InvalidOutput`, anything else is a `Trap`. (`wasmtime 45` returns its own
    /// `wasmtime::Error`, distinct from `anyhow::Error`; we convert into `anyhow::Error` for
    /// storage.)
    pub(crate) fn from_call(e: wasmtime::Error) -> Self {
        if e.downcast_ref::<InvalidGuestOutput>().is_some() {
            return RunError::InvalidOutput;
        }
        match e.downcast_ref::<wasmtime::Trap>() {
            Some(wasmtime::Trap::Interrupt) => RunError::Deadline,
            _ => RunError::Trap(anyhow::Error::from(e)),
        }
    }

    /// A synthetic, fail-closed response for this fault (host helper; the fast path may send
    /// it directly). Deadline → 504, every other fault → 502. Never a pass-through.
    pub fn fail_closed_response(&self) -> HttpResponse {
        let (status, fault, msg): (u16, &str, &str) = match self {
            RunError::Deadline => (504, "deadline", "filter deadline exceeded"),
            RunError::Trap(_) => (502, "trap", "filter trapped"),
            RunError::Instantiate(_) => (502, "instantiate", "filter instantiation failed"),
            RunError::Unavailable => (503, "unavailable", "filter temporarily unavailable"),
            RunError::InvalidOutput => (502, "invalid-output", "filter returned invalid output"),
        };
        HttpResponse {
            status,
            headers: vec![Header {
                name: "x-plecto-fault".to_string(),
                value: fault.to_string().into_bytes(),
            }],
            body: msg.as_bytes().to_vec(),
        }
    }
}

/// Why [`Host::load`] rejected a filter (bp-rust: typed library errors, not ad hoc
/// `anyhow::ensure!`). Every variant is a fail-closed rejection at the provenance/id gate, before
/// wasmtime ever touches the component bytes — except [`LoadError::Instantiate`] /
/// [`LoadError::Wasmtime`], which surface a failure from wasmtime itself (linking / type-checking
/// / the eager trusted-instance build). `Host::load`'s public signature stays `anyhow::Result`
/// (unchanged, so `plecto-control::ControlError::Load`'s existing `anyhow::Error` passthrough
/// keeps working); callers that want the concrete variant can `downcast_ref::<LoadError>()`.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("filter id must be non-empty")]
    EmptyFilterId,
    #[error("filter id must not contain the KV namespace delimiter")]
    FilterIdContainsDelimiter,
    #[error("a signed SBOM is required to load a filter (fail-closed; ADR 000006)")]
    MissingSbom,
    #[error("component signature is not verified by any trusted key (fail-closed; ADR 000006)")]
    UnverifiedComponentSignature,
    #[error("SBOM signature is not verified by any trusted key (fail-closed; ADR 000006)")]
    UnverifiedSbomSignature,
    #[error("SBOM is not a valid in-toto statement: {0}")]
    MalformedSbom(#[source] serde_json::Error),
    #[error(
        "SBOM does not attest this component: no subject digest matches sha256(component) \
         (fail-closed; ADR 000006 / review f000003)"
    )]
    SbomNotBound,
    /// The component's decoded imports name no recognised `plecto:filter@0.N` track (absent, or
    /// a future version the host does not yet bind). Fail-closed at load — never guess V03.
    #[error(
        "component does not import a recognised plecto:filter contract version \
         (need @0.1 / @0.2 / @0.3; fail-closed)"
    )]
    UnsupportedContractVersion,
    /// The eager trusted-instance build (`Host::load`'s `Isolation::Trusted` path) failed —
    /// carries the same error `RunError::Instantiate` would for a later rebuild.
    #[error("filter instantiation failed: {0}")]
    Instantiate(anyhow::Error),
    #[error(transparent)]
    Wasmtime(#[from] wasmtime::Error),
}

/// Verify the SBOM attests THIS component: parse it as an in-toto-style statement and require
/// at least one `subject[].digest.sha256` to equal `sha256(component)`. Fail-closed on a
/// malformed SBOM or a missing / mismatched subject (review f000003 #1). Without this, a
/// validly-signed but UNRELATED SBOM could be paired with the component — harmless while the
/// SBOM is opaque, a latent gap the moment its content becomes load-bearing (CVE / license).
pub(crate) fn sbom_binds_component(
    sbom: &[u8],
    component: &[u8],
) -> std::result::Result<(), LoadError> {
    let statement: serde_json::Value =
        serde_json::from_slice(sbom).map_err(LoadError::MalformedSbom)?;
    let want = hex::encode(Sha256::digest(component));
    let bound = statement
        .get("subject")
        .and_then(|s| s.as_array())
        .is_some_and(|subjects| {
            subjects.iter().any(|subject| {
                subject
                    .get("digest")
                    .and_then(|d| d.get("sha256"))
                    .and_then(|h| h.as_str())
                    == Some(want.as_str())
            })
        });
    if bound {
        Ok(())
    } else {
        Err(LoadError::SbomNotBound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_error_maps_to_fail_closed_response() {
        // The host's synthetic responses are fail-closed (5xx), never a pass-through, and
        // distinguish a deadline (504) from any other trap (502) for observability (ADR 000006).
        let deadline = RunError::Deadline.fail_closed_response();
        assert_eq!(deadline.status, 504);
        assert!(
            deadline
                .headers
                .iter()
                .any(|h| h.name == "x-plecto-fault" && h.value.as_slice() == b"deadline")
        );

        let trap = RunError::Trap(anyhow::anyhow!("boom")).fail_closed_response();
        assert_eq!(trap.status, 502);
        assert!(
            trap.headers
                .iter()
                .any(|h| h.name == "x-plecto-fault" && h.value.as_slice() == b"trap")
        );
    }

    #[test]
    fn invalid_guest_output_is_classified_apart_from_a_trap() {
        // A guest that returns cleanly but violates the header rules (ADR 000071) must be
        // observable as `invalid-output`, not conflated with a crash.
        let e = wasmtime::Error::new(InvalidGuestOutput);
        assert!(matches!(RunError::from_call(e), RunError::InvalidOutput));

        let resp = RunError::InvalidOutput.fail_closed_response();
        assert_eq!(resp.status, 502);
        assert!(
            resp.headers
                .iter()
                .any(|h| h.name == "x-plecto-fault" && h.value.as_slice() == b"invalid-output")
        );
    }
}
