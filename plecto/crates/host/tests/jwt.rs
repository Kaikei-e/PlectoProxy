//! Host-level behaviour of the JWT Resource-Server reference filter (`filter-jwt`, ADR 000070).
//!
//! Built only with the `outbound-http` feature (the guest is wasm32-wasip2 so JWKS-at-init can
//! import `wasi:http/outgoing-handler`). These tests exercise the **static PEM** path end-to-end
//! through the real provenance gate (sign → load trusted → run). A successful JWKS fetch against
//! loopback is intentionally not covered here: the SSRF floor always blocks loopback
//! (`BlockedReserved`), independent of `allow_private`.
#![cfg(feature = "outbound-http")]

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use plecto_host::test_support::{TestSigner, bound_sbom, filter_jwt_component};
use plecto_host::{
    AllowEntry, Header as HttpHeader, Host, HttpRequest, LoadOptions, LoadedFilter, LogLine,
    RequestDecision, RequestTrace, RunError, Scheme, SignedArtifact,
};
use serde::Serialize;

/// Fixed ES256 keypair for deterministic host tests (not a production secret).
const TEST_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEuSqqxo8YYnHKw+6izC8wO5IjLrab\n\
tCAhQhdwQZydUk0s/8m5jtEF5RHo9MaOdU/Jcbh364V6jnqe5cVqmb6mUQ==\n\
-----END PUBLIC KEY-----\n";

const TEST_PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgjoI3oSDk2InZUT0g\n\
pbfdpVa879hvqGOkeLsUh4Quf+OhRANCAAS5KqrGjxhiccrD7qLMLzA7kiMutpu0\n\
ICFCF3BBnJ1STSz/ybmO0QXlEej0xo51T8lxuHfrhXqOep7lxWqZvqZR\n\
-----END PRIVATE KEY-----\n";

/// Raw SEC1 point (`04 || x || y`) for `TEST_PUBLIC_PEM`, base64url-encoded — lets the JWK-config
/// tests exercise `key_from_jwk`'s EC branch against the exact same keypair the PEM tests use.
const TEST_JWK_X: &str = "uSqqxo8YYnHKw-6izC8wO5IjLrabtCAhQhdwQZydUk0";
const TEST_JWK_Y: &str = "LP_JuY7RBeUR6PTGjnVPyXG4d-uFeo56nuXFapm-plE";

/// Fixed 2048-bit RSA keypair (test-only) for RS256 host tests.
const RSA_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\n\
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAtp/VQGbbGic8VI81P8UC\n\
xyCGN/23rSXNiQt7t8ng8kyeJk5Ly7RXveo4MxXNFTTEXoJLi4ghgWFBkdh8fY6x\n\
ogR/CXeicec+/i5QS62gdwjV0r7veABvnjX/unlQDQIbmyKU+6cIRvWpV7QnLPpp\n\
0CDRjd1OoiTqBeXcIPrU+wspjpOlplOMOqI5IarWFCWG1MxMgusHr1BCKRb2CtoG\n\
ta5FXrZSL14Lp2zQeg6UvnbKbE8eVRmh3CKOhULUSM8gPlx0o7gIdNNVwD7gWkNN\n\
FUcM4hQ8HMrV/gQ4zKPrbcR6g5IgJRcK5YQ5YKGlGrzvh2tPcbisWj/Uax0zBhqH\n\
ewIDAQAB\n\
-----END PUBLIC KEY-----\n";

