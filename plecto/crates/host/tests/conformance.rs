//! WIT-conformance (tdd-workflow Phase 1): assert a filter component satisfies the
//! `plecto:filter@0.1.0` world and that the host honours the contract it lends.
//!
//! Loading the component type-checks it against the world (InstancePre resolves
//! every import/export); a non-conforming component fails at `load`. This is
//! Plecto's consumer-driven contract test.
//!
//! Deny-by-default (ADR 000006) is enforced structurally: `Host::load` adds ONLY
//! the plecto host-API to the `Linker` (no WASI / network / filesystem / sockets),
//! and the guest is built without WASI imports, so a filter can reach nothing it
//! was not explicitly lent.

use plecto_host::{Host, HttpResponse, ResponseDecision};

fn component_bytes() -> Vec<u8> {
    std::fs::read(env!("FILTER_HELLO_COMPONENT")).expect("read filter-hello component")
}

#[test]
fn component_satisfies_plecto_filter_world() {
    let host = Host::new().unwrap();
    host.load(&component_bytes())
        .expect("filter-hello must satisfy plecto:filter@0.1.0 (imports/exports resolve)");
}

#[test]
fn response_hook_is_honoured() {
    let host = Host::new().unwrap();
    let filter = host.load(&component_bytes()).unwrap();

    let resp = HttpResponse {
        status: 200,
        headers: vec![],
        body: vec![],
    };
    let (decision, _logs) = filter.on_response(&resp).unwrap();
    assert!(matches!(decision, ResponseDecision::Continue));
}
