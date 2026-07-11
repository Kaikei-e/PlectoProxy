//! Host-level behaviour of the CORS reference filter (`filter-cors`, ADR 000073 / F2 shelf).
//!
//! This is the living proof of the 0.3.0 response-side contract: the dynamic origin echo reads
//! the request's `Origin` from the as-forwarded snapshot `on-response` now receives — the exact
//! capability that was inexpressible under 0.2 (the pool checks the two hooks out independently,
//! so guest globals cannot carry the origin across; ADR 000011 / 000073). The suite drives the
//! filter through the real provenance path (sign → load → run) and asserts:
//!   - a preflight (OPTIONS + Origin + Access-Control-Request-Method) from an allowed origin
//!     short-circuits 204 with the CORS grant (never reaches upstream);
//!   - a preflight from a disallowed origin short-circuits 204 with NO CORS headers (the
//!     browser enforces the block);
//!   - an actual response gains `Access-Control-Allow-Origin` echoing the allowed origin;
//!   - a disallowed / absent origin leaves the response untouched (`continue`);
//!   - the operator's config is the policy source (`[filter.config]`, ADR 000066): with no
//!     `allowed-origins` the filter adds nothing (fail-safe).

use std::collections::BTreeMap;

use plecto_host::test_support::{TestSigner, bound_sbom, filter_cors_component};
use plecto_host::{
    Header, Host, HttpRequest, HttpResponse, LoadOptions, LoadedFilter, RequestDecision,
    RequestTrace, ResponseDecision, SignedArtifact,
};

/// Sign filter-cors with a fresh ephemeral key and load it untrusted (fresh instance per
/// request — the strictest mode, and the one that proves no state carries between hooks).
fn signed_load(config: &[(&str, &str)]) -> (Host, LoadedFilter) {
    let bytes = filter_cors_component();
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
    let config: BTreeMap<String, String> = config
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let filter = host
        .load(
            "filter-cors",
            &artifact,
            LoadOptions::untrusted().with_config(config),
        )
        .unwrap();
    (host, filter)
}

fn request(method: &str, headers: &[(&str, &str)]) -> HttpRequest {
    HttpRequest {
        method: method.to_string(),
        path: "/api/data".to_string(),
        authority: "api.example.test".to_string(),
        scheme: "https".to_string(),
        headers: headers
            .iter()
            .map(|(n, v)| Header {
                name: (*n).to_string(),
                value: v.as_bytes().to_vec(),
            })
            .collect(),
    }
}

fn plain_response() -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![Header {
            name: "content-type".to_string(),
            value: b"application/json".to_vec(),
        }],
        body: vec![],
    }
}

fn header_value<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(&h.value).ok())
}

#[test]
fn preflight_from_an_allowed_origin_short_circuits_with_the_grant() {
    let (_host, filter) = signed_load(&[
        ("allowed-origins", "https://app.example.test"),
        ("allow-methods", "GET, POST"),
        ("max-age", "600"),
    ]);
    let req = request(
        "OPTIONS",
        &[
            ("origin", "https://app.example.test"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type"),
        ],
    );
    let (decision, _logs) = filter.on_request(&req, &RequestTrace::root()).unwrap();
    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(resp.status, 204);
            assert_eq!(
                header_value(&resp.headers, "access-control-allow-origin"),
                Some("https://app.example.test"),
                "the concrete origin is echoed"
            );
            assert_eq!(
                header_value(&resp.headers, "access-control-allow-methods"),
                Some("GET, POST")
            );
            assert_eq!(
                header_value(&resp.headers, "access-control-allow-headers"),
                Some("content-type"),
                "no allow-headers config: the requested headers are echoed"
            );
            assert_eq!(
                header_value(&resp.headers, "access-control-max-age"),
                Some("600")
            );
            assert_eq!(header_value(&resp.headers, "vary"), Some("Origin"));
        }
        other => panic!("a preflight must short-circuit, got {other:?}"),
    }
}