const RSA_PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQC2n9VAZtsaJzxU\n\
jzU/xQLHIIY3/betJc2JC3u3yeDyTJ4mTkvLtFe96jgzFc0VNMRegkuLiCGBYUGR\n\
2Hx9jrGiBH8Jd6Jx5z7+LlBLraB3CNXSvu94AG+eNf+6eVANAhubIpT7pwhG9alX\n\
tCcs+mnQINGN3U6iJOoF5dwg+tT7CymOk6WmU4w6ojkhqtYUJYbUzEyC6wevUEIp\n\
FvYK2ga1rkVetlIvXgunbNB6DpS+dspsTx5VGaHcIo6FQtRIzyA+XHSjuAh001XA\n\
PuBaQ00VRwziFDwcytX+BDjMo+ttxHqDkiAlFwrlhDlgoaUavO+Ha09xuKxaP9Rr\n\
HTMGGod7AgMBAAECggEADi9EnreeewiNzLpBgON/f2dM+tpabY4gdG2Pkss7t/Yw\n\
LnoptE0cp46isNfCUcxYbZ13MAOcfp0cPQTF8/FPu22CpIHwpyI2qALW5RnmUHiU\n\
4G/zelb7qcaN8guW0SaBwxSg4m8AhepyLcgQvD3zAWHF2lv/IzmfcmJ+gj1D/6pQ\n\
p8LULZYAeJ6T44utZVmk3afRQjW+fNZNsAXdDDBWvLyOOEAeqbjc8Vu1VTI7OAyJ\n\
dmQwWX8hHQ6kiZh1m8YPU6x3ubqH4vCZWfjnY/9bEbzvh25GvrJSDj+wuP7aYpjw\n\
BloluP1EFZrI7Hsteub81I0NyuoOankjpWkWUofhsQKBgQD7IZHYSA6hu6XJqVp8\n\
SFce32aVGIWjlcGwcY/nSqscolhhGJ7A5Vz58eZu4xuLrxpxjWek63kGmZV0I4nA\n\
VLci7m/wqrRrfYNgdRHgeGMzgb5mjfEAKwjr9elllLn8UUmwqmJc7SQDBtuVIVrS\n\
HZURzAX1bAj+qvp3TKY+muLCkwKBgQC6Kj759LLRDQX3GYUGi7MR+o9LhbzYoRpN\n\
eMlWkG/x4jg40spTBP6jdY4XYY/StuTHXVIfMFra62RFIlsfk2qycROFhZlmeg/0\n\
wxMMNWwTBFZPtVxW73lsV3GhJNuh0wEixAnsRCdp9hWYs0btPKrBLEW8Dvsh6jwe\n\
8qycw2sweQKBgBlf9PqjnUbeTQwpXok8TgFClXzvM2GqGh4X+3BlbRDBnqiA8lmP\n\
U2u185C0xe3BTay3mwdg+6OdFSrdBGg4pyCScyEgPoa18fZnHd1OjMeBjpmSMg3Q\n\
S2B8Qo8PDhPeqtF9Bd9Z3s+ne7x/2Etuzcc0lE2OEwKYiCJRzmJ5B/ydAoGAOKob\n\
OSHOO+tm4WuXHgLvoo1NiINQk++VffdB8WNNb6aXzlP62YIvr7lcYqmDiXO59yTk\n\
ljG1teToRFLMwbOxSlc4xe+AXbzRloK6DYFFQBSV4PUnAh8qKlwDbjU11O/Q7LAX\n\
BR9Jj+sjb7NB53wLzXiYUUGOFyig3Bqph53DxqECgYByaNJG4t13LpDvbAxPQSRp\n\
ruTIH6H7u6odKBo+aJHe+ooGIGK5qMh/zjUVR6p7+D0OhhxQbP54dkj3jRoTPMq3\n\
bXKZ/mBpcZhE8nyip6/jJ74dKKofscjBtlbteqBdLu+pGEsJ1MdJNDvTr+IaHPzC\n\
LCEZgj70fhi2rerlAZ3mEw==\n\
-----END PRIVATE KEY-----\n";

const ISSUER: &str = "https://idp.example.test";
const AUDIENCE: &str = "plecto-api";

