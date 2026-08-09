//! filter-v04 — the 0.4.0-native fixture (ADR 000098 / 000104). Every in-tree reference filter
//! is still built against the frozen 0.3.0 contract, which is how the compat rail stays
//! falsifiable; this guest is the other half of that proof — it targets the CURRENT contract and
//! exercises the three things only 0.4.0 can express:
//!   - `on-request` decides on `path-with-query`, so the query is visibly reachable under the
//!     renamed field (a request carrying `deny=1` short-circuits 403);
//!   - `on-request-body` returns the BARE `%continue` arm for an ordinary body — the host must
//!     forward the buffered bytes without the guest handing them back;
//!   - a body starting with `rewrite` returns `modified(request-body-edit)`: the transformed
//!     body PLUS the header edits a body transform usually forces (`x-plecto-body-edited` set,
//!     `x-drop-me` removed);
//!   - a body carrying the `deny-body` marker short-circuits 403 before upstream.
//! `init` logs once so the component keeps at least one `plecto:filter/…@0.4.0` import after
//! componentization prunes unused ones (version detection keys on the import names, ADR 000071).

// wit-bindgen flattens records into many core-wasm ABI args, so generated FFI shims trip
// clippy::too_many_arguments. This allow scopes ONLY to generated code.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../../../wit",
    world: "filter-body",
});

use crate::plecto::filter::host_log;
use crate::plecto::filter::types::{Header, RequestBodyEdit};

struct FilterV04;

impl Guest for FilterV04 {
    fn init() {
        host_log::log(host_log::Level::Info, "filter-v04: init (0.4.0 guest)");
    }

    fn on_request(req: HttpRequest) -> RequestDecision {
        if req.path_with_query.contains("deny=1") {
            return RequestDecision::ShortCircuit(HttpResponse {
                status: 403,
                headers: vec![Header {
                    name: "x-plecto-v04".to_string(),
                    value: b"query-seen".to_vec(),
                }],
                body: b"blocked by filter-v04".to_vec(),
            });
        }
        RequestDecision::Continue
    }

    fn on_request_body(body: Vec<u8>) -> RequestBodyDecision {
        if body
            .windows(9)
            .any(|w| w.eq_ignore_ascii_case(b"deny-body"))
        {
            return RequestBodyDecision::ShortCircuit(HttpResponse {
                status: 403,
                headers: vec![],
                body: b"blocked body by filter-v04".to_vec(),
            });
        }
        if body.starts_with(b"rewrite") {
            return RequestBodyDecision::Modified(RequestBodyEdit {
                body: body.to_ascii_uppercase(),
                set_headers: vec![Header {
                    name: "x-plecto-body-edited".to_string(),
                    value: b"1".to_vec(),
                }],
                remove_headers: vec!["x-drop-me".to_string()],
            });
        }
        RequestBodyDecision::Continue
    }

    fn on_response(_req: HttpRequest, _resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Continue
    }
}

export!(FilterV04);
