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
//!       * if `x-plecto-ratelimit` is present, consult a host-native token bucket keyed by the
//!         header VALUE (distinct callers → independent buckets) and short-circuit 429 when
//!         empty (ADR 000005 / 000026);
//!       * if `x-plecto-block` is present, short-circuit 403;
//!       * otherwise continue.
//!   - on-response (0.3.0 contract, ADR 000073 — the markers ride the REQUEST, so they also
//!     prove the as-forwarded snapshot reaches the response phase):
//!       * if the request carried `x-plecto-resp-replace`, `replace` with a synthesised 418
//!         (the upstream response is dropped);
//!       * if the request carried `x-plecto-resp-echo`, `modified` echoing the request's path
//!         (`x-plecto-req-path`) and, when present, its `Origin` (`x-plecto-echo-origin`);
//!       * if the response carries `x-plecto-respedit`, `modified` stamping
//!         `x-plecto-respadded` (the pre-0.3 response-edit exercise);
//!       * otherwise continue.

// wit-bindgen flattens records into many core-wasm ABI args, so generated FFI shims trip
// clippy::too_many_arguments. This allow scopes ONLY to generated code.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../../wit",
    world: "filter-body",
});

use crate::plecto::filter::host_counter;
use crate::plecto::filter::host_log;
use crate::plecto::filter::host_ratelimit;
use crate::plecto::filter::types::{Header, RequestEdit, ResponseEdit};

struct FilterHello;

fn has_header(req: &HttpRequest, name: &str) -> bool {
    req.headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case(name))
}