#[derive(Serialize)]
struct Claims {
    sub: String,
    iss: String,
    aud: String,
    exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    nbf: Option<u64>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

fn claims(sub: &str, exp: u64, nbf: Option<u64>) -> Claims {
    Claims {
        sub: sub.to_string(),
        iss: ISSUER.to_string(),
        aud: AUDIENCE.to_string(),
        exp,
        nbf,
    }
}

fn mint_token(sub: &str, exp: u64, nbf: Option<u64>) -> String {
    let key = EncodingKey::from_ec_pem(TEST_PRIVATE_PEM.as_bytes()).expect("encoding key");
    encode(&Header::new(Algorithm::ES256), &claims(sub, exp, nbf), &key).expect("encode jwt")
}

fn mint_token_rs256(sub: &str, exp: u64) -> String {
    let key = EncodingKey::from_rsa_pem(RSA_PRIVATE_PEM.as_bytes()).expect("encoding key");
    encode(
        &Header::new(Algorithm::RS256),
        &claims(sub, exp, None),
        &key,
    )
    .expect("encode jwt")
}

fn mint_token_hs256(sub: &str, exp: u64) -> String {
    let key = EncodingKey::from_secret(b"not-a-real-secret-either");
    encode(
        &Header::new(Algorithm::HS256),
        &claims(sub, exp, None),
        &key,
    )
    .expect("encode jwt")
}

fn mint_token_es256_with_kid(sub: &str, exp: u64, kid: &str) -> String {
    let key = EncodingKey::from_ec_pem(TEST_PRIVATE_PEM.as_bytes()).expect("encoding key");
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(kid.to_string());
    encode(&header, &claims(sub, exp, None), &key).expect("encode jwt")
}

/// Hand-built token with an attacker-chosen `alg` (`jsonwebtoken` refuses to encode `"none"`,
/// so this bypasses it entirely) — split serialization, unvalidated signature segment. Exercises
/// the alg-allowlist gate itself, ahead of and independent from any signature check.
fn raw_token_with_alg(alg: &str, sub: &str, exp: u64) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = format!(r#"{{"alg":"{alg}","typ":"JWT"}}"#);
    let payload = serde_json::to_string(&claims(sub, exp, None)).expect("claims json");
    let h = URL_SAFE_NO_PAD.encode(header);
    let p = URL_SAFE_NO_PAD.encode(payload);
    format!("{h}.{p}.")
}

fn base_config() -> BTreeMap<String, String> {
    let mut cfg = BTreeMap::new();
    cfg.insert("issuer".into(), ISSUER.into());
    cfg.insert("audience".into(), AUDIENCE.into());
    cfg.insert("realm".into(), "test".into());
    cfg.insert("public_key_pem".into(), TEST_PUBLIC_PEM.into());
    cfg
}

fn rsa_config() -> BTreeMap<String, String> {
    let mut cfg = base_config();
    cfg.remove("public_key_pem");
    cfg.insert("public_key_pem".into(), RSA_PUBLIC_PEM.into());
    cfg
}

/// Static JWK config path, using the same EC keypair as `TEST_PUBLIC_PEM`/`TEST_PRIVATE_PEM`
/// (via its raw SEC1 point) so tokens minted for the PEM tests verify here too.
fn jwk_config(kid: Option<&str>) -> BTreeMap<String, String> {
    let mut cfg = base_config();
    cfg.remove("public_key_pem");
    let kid_field = kid.map(|k| format!(r#","kid":"{k}""#)).unwrap_or_default();
    cfg.insert(
        "jwk".into(),
        format!(r#"{{"kty":"EC","crv":"P-256","x":"{TEST_JWK_X}","y":"{TEST_JWK_Y}"{kid_field}}}"#),
    );
    cfg
}

fn signed_load(cfg: BTreeMap<String, String>) -> Result<(Host, LoadedFilter), String> {
    let bytes = filter_jwt_component();
    let signer = TestSigner::new().map_err(|e| e.to_string())?;
    let component_signature = signer.sign(&bytes).map_err(|e| e.to_string())?;
    let sbom = bound_sbom(&bytes);
    let sbom_signature = signer.sign(&sbom).map_err(|e| e.to_string())?;
    let host =
        Host::new(signer.trust_policy().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let artifact = SignedArtifact {
        component_bytes: &bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };
    // wasip2 guests import wasi:io even when the static PEM path never calls outbound — lend an
    // empty allowlist so the host links the WASI base + http interfaces (deny-by-default).
    let opts = LoadOptions::trusted().with_config(cfg).with_outbound_http(
        vec![],
        vec![],
        Some(500),
        Some(1_000),
        Some(64 * 1024),
        Some(2),
    );
    let filter = host
        .load("filter-jwt", &artifact, opts)
        .map_err(|e| e.to_string())?;
    Ok((host, filter))
}

fn on_req(f: &LoadedFilter, r: &HttpRequest) -> Result<(RequestDecision, Vec<LogLine>), RunError> {
    f.on_request(r, &RequestTrace::root())
}

fn request(headers: &[(&str, &str)]) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: "/api/data".to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        headers: headers
            .iter()
            .map(|(n, v)| HttpHeader {
                name: (*n).to_string(),
                value: (*v).to_string(),
            })
            .collect(),
    }
}

fn header_value<'a>(headers: &'a [HttpHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

#[test]
fn missing_bearer_is_401_without_error_attribute() {
    let (_host, filter) = signed_load(base_config()).expect("load");
    let (decision, _) = on_req(&filter, &request(&[])).unwrap();
    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(resp.status, 401);
            let www = header_value(&resp.headers, "www-authenticate").expect("www-authenticate");
            assert!(www.starts_with("Bearer realm=\"test\""), "got {www}");
            assert!(
                !www.contains("error="),
                "RFC 6750: missing creds must not set error= (got {www})"
            );
        }
        other => panic!("expected short-circuit, got {other:?}"),
    }
}

#[test]
fn valid_token_stamps_sub_and_iss() {
    let (_host, filter) = signed_load(base_config()).expect("load");
    let token = mint_token("alice", now_secs() + 3600, None);
    let (decision, _) = on_req(
        &filter,
        &request(&[("authorization", &format!("Bearer {token}"))]),
    )
    .unwrap();
    match decision {
        RequestDecision::Modified(edit) => {
            assert_eq!(
                header_value(&edit.set_headers, "x-authenticated-user"),
                Some("alice")
            );
            assert_eq!(
                header_value(&edit.set_headers, "x-jwt-issuer"),
                Some(ISSUER)
            );
        }
        other => panic!("expected modified, got {other:?}"),
    }
}

#[test]
fn expired_token_is_401_invalid_token() {
    let (_host, filter) = signed_load(base_config()).expect("load");
    let token = mint_token("alice", now_secs().saturating_sub(60), None);
    let (decision, _) = on_req(
        &filter,
        &request(&[("authorization", &format!("Bearer {token}"))]),
    )
    .unwrap();
    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(resp.status, 401);
            let www = header_value(&resp.headers, "www-authenticate").expect("www-authenticate");
            assert!(
                www.contains("error=\"invalid_token\""),
                "expired must be invalid_token (got {www})"
            );
        }
        other => panic!("expected short-circuit, got {other:?}"),
    }
}

