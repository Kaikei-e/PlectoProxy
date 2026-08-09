//! The 0.4.0 contract rail (ADR 000098 / 000104): a guest built against the CURRENT
//! `plecto:filter@0.4.0` (`fixtures/filter-v04`, `wit/`) binds the V04 rail natively and can
//! express what only 0.4.0 has:
//!   - `http-request.path-with-query` — the query reaches the guest under the renamed field;
//!   - the BARE `%continue` body arm — "forward what you buffered", with no bytes handed back;
//!   - `modified(request-body-edit)` — a new body PLUS the header edits a transform forces.
//!
//! The frozen 0.1 / 0.2 / 0.3 rails are covered by `compat_v02.rs` / `compat_v03.rs`; together
//! they are the four-version load-and-dispatch matrix ADR 000064's compat promise rests on.

use plecto_host::test_support::{TestSigner, bound_sbom, filter_v04_component};
use plecto_host::{
    BodyHooks, ContractVersion, Header, Host, HttpRequest, HttpResponse, LoadOptions, LoadedFilter,
    RequestBodyDecision, RequestDecision, RequestTrace, ResponseDecision, SignedArtifact,
};

fn signed_load() -> (Host, LoadedFilter) {
    let bytes = filter_v04_component();
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
        .load("filter-v04", &artifact, LoadOptions::untrusted())
        .unwrap();
    (host, filter)
}

fn req(target: &str) -> HttpRequest {
    HttpRequest {
        method: "POST".to_string(),
        path_with_query: target.to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        headers: vec![Header {
            name: "x-blob".to_string(),
            value: vec![0xC3, 0x28, 0xFF],
        }],
    }
}

#[test]
fn a_040_guest_binds_the_040_rail() {
    let (_host, filter) = signed_load();
    assert_eq!(filter.contract_version(), ContractVersion::V04);
    assert_eq!(filter.contract_version().package(), "plecto:filter@0.4.0");
    assert_eq!(
        filter.body_hooks(),
        BodyHooks {
            request: true,
            response: true
        },
        "the fixture targets the ceiling world, so the host buffers both directions for it"
    );
}

#[test]
fn on_request_decides_on_the_query_under_the_renamed_field() {
    // ADR 000104 decision 1: the field carries path AND query, and now says so. A guest that
    // keys on the query proves the rename reached guest memory, not just the WIT text.
    let (_host, filter) = signed_load();

    let (decision, _logs) = filter
        .on_request(&req("/api?deny=1"), &RequestTrace::root())
        .unwrap();
    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(resp.status, 403);
            assert!(
                resp.headers
                    .iter()
                    .any(|h| h.name == "x-plecto-v04" && h.value == b"query-seen"),
            );
        }
        other => panic!("the query marker must short-circuit, got {other:?}"),
    }

    let (decision, _logs) = filter
        .on_request(&req("/api?deny=0"), &RequestTrace::root())
        .unwrap();
    assert!(
        matches!(decision, RequestDecision::Continue),
        "the same path without the marker continues"
    );
}

#[test]
fn the_bare_continue_arm_forwards_the_buffered_body_untouched() {
    // ADR 000098: `%continue` carries no payload — an inspecting filter says "unchanged" without
    // paying a full copy back across the boundary. The host keeps the bytes it already had.
    let (_host, filter) = signed_load();

    let (decision, _logs) = filter
        .on_request_body(b"plain body", &RequestTrace::root())
        .unwrap();
    assert!(
        matches!(decision, RequestBodyDecision::Continue),
        "an ordinary body takes the bare continue arm"
    );
}

#[test]
fn the_modified_arm_carries_a_new_body_and_its_header_edits() {
    let (_host, filter) = signed_load();

    let (decision, _logs) = filter
        .on_request_body(b"rewrite me", &RequestTrace::root())
        .unwrap();
    match decision {
        RequestBodyDecision::Modified(edit) => {
            assert_eq!(edit.body, b"REWRITE ME".to_vec());
            assert!(
                edit.set_headers
                    .iter()
                    .any(|h| h.name == "x-plecto-body-edited" && h.value == b"1"),
                "a body transform's accompanying header edit rides along"
            );
            assert_eq!(edit.remove_headers, vec!["x-drop-me".to_string()]);
        }
        other => panic!("expected modified, got {other:?}"),
    }
}

#[test]
fn a_body_marker_still_short_circuits_before_upstream() {
    let (_host, filter) = signed_load();

    let (decision, _logs) = filter
        .on_request_body(b"please deny-body now", &RequestTrace::root())
        .unwrap();
    match decision {
        RequestBodyDecision::ShortCircuit(resp) => assert_eq!(resp.status, 403),
        other => panic!("expected short-circuit, got {other:?}"),
    }
}

#[test]
fn the_response_hook_runs_on_the_040_rail() {
    let (_host, filter) = signed_load();
    let resp = HttpResponse {
        status: 200,
        headers: vec![],
        body: vec![],
    };
    let (decision, _logs) = filter
        .on_response(&req("/api"), &resp, &RequestTrace::root())
        .unwrap();
    assert!(matches!(decision, ResponseDecision::Continue));
}