fn header_value<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(&h.value).ok())
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

        // Guest-LOCAL linear-memory state (NOT host KV): a function-local `static` that
        // persists across calls on a reused (trusted) instance and resets on a fresh-per-
        // request (untrusted) one. This makes zeroization falsifiable (ADR 000006 / 000011):
        // under `untrusted`, memory is fresh by construction, so `local-state` must stay 1.
        static LOCAL_HITS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        let local = LOCAL_HITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        host_log::log(host_log::Level::Info, &format!("local-state={local}"));

        // Deliberately runaway: the host's epoch deadline (ADR 000006) must interrupt this
        // and fail-closed, not hang the calling thread.
        if has_header(&req, "x-plecto-spin") {
            let mut n: u64 = 0;
            loop {
                n = n.wrapping_add(1);
                core::hint::black_box(n);
            }
        }

        // Deliberately over-allocate past the Store memory limit (ADR 000006). The linear-
        // memory grow fails, the guest allocator aborts, and the host observes a trap — the
        // host process itself must survive.
        if has_header(&req, "x-plecto-balloon") {
            let big: Vec<u8> = Vec::with_capacity(256 << 20);
            core::hint::black_box(&big);
        }

        // A BOUNDED busy loop (no trap): burn `x-plecto-busy` iterations of cheap work, then
        // continue normally. Unlike `x-plecto-spin` (which runs forever and must hit the epoch
        // deadline), this returns on its own — so the host's pool keeps this instance CHECKED
        // OUT for the duration. The trusted-pool tests use it to hold an instance long enough
        // to observe saturation (fail-closed under contention) and real concurrency (the pool
        // builds a second instance while the first is busy), deterministically and without a
        // guest sleep capability (none is lent). Iteration count comes from the header so a
        // test can tune the hold; keep the per-request deadline generous when using it.
        if let Some(h) = req
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("x-plecto-busy"))
        {
            let iters: u64 = std::str::from_utf8(&h.value)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let mut acc: u64 = 0;
            let mut i: u64 = 0;
            while i < iters {
                acc = acc.wrapping_add(i);
                core::hint::black_box(acc);
                i += 1;
            }
            core::hint::black_box(acc);
        }

        // Ask the host to rewrite the request and continue (chain-dispatch edit application,
        // ADR 000007). The next filter / upstream sees the added header.
        if has_header(&req, "x-plecto-addheader") {
            return RequestDecision::Modified(RequestEdit {
                set_headers: vec![Header {
                    name: "x-plecto-added".to_string(),
                    value: b"1".to_vec(),
                }],
                remove_headers: vec![],
            });
        }

        if let Some(rl) = header_value(&req, "x-plecto-ratelimit") {
            // The header VALUE selects the bucket key (e.g. a tenant / client id), so distinct
            // callers get independent buckets; an empty value falls back to a shared "default".
            // The bucket spec is host-configured in the manifest (ADR 000026); the filter only
            // decides to consult the limiter and on what key. No spec to pass (and none to forge).
            let key = if rl.is_empty() { "default" } else { rl };
            let outcome = host_ratelimit::try_acquire(key, 1);
            if !outcome.allowed {
                return RequestDecision::ShortCircuit(HttpResponse {
                    status: 429,
                    headers: vec![Header {
                        name: "retry-after-ms".to_string(),
                        value: outcome.retry_after_ms.to_string().into_bytes(),
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
                    value: b"blocked".to_vec(),
                }],
                body: b"blocked by filter-hello".to_vec(),
            })
        } else {
            RequestDecision::Continue
        }
    }

    fn on_request_body(body: Vec<u8>) -> RequestBodyDecision {
        // buffer-then-decide (ADR 000025): the host buffered the whole body and handed it over.
        // Short-circuit on a marker (exercises the SC path), otherwise transform (uppercase) and
        // continue — both before upstream is reached, so a short-circuit is always clean.
        host_log::log(host_log::Level::Info, "filter-hello: on-request-body");
        if body
            .windows(9)
            .any(|w| w.eq_ignore_ascii_case(b"deny-body"))
        {
            RequestBodyDecision::ShortCircuit(HttpResponse {
                status: 403,
                headers: vec![Header {
                    name: "x-plecto".to_string(),
                    value: b"blocked-body".to_vec(),
                }],
                body: b"blocked body by filter-hello".to_vec(),
            })
        } else {
            RequestBodyDecision::Continue(body.to_ascii_uppercase())
        }
    }

    fn on_response(req: HttpRequest, resp: HttpResponse) -> ResponseDecision {
        // Opt-in replace (ADR 000073): the marker rides the REQUEST, so hitting this arm also
        // proves the as-forwarded snapshot reached the response phase. The synthesised response
        // supplants the upstream one (which is dropped unread).
        if has_header(&req, "x-plecto-resp-replace") {
            return ResponseDecision::Replace(HttpResponse {
                status: 418,
                headers: vec![Header {
                    name: "x-plecto-replaced".to_string(),
                    value: b"1".to_vec(),
                }],
                body: b"replaced by filter-hello".to_vec(),
            });
        }

        // Opt-in request-context echo (ADR 000073): reflect data only the request knows —
        // its path, Origin when present, a request-chain stamp (`x-plecto-added`), and whether
        // a host-egress-only header (`traceparent` injected at forward time) is absent from the
        // as-forwarded snapshot when the client did not send one.
        if has_header(&req, "x-plecto-resp-echo") {
            let mut set_headers = vec![Header {
                name: "x-plecto-req-path".to_string(),
                value: req.path.clone().into_bytes(),
            }];
            if let Some(origin) = header_value(&req, "origin") {
                set_headers.push(Header {
                    name: "x-plecto-echo-origin".to_string(),
                    value: origin.as_bytes().to_vec(),
                });
            }
            if has_header(&req, "x-plecto-added") {
                set_headers.push(Header {
                    name: "x-plecto-echo-stamp".to_string(),
                    value: b"1".to_vec(),
                });
            }
            set_headers.push(Header {
                name: "x-plecto-echo-has-traceparent".to_string(),
                value: if has_header(&req, "traceparent") {
                    b"1".to_vec()
                } else {
                    b"0".to_vec()
                },
            });
            return ResponseDecision::Modified(ResponseEdit {
                set_status: None,
                set_headers,
                remove_headers: vec![],
            });
        }

        // Opt-in response rewrite (gated on a header so default responses still `continue`):
        // exercises the response-side chain dispatch + edit application (ADR 000007).
        if resp
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("x-plecto-respedit"))
        {
            return ResponseDecision::Modified(ResponseEdit {
                set_status: None,
                set_headers: vec![Header {
                    name: "x-plecto-respadded".to_string(),
                    value: b"1".to_vec(),
                }],
                remove_headers: vec![],
            });
        }
        ResponseDecision::Continue
    }
}

export!(FilterHello);
