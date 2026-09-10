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
//! allowed. A disallowed origin gets **no CORS grant** — the browser enforces the block — while
//! `Vary: Origin` keeps cache entries separated (a missing/empty allowlist grants nothing).
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
    path: "../../../wit/v0.3.0",
    world: "filter",
});

use crate::plecto::filter::host_config;
use crate::plecto::filter::types::{Header, ResponseEdit};

struct FilterCors;

const DEFAULT_ALLOW_METHODS: &str = "GET, POST, OPTIONS";

// The filter owns the CORS protocol surface. Response edits remove every CORS response header
// before adding the operator-authorized subset, so an upstream cannot widen the policy with a
// stale or independently configured grant.
const CORS_RESPONSE_HEADERS: &[&str] = &[
    "access-control-allow-origin",
    "access-control-allow-credentials",
    "access-control-expose-headers",
    "access-control-allow-methods",
    "access-control-allow-headers",
    "access-control-max-age",
];

fn remove_cors_response_headers() -> Vec<String> {
    CORS_RESPONSE_HEADERS
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

/// Return a header value only when the request has exactly one valid UTF-8 occurrence. CORS
/// policy headers are security boundaries; choosing an arbitrary duplicate lets another hop
/// interpret a different value.
fn unique_header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    let mut headers = req
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case(name));
    let header = headers.next()?;
    if headers.next().is_some() {
        return None;
    }
    std::str::from_utf8(&header.value).ok()
}

fn has_header(req: &HttpRequest, name: &str) -> bool {
    req.headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case(name))
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

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn vary(existing: &[Header], additions: &[&str]) -> Header {
    let mut values = Vec::new();
    for header in existing
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("vary"))
    {
        let Ok(value) = std::str::from_utf8(&header.value) else {
            // Dropping an unparseable upstream Vary weakens its cache constraint. `*` is the
            // conservative representation when this filter cannot preserve it exactly.
            return h("vary", "*");
        };
        for token in value
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            if token == "*" {
                return h("vary", "*");
            }
            if !token.as_bytes().iter().copied().all(is_tchar) {
                return h("vary", "*");
            }
            if !values
                .iter()
                .any(|known: &String| known.eq_ignore_ascii_case(token))
            {
                values.push(token.to_string());
            }
        }
    }
    for addition in additions {
        if !values
            .iter()
            .any(|known| known.eq_ignore_ascii_case(addition))
        {
            values.push((*addition).to_string());
        }
    }
    h("vary", &values.join(", "))
}

/// The CORS headers shared by preflight and actual responses. `Vary` marks the response's CORS
/// policy inputs as cache variants.
fn common_headers(allow_origin: &str, vary: Header) -> Vec<Header> {
    let mut out = vec![h("access-control-allow-origin", allow_origin), vary];
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
        if !has_header(&req, "origin") || !has_header(&req, "access-control-request-method") {
            return RequestDecision::Continue;
        }
        let (Some(origin), Some(_)) = (
            unique_header(&req, "origin"),
            unique_header(&req, "access-control-request-method"),
        ) else {
            return RequestDecision::ShortCircuit(HttpResponse {
                status: 204,
                headers: vec![vary(&[], &["Origin", "Access-Control-Request-Headers"])],
                body: Vec::new(),
            });
        };
        if has_header(&req, "access-control-request-headers")
            && unique_header(&req, "access-control-request-headers").is_none()
        {
            return RequestDecision::ShortCircuit(HttpResponse {
                status: 204,
                headers: vec![vary(&[], &["Origin", "Access-Control-Request-Headers"])],
                body: Vec::new(),
            });
        }

        let Some(allow) = allow_origin_value(origin) else {
            // Disallowed origin: answer the preflight with NO CORS headers — the browser
            // fails the check. The preflight still never reaches upstream (it is addressed
            // to the gateway's CORS layer, not the application).
            return RequestDecision::ShortCircuit(HttpResponse {
                status: 204,
                headers: vec![vary(&[], &["Origin", "Access-Control-Request-Headers"])],
                body: Vec::new(),
            });
        };
        let mut headers = common_headers(
            &allow,
            vary(&[], &["Origin", "Access-Control-Request-Headers"]),
        );
        let methods =
            host_config::get("allow-methods").unwrap_or_else(|| DEFAULT_ALLOW_METHODS.to_string());
        headers.push(h("access-control-allow-methods", &methods));
        let requested = unique_header(&req, "access-control-request-headers");
        if let Some(allow_headers) = host_config::get("allow-headers")
            .or_else(|| requested.map(str::to_string))
            .filter(|v| !v.is_empty())
        {
            headers.push(h("access-control-allow-headers", &allow_headers));
        }
        if let Some(max_age) = host_config::get("max-age").filter(|v| !v.is_empty()) {
            headers.push(h("access-control-max-age", &max_age));
        }
        RequestDecision::ShortCircuit(HttpResponse {
            status: 204,
            headers,
            body: Vec::new(),
        })
    }

    fn on_response(req: HttpRequest, resp: HttpResponse) -> ResponseDecision {
        // Dynamic origin echo (ADR 000073): the request's Origin is read from the as-forwarded
        // snapshot — no guest global, no host query, works on any pooled instance.
        let vary = vary(&resp.headers, &["Origin"]);
        let Some(origin) = unique_header(&req, "origin") else {
            return ResponseDecision::Modified(ResponseEdit {
                set_status: None,
                set_headers: vec![vary],
                remove_headers: remove_cors_response_headers(),
            });
        };
        let Some(allow) = allow_origin_value(origin) else {
            return ResponseDecision::Modified(ResponseEdit {
                set_status: None,
                set_headers: vec![vary],
                remove_headers: remove_cors_response_headers(),
            });
        };
        ResponseDecision::Modified(ResponseEdit {
            set_status: None,
            set_headers: common_headers(&allow, vary),
            remove_headers: remove_cors_response_headers(),
        })
    }
}

export!(FilterCors);
