//! WIT-conformance for `on-response-body` and the acceptance LATTICE (ADR 000098 decision 2).
//!
//! The load-bearing claim is that the contract is the lattice — `filter`'s exports plus ANY subset
//! of the body hooks — and that `filter` / `filter-body` are only its two ends. `filter-respbody`
//! is the machine check: it exports `on-response-body` and NOT `on-request-body`, a shape neither
//! published world names, and the host must accept it, buffer only the response direction, and
//! leave the request direction on the zero-copy path.
//!
//! The other half is the regression guard: every filter that existed before this hook — across all
//! four contract versions — must still report exactly the directions it declared, so nothing
//! re-imports the body tax ADR 000038 removed.

use plecto_host::test_support::{
    TestSigner, bound_sbom, filter_apikey_component, filter_compat_v01_component,
    filter_compat_v02_component, filter_hello_component, filter_quickstart_component,
    filter_respbody_component, filter_v04_component,
};
use plecto_host::{
    BodyHooks, ContractVersion, Header, Host, HttpRequest, HttpResponse, LoadOptions, LoadedFilter,
    RequestBodyDecision, RequestTrace, ResponseBodyDecision, SignedArtifact,
};

fn load(id: &str, bytes: &[u8]) -> (Host, LoadedFilter) {
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(bytes).unwrap();
    let sbom = bound_sbom(bytes);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let artifact = SignedArtifact {
        component_bytes: bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };
    let filter = host.load(id, &artifact, LoadOptions::untrusted()).unwrap();
    (host, filter)
}

fn req(target: &str) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path_with_query: target.to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        headers: vec![],
    }
}

/// The response as the hook sees it: status + headers only, body empty (the bytes arrive as their
/// own parameter).
fn resp() -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![Header {
            name: "content-type".to_string(),
            value: b"application/json".to_vec(),
        }],
        body: Vec::new(),
    }
}

#[test]
fn a_response_body_only_component_is_accepted_and_buffers_only_that_direction() {
    // The lattice check: `filter-respbody` targets neither published world — it exports the base
    // `filter` shape plus `on-response-body` alone. The host must accept it on the 0.4 rail and
    // record ONE direction, leaving the request body on the zero-copy path.
    let (_host, filter) = load("filter-respbody", &filter_respbody_component());

    assert_eq!(filter.contract_version(), ContractVersion::V04);
    assert_eq!(
        filter.body_hooks(),
        BodyHooks {
            request: false,
            response: true
        },
        "a component exporting only on-response-body must arm only the response direction"
    );
}

#[test]
fn the_request_direction_stays_zero_copy_for_a_response_only_filter() {
    // The other side of the same claim: the request hook is not merely unused, it is absent — the
    // host's defensive floor answers with the bare `%continue` without instantiating a call, and
    // the fast path never buffers because `body_hooks().request` is false.
    let (_host, filter) = load("filter-respbody", &filter_respbody_component());

    let (decision, _logs) = filter
        .on_request_body(b"a body no hook declared", &RequestTrace::root())
        .unwrap();
    assert!(
        matches!(decision, RequestBodyDecision::Continue),
        "an absent request-body hook forwards the buffered bytes untouched"
    );
}

#[test]
fn every_pre_existing_filter_still_declares_exactly_the_directions_it_had() {
    // The ADR 000038 regression guard, now per direction: adding a response hook to the contract
    // must not make one single existing filter start buffering anything. The frozen 0.1 / 0.2
    // tracks cannot even express the response hook, and the 0.3 shelf never declared it.
    let cases: [(&str, Vec<u8>, BodyHooks); 5] = [
        (
            // 0.1, world `filter-body`: request only.
            "compat-v01",
            filter_compat_v01_component(),
            BodyHooks {
                request: true,
                response: false,
            },
        ),
        // 0.2, world `filter`: neither.
        ("compat-v02", filter_compat_v02_component(), BodyHooks::NONE),
        (
            // 0.3, world `filter-body`: request only.
            "hello",
            filter_hello_component(),
            BodyHooks {
                request: true,
                response: false,
            },
        ),
        // 0.3, world `filter`: neither.
        ("apikey", filter_apikey_component(), BodyHooks::NONE),
        ("quickstart", filter_quickstart_component(), BodyHooks::NONE),
    ];
    for (id, bytes, want) in cases {
        let (_host, filter) = load(id, &bytes);
        assert_eq!(filter.body_hooks(), want, "{id} changed its declared shape");
    }

    // And the ceiling: the only in-tree filter that reads BOTH is the one that says so.
    let (_host, filter) = load("v04", &filter_v04_component());
    assert_eq!(
        filter.body_hooks(),
        BodyHooks {
            request: true,
            response: true
        }
    );
}

