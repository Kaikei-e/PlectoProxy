//! filter-jwt — Resource-Server JWT verification (`plecto:filter`, ADR 000070).
//!
//! Verifies `Authorization: Bearer` with ES256 or RS256 only. Key material comes from
//! `[filter.config]` as exactly one of: static PEM, static JWK, or a JWKS URL fetched once
//! in `init` over outbound HTTP. Requires `isolation = "trusted"` so config / JWKS failures
//! surface at load (ADR 000066).
//!
//! Signature verification follows RFC 7515 compact JWS: split serialization, verify
//! over `header.payload`, then check claims (RFC 7519 / RFC 8725). Failures follow RFC 6750.
//! Crypto is verify-only (`p256` / `rsa`): ECDSA/RSA signature *verification* needs no RNG, so
//! this guest never pulls in `getrandom`/`wasi:random` at all, sidestepping a component-link
//! failure hit with a full JWT crate (its `getrandom` backend pinned a `wasi` crate release the
//! outbound-http host's WASI grant did not satisfy).
//!
//! `#![allow(clippy::too_many_arguments)]`: the `wit_bindgen::generate!` bindings below expand to
//! a function past clippy's default threshold (the WIT world has a wide config shape); none of
//! this crate's own functions need the suppression.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../../wit",
    world: "filter",
});

use crate::plecto::filter::host_clock;
use crate::plecto::filter::host_config;
use crate::plecto::filter::host_kv;
use crate::plecto::filter::host_log;
use crate::plecto::filter::types::{Header, RequestEdit};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Verifier as EcdsaVerifier;
use p256::ecdsa::{Signature as EcdsaSignature, VerifyingKey as EcdsaVerifyingKey};
use p256::pkcs8::DecodePublicKey;
use rsa::RsaPublicKey;
use rsa::pkcs1v15::{Signature as RsaSignature, VerifyingKey as RsaVerifyingKey};
use rsa::signature::Verifier as RsaVerifierTrait;
use serde::Deserialize;
use sha2::Sha256;
use std::cell::RefCell;
use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, Method, OutgoingRequest, Scheme};

struct FilterJwt;

enum PublicKey {
    Es256(EcdsaVerifyingKey),
    Rs256(RsaPublicKey),
}

struct KeyedKey {
    kid: Option<String>,
    key: PublicKey,
}

struct JwtVerifier {
    keys: Vec<KeyedKey>,
    issuer: String,
    audience: String,
    realm: String,
}

thread_local! {
    static VERIFIER: RefCell<Option<JwtVerifier>> = const { RefCell::new(None) };
}

const MAX_JWKS_BYTES: usize = 256 * 1024;

/// KV key for a cached JWKS body, scoped to the configured URL so a `jwks_url` change across a
/// manifest reload naturally misses the old cache instead of silently reusing stale keys.
fn kv_jwks_key(url: &str) -> String {
    format!("jwks:raw:{url}")
}

#[derive(Debug, Deserialize)]
struct JoseHeader {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    iss: String,
    aud: Aud,
    exp: u64,
    #[serde(default)]
    nbf: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Aud {
    One(String),
    Many(Vec<String>),
}

impl Aud {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Aud::One(s) => s == expected,
            Aud::Many(v) => v.iter().any(|s| s == expected),
        }
    }
}

#[derive(Debug, Deserialize)]
struct JwkSet {
    keys: Vec<serde_json::Value>,
}

fn required_config(key: &str) -> String {
    match host_config::get(key) {
        Some(v) if !v.is_empty() => v,
        _ => panic!(
            "filter-jwt: [filter.config] must declare a non-empty '{key}' \
             (ADR 000070 / 000066); requires isolation = \"trusted\" so this fails at load"
        ),
    }
}

fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(&h.value).ok())
}

fn bearer_token(req: &HttpRequest) -> Option<&str> {
    let value = header(req, "authorization")?;
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = rest.trim();
    if token.is_empty() { None } else { Some(token) }
}

fn unauthorized_missing(realm: &str) -> RequestDecision {
    RequestDecision::ShortCircuit(HttpResponse {
        status: 401,
        headers: vec![
            Header {
                name: "www-authenticate".to_string(),
                value: format!("Bearer realm=\"{realm}\"").into_bytes(),
            },
            Header {
                name: "content-type".to_string(),
                value: b"application/json".to_vec(),
            },
        ],
        body: br#"{"error":"missing_token"}"#.to_vec(),
    })
}