#[test]
fn spoofed_identity_without_token_is_rejected() {
    let (_host, filter) = signed_load(base_config()).expect("load");
    let (decision, _) = on_req(&filter, &request(&[("x-authenticated-user", "admin")])).unwrap();
    assert!(
        matches!(decision, RequestDecision::ShortCircuit(resp) if resp.status == 401),
        "spoofed identity without bearer must be 401"
    );
}

#[test]
fn valid_token_overwrites_spoofed_identity_stamp() {
    let (_host, filter) = signed_load(base_config()).expect("load");
    let token = mint_token("alice", now_secs() + 3600, None);
    let (decision, _) = on_req(
        &filter,
        &request(&[
            ("authorization", &format!("Bearer {token}")),
            ("x-authenticated-user", "admin"),
            ("x-jwt-issuer", "https://evil.test"),
        ]),
    )
    .unwrap();
    match decision {
        RequestDecision::Modified(edit) => {
            assert_eq!(
                header_value(&edit.set_headers, "x-authenticated-user"),
                Some("alice")
            );
            assert_eq!(
                header_value(&edit.set_headers, "x-jwt-issuer"),
                Some(ISSUER)
            );
        }
        other => panic!("expected modified, got {other:?}"),
    }
}

#[test]
fn missing_issuer_config_fails_at_load() {
    let mut cfg = base_config();
    cfg.remove("issuer");
    assert!(
        signed_load(cfg).is_err(),
        "missing issuer must fail load under trusted isolation"
    );
}

#[test]
fn both_pem_and_jwks_url_fails_at_load() {
    let mut cfg = base_config();
    cfg.insert(
        "jwks_url".into(),
        "https://idp.example.test/.well-known/jwks.json".into(),
    );
    assert!(
        signed_load(cfg).is_err(),
        "XOR violation (pem + jwks_url) must fail load"
    );
}

#[test]
fn jwks_url_without_usable_outbound_fails_at_load() {
    // JWKS path is wired: init attempts outbound; allowlist deny / SSRF → init trap → load fail.
    let mut cfg = BTreeMap::new();
    cfg.insert("issuer".into(), ISSUER.into());
    cfg.insert("audience".into(), AUDIENCE.into());
    cfg.insert(
        "jwks_url".into(),
        "http://127.0.0.1:9/.well-known/jwks.json".into(),
    );

    let bytes = filter_jwt_component();
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
    let opts = LoadOptions::trusted().with_config(cfg).with_outbound_http(
        vec![AllowEntry {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_string(),
            port: 9,
        }],
        vec![],
        Some(500),
        Some(1_000),
        Some(64 * 1024),
        Some(2),
    );
    assert!(
        host.load("filter-jwt", &artifact, opts).is_err(),
        "JWKS fetch to loopback must fail closed at load (SSRF floor)"
    );
}

#[test]
fn authorization_header_lookup_is_case_insensitive() {
    let (_host, filter) = signed_load(base_config()).expect("load");
    let token = mint_token("bob", now_secs() + 3600, None);
    let (decision, _) = on_req(
        &filter,
        &request(&[("Authorization", &format!("Bearer {token}"))]),
    )
    .unwrap();
    assert!(
        matches!(decision, RequestDecision::Modified(edit)
            if header_value(&edit.set_headers, "x-authenticated-user") == Some("bob")),
        "Authorization case must not matter"
    );
}

