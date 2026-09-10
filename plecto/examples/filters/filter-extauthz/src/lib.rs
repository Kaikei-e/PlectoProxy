//! filter-extauthz — an ext_authz-style `plecto:filter` that calls an external authorization
//! endpoint over the lent outbound HTTP capability (ADR 000036) and decides:
//!   - a 2xx from the authz endpoint → `continue`,
//!   - any other status, or ANY outbound error (allowlist deny / SSRF block / timeout / protocol) →
//!     short-circuit 403. A failed or blocked authz check is NEVER treated as "allow" (fail-closed).
//!
//! The target URL is supplied by the operator as the read-only `authz-url` filter configuration.
//! It is never taken from a request header: a client must not be able to select which authorization
//! service decides its request. Built for wasm32-wasip2 — unlike the header-only filters it imports
//! `wasi:http/outgoing-handler` (via the `wasi` crate). The host still gates every call by the
//! operator allowlist + SSRF guard; this guest cannot widen either boundary.
#![allow(clippy::all)]

wit_bindgen::generate!({
    path: "../../../wit/v0.3.0",
    world: "filter",
});

use crate::plecto::filter::types::Header;
use crate::plecto::filter::{host_config, host_log};

use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, Method, OutgoingRequest, Scheme};

struct FilterExtAuthz;

const AUTHZ_URL_CONFIG: &str = "authz-url";

fn forbid(reason: &str) -> RequestDecision {
    RequestDecision::ShortCircuit(HttpResponse {
        status: 403,
        headers: vec![Header {
            name: "content-type".to_string(),
            value: b"text/plain".to_vec(),
        }],
        body: format!("ext_authz denied: {reason}").into_bytes(),
    })
}

/// Split `scheme://authority/path?query` into its parts (no url crate — keep the guest tiny).
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

/// Call the authz endpoint. `Ok(status)` on a completed HTTP response; `Err(reason)` on any failure —
/// the caller treats `Err` as deny (fail-closed). `reason` carries the wasi `error-code` so a test
/// can distinguish an allowlist deny from an SSRF block.
fn authorize(url: &str) -> Result<u16, String> {
    let (scheme, authority, path) = parse_url(url).ok_or_else(|| "bad url".to_string())?;

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
    loop {
        match future.get() {
            Some(result) => {
                let inner = result.map_err(|_| "future already consumed".to_string())?;
                let response = inner.map_err(|e| format!("{e:?}"))?;
                return Ok(response.status());
            }
            None => pollable.block(),
        }
    }
}

impl Guest for FilterExtAuthz {
    fn init() {
        host_log::log(host_log::Level::Info, "filter-extauthz: init");
    }

    fn on_request(_req: HttpRequest) -> RequestDecision {
        let Some(url) = host_config::get(AUTHZ_URL_CONFIG).filter(|url| !url.is_empty()) else {
            return forbid("no authz url");
        };
        match authorize(&url) {
            Ok(status) if (200..300).contains(&status) => RequestDecision::Continue,
            Ok(status) => forbid(&format!("authz status {status}")),
            Err(reason) => forbid(&reason),
        }
    }

    fn on_response(_req: HttpRequest, _resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Continue
    }
}

export!(FilterExtAuthz);
