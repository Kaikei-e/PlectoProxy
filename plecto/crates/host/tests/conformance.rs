//! WIT-conformance (tdd-workflow Phase 1): assert a filter component satisfies the
//! `plecto:filter@0.1.0` world and that the host honours the contract it lends.
//!
//! Loading the component type-checks it against the world (InstancePre resolves every
//! import/export); a non-conforming component fails at `load`. This is Plecto's
//! consumer-driven contract test. ADR 000004 widened the lent host-API (host-counter /
//! host-ratelimit) and added the trusted/untrusted lifecycle — both are pinned here.
//!
//! Deny-by-default (ADR 000006) is enforced structurally: `Host::load` adds ONLY the
//! plecto host-API to the `Linker` (no WASI / network / filesystem / sockets), and the
//! guest is built without WASI imports, so a filter can reach nothing it was not lent.

use plecto_host::{Host, HttpResponse, LoadOptions, ResponseDecision};

fn component_bytes() -> Vec<u8> {
    std::fs::read(env!("FILTER_HELLO_COMPONENT")).expect("read filter-hello component")
}

#[test]
fn component_satisfies_plecto_filter_world() {
    // Resolving imports proves the host provides the full lent surface — including the
    // ADR 000004 additions host-counter / host-ratelimit (provider-tightening pin).
    let host = Host::new().unwrap();
    host.load("filter-hello", &component_bytes(), LoadOptions::untrusted())
        .expect("filter-hello must satisfy plecto:filter@0.1.0 (imports/exports resolve)");
}

#[test]
fn component_loads_under_both_isolation_modes() {
    // Both engines (pooling for trusted, on-demand for untrusted) must instantiate the
    // contract. Trusted load also runs init once at load time.
    let host = Host::new().unwrap();
    let trusted = host
        .load("filter-hello", &component_bytes(), LoadOptions::trusted())
        .expect("pooling (trusted) engine must instantiate the component");
    assert_eq!(trusted.isolation(), plecto_host::Isolation::Trusted);

    let untrusted = host
        .load("filter-hello", &component_bytes(), LoadOptions::untrusted())
        .expect("on-demand (untrusted) engine must instantiate the component");
    assert_eq!(untrusted.isolation(), plecto_host::Isolation::Untrusted);
}

#[test]
fn response_hook_is_honoured() {
    let host = Host::new().unwrap();
    let filter = host
        .load("filter-hello", &component_bytes(), LoadOptions::untrusted())
        .unwrap();

    let resp = HttpResponse {
        status: 200,
        headers: vec![],
        body: vec![],
    };
    let (decision, _logs) = filter.on_response(&resp).unwrap();
    assert!(matches!(decision, ResponseDecision::Continue));
}