#[test]
fn preflight_from_a_disallowed_origin_gets_no_cors_headers() {
    let (_host, filter) = signed_load(&[("allowed-origins", "https://app.example.test")]);
    let req = request(
        "OPTIONS",
        &[
            ("origin", "https://evil.example.test"),
            ("access-control-request-method", "POST"),
        ],
    );
    let (decision, _logs) = filter.on_request(&req, &RequestTrace::root()).unwrap();
    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(resp.status, 204);
            assert!(
                header_value(&resp.headers, "access-control-allow-origin").is_none(),
                "a disallowed origin must not receive a grant — the browser enforces the block"
            );
        }
        other => panic!("a preflight must short-circuit, got {other:?}"),
    }
}

#[test]
fn a_plain_options_request_without_preflight_markers_continues_upstream() {
    let (_host, filter) = signed_load(&[("allowed-origins", "*")]);
    let req = request("OPTIONS", &[]);
    let (decision, _logs) = filter.on_request(&req, &RequestTrace::root()).unwrap();
    assert!(
        matches!(decision, RequestDecision::Continue),
        "OPTIONS without Origin + Access-Control-Request-Method is not a preflight"
    );
}

#[test]
fn response_gains_the_dynamic_origin_echo_from_the_request_snapshot() {
    let (_host, filter) = signed_load(&[(
        "allowed-origins",
        "https://app.example.test, https://admin.example.test",
    )]);
    // The actual (non-preflight) request: Origin rides the as-forwarded snapshot.
    let req = request("GET", &[("origin", "https://admin.example.test")]);
    let (decision, _logs) = filter
        .on_response(&req, &plain_response(), &RequestTrace::root())
        .unwrap();
    match decision {
        ResponseDecision::Modified(edit) => {
            let set = |name: &str| {
                edit.set_headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case(name))
                    .and_then(|h| std::str::from_utf8(&h.value).ok())
            };
            assert_eq!(
                set("access-control-allow-origin"),
                Some("https://admin.example.test"),
                "the SECOND allowlisted origin is echoed dynamically — a static header cannot do this"
            );
            assert_eq!(set("vary"), Some("Origin"));
        }
        other => panic!("an allowed origin must gain the CORS headers, got {other:?}"),
    }
}

#[test]
fn wildcard_answers_star_but_credentials_refuse_the_wildcard() {
    let (_host, star) = signed_load(&[("allowed-origins", "*")]);
    let req = request("GET", &[("origin", "https://anywhere.example.test")]);
    let (decision, _logs) = star
        .on_response(&req, &plain_response(), &RequestTrace::root())
        .unwrap();
    match decision {
        ResponseDecision::Modified(edit) => {
            assert_eq!(
                edit.set_headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case("access-control-allow-origin"))
                    .map(|h| h.value.as_slice()),
                Some(b"*".as_slice())
            );
        }
        other => panic!("wildcard must grant, got {other:?}"),
    }

    // `*` + credentials would otherwise echo every Origin under ACA-Credentials — refuse the
    // wildcard entry and leave the response untouched (list concrete origins instead).
    let (_host, creds) = signed_load(&[("allowed-origins", "*"), ("allow-credentials", "true")]);
    let (decision, _logs) = creds
        .on_response(&req, &plain_response(), &RequestTrace::root())
        .unwrap();
    assert!(
        matches!(decision, ResponseDecision::Continue),
        "credentialed wildcard must not grant, got {decision:?}"
    );
}

#[test]
fn disallowed_or_absent_origin_and_missing_config_all_leave_the_response_untouched() {
    let (_host, filter) = signed_load(&[("allowed-origins", "https://app.example.test")]);

    let disallowed = request("GET", &[("origin", "https://evil.example.test")]);
    let (decision, _logs) = filter
        .on_response(&disallowed, &plain_response(), &RequestTrace::root())
        .unwrap();
    assert!(matches!(decision, ResponseDecision::Continue));

    let no_origin = request("GET", &[]);
    let (decision, _logs) = filter
        .on_response(&no_origin, &plain_response(), &RequestTrace::root())
        .unwrap();
    assert!(matches!(decision, ResponseDecision::Continue));

    // No allowed-origins declared at all: fail-safe — the filter grants nothing.
    let (_host, unconfigured) = signed_load(&[]);
    let allowed_shape = request("GET", &[("origin", "https://app.example.test")]);
    let (decision, _logs) = unconfigured
        .on_response(&allowed_shape, &plain_response(), &RequestTrace::root())
        .unwrap();
    assert!(matches!(decision, ResponseDecision::Continue));
}
