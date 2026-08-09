//! The V01 adapter rail, end to end (ADR 000064): a guest built against the FROZEN
//! `plecto:filter@0.1.0` (`fixtures/filter-compat-v01`, `wit/v0.1.0/`) still loads and runs on
//! the 0.4-native host — the oldest track the compat promise covers.
//!
//! What it pins that the newer rails cannot: the lossy string↔bytes header projection
//! (ADR 000071), the `on-response` signature with no request-context parameter, and the 0.1 body
//! arm `continue(list<u8>)` landing on 0.4.0's `modified` with its transform intact.

use plecto_host::test_support::{TestSigner, bound_sbom, filter_compat_v01_component};
use plecto_host::{
    ContractVersion, Header, Host, HttpRequest, HttpResponse, LoadOptions, LoadedFilter,
    RequestBodyDecision, RequestDecision, RequestTrace, ResponseDecision, SignedArtifact,
};

fn signed_load() -> (Host, LoadedFilter) {
    let bytes = filter_compat_v01_component();
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
        .load("filter-compat-v01", &artifact, LoadOptions::untrusted())
        .unwrap();
    (host, filter)
}

fn req() -> HttpRequest {
    HttpRequest {
        method: "POST".to_string(),
        path_with_query: "/legacy?x=1".to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        // Non-UTF-8 bytes: the 0.1 projection is lossy by design, and the guest must still run.
        headers: vec![Header {
            name: "x-blob".to_string(),
            value: vec![0xC3, 0x28, 0xFF],
        }],
    }
}

#[test]
fn a_frozen_01_guest_loads_and_runs_every_hook_on_the_04_host() {
    let (_host, filter) = signed_load();
    assert_eq!(filter.contract_version(), ContractVersion::V01);
    assert_eq!(filter.contract_version().package(), "plecto:filter@0.1.0");

    let (decision, _logs) = filter.on_request(&req(), &RequestTrace::root()).unwrap();
    assert!(matches!(decision, RequestDecision::Continue));

    let resp = HttpResponse {
        status: 200,
        headers: vec![],
        body: vec![],
    };
    // The 0.4 host passes the request snapshot; the V01 adapter drops it before the guest —
    // this call succeeding IS the adapter working.
    let (decision, _logs) = filter
        .on_response(&req(), &resp, &RequestTrace::root())
        .unwrap();
    match decision {
        ResponseDecision::Modified(edit) => {
            assert!(
                edit.set_headers
                    .iter()
                    .any(|h| h.name == "x-plecto-v01-ran" && h.value == b"1"),
                "the 0.1 guest's string-valued edit maps onto the byte-valued native type"
            );
        }
        other => panic!("the 0.1 guest always answers modified, got {other:?}"),
    }
}

#[test]
fn a_frozen_01_body_rewrite_lands_on_the_040_modified_arm() {
    let (_host, filter) = signed_load();

    let (decision, _logs) = filter
        .on_request_body(b"hello world", &RequestTrace::root())
        .unwrap();
    match decision {
        RequestBodyDecision::Modified(edit) => {
            assert_eq!(edit.body, b"HELLO WORLD".to_vec());
        }
        other => panic!("a 0.1 continue(bytes) must become modified, got {other:?}"),
    }
}
