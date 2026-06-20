//! filter-hello — a minimal `plecto:filter` for the v0.1.0 contract slice (ADR 000010).
//!
//! Doubles as the hand-written conformance fixture the host's E2E/conformance
//! tests load (tdd-workflow Phase 0/1). Behaviour:
//!   - on-request: short-circuit 403 if the `x-plecto-block` header is present,
//!     otherwise continue.
//!   - on-response: continue.

// wit-bindgen flattens records into many core-wasm ABI args (e.g. http-request's
// 5 string/list fields), so the generated FFI shims trip clippy::too_many_arguments.
// This allow scopes ONLY to generated code, not to hand-written filter logic.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../wit",
    world: "filter",
});

use crate::plecto::filter::host_log;
use crate::plecto::filter::types::Header;

struct FilterHello;

impl Guest for FilterHello {
    fn init() {}

    fn on_request(req: HttpRequest) -> RequestDecision {
        host_log::log(host_log::Level::Info, "filter-hello: on-request");
        let blocked = req
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("x-plecto-block"));
        if blocked {
            RequestDecision::ShortCircuit(HttpResponse {
                status: 403,
                headers: vec![Header {
                    name: "x-plecto".to_string(),
                    value: "blocked".to_string(),
                }],
                body: b"blocked by filter-hello".to_vec(),
            })
        } else {
            RequestDecision::Continue
        }
    }

    fn on_response(_resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Continue
    }
}

export!(FilterHello);
