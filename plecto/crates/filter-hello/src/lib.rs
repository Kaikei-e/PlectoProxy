//! filter-hello — a minimal `plecto:filter` exercising the ADR 000004 host runtime.
//!
//! Doubles as the hand-written conformance fixture the host's E2E/conformance tests load
//! (tdd-workflow Phase 0/1). It imports — and calls — every lent capability so loading it
//! proves the whole host-API surface resolves (consumer-driven contract). Behaviour:
//!   - init: bump the host counter `init-calls`. Observing this from on-request lets a
//!     test see init-ONCE for trusted filters vs init-per-request for untrusted ones
//!     (ADR 000011 / Tenet 4).
//!   - on-request:
//!       * log how many times init has run so far (reads the `init-calls` counter);
//!       * if `x-plecto-ratelimit` is present, consult a tiny host-native token bucket
//!         and short-circuit 429 when empty (ADR 000005);
//!       * if `x-plecto-block` is present, short-circuit 403;
//!       * otherwise continue.
//!   - on-response: continue.

// wit-bindgen flattens records into many core-wasm ABI args, so generated FFI shims trip
// clippy::too_many_arguments. This allow scopes ONLY to generated code.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../wit",
    world: "filter",
});

use crate::plecto::filter::host_counter;
use crate::plecto::filter::host_log;
use crate::plecto::filter::host_ratelimit::{self, Bucket};
use crate::plecto::filter::types::Header;

struct FilterHello;

fn has_header(req: &HttpRequest, name: &str) -> bool {
    req.headers.iter().any(|h| h.name.eq_ignore_ascii_case(name))
}

impl Guest for FilterHello {
    fn init() {
        // Heavy-init marker: how many times has init run for this filter identity?
        host_counter::increment("init-calls", 1);
    }

    fn on_request(req: HttpRequest) -> RequestDecision {
        host_log::log(host_log::Level::Info, "filter-hello: on-request");
        // observable init-once signal: stays 1 for a reused (trusted) instance, grows for
        // a fresh-per-request (untrusted) one.
        let inits = host_counter::get("init-calls");
        host_log::log(host_log::Level::Info, &format!("init-calls={inits}"));

        if has_header(&req, "x-plecto-ratelimit") {
            let outcome = host_ratelimit::try_acquire(
                "default",
                1,
                Bucket { capacity: 2, refill_tokens: 1, refill_interval_ms: 60_000 },
            );
            if !outcome.allowed {
                return RequestDecision::ShortCircuit(HttpResponse {
                    status: 429,
                    headers: vec![Header {
                        name: "retry-after-ms".to_string(),
                        value: outcome.retry_after_ms.to_string(),
                    }],
                    body: b"rate limited by filter-hello".to_vec(),
                });
            }
        }

        if has_header(&req, "x-plecto-block") {
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
