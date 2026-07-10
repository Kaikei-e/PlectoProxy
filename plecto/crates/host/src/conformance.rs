//! Generic conformance checks for a `plecto:filter` component (ADR 000065): properties any
//! conformant filter can be expected to have, NOT a specific fixture's policy (the
//! `filter-hello` family's `x-plecto-block` header etc., exercised by `tests/polyglot.rs` as
//! an internal Rust regression suite). `plecto conformance <component.wasm>` is the CLI
//! surface over [`check`]. See host/CONTEXT.md "Conformant (component)".
//!
//! Each run self-signs with a fresh, throwaway [`DevSigner`] key that is never persisted —
//! NOT the same key `plecto dev` keeps at `.plecto/dev-key`. Since the CLI controls both the
//! key and the signature it produces, a load-gate failure here can only mean the component
//! itself is malformed (wrong world, bad export shape) — never a provenance problem, so
//! "loads under the plecto:filter contract" already covers both structural and load-gate
//! conformance in one observable check.

use crate::dev_signer::{DevSigner, bound_sbom};
use crate::{Header, Host, HttpRequest, LoadOptions, LoadedFilter, RequestTrace, SignedArtifact};

/// One named property, pass/fail, with a human-readable detail (the failure reason, or a short
/// confirmation on success).
pub struct ConformanceCheck {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

/// The full battery's result. `plecto conformance` exits non-zero unless
/// [`ConformanceReport::is_conformant`] is true.
pub struct ConformanceReport {
    pub checks: Vec<ConformanceCheck>,
}

impl ConformanceReport {
    pub fn is_conformant(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }
}

/// Run the generic conformance battery against raw component bytes.
pub fn check(component_bytes: &[u8]) -> ConformanceReport {
    let mut checks = Vec::new();

    let loaded = load_self_signed(component_bytes);
    checks.push(ConformanceCheck {
        name: "loads under the plecto:filter contract",
        passed: loaded.is_ok(),
        detail: match &loaded {
            Ok(_) => "component/SBOM self-signature verified, world satisfied".to_string(),
            Err(e) => format!("{e:#}"),
        },
    });

    let Ok(filter) = loaded else {
        checks.push(ConformanceCheck {
            name: "handles a generic request without trapping or exceeding its deadline",
            passed: false,
            detail: "skipped: component did not load".to_string(),
        });
        return ConformanceReport { checks };
    };

    let outcome = filter.on_request(&generic_request(), &RequestTrace::root());
    checks.push(ConformanceCheck {
        name: "handles a generic request without trapping or exceeding its deadline",
        passed: outcome.is_ok(),
        detail: match &outcome {
            Ok((decision, _logs)) => format!("responded with {}", decision_kind(decision)),
            Err(e) => e.to_string(),
        },
    });

    ConformanceReport { checks }
}

fn load_self_signed(component_bytes: &[u8]) -> anyhow::Result<LoadedFilter> {
    let (signer, _private_key_pem) = DevSigner::generate()?;
    let component_signature = signer.sign(component_bytes)?;
    let sbom = bound_sbom(component_bytes);
    let sbom_signature = signer.sign(&sbom)?;
    let host = Host::new(signer.trust_policy()?)?;
    let artifact = SignedArtifact {
        component_bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };
    // `Untrusted` (fresh-per-request, tight default deadlines): the conservative assumption for
    // an arbitrary component this CLI has never seen before.
    host.load("conformance", &artifact, LoadOptions::untrusted())
}

fn generic_request() -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: "/".to_string(),
        authority: "conformance.invalid".to_string(),
        scheme: "https".to_string(),
        headers: vec![Header {
            name: "user-agent".to_string(),
            value: b"plecto-conformance".to_vec(),
        }],
    }
}

fn decision_kind(decision: &crate::RequestDecision) -> &'static str {
    match decision {
        crate::RequestDecision::Continue => "continue",
        crate::RequestDecision::Modified(_) => "modified",
        crate::RequestDecision::ShortCircuit(_) => "short-circuit",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_real_filter_is_conformant() {
        let component = crate::test_support::filter_hello_component();
        let report = check(&component);
        for c in &report.checks {
            assert!(c.passed, "{}: {}", c.name, c.detail);
        }
        assert!(report.is_conformant());
    }

    #[test]
    fn garbage_bytes_fail_the_load_check() {
        let report = check(b"not a wasm component");
        assert!(!report.is_conformant());
        assert!(!report.checks[0].passed);
        assert_eq!(
            report.checks.len(),
            2,
            "the runtime check is skipped, not silently dropped"
        );
        assert!(!report.checks[1].passed);
        assert!(report.checks[1].detail.contains("skipped"));
    }
}
