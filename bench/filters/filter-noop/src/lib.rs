//! filter-noop — the leanest possible `plecto:filter`: it makes NO host-API calls and returns
//! `continue` on every hook.
//!
//! It is the benchmark's "pure WASM no-op" rung of the cost ladder (ADR 000005 / 000012). The
//! delta between the native `/baseline` route and a route running this filter isolates the
//! irreducible extension-plane per-request cost — chain dispatch + instance acquisition + one empty
//! host↔guest crossing — with none of the host-KV / header / body work a real filter adds. Running
//! it pooled (trusted) vs fresh-per-request (untrusted) then isolates the instantiation cost the
//! pool amortizes. Built for wasm32-unknown-unknown (ADR 000010).

// wit-bindgen flattens records into many core-wasm ABI args; the generated FFI shims trip
// clippy::too_many_arguments. Scope the allow to this crate's generated code only.
#![allow(clippy::too_many_arguments)]

// It reads the request body (the memory probe drives `on-request-body` through it) and nothing
// else, so it declares exactly that hook: the acceptance lattice (ADR 000098) admits any subset of
// the body hooks, and taking the both-hooks world would make every route running this filter buffer
// RESPONSE bodies too — a cost the ladder is not trying to measure.
wit_bindgen::generate!({
    path: "../../../plecto/wit",
    inline: r#"
package plecto:noop-bench;

world filter-request-body {
    use plecto:filter/types@0.4.0.{http-request, http-response, request-decision, response-decision, request-body-decision};

    import plecto:filter/host-log@0.4.0;

    export init: func();
    export on-request: func(req: http-request) -> request-decision;
    export on-request-body: func(body: list<u8>) -> request-body-decision;
    export on-response: func(req: http-request, resp: http-response) -> response-decision;
}
"#,
    world: "filter-request-body",
    generate_all,
});

struct FilterNoop;

impl Guest for FilterNoop {
    fn init() {}

    fn on_request(_req: HttpRequest) -> RequestDecision {
        RequestDecision::Continue
    }

    fn on_request_body(_body: Vec<u8>) -> RequestBodyDecision {
        // Pass the body through untouched (only invoked for a route with a body; the no-op ladder
        // scenario drives bodyless GETs, so this stays off the measured path).
        RequestBodyDecision::Continue
    }

    fn on_response(_req: HttpRequest, _resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Continue
    }
}

export!(FilterNoop);
