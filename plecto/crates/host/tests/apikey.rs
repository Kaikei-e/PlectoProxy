//! Host-level behaviour of the real example auth filter (`filter-apikey`).
//!
//! `filter-hello` exercises the runtime mechanics (init-once, metering, decisions); this suite
//! pins the *security* behaviour of the filter Plecto ships as its API-gateway showcase — the
//! one whose decision is "let this request through or not". It drives the filter through the real
//! provenance path (sign → load → run) and asserts the typed `decision` for each auth outcome:
//!   - no / unknown key  → `short-circuit` 401 (the chain stops; upstream is never reached);
//!   - a valid key       → `modified`, stamping the authoritative `x-authenticated-user`;
//!   - the identity stamp is emitted unconditionally, so it OVERWRITES any value the client tried
//!     to send (the end-to-end overwrite is proven in `plecto-server`'s `auth` E2E).
//!
//! Header lookup is case-insensitive (HTTP semantics), which a spoofer must not be able to dodge
//! by changing the case of the auth header.

use plecto_host::test_support::{TestSigner, bound_sbom, filter_apikey_component};
use plecto_host::{
    Header, Host, HttpRequest, LoadOptions, LoadedFilter, LogLine, RequestDecision, RequestTrace,
    RunError, SignedArtifact,
};

fn on_req(f: &LoadedFilter, r: &HttpRequest) -> Result<(RequestDecision, Vec<LogLine>), RunError> {
    f.on_request(r, &RequestTrace::root())
}

/// Sign filter-apikey with a fresh ephemeral key, build a `Host` trusting exactly that key, and
/// load it trusted (so `init` seeds the demo key→user map once). The `Host` owns the epoch ticker
/// and must outlive the filter, so it is returned alongside.
fn signed_load() -> (Host, LoadedFilter) {
    let bytes = filter_apikey_component();
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
        .load("filter-apikey", &artifact, LoadOptions::trusted())
        .unwrap();
    (host, filter)
}

fn request(headers: &[(&str, &str)]) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: "/api/data".to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        headers: headers
            .iter()
            .map(|(n, v)| Header {
                name: (*n).to_string(),
                value: (*v).to_string(),
            })
            .collect(),
    }
}

fn header_value<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

#[test]
fn missing_api_key_is_401_short_circuit() {
    // No credential at all → the chain must stop with a 401 and a `WWW-Authenticate` challenge;
    // it must NOT continue (which would forward an unauthenticated request to the upstream).
    let (_host, filter) = signed_load();

    let (decision, _logs) = on_req(&filter, &request(&[])).unwrap();
    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(resp.status, 401, "a missing key must be rejected 401");
            assert_eq!(
                header_value(&resp.headers, "www-authenticate"),
                Some("ApiKey"),
                "a 401 must advertise the auth scheme"
            );
            assert!(
                String::from_utf8_lossy(&resp.body).contains("missing API key"),
                "the body names the failure"
            );
        }
        _ => panic!("a missing API key must short-circuit, not continue"),
    }
}

#[test]
fn unknown_api_key_is_401_short_circuit() {
    // A syntactically-present but unknown key is still a rejection — fail-closed, never continue.
    let (_host, filter) = signed_load();

    let (decision, _logs) =
        on_req(&filter, &request(&[("x-api-key", "definitely-not-valid")])).unwrap();
    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(resp.status, 401, "an unknown key must be rejected 401");
            assert!(
                String::from_utf8_lossy(&resp.body).contains("invalid API key"),
                "the body distinguishes an invalid key from a missing one"
            );
        }
        _ => panic!("an unknown API key must short-circuit, not continue"),
    }
}

#[test]
fn valid_key_stamps_authenticated_user() {
    // A valid key → `modified`, stamping the caller's real identity for the upstream / later
    // filters. The stamp is the authoritative output of authentication.
    let (_host, filter) = signed_load();

    let (decision, _logs) = on_req(&filter, &request(&[("x-api-key", "alice-secret")])).unwrap();
    match decision {
        RequestDecision::Modified(edit) => {
            assert_eq!(
                header_value(&edit.set_headers, "x-authenticated-user"),
                Some("alice"),
                "a valid key for alice stamps x-authenticated-user: alice"
            );
        }
        _ => panic!("a valid key must produce a Modified edit that stamps the identity"),
    }
}

#[test]
fn api_key_header_lookup_is_case_insensitive() {
    // HTTP header names are case-insensitive; the gate must accept `X-API-Key` exactly as
    // `x-api-key`, so a client cannot dodge (or a proxy cannot accidentally break) auth via case.
    let (_host, filter) = signed_load();

    let (decision, _logs) = on_req(&filter, &request(&[("X-API-Key", "bob-secret")])).unwrap();
    assert!(
        matches!(decision, RequestDecision::Modified(edit)
            if header_value(&edit.set_headers, "x-authenticated-user") == Some("bob")),
        "an upper-cased auth header must authenticate just the same"
    );
}

#[test]
fn valid_key_emits_authoritative_stamp_overriding_a_spoofed_inbound_one() {
    // Defence against header spoofing (the core of the auth bypass class): even when the client
    // sends its own `x-authenticated-user: admin`, the filter's edit sets the identity to the
    // KEY's real user. Because the chain applies `set_headers` as a case-insensitive REPLACE, this
    // edit overwrites the spoofed value — proven end-to-end in plecto-server's `auth` E2E; here we
    // pin that the filter always emits the authoritative stamp regardless of the inbound claim.
    let (_host, filter) = signed_load();

    let (decision, _logs) = on_req(
        &filter,
        &request(&[
            ("x-api-key", "alice-secret"),
            ("x-authenticated-user", "admin"),
        ]),
    )
    .unwrap();
    match decision {
        RequestDecision::Modified(edit) => assert_eq!(
            header_value(&edit.set_headers, "x-authenticated-user"),
            Some("alice"),
            "the stamp reflects the key's real user (alice), never the client's claim (admin)"
        ),
        _ => panic!("a valid key with a spoofed identity header must still produce the real stamp"),
    }
}

#[test]
fn a_spoofed_identity_header_without_a_key_is_still_rejected() {
    // Sending only a forged `x-authenticated-user` (no key) must be a 401 — the gate authenticates
    // on the key, not on a header the client controls. (And a 401 short-circuit never forwards, so
    // the forged header can never reach the upstream.)
    let (_host, filter) = signed_load();

    let (decision, _logs) =
        on_req(&filter, &request(&[("x-authenticated-user", "admin")])).unwrap();
    assert!(
        matches!(decision, RequestDecision::ShortCircuit(resp) if resp.status == 401),
        "a forged identity header with no key must be rejected 401"
    );
}