/// Deliberately generic: the specific rejection reason (expired vs bad signature vs unknown
/// `kid` vs iss/aud mismatch) stays in the host log only. Handing it back to the client turns
/// the response into an oracle a caller without a valid key could use to probe the verifier
/// (RFC 6750 leaves the level of detail to the resource server; this reference keeps it minimal).
const INVALID_TOKEN_DESCRIPTION: &str = "the bearer token is invalid, expired, or not trusted";

fn unauthorized_invalid(realm: &str) -> RequestDecision {
    let www = format!(
        "Bearer realm=\"{realm}\", error=\"invalid_token\", error_description=\"{INVALID_TOKEN_DESCRIPTION}\""
    );
    RequestDecision::ShortCircuit(HttpResponse {
        status: 401,
        headers: vec![
            Header {
                name: "www-authenticate".to_string(),
                value: www.into_bytes(),
            },
            Header {
                name: "content-type".to_string(),
                value: b"application/json".to_vec(),
            },
        ],
        body: serde_json::json!({
            "error": "invalid_token",
            "error_description": INVALID_TOKEN_DESCRIPTION,
        })
        .to_string()
        .into_bytes(),
    })
}

fn parse_url(url: &str) -> Option<(Scheme, String, String)> {
    let (scheme, rest) = if let Some(r) = url.strip_prefix("https://") {
        (Scheme::Https, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (Scheme::Http, r)
    } else {
        return None;
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
        None => (rest.to_string(), "/".to_string()),
    };
    if authority.is_empty() {
        return None;
    }
    Some((scheme, authority, path))
}

fn http_get_body(url: &str) -> Result<Vec<u8>, String> {
    let (scheme, authority, path) = parse_url(url).ok_or_else(|| "bad jwks_url".to_string())?;
    let req = OutgoingRequest::new(Fields::new());
    req.set_method(&Method::Get)
        .map_err(|_| "method".to_string())?;
    req.set_scheme(Some(&scheme))
        .map_err(|_| "scheme".to_string())?;
    req.set_authority(Some(&authority))
        .map_err(|_| "authority".to_string())?;
    req.set_path_with_query(Some(&path))
        .map_err(|_| "path".to_string())?;

    let future = outgoing_handler::handle(req, None).map_err(|e| format!("{e:?}"))?;
    let pollable = future.subscribe();
    let response = loop {
        match future.get() {
            Some(result) => {
                let inner = result.map_err(|_| "future already consumed".to_string())?;
                break inner.map_err(|e| format!("{e:?}"))?;
            }
            None => pollable.block(),
        }
    };

    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(format!("jwks http status {status}"));
    }
    let body = response.consume().map_err(|_| "consume body".to_string())?;
    let stream = body.stream().map_err(|_| "body stream".to_string())?;
    let mut buf = Vec::new();
    loop {
        let chunk = stream
            .blocking_read(8192)
            .map_err(|e| format!("read: {e:?}"))?;
        if chunk.is_empty() {
            break;
        }
        if buf.len().saturating_add(chunk.len()) > MAX_JWKS_BYTES {
            return Err("jwks body too large".to_string());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| "base64url".to_string())
}

fn key_from_pem(pem: &str) -> Result<PublicKey, String> {
    if let Ok(vk) = EcdsaVerifyingKey::from_public_key_pem(pem) {
        return Ok(PublicKey::Es256(vk));
    }
    let rsa = RsaPublicKey::from_public_key_pem(pem).map_err(|e| format!("public_key_pem: {e}"))?;
    Ok(PublicKey::Rs256(rsa))
}

fn key_from_jwk(jwk: &serde_json::Value) -> Result<Option<KeyedKey>, String> {
    let kty = jwk.get("kty").and_then(|v| v.as_str()).unwrap_or("");
    let alg = jwk.get("alg").and_then(|v| v.as_str());
    let kid = jwk.get("kid").and_then(|v| v.as_str()).map(str::to_string);

    match kty {
        "EC" => {
            if let Some(a) = alg
                && a != "ES256"
            {
                return Ok(None);
            }
            let crv = jwk.get("crv").and_then(|v| v.as_str()).unwrap_or("");
            if crv != "P-256" {
                return Ok(None);
            }
            let x = jwk
                .get("x")
                .and_then(|v| v.as_str())
                .ok_or("jwk missing x")?;
            let y = jwk
                .get("y")
                .and_then(|v| v.as_str())
                .ok_or("jwk missing y")?;
            let mut point = Vec::with_capacity(65);
            point.push(0x04);
            point.extend(b64url_decode(x)?);
            point.extend(b64url_decode(y)?);
            let vk =
                EcdsaVerifyingKey::from_sec1_bytes(&point).map_err(|e| format!("ec jwk: {e}"))?;
            Ok(Some(KeyedKey {
                kid,
                key: PublicKey::Es256(vk),
            }))
        }
        "RSA" => {
            if let Some(a) = alg
                && a != "RS256"
            {
                return Ok(None);
            }
            let n = jwk
                .get("n")
                .and_then(|v| v.as_str())
                .ok_or("jwk missing n")?;
            let e = jwk
                .get("e")
                .and_then(|v| v.as_str())
                .ok_or("jwk missing e")?;
            let n = rsa::BigUint::from_bytes_be(&b64url_decode(n)?);
            let e = rsa::BigUint::from_bytes_be(&b64url_decode(e)?);
            let rsa = RsaPublicKey::new(n, e).map_err(|e| format!("rsa jwk: {e}"))?;
            Ok(Some(KeyedKey {
                kid,
                key: PublicKey::Rs256(rsa),
            }))
        }
        _ => Ok(None),
    }
}

fn keys_from_jwks_bytes(bytes: &[u8]) -> Result<Vec<KeyedKey>, String> {
    let set: JwkSet = serde_json::from_slice(bytes).map_err(|e| format!("jwks json: {e}"))?;
    if set.keys.is_empty() {
        return Err("jwks has no keys".to_string());
    }
    let mut out = Vec::new();
    for jwk in &set.keys {
        if let Some(k) = key_from_jwk(jwk)? {
            out.push(k);
        }
    }
    if out.is_empty() {
        return Err("jwks has no ES256/RS256 keys".to_string());
    }
    Ok(out)
}

fn keys_from_config() -> Result<Vec<KeyedKey>, String> {
    let pem = host_config::get("public_key_pem").filter(|s| !s.is_empty());
    let jwk = host_config::get("jwk").filter(|s| !s.is_empty());
    let jwks_url = host_config::get("jwks_url").filter(|s| !s.is_empty());

    let present = [pem.is_some(), jwk.is_some(), jwks_url.is_some()]
        .into_iter()
        .filter(|&p| p)
        .count();
    if present != 1 {
        return Err(
            "exactly one of public_key_pem, jwk, or jwks_url must be set (ADR 000070)".into(),
        );
    }

    if let Some(pem) = pem {
        return Ok(vec![KeyedKey {
            kid: None,
            key: key_from_pem(&pem)?,
        }]);
    }
    if let Some(jwk_str) = jwk {
        let value: serde_json::Value =
            serde_json::from_str(&jwk_str).map_err(|e| format!("jwk json: {e}"))?;
        let keyed = key_from_jwk(&value)?
            .ok_or_else(|| "jwk must be ES256 (EC/P-256) or RS256 (RSA)".to_string())?;
        return Ok(vec![keyed]);
    }

    let url = jwks_url.expect("xor checked");
    let kv_key = kv_jwks_key(&url);
    // A trusted instance's `init` runs on every pool build-out and every recycle (ADR 000012),
    // not just once for the filter's lifetime — without this cache-first check each of those
    // would re-fetch over the network, contradicting the "fetch once, fixed until reload" design
    // (ADR 000070 §3). The key is scoped to `url` so a `jwks_url` change on reload naturally
    // misses and re-fetches instead of serving stale keys from a prior configuration.
    if let Some(cached) = host_kv::get(&kv_key) {
        return keys_from_jwks_bytes(&cached);
    }
    let body = http_get_body(&url)?;
    host_kv::set(&kv_key, &body);
    keys_from_jwks_bytes(&body)
}

fn build_verifier() -> JwtVerifier {
    let issuer = required_config("issuer");
    let audience = required_config("audience");
    let realm = host_config::get("realm")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "plecto".to_string());
    let keys = match keys_from_config() {
        Ok(k) => k,
        Err(e) => panic!("filter-jwt: {e}"),
    };
    JwtVerifier {
        keys,
        issuer,
        audience,
        realm,
    }
}

