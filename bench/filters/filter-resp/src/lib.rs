//! filter-resp — lean `plecto:filter` for the ADR 000073 response-side cost ladder.
//!
//! No host-API calls. `on-request` is a pure `continue`. `on-response` always *reads* the
//! as-forwarded request snapshot (path + header scan); with `x-plecto-resp-replace` it
//! `replace`s with a synthesised 418, otherwise `continue`. Adjacent deltas vs `/noop-pooled`
//! isolate (1) response-context read cost and (2) `replace` synthesis cost.

#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../../plecto/wit",
    world: "filter",
});

use crate::plecto::filter::types::Header;

struct FilterResp;

fn has_header(req: &HttpRequest, name: &str) -> bool {
    req.headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case(name))
}

impl Guest for FilterResp {
    fn init() {}

    fn on_request(_req: HttpRequest) -> RequestDecision {
        RequestDecision::Continue
    }

    fn on_response(req: HttpRequest, _resp: HttpResponse) -> ResponseDecision {
        // Touch the as-forwarded snapshot — the contract surface ADR 000073 added to every
        // `on-response` crossing. Path length + header scan is the "read-only" rung.
        let _ = core::hint::black_box(req.path.len());
        let _ = core::hint::black_box(req.headers.len());

        if has_header(&req, "x-plecto-resp-replace") {
            return ResponseDecision::Replace(HttpResponse {
                status: 418,
                headers: vec![Header {
                    name: "x-plecto-replaced".to_string(),
                    value: b"1".to_vec(),
                }],
                body: b"replaced by filter-resp".to_vec(),
            });
        }

        ResponseDecision::Continue
    }
}

export!(FilterResp);
