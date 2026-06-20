//! E2E (tdd-workflow Phase 0): drive requests through the host + a real `plecto:filter`
//! component (filter-hello) and assert the client-visible outcome — the taken `decision`,
//! the synthesised response, and the lifecycle effect ADR 000004 promises (init-once for
//! trusted filters, fresh-per-request for untrusted ones).

use plecto_host::{Header, Host, HttpRequest, LoadOptions, LogLine, RequestDecision};

fn component_bytes() -> Vec<u8> {
    std::fs::read(env!("FILTER_HELLO_COMPONENT")).expect("read filter-hello component")
}

fn request(headers: &[(&str, &str)]) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: "/".to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        headers: headers
            .iter()
            .map(|(n, v)| Header {
                name: (*n).to_string(),
                value: (*v).to_string(),
            })
            .collect(),
    }
}

/// The filter logs `init-calls=N` each request; recover N to observe the lifecycle.
fn init_calls(logs: &[LogLine]) -> u64 {
    logs.iter()
        .find_map(|l| {
            l.message
                .strip_prefix("init-calls=")
                .and_then(|n| n.parse().ok())
        })
        .expect("filter-hello logs init-calls=N every request")
}

#[test]
fn continues_when_request_is_not_blocked() {
    let host = Host::new().unwrap();
    let filter = host
        .load("filter-hello", &component_bytes(), LoadOptions::untrusted())
        .unwrap();

    let (decision, logs) = filter.on_request(&request(&[])).unwrap();

    assert!(
        matches!(decision, RequestDecision::Continue),
        "an unblocked request should continue down the chain"
    );
    assert!(logs.iter().any(|l| l.message.contains("on-request")));
}

#[test]
fn short_circuits_when_block_header_present() {
    let host = Host::new().unwrap();
    let filter = host
        .load("filter-hello", &component_bytes(), LoadOptions::untrusted())
        .unwrap();

    let (decision, _logs) = filter
        .on_request(&request(&[("x-plecto-block", "1")]))
        .unwrap();

    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(resp.status, 403, "blocked request must get 403");
            assert!(
                resp.headers
                    .iter()
                    .any(|h| h.name == "x-plecto" && h.value == "blocked"),
                "short-circuit response must carry the filter's header"
            );
        }
        _ => panic!("expected short-circuit for a blocked request"),
    }
}

#[test]
fn trusted_filter_runs_init_once_across_requests() {
    // Tenet 4 effect (ADR 000004 / 000011): a trusted filter's init runs exactly once;
    // the persistent instance is reused, so the host counter stays at 1.
    let host = Host::new().unwrap();
    let filter = host
        .load("filter-hello", &component_bytes(), LoadOptions::trusted())
        .unwrap();

    for _ in 0..3 {
        let (_decision, logs) = filter.on_request(&request(&[])).unwrap();
        assert_eq!(init_calls(&logs), 1, "trusted init must run exactly once");
    }
}

#[test]
fn untrusted_filter_reinitializes_each_request() {
    // The isolation trade (ADR 000011): a fresh instance per request re-runs init, so the
    // host counter climbs with each request.
    let host = Host::new().unwrap();
    let filter = host
        .load("filter-hello", &component_bytes(), LoadOptions::untrusted())
        .unwrap();

    let mut seen = Vec::new();
    for _ in 0..3 {
        let (_decision, logs) = filter.on_request(&request(&[])).unwrap();
        seen.push(init_calls(&logs));
    }
    assert_eq!(seen, vec![1, 2, 3], "untrusted init must run every request");
}

#[test]
fn rate_limit_short_circuits_after_capacity() {
    // Host-native token bucket (ADR 000005): capacity 2 → first two pass, third is denied
    // with a synthesised 429. State lives host-side, so it persists across requests.
    let host = Host::new().unwrap();
    let filter = host
        .load("filter-hello", &component_bytes(), LoadOptions::trusted())
        .unwrap();

    let limited = request(&[("x-plecto-ratelimit", "1")]);

    for n in 1..=2 {
        let (decision, _logs) = filter.on_request(&limited).unwrap();
        assert!(
            matches!(decision, RequestDecision::Continue),
            "request {n} within capacity should continue"
        );
    }

    let (decision, _logs) = filter.on_request(&limited).unwrap();
    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(resp.status, 429, "exhausted bucket must get 429");
            assert!(
                resp.headers.iter().any(|h| h.name == "retry-after-ms"),
                "429 must advertise a retry-after"
            );
        }
        _ => panic!("expected short-circuit once the bucket is empty"),
    }
}

#[test]
fn rate_limit_counting_is_host_native_across_fresh_untrusted_instances() {
    // ADR 000005 conformance (mechanism 3: super-hot paths go host-native). The token
    // bucket's refill + counting live host-side and never cross the WASM boundary, so the
    // count is NOT held in a filter's linear memory. This test makes that claim falsifiable:
    // run under `untrusted`, where every request gets a FRESH instance with fresh linear
    // memory (init re-runs, so `init-calls` climbs 1→2→3). If counting lived in WASM memory
    // it would reset each request and the bucket would never drain; because it is host-native,
    // the same capacity-2 bucket still drains across those fresh instances and the third
    // request is denied 429.
    let host = Host::new().unwrap();
    let filter = host
        .load("filter-hello", &component_bytes(), LoadOptions::untrusted())
        .unwrap();

    let limited = request(&[("x-plecto-ratelimit", "1")]);

    for n in 1..=2 {
        let (decision, logs) = filter.on_request(&limited).unwrap();
        assert_eq!(
            init_calls(&logs),
            n,
            "untrusted instance must be fresh each request (init re-runs)"
        );
        assert!(
            matches!(decision, RequestDecision::Continue),
            "request {n} within capacity should continue even on a fresh instance"
        );
    }

    let (decision, logs) = filter.on_request(&limited).unwrap();
    assert_eq!(
        init_calls(&logs),
        3,
        "third request is still a fresh instance"
    );
    assert!(
        matches!(decision, RequestDecision::ShortCircuit(resp) if resp.status == 429),
        "host-native bucket drains across fresh instances → third request denied 429"
    );
}
