//! filter-compat-v02 — the frozen-contract fixture: a guest deliberately built against
//! `plecto:filter@0.2.0` (`wit/v0.2.0/`, ADR 000071) after the in-tree default moved to 0.3.0
//! (ADR 000073). Loading and running it proves the compat promise end to end (ADR 000064):
//! version detection picks the V02 binding, the 0.2 host-API still links, and the response
//! hook runs with the request-context parameter transparently dropped by the adapter.
//!
//! Behaviour: `on-response` always answers `modified` stamping `x-plecto-v02-ran: 1` — a 0.2
//! response edit the tests can observe through the 0.3 host. `init` logs once so the component
//! keeps at least one `plecto:filter/…@0.2.0` import after componentization prunes unused ones
//! (version detection keys on the import names, ADR 000071).

#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../../../wit/v0.2.0",
    world: "filter",
});

use crate::plecto::filter::host_log;
use crate::plecto::filter::types::{Header, ResponseEdit};

struct FilterCompatV02;

impl Guest for FilterCompatV02 {
    fn init() {
        host_log::log(host_log::Level::Info, "filter-compat-v02: init (0.2.0 guest)");
    }

    fn on_request(_req: HttpRequest) -> RequestDecision {
        RequestDecision::Continue
    }

    fn on_response(_resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Modified(ResponseEdit {
            set_status: None,
            set_headers: vec![Header {
                name: "x-plecto-v02-ran".to_string(),
                value: b"1".to_vec(),
            }],
            remove_headers: vec![],
        })
    }
}

export!(FilterCompatV02);
