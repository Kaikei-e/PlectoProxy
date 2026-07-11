//! filter-cors — a reference `plecto:filter`: a **CORS policy filter** (the ADR 000073
//! motivating use case, shelved as an F2 reference filter per ADR 000068).
//!
//! It exercises both response-side capabilities the 0.3.0 contract adds:
//!   - **request context on `on-response`** — the dynamic origin echo (`Access-Control-Allow-Origin`
//!     reflecting the request's `Origin`) reads the as-forwarded request snapshot the host passes
//!     as `on-response`'s first parameter. Before 0.3.0 this was not expressible: the pool checks
//!     the two hooks out independently, so guest globals cannot carry the origin across
//!     (ADR 000011 / 000073).
//!   - **typed decisions end to end** — the preflight answer is a request-side `short-circuit`
//!     (never reaches upstream); actual-response headers are a `modified` edit.
//!
//! The policy is the general CORS protocol shape (WHATWG Fetch): a *preflight* (`OPTIONS` +
//! `Origin` + `Access-Control-Request-Method`) is answered by the gateway; an *actual* request
//! flows upstream and its response gains the `Access-Control-Allow-*` headers when the origin is
//! allowed. A disallowed origin simply gets **no** CORS headers — the browser enforces the block
//! (fail-safe: a missing/empty allowlist means no header is ever added).
//!
//! Operator config (`[filter.config]`, ADR 000066 — the filter cannot widen its own policy):
//!   - `allowed-origins`  comma-separated exact origins, or `*` (required for any effect)
//!   - `allow-methods`    preflight `Access-Control-Allow-Methods` (default `GET, POST, OPTIONS`)
//!   - `allow-headers`    preflight `Access-Control-Allow-Headers` (default: echo the request's
//!     `Access-Control-Request-Headers`)
//!   - `allow-credentials` `"true"` adds `Access-Control-Allow-Credentials` (and disables `*`
//!     in `allowed-origins` — list concrete origins; do not echo every Origin)
//!     form even under `*` (the credentialed wildcard is forbidden by the protocol)
//!   - `max-age`          preflight `Access-Control-Max-Age` seconds

// wit-bindgen flattens records into many core-wasm ABI args; the generated FFI shims trip
// clippy::too_many_arguments. Scope the allow to this crate's generated code only.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../../wit",
    world: "filter",
});

use crate::plecto::filter::host_config;
use crate::plecto::filter::types::{Header, ResponseEdit};

struct FilterCors;

const DEFAULT_ALLOW_METHODS: &str = "GET, POST, OPTIONS";

fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(&h.value).ok())
}

fn h(name: &str, value: &str) -> Header {
    Header {
        name: name.to_string(),
        value: value.as_bytes().to_vec(),
    }
}

/// The origin's allowlist verdict: `None` = not allowed (add nothing), `Some(value)` = the
/// `Access-Control-Allow-Origin` value to send. Origins compare byte-exact (the serialized-origin
/// comparison of the CORS protocol). A bare `*` is answered literally when credentials are off;
/// when credentials are on, `*` is ignored (listing concrete origins is required — echoing every
/// Origin under `Access-Control-Allow-Credentials: true` is an operator footgun).
fn allow_origin_value(origin: &str) -> Option<String> {
    let allowlist = host_config::get("allowed-origins")?;
    let credentials = allows_credentials();
    for entry in allowlist.split(',') {
        let entry = entry.trim();
        if entry == "*" {
            // Credentialed wildcard would echo every Origin — a common operator footgun.
            // Refuse the `*` entry when credentials are on; list concrete origins instead.
            if credentials {
                continue;
            }
            return Some("*".to_string());
        }
        if entry == origin {
            return Some(origin.to_string());
        }
    }
    None
}

fn allows_credentials() -> bool {
    host_config::get("allow-credentials").as_deref() == Some("true")
}

/// The CORS headers shared by preflight and actual responses. `Vary: Origin` marks the response
/// as origin-dependent for caches whenever the echo form (not the literal `*`) is used.
fn common_headers(allow_origin: &str) -> Vec<Header> {
    let mut out = vec![h("access-control-allow-origin", allow_origin)];
    if allow_origin != "*" {
        out.push(h("vary", "Origin"));
    }
    if allows_credentials() {
        out.push(h("access-control-allow-credentials", "true"));
    }
    out
}

impl Guest for FilterCors {
    fn init() {}

    fn on_request(req: HttpRequest) -> RequestDecision {
        // A preflight is exactly: OPTIONS + Origin + Access-Control-Request-Method. Anything
        // else (including a plain OPTIONS) flows upstream untouched; the Origin header rides
        // the as-forwarded snapshot to on-response.
        if !req.method.eq_ignore_ascii_case("OPTIONS") {
            return RequestDecision::Continue;
        }
        let (Some(origin), Some(_)) = (
            header(&req, "origin"),
            header(&req, "access-control-request-method"),
        ) else {
            return RequestDecision::Continue;
        };

        let mut headers = match allow_origin_value(origin) {
            Some(allow) => common_headers(&allow),
            // Disallowed origin: answer the preflight with NO CORS headers — the browser
            // fails the check. The preflight still never reaches upstream (it is addressed
            // to the gateway's CORS layer, not the application).
            None => Vec::new(),
        };
        if !headers.is_empty() {
            let methods = host_config::get("allow-methods")
                .unwrap_or_else(|| DEFAULT_ALLOW_METHODS.to_string());
            headers.push(h("access-control-allow-methods", &methods));
            let requested = header(&req, "access-control-request-headers");
            if let Some(allow_headers) = host_config::get("allow-headers")
                .or_else(|| requested.map(str::to_string))
                .filter(|v| !v.is_empty())
            {
                headers.push(h("access-control-allow-headers", &allow_headers));
            }
            if let Some(max_age) = host_config::get("max-age").filter(|v| !v.is_empty()) {
                headers.push(h("access-control-max-age", &max_age));
            }
        }
        RequestDecision::ShortCircuit(HttpResponse {
            status: 204,
            headers,
            body: Vec::new(),
        })
    }

    fn on_response(req: HttpRequest, _resp: HttpResponse) -> ResponseDecision {
        // Dynamic origin echo (ADR 000073): the request's Origin is read from the as-forwarded
        // snapshot — no guest global, no host query, works on any pooled instance.
        let Some(origin) = header(&req, "origin") else {
            return ResponseDecision::Continue;
        };
        let Some(allow) = allow_origin_value(origin) else {
            return ResponseDecision::Continue;
        };
        ResponseDecision::Modified(ResponseEdit {
            set_status: None,
            set_headers: common_headers(&allow),
            remove_headers: vec![],
        })
    }
}

export!(FilterCors);
