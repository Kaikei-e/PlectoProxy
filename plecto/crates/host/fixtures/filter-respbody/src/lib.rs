//! filter-respbody — the acceptance-lattice fixture (ADR 000098 decision 2). It exports
//! `on-response-body` and NOT `on-request-body`: a subset of the body hooks that neither published
//! world names, which is the machine-checkable statement that the LATTICE is the contract and the
//! world list is only its two ends. Loading it must therefore buffer the response body and leave
//! the request body on the zero-copy path.
//!
//! Its world is declared inline here against the canonical `wit/` package instead of being added
//! to that package, so no combination world ever reaches the public surface.
//!
//! The decisions are driven from the request target (so an E2E can pick one per request) and from
//! the upstream body:
//!   - `respbody=replace` → `replace`, discarding the buffered upstream body;
//!   - `respbody=content-length` → `modified` carrying a guest `content-length`, which the host
//!     owns and must reject fail-closed;
//!   - `respbody=inflate` → `modified` with a body far larger than a small per-route cap, which
//!     the host must re-validate against that cap;
//!   - `respbody=shrink` → `modified` with a body well inside any cap, so a transform can be
//!     driven without tripping that re-validation;
//!   - a body containing `SECRET` → `modified` that redacts it, plus the header edits a body
//!     transform forces;
//!   - anything else → the BARE `%continue` arm, so the host forwards what it buffered.

// wit-bindgen flattens records into many core-wasm ABI args, so generated FFI shims trip
// clippy::too_many_arguments. This allow scopes ONLY to generated code.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../../../wit",
    inline: r#"
package plecto:respbody-fixture;

world filter-response-body {
    use plecto:filter/types@0.4.0.{http-request, http-response, request-decision, response-decision, response-body-decision};

    import plecto:filter/host-log@0.4.0;

    export init: func();
    export on-request: func(req: http-request) -> request-decision;
    export on-response: func(req: http-request, resp: http-response) -> response-decision;
    export on-response-body: func(req: http-request, resp: http-response, body: list<u8>) -> response-body-decision;
}
"#,
    world: "filter-response-body",
    // The world's types and its one import live in the foreign `plecto:filter` package, so
    // bindings for them have to be generated rather than mapped onto an existing module.
    generate_all,
});

use crate::plecto::filter::host_log;
use crate::plecto::filter::types::{Header, ResponseBodyEdit};

/// The redacted stand-in, byte-length-different from the marker on purpose: the host re-derives
/// content-length from what it actually forwards, so a transform that changes the length must
/// still arrive framed correctly.
const MARKER: &[u8] = b"SECRET";
const REDACTED: &[u8] = b"[redacted]";

struct FilterRespBody;

fn header(name: &str, value: &[u8]) -> Header {
    Header {
        name: name.to_string(),
        value: value.to_vec(),
    }
}

fn redact(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i..].starts_with(MARKER) {
            out.extend_from_slice(REDACTED);
            i += MARKER.len();
        } else {
            out.push(body[i]);
            i += 1;
        }
    }
    out
}

impl Guest for FilterRespBody {
    fn init() {
        host_log::log(
            host_log::Level::Info,
            "filter-respbody: init (response-body-only guest)",
        );
    }

    fn on_request(_req: HttpRequest) -> RequestDecision {
        RequestDecision::Continue
    }

    fn on_response(_req: HttpRequest, _resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Continue
    }

    fn on_response_body(
        req: HttpRequest,
        _resp: HttpResponse,
        body: Vec<u8>,
    ) -> ResponseBodyDecision {
        if req.path_with_query.contains("respbody=replace") {
            return ResponseBodyDecision::Replace(HttpResponse {
                status: 418,
                headers: vec![header("x-plecto-respbody", b"replaced")],
                body: b"replaced by filter-respbody".to_vec(),
            });
        }
        if req.path_with_query.contains("respbody=content-length") {
            return ResponseBodyDecision::Modified(ResponseBodyEdit {
                body: b"guest framing attempt".to_vec(),
                set_headers: vec![header("content-length", b"1")],
                remove_headers: vec![],
            });
        }
        if req.path_with_query.contains("respbody=shrink") {
            return ResponseBodyDecision::Modified(ResponseBodyEdit {
                body: b"shrunk".to_vec(),
                set_headers: vec![],
                remove_headers: vec![],
            });
        }
        if req.path_with_query.contains("respbody=inflate") {
            return ResponseBodyDecision::Modified(ResponseBodyEdit {
                body: vec![b'x'; 4096],
                set_headers: vec![],
                remove_headers: vec![],
            });
        }
        if body.windows(MARKER.len()).any(|w| w == MARKER) {
            return ResponseBodyDecision::Modified(ResponseBodyEdit {
                body: redact(&body),
                set_headers: vec![header("x-plecto-redacted", b"1")],
                remove_headers: vec!["x-drop-me".to_string()],
            });
        }
        ResponseBodyDecision::Continue
    }
}

export!(FilterRespBody);
