//! The V02 adapter rail, end to end (ADR 000073): a guest built against the FROZEN
//! `plecto:filter@0.2.0` contract (`fixtures/filter-compat-v02`, `wit/v0.2.0/`) loads and runs
//! on the 0.3-native host. This is the compat promise of ADR 000064 kept falsifiable in CI now
//! that every in-tree example targets 0.3.0:
//!   - load-time version detection picks the V02 binding from the decoded 0.2 import names;
//!   - `on-response` runs with the request-context parameter transparently DROPPED by the
//!     adapter (the 0.2 signature has no such parameter — nothing for the guest to see);
//!   - its 0.2-shaped `modified` edit maps through the validating adapter onto the native
//!     byte-valued types.

use plecto_host::test_support::{TestSigner, bound_sbom, filter_compat_v02_component};
use plecto_host::{
    Header, Host, HttpRequest, HttpResponse, LoadOptions, LoadedFilter, RequestDecision,
    RequestTrace, ResponseDecision, SignedArtifact,
};

fn signed_load() -> (Host, LoadedFilter) {
    let bytes = filter_compat_v02_component();
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(&bytes).unwrap();
    let sbom = bound_sbom(&bytes);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let artifact = SignedArtifact {
        component_bytes: &bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };
    let filter = host
        .load("filter-compat-v02", &artifact, LoadOptions::untrusted())
        .unwrap();
    (host, filter)
}

fn req() -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: "/legacy".to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        // Non-UTF-8 bytes prove the 0.2 projection stays byte-faithful (it is a clone, not the
        // 0.1 lossy-UTF-8 form).
        headers: vec![Header {
            name: "x-blob".to_string(),
            value: vec![0xC3, 0x28, 0xFF],
        }],
    }
}

#[test]
fn a_frozen_02_guest_loads_and_runs_both_hooks_on_the_03_host() {
    let (_host, filter) = signed_load();

    let (decision, _logs) = filter.on_request(&req(), &RequestTrace::root()).unwrap();
    assert!(matches!(decision, RequestDecision::Continue));

    let resp = HttpResponse {
        status: 200,
        headers: vec![],
        body: vec![],
    };
    // The 0.3 host passes the request snapshot; the V02 adapter drops it before the guest —
    // this call succeeding IS the adapter working.
    let (decision, _logs) = filter
        .on_response(&req(), &resp, &RequestTrace::root())
        .unwrap();
    match decision {
        ResponseDecision::Modified(edit) => {
            assert!(
                edit.set_headers
                    .iter()
                    .any(|h| h.name == "x-plecto-v02-ran" && h.value == b"1"),
                "the 0.2 guest's response edit must map through the validating adapter"
            );
        }
        other => panic!("the 0.2 guest always answers modified, got {other:?}"),
    }
}
