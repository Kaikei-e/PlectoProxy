//! E2E (tdd-workflow Phase 0): drive a request through the host + a real
//! `plecto:filter` component (filter-hello) and assert the client-visible
//! outcome — the taken `decision` and the synthesised response.

use plecto_host::{Header, Host, HttpRequest, RequestDecision};

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

#[test]
fn continues_when_request_is_not_blocked() {
    let host = Host::new().unwrap();
    let filter = host.load(&component_bytes()).unwrap();

    let (decision, logs) = filter.on_request(&request(&[])).unwrap();

    assert!(
        matches!(decision, RequestDecision::Continue),
        "an unblocked request should continue down the chain"
    );
    // the filter exercised the lent host-log capability
    assert!(logs.iter().any(|l| l.message.contains("on-request")));
}

#[test]
fn short_circuits_when_block_header_present() {
    let host = Host::new().unwrap();
    let filter = host.load(&component_bytes()).unwrap();

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
