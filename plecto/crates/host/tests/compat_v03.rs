//! The V03 adapter rail, end to end (ADR 000098 / 000104): `filter-hello` — and with it the
//! whole in-tree reference shelf — is built against the FROZEN `plecto:filter@0.3.0`
//! (`wit/v0.3.0/`) and keeps running unchanged on the 0.4-native host. This is ADR 000064's
//! compat promise for the version that just moved, kept falsifiable in CI.
//!
//! The load-bearing case is the body decision. 0.3.0 spelled it `%continue(list<u8>)` and that
//! ALWAYS meant "forward THIS body" — a rewriting guest carried its transform in that arm. 0.4.0
//! splits the two meanings apart (`%continue` = unchanged, `modified` = forward these bytes), so
//! the adapter must land a 0.3 `continue(bytes)` on `modified`: mapping it to the bare
//! `%continue` would silently discard the guest's rewrite.

use plecto_host::test_support::{TestSigner, bound_sbom, filter_hello_component};
use plecto_host::{
    ContractVersion, Header, Host, HttpRequest, HttpResponse, LoadOptions, LoadedFilter,
    RequestBodyDecision, RequestTrace, ResponseDecision, SignedArtifact,
};

fn signed_load() -> (Host, LoadedFilter) {
    let bytes = filter_hello_component();
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
        .load("filter-hello", &artifact, LoadOptions::untrusted())
        .unwrap();
    (host, filter)
}

fn req(headers: Vec<Header>) -> HttpRequest {
    HttpRequest {
        method: "POST".to_string(),
        path_with_query: "/legacy?x=1".to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        headers,
    }
}

fn header(name: &str, value: &str) -> Header {
    Header {
        name: name.to_string(),
        value: value.as_bytes().to_vec(),
    }
}

#[test]
fn a_frozen_03_guest_binds_the_v03_rail_on_the_04_host() {
    let (_host, filter) = signed_load();
    assert_eq!(filter.contract_version(), ContractVersion::V03);
    assert_eq!(filter.contract_version().package(), "plecto:filter@0.3.0");
}

#[test]
fn a_frozen_03_body_rewrite_lands_on_the_040_modified_arm() {
    // THE mapping this slice must not get wrong: filter-hello's 0.3 `continue(uppercased)` is a
    // rewrite. On 0.4 semantics that is `modified`, carrying the same bytes — not `%continue`,
    // which would forward the ORIGINAL body and drop the transform on the floor.
    let (_host, filter) = signed_load();

    let (decision, _logs) = filter
        .on_request_body(b"hello world", &RequestTrace::root())
        .unwrap();
    match decision {
        RequestBodyDecision::Modified(edit) => {
            assert_eq!(
                edit.body,
                b"HELLO WORLD".to_vec(),
                "the 0.3 guest's transformed bytes survive the adapter"
            );
            assert!(
                edit.set_headers.is_empty() && edit.remove_headers.is_empty(),
                "0.3 had no body-side header edits to express, so the adapter invents none"
            );
        }
        other => panic!("a 0.3 continue(bytes) must become modified, got {other:?}"),
    }
}

#[test]
fn a_frozen_03_guest_still_short_circuits_on_the_body_marker() {
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
fn path_with_query_reaches_a_frozen_03_guest_as_its_own_path_field() {
    // The rename is a field-name difference only (ADR 000104): the adapter repacks the canonical
    // `path_with_query` into the 0.3 record's `path`, query intact. filter-hello echoes what it
    // saw, so the value is observable from the host side.
    let (_host, filter) = signed_load();
    let request = req(vec![header("x-plecto-resp-echo", "1")]);
    let resp = HttpResponse {
        status: 200,
        headers: vec![],
        body: vec![],
    };

    let (decision, _logs) = filter
        .on_response(&request, &resp, &RequestTrace::root())
        .unwrap();
    match decision {
        ResponseDecision::Modified(edit) => {
            let echoed = edit
                .set_headers
                .iter()
                .find(|h| h.name == "x-plecto-req-path")
                .expect("the 0.3 guest echoes the path it was handed");
            assert_eq!(echoed.value, b"/legacy?x=1".to_vec());
        }
        other => panic!("expected modified, got {other:?}"),
    }
}

#[test]
fn a_frozen_03_guest_can_still_replace_the_response() {
    // ADR 000073's `replace` arm exists on both 0.3 and 0.4, so the V03 adapter must carry it
    // across — the response side of the frozen rail, not just the request side.
    let (_host, filter) = signed_load();
    let request = req(vec![header("x-plecto-resp-replace", "1")]);
    let resp = HttpResponse {
        status: 200,
        headers: vec![],
        body: vec![],
    };

    let (decision, _logs) = filter
        .on_response(&request, &resp, &RequestTrace::root())
        .unwrap();
    match decision {
        ResponseDecision::Replace(replacement) => {
            assert_eq!(replacement.status, 418);
            assert_eq!(replacement.body, b"replaced by filter-hello".to_vec());
        }
        other => panic!("expected replace, got {other:?}"),
    }
}
