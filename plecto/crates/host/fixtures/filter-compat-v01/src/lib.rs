//! filter-compat-v01 — the oldest frozen-contract fixture: a guest deliberately built against
//! `plecto:filter@0.1.0` (`wit/v0.1.0/`, ADR 000010) and loaded by a host whose current contract
//! is 0.4.0. It keeps the V01 adapter rail falsifiable in CI (ADR 000064): string-valued headers
//! project through, `on-response` runs with the request-context parameter transparently dropped,
//! and the 0.1 body arm — `continue(list<u8>)`, which always meant "forward THIS body" — carries
//! its transform onto the 0.4.0 `modified` arm instead of being read as "unchanged".
//!
//! `init` logs once so the component keeps at least one `plecto:filter/…@0.1.0` import after
//! componentization prunes unused ones (version detection keys on the import names, ADR 000071).

#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../../../wit/v0.1.0",
    world: "filter-body",
});

use crate::plecto::filter::host_log;
use crate::plecto::filter::types::{Header, ResponseEdit};

struct FilterCompatV01;

impl Guest for FilterCompatV01 {
    fn init() {
        host_log::log(host_log::Level::Info, "filter-compat-v01: init (0.1.0 guest)");
    }

    fn on_request(_req: HttpRequest) -> RequestDecision {
        RequestDecision::Continue
    }

    fn on_request_body(body: Vec<u8>) -> RequestBodyDecision {
        RequestBodyDecision::Continue(body.to_ascii_uppercase())
    }

    fn on_response(_resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Modified(ResponseEdit {
            set_status: None,
            set_headers: vec![Header {
                name: "x-plecto-v01-ran".to_string(),
                value: "1".to_string(),
            }],
            remove_headers: vec![],
        })
    }
}

export!(FilterCompatV01);