#[test]
fn a_filter_without_the_hook_never_inspects_a_response_body() {
    // The defensive floor in the other direction: calling the hook on a filter that never declared
    // it must forward what the host buffered, not reach a guest. `filter-hello` would redact the
    // marker if it had the export; it does not, so the bytes are untouched.
    let (_host, filter) = load("hello", &filter_hello_component());

    let (decision, _logs) = filter
        .on_response_body(
            &req("/x"),
            &resp(),
            b"a body carrying SECRET",
            &RequestTrace::root(),
        )
        .unwrap();
    assert!(matches!(decision, ResponseBodyDecision::Continue));
}

#[test]
fn the_bare_continue_arm_keeps_the_buffered_response_body() {
    // ADR 000098: the dominant response-side case is pure inspection, so `%continue` carries no
    // payload — the bytes are never lowered back across the boundary to say "unchanged".
    let (_host, filter) = load("filter-respbody", &filter_respbody_component());

    let (decision, _logs) = filter
        .on_response_body(
            &req("/api"),
            &resp(),
            b"nothing interesting here",
            &RequestTrace::root(),
        )
        .unwrap();
    assert!(matches!(decision, ResponseBodyDecision::Continue));
}

#[test]
fn the_modified_arm_carries_new_bytes_and_the_header_edits_a_transform_forces() {
    let (_host, filter) = load("filter-respbody", &filter_respbody_component());

    let (decision, _logs) = filter
        .on_response_body(
            &req("/api"),
            &resp(),
            b"before SECRET after",
            &RequestTrace::root(),
        )
        .unwrap();
    match decision {
        ResponseBodyDecision::Modified(edit) => {
            assert_eq!(edit.body, b"before [redacted] after".to_vec());
            assert!(
                edit.set_headers
                    .iter()
                    .any(|h| h.name == "x-plecto-redacted" && h.value == b"1"),
            );
            assert_eq!(edit.remove_headers, vec!["x-drop-me".to_string()]);
        }
        other => panic!("expected modified, got {other:?}"),
    }
}

#[test]
fn the_replace_arm_authors_a_whole_response_of_its_own() {
    let (_host, filter) = load("filter-respbody", &filter_respbody_component());

    let (decision, _logs) = filter
        .on_response_body(
            &req("/api?respbody=replace"),
            &resp(),
            b"upstream bytes the filter discards",
            &RequestTrace::root(),
        )
        .unwrap();
    match decision {
        ResponseBodyDecision::Replace(replacement) => {
            assert_eq!(replacement.status, 418);
            assert_eq!(replacement.body, b"replaced by filter-respbody".to_vec());
        }
        other => panic!("expected replace, got {other:?}"),
    }
}

#[test]
fn a_guest_supplied_content_length_is_refused_fail_closed() {
    // ADR 000098 decision 1: content-length describes the bytes the HOST decided to send. A guest
    // that names it is making a claim it cannot keep, and forwarding the disagreement is a
    // response-desync primitive — so the whole decision is refused, not quietly stripped.
    let (_host, filter) = load("filter-respbody", &filter_respbody_component());

    let err = filter
        .on_response_body(
            &req("/api?respbody=content-length"),
            &resp(),
            b"body",
            &RequestTrace::root(),
        )
        .expect_err("a guest content-length must fail the decision");
    let fail_closed = err.fail_closed_response();
    assert_eq!(fail_closed.status, 502);
    assert!(
        fail_closed
            .headers
            .iter()
            .any(|h| h.name == "x-plecto-fault" && h.value == b"invalid-output"),
        "it fails as invalid guest OUTPUT, observably distinct from a trap"
    );
}