fn verify_sig(key: &PublicKey, alg: &str, message: &[u8], sig: &[u8]) -> Result<(), String> {
    match (key, alg) {
        (PublicKey::Es256(vk), "ES256") => {
            let signature =
                EcdsaSignature::from_slice(sig).map_err(|_| "bad es256 signature".to_string())?;
            EcdsaVerifier::verify(vk, message, &signature)
                .map_err(|_| "es256 verify failed".to_string())
        }
        (PublicKey::Rs256(pk), "RS256") => {
            let verifying = RsaVerifyingKey::<Sha256>::new(pk.clone());
            let signature =
                RsaSignature::try_from(sig).map_err(|_| "bad rs256 signature".to_string())?;
            RsaVerifierTrait::verify(&verifying, message, &signature)
                .map_err(|_| "rs256 verify failed".to_string())
        }
        _ => Err("key/alg mismatch".to_string()),
    }
}

fn verify_token(v: &JwtVerifier, token: &str, now_ms: u64) -> Result<(String, String), String> {
    let mut parts = token.split('.');
    let h_b64 = parts.next().ok_or_else(|| "malformed jwt".to_string())?;
    let p_b64 = parts.next().ok_or_else(|| "malformed jwt".to_string())?;
    let s_b64 = parts.next().ok_or_else(|| "malformed jwt".to_string())?;
    if parts.next().is_some() {
        return Err("malformed jwt".to_string());
    }

    let header_bytes = b64url_decode(h_b64)?;
    let header: JoseHeader =
        serde_json::from_slice(&header_bytes).map_err(|_| "bad jwt header".to_string())?;
    if header.alg != "ES256" && header.alg != "RS256" {
        return Err("unsupported alg".to_string());
    }

    let signing_input = format!("{h_b64}.{p_b64}");
    let sig = b64url_decode(s_b64)?;

    let candidates: Vec<&KeyedKey> = v
        .keys
        .iter()
        .filter(|k| match (&header.kid, &k.kid) {
            (Some(want), Some(have)) => want == have,
            (Some(_), None) => false,
            (None, _) => true,
        })
        .filter(|k| {
            matches!(
                (&k.key, header.alg.as_str()),
                (PublicKey::Es256(_), "ES256") | (PublicKey::Rs256(_), "RS256")
            )
        })
        .collect();
    if candidates.is_empty() {
        return Err("no matching key".to_string());
    }

    let mut verified = false;
    for keyed in candidates {
        if verify_sig(&keyed.key, &header.alg, signing_input.as_bytes(), &sig).is_ok() {
            verified = true;
            break;
        }
    }
    if !verified {
        return Err("signature".to_string());
    }

    let claims_bytes = b64url_decode(p_b64)?;
    let claims: Claims =
        serde_json::from_slice(&claims_bytes).map_err(|_| "bad jwt claims".to_string())?;

    let now_secs = now_ms / 1000;
    if claims.iss != v.issuer {
        return Err("iss mismatch".to_string());
    }
    if !claims.aud.contains(&v.audience) {
        return Err("aud mismatch".to_string());
    }
    if claims.sub.is_empty() {
        return Err("missing sub".to_string());
    }
    if claims.exp <= now_secs {
        return Err("token expired".to_string());
    }
    if let Some(nbf) = claims.nbf
        && nbf > now_secs
    {
        return Err("token not yet valid".to_string());
    }
    Ok((claims.sub, claims.iss))
}

