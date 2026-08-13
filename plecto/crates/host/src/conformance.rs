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

/// The verdict vocabulary of one conformance case (ADR 000108). `environment` — a PIXIT
/// shortfall, a missing binary feature, an unreachable test dependency — is deliberately NOT
/// `fail`: "nothing lent this component the capability it imports" is a different statement from
/// "this component does not satisfy the world", and only the latter is the component's problem.
/// `na` means the case is out of profile for this component's contract (PICS), which is not a pass
/// either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Na,
    Inconclusive,
    Environment,
}

impl Verdict {
    /// The stable wire spelling (`checks[].verdict` in `plecto conformance --json`).
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Na => "na",
            Verdict::Inconclusive => "inconclusive",
            Verdict::Environment => "environment",
        }
    }
}

/// Which frozen case set a run executes (ADR 000108). The caller PINS it — `plecto conformance
/// --battery <semver>`, a burnt-in pin for `package` / `dev` — so deepening the battery can never
/// turn an already-shipped component's gate red on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatteryVersion {
    /// The frozen pair: `v1.0.0-S1` (world satisfaction + load gate) and `v1.0.0-D1` (a generic
    /// `on-request` stimulus).
    #[default]
    V1_0_0,
}

impl BatteryVersion {
    /// The stable wire spelling (`battery_version` in the JSON report).
    pub fn as_str(self) -> &'static str {
        match self {
            BatteryVersion::V1_0_0 => "1.0.0",
        }
    }
}

/// What a run is given besides the bytes (ADR 000108): the battery pin, and the PIXIT — the
/// capability set, config and budgets an operator's manifest entry lends this filter, lowered the
/// same way production lowers it. The default is the no-manifest floor: `battery@1.0.0` with
/// `LoadOptions::untrusted()`, i.e. the zero-WASI host API and an empty config lane.
#[derive(Debug, Clone, Default)]
pub struct ConformanceOptions {
    pub battery: BatteryVersion,
    pub pixit: LoadOptions,
}

impl ConformanceOptions {
    /// Run against the capabilities an operator's manifest entry lends this filter.
    pub fn with_pixit(mut self, pixit: LoadOptions) -> Self {
        self.pixit = pixit;
        self
    }
}