#[test]
fn none_alg_is_rejected() {
    let (_host, filter) = signed_load(base_config()).expect("load");
    let token = raw_token_with_alg("none", "eve", now_secs() + 3600);
    let (decision, _) = on_req(
        &filter,
        &request(&[("authorization", &format!("Bearer {token}"))]),
    )
    .unwrap();
    assert!(
        matches!(decision, RequestDecision::ShortCircuit(resp) if resp.status == 401),
        "alg=none (RFC 8725 §3.1) must never authenticate"
    );
}

#[test]
fn hs256_alg_is_rejected() {
    let (_host, filter) = signed_load(base_config()).expect("load");
    let token = mint_token_hs256("eve", now_secs() + 3600);
    let (decision, _) = on_req(
        &filter,
        &request(&[("authorization", &format!("Bearer {token}"))]),
    )
    .unwrap();
    assert!(
        matches!(decision, RequestDecision::ShortCircuit(resp) if resp.status == 401),
        "only ES256/RS256 are allowed; HS256 must be rejected"
    );
}

#[test]
fn rs256_static_pem_path_authenticates() {
    let (_host, filter) = signed_load(rsa_config()).expect("load");
    let token = mint_token_rs256("carol", now_secs() + 3600);
    let (decision, _) = on_req(
        &filter,
        &request(&[("authorization", &format!("Bearer {token}"))]),
    )
    .unwrap();
    match decision {
        RequestDecision::Modified(edit) => {
            assert_eq!(
                header_value(&edit.set_headers, "x-authenticated-user"),
                Some("carol")
            );
        }
        other => panic!("expected modified, got {other:?}"),
    }
}

#[test]
fn rs256_token_signed_by_unknown_key_is_rejected() {
    let (_host, filter) = signed_load(rsa_config()).expect("load");
    // Signed with the ES256 test key run through the RS256 encoder is nonsensical; instead prove
    // the negative by pointing the verifier at a *different* config than the signer used: reuse
    // the ES256 token against the RSA-configured filter, which must fail on both alg and key.
    let token = mint_token("mallory", now_secs() + 3600, None);
    let (decision, _) = on_req(
        &filter,
        &request(&[("authorization", &format!("Bearer {token}"))]),
    )
    .unwrap();
    assert!(
        matches!(decision, RequestDecision::ShortCircuit(resp) if resp.status == 401),
        "ES256 token must not verify against an RS256-only configured key set"
    );
}

#[test]
fn static_jwk_config_path_authenticates() {
    let (_host, filter) = signed_load(jwk_config(None)).expect("load");
    let token = mint_token("dave", now_secs() + 3600, None);
    let (decision, _) = on_req(
        &filter,
        &request(&[("authorization", &format!("Bearer {token}"))]),
    )
    .unwrap();
    match decision {
        RequestDecision::Modified(edit) => {
            assert_eq!(
                header_value(&edit.set_headers, "x-authenticated-user"),
                Some("dave")
            );
        }
        other => panic!("expected modified, got {other:?}"),
    }
}

#[test]
fn unknown_kid_is_rejected() {
    let (_host, filter) = signed_load(jwk_config(Some("key-1"))).expect("load");
    let token = mint_token_es256_with_kid("dave", now_secs() + 3600, "key-does-not-exist");
    let (decision, _) = on_req(
        &filter,
        &request(&[("authorization", &format!("Bearer {token}"))]),
    )
    .unwrap();
    assert!(
        matches!(decision, RequestDecision::ShortCircuit(resp) if resp.status == 401),
        "a kid absent from the configured JWK set must not authenticate"
    );
}

#[test]
fn invalid_token_description_does_not_reveal_rejection_reason() {
    // Expired vs unsupported-alg vs bad-signature must all read identically to the client — the
    // specific reason stays server-side (host log) only, so a caller without a valid key cannot
    // use the response to distinguish "wrong kid" from "expired" from "bad signature".
    let (_host, filter) = signed_load(base_config()).expect("load");
    let expired = mint_token("alice", now_secs().saturating_sub(60), None);
    let none_alg = raw_token_with_alg("none", "alice", now_secs() + 3600);
    let mut descriptions = Vec::new();
    for token in [expired, none_alg] {
        let (decision, _) = on_req(
            &filter,
            &request(&[("authorization", &format!("Bearer {token}"))]),
        )
        .unwrap();
        match decision {
            RequestDecision::ShortCircuit(resp) => {
                let www =
                    header_value(&resp.headers, "www-authenticate").expect("www-authenticate");
                descriptions.push(www.to_string());
            }
            other => panic!("expected short-circuit, got {other:?}"),
        }
    }
    assert_eq!(
        descriptions[0], descriptions[1],
        "different rejection reasons must not produce different client-visible descriptions"
    );
}