impl Guest for FilterJwt {
    fn init() {
        let verifier = build_verifier();
        host_log::log(
            host_log::Level::Info,
            &format!(
                "filter-jwt: init ok issuer={} audience={} keys={}",
                verifier.issuer,
                verifier.audience,
                verifier.keys.len()
            ),
        );
        VERIFIER.with(|slot| *slot.borrow_mut() = Some(verifier));
    }

    fn on_request(req: HttpRequest) -> RequestDecision {
        let realm = VERIFIER.with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|v| v.realm.clone())
                .unwrap_or_else(|| "plecto".to_string())
        });

        let Some(token) = bearer_token(&req) else {
            host_log::log(host_log::Level::Info, "filter-jwt: missing bearer");
            return unauthorized_missing(&realm);
        };

        let now_ms = host_clock::now_ms();
        let outcome = VERIFIER.with(|slot| {
            let borrow = slot.borrow();
            let Some(v) = borrow.as_ref() else {
                return Err("verifier not initialized".to_string());
            };
            verify_token(v, token, now_ms)
        });

        match outcome {
            Ok((sub, iss)) => {
                host_log::log(
                    host_log::Level::Info,
                    &format!("filter-jwt: authenticated sub={sub} iss={iss}"),
                );
                RequestDecision::Modified(RequestEdit {
                    set_headers: vec![
                        Header {
                            name: "x-authenticated-user".to_string(),
                            value: sub.into_bytes(),
                        },
                        Header {
                            name: "x-jwt-issuer".to_string(),
                            value: iss.into_bytes(),
                        },
                    ],
                    remove_headers: vec![],
                })
            }
            Err(reason) => {
                host_log::log(
                    host_log::Level::Info,
                    &format!("filter-jwt: reject ({reason})"),
                );
                unauthorized_invalid(&realm)
            }
        }
    }

    fn on_response(_resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Continue
    }
}

export!(FilterJwt);