/// One named property with its verdict and a human-readable detail (the failure reason, or a
/// short confirmation on success).
pub struct ConformanceCheck {
    /// Stable case ID (ADR 000108), e.g. `v1.0.0-S1` — what CI tracks across battery versions.
    pub id: &'static str,
    pub name: &'static str,
    /// Projection of `verdict == Verdict::Pass`, kept for the existing JSON key.
    pub passed: bool,
    pub verdict: Verdict,
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

/// Run the generic conformance battery against raw component bytes: `battery@1.0.0` with no
/// PIXIT — the alias of [`check_with`] for a caller that has no manifest entry to lend.
pub fn check(component_bytes: &[u8]) -> ConformanceReport {
    check_with(component_bytes, ConformanceOptions::default())
}

/// Run the battery `options` pins, with the capabilities and config its PIXIT lends (ADR 000108).
pub fn check_with(component_bytes: &[u8], options: ConformanceOptions) -> ConformanceReport {
    let _ = options;
    let mut checks = Vec::new();

    let loaded = load_self_signed(component_bytes);
    checks.push(ConformanceCheck {
        id: "v1.0.0-S1",
        name: "loads under the plecto:filter contract",
        passed: loaded.is_ok(),
        verdict: if loaded.is_ok() {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        detail: match &loaded {
            Ok(_) => "component/SBOM self-signature verified, world satisfied".to_string(),
            Err(e) => format!("{e:#}"),
        },
    });

    let Ok(filter) = loaded else {
        checks.push(ConformanceCheck {
            id: "v1.0.0-D1",
            name: "handles a generic request without trapping or exceeding its deadline",
            passed: false,
            verdict: Verdict::Fail,
            detail: "skipped: component did not load".to_string(),
        });
        return ConformanceReport { checks };
    };

    let outcome = filter.on_request(&generic_request(), &RequestTrace::root());
    checks.push(ConformanceCheck {
        id: "v1.0.0-D1",
        name: "handles a generic request without trapping or exceeding its deadline",
        passed: outcome.is_ok(),
        verdict: if outcome.is_ok() {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
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
        path_with_query: "/".to_string(),
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
    fn every_shipped_contract_version_passes_the_battery() {
        // `plecto conformance` gates a filter on "loads under the plecto:filter contract", so it
        // has to recognise every version the host ships support for — the newest one included,
        // and the frozen ones the compat promise keeps loadable (ADR 000064).
        let cases: [(&str, Vec<u8>); 4] = [
            ("0.1.0", crate::test_support::filter_compat_v01_component()),
            ("0.2.0", crate::test_support::filter_compat_v02_component()),
            ("0.3.0", crate::test_support::filter_hello_component()),
            ("0.4.0", crate::test_support::filter_v04_component()),
        ];
        for (version, component) in cases {
            let report = check(&component);
            for c in &report.checks {
                assert!(c.passed, "{version}: {}: {}", c.name, c.detail);
            }
        }
    }

    // --- ADR 000108: the PIXIT-aware entry point and the five-valued verdict ---

    /// The `v1.0.0-S1` case of a report (world satisfaction + the load gate).
    #[cfg(feature = "outbound-tcp")]
    fn s1(report: &ConformanceReport) -> &ConformanceCheck {
        report
            .checks
            .iter()
            .find(|c| c.id == "v1.0.0-S1")
            .expect("the battery always reports the S1 case")
    }

    #[cfg(feature = "outbound-tcp")]
    #[test]
    fn s1_without_a_pixit_is_environment_not_fail() {
        // filter-tcp-gate imports `wasi:sockets`. The no-manifest floor lends only the zero-WASI
        // host API, so S1 cannot link it — but "the run lent it nothing" is an ENVIRONMENT
        // shortfall, never world non-conformance (ADR 000108). The capability set stays the
        // operator's to grant: the battery must not read the component's imports as a request.
        let report = check_with(
            &crate::test_support::filter_tcp_gate_component(),
            ConformanceOptions::default(),
        );
        let s1 = s1(&report);
        assert_eq!(
            s1.verdict,
            Verdict::Environment,
            "a capability shortfall must be told apart from world non-conformance, got {} — {}",
            s1.verdict.as_str(),
            s1.detail
        );
        let detail = s1.detail.to_lowercase();
        assert!(
            detail.contains("wasi:sockets"),
            "the diagnostic must name the import nothing lent: {}",
            s1.detail
        );
        assert!(
            detail.contains("lent") || detail.contains("pixit"),
            "the diagnostic must say the run lent no such capability, not merely repeat \
             wasmtime's unresolved-import text: {}",
            s1.detail
        );
        assert!(
            !report.is_conformant(),
            "an environment verdict on a MUST is still fail-closed"
        );
    }

    #[cfg(feature = "outbound-tcp")]
    #[test]
    fn s1_passes_when_the_pixit_lends_outbound_tcp() {
        // The other half of the contrast: the same bytes, an operator that lends outbound TCP →
        // the component links and S1 passes. S1 stops at link + `InstancePre` and never runs
        // `init` (ADR 000108), so nothing here connects: the allowlisted endpoint is a reserved
        // non-production name that must stay unreachable.
        let pixit = LoadOptions::untrusted().with_outbound_tcp(
            vec![crate::TcpAllowEntry {
                host: "battery.invalid".to_string(),
                port: 6379,
            }],
            vec![],
            Some(1),
            Some(1_000),
        );
        let report = check_with(
            &crate::test_support::filter_tcp_gate_component(),
            ConformanceOptions::default().with_pixit(pixit),
        );
        let s1 = s1(&report);
        assert_eq!(
            s1.verdict,
            Verdict::Pass,
            "with the capability lent, S1 must link the same bytes it could not link before: {}",
            s1.detail
        );
        assert!(
            s1.passed,
            "`passed` stays the projection of `verdict == pass`"
        );
    }

    #[test]
    fn conformance_is_decided_by_the_verdict_not_the_legacy_bool() {
        // The five-valued vocabulary in its stable wire spelling (ADR 000108).
        assert_eq!(
            [
                Verdict::Pass,
                Verdict::Fail,
                Verdict::Na,
                Verdict::Inconclusive,
                Verdict::Environment,
            ]
            .map(Verdict::as_str),
            ["pass", "fail", "na", "inconclusive", "environment"]
        );

        // `conformant` is true only when every APPLICABLE MUST passed: `na` is out of profile
        // (the contract cannot express that hook), so it does not block conformance, while
        // `fail` / `inconclusive` / `environment` each do.
        fn report_with(second: Verdict) -> ConformanceReport {
            let case = |id, verdict| ConformanceCheck {
                id,
                name: "case",
                passed: verdict == Verdict::Pass,
                verdict,
                detail: String::new(),
            };
            ConformanceReport {
                checks: vec![case("v1.0.0-S1", Verdict::Pass), case("v1.0.0-D1", second)],
            }
        }

        assert!(
            report_with(Verdict::Na).is_conformant(),
            "`na` marks a case as out of profile, not as a failure"
        );
        for verdict in [Verdict::Fail, Verdict::Inconclusive, Verdict::Environment] {
            assert!(
                !report_with(verdict).is_conformant(),
                "a MUST at `{}` must fail closed",
                verdict.as_str()
            );
        }
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
