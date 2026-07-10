//! End-to-end behaviour of the outbound TCP capability (ADR 000060) through the real
//! `filter-tcp-gate` wasm guest. Compiled only with the `outbound-tcp` feature (OFF by default).
//!
//! The guest consults a raw TCP backend (target from the `x-tcp-target` header) over
//! `wasi:sockets`; the host gates every name resolution and connect by the operator allowlist +
//! the SSRF guard + the IP pin, and the guest fails closed (503) on any error. These tests pin
//! the gates end-to-end:
//!   - a NAME not on the allowlist is denied at resolution — before any DNS;
//!   - an allowlisted NAME that resolves to a blocked IP (loopback) is denied on the resolved
//!     address, even with a live server listening there — the DNS rebinding defense;
//!   - an IP-literal connect that bypasses name resolution is still denied at the connect gate
//!     (allowlist + IP pin), proving resolution cannot be sidestepped;
//!   - a vetted name→resolve→pin→connect chain SUCCEEDS against a live backend on a
//!     non-loopback address, and the per-request connect budget resets across requests on the
//!     same pooled instance.
#![cfg(feature = "outbound-tcp")]

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};

use plecto_host::test_support::{TestSigner, bound_sbom, filter_tcp_gate_component};
use plecto_host::{
    Header, Host, HttpRequest, LoadOptions, LoadedFilter, RequestDecision, RequestTrace,
    SignedArtifact, TcpAllowEntry,
};

/// A raw TCP server that answers every connection with `reply` after reading the probe line.
fn spawn_tcp_server(bind: IpAddr, reply: &'static [u8]) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind((bind, 0))?;
    let addr = listener.local_addr()?;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 64];
            let _ = s.read(&mut buf);
            let _ = s.write_all(reply);
        }
    });
    Ok(addr)
}

/// Sign + load filter-tcp-gate with the given options. The `Host` owns the epoch ticker and the
/// outbound tokio runtime, so it must outlive the filter — returned alongside.
fn signed_load(opts: LoadOptions) -> (Host, LoadedFilter) {
    let bytes = filter_tcp_gate_component();
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(&bytes).unwrap();
    let sbom = bound_sbom(&bytes);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let artifact = SignedArtifact {
        component_bytes: &bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };
    let filter = host
        .load("filter-tcp-gate", &artifact, opts)
        .expect("load filter-tcp-gate");
    (host, filter)
}

fn request(target: &str) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: "/protected".to_string(),
        authority: "gateway.test".to_string(),
        scheme: "https".to_string(),
        headers: vec![Header {
            name: "x-tcp-target".to_string(),
            value: target.as_bytes().to_vec(),
        }],
    }
}

fn allow(host: &str, port: u16) -> TcpAllowEntry {
    TcpAllowEntry {
        host: host.to_string(),
        port,
    }
}

fn outbound_tcp_opts(allow: Vec<TcpAllowEntry>, allow_private: Vec<String>) -> LoadOptions {
    LoadOptions::untrusted().with_outbound_tcp(allow, allow_private, Some(4), Some(5_000))
}

fn short_circuit_body(f: &LoadedFilter, r: &HttpRequest) -> (u16, String) {
    let (decision, _logs) = f
        .on_request(r, &RequestTrace::root())
        .expect("run on_request");
    match decision {
        RequestDecision::ShortCircuit(resp) => (
            resp.status,
            String::from_utf8_lossy(&resp.body).into_owned(),
        ),
        other => panic!("expected a fail-closed short-circuit, got {other:?}"),
    }
}

#[test]
fn unlisted_name_is_denied_at_resolution() {
    // The allowlist names one host; the guest asks to resolve a different one → deny-by-default
    // at the name-resolution gate, before any DNS.
    let opts = outbound_tcp_opts(vec![allow("db.allowed.test", 6379)], vec![]);
    let (_host, filter) = signed_load(opts);

    let (status, body) = short_circuit_body(&filter, &request("evil.example.test:6379"));
    assert_eq!(status, 503, "an unlisted name must fail closed");
    assert!(
        body.contains("resolve"),
        "the name-resolution gate denies it (body: {body:?})"
    );
}

#[test]
fn allowlisted_name_resolving_to_loopback_is_ssrf_blocked() {
    // The SSRF/rebinding defense end-to-end: allowlist `localhost:PORT`, run a real server there,
    // but the guard rejects the resolved 127.0.0.1 — the guest never reaches the listener.
    let addr = spawn_tcp_server(IpAddr::from([127, 0, 0, 1]), b"OK\n").expect("bind loopback");
    let opts = outbound_tcp_opts(vec![allow("localhost", addr.port())], vec![]);
    let (_host, filter) = signed_load(opts);

    let (status, body) =
        short_circuit_body(&filter, &request(&format!("localhost:{}", addr.port())));
    assert_eq!(status, 503, "a loopback-resolving name must fail closed");
    assert!(
        body.contains("resolve"),
        "the vetted resolution blocks the loopback result (body: {body:?})"
    );
}

#[test]
fn ip_literal_bypassing_resolution_is_denied_at_connect() {
    // The connect gate cannot be sidestepped by skipping name resolution: a live server on
    // loopback, its LITERAL address as the target, and even an allowlist entry for that exact
    // (ip, port) — the reserved floor still denies the connect (loopback is not opt-in-able).
    let addr = spawn_tcp_server(IpAddr::from([127, 0, 0, 1]), b"OK\n").expect("bind loopback");
    let literal = addr.ip().to_string();
    let opts = outbound_tcp_opts(vec![allow(&literal, addr.port())], vec![]);
    let (_host, filter) = signed_load(opts);

    let (status, body) =
        short_circuit_body(&filter, &request(&format!("{literal}:{}", addr.port())));
    assert_eq!(status, 503, "a floor-blocked literal must fail closed");
    assert!(
        body.contains("connect"),
        "the connect gate denies the un-pinned/blocked address (body: {body:?})"
    );
}

/// A non-loopback local address to bind a live backend on (the reserved floor makes loopback
/// unusable for the success path). Discovered via the UDP-connect trick — no packet is sent.
fn non_loopback_local_ip() -> Option<IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("192.0.2.1:9").ok()?; // TEST-NET-1: never actually reached
    let ip = probe.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[test]
fn vetted_name_resolve_pin_connect_succeeds_and_budget_resets_per_request() {
    // The full allowed chain end-to-end: an allowlisted NAME, statically resolved to a live
    // non-loopback backend (private space opted in via CIDR), pins the IP; the guest's connect to
    // the pinned address succeeds and the backend's `OK` lets the request continue. Ran twice on
    // a trusted (pooled) instance with a budget of 1 connect: the second request only succeeds
    // because the per-request budget RESETS (a lifetime budget would starve it).
    let Some(ip) = non_loopback_local_ip() else {
        eprintln!("skip: no non-loopback local address on this machine");
        return;
    };
    let Ok(addr) = spawn_tcp_server(ip, b"OK\n") else {
        eprintln!("skip: cannot bind a listener on {ip}");
        return;
    };
    let cidr = match ip {
        IpAddr::V4(_) => format!("{ip}/32"),
        IpAddr::V6(_) => format!("{ip}/128"),
    };
    let opts = LoadOptions::trusted()
        .with_outbound_tcp(
            vec![allow("backend.test", addr.port())],
            vec![cidr],
            Some(1), // one connect per request: the reset is what makes request #2 pass
            Some(5_000),
        )
        .with_outbound_tcp_static_resolver(vec![("backend.test".to_string(), vec![ip])]);
    let (_host, filter) = signed_load(opts);

    let req = request(&format!("backend.test:{}", addr.port()));
    for attempt in 1..=2 {
        let (decision, _logs) = filter
            .on_request(&req, &RequestTrace::root())
            .expect("run on_request");
        assert!(
            matches!(decision, RequestDecision::Continue),
            "attempt {attempt}: a vetted resolve→pin→connect chain must continue, got {decision:?}"
        );
    }
}

#[test]
fn no_target_fails_closed() {
    // Defensive: a request the filter cannot consult on (no target) must not be allowed through.
    let opts = outbound_tcp_opts(vec![allow("db.allowed.test", 6379)], vec![]);
    let (_host, filter) = signed_load(opts);

    let req = HttpRequest {
        method: "GET".to_string(),
        path: "/protected".to_string(),
        authority: "gateway.test".to_string(),
        scheme: "https".to_string(),
        headers: vec![],
    };
    let (status, _body) = short_circuit_body(&filter, &req);
    assert_eq!(status, 503);
}

/// ADR 000063 Decision 4's other half: the fat-guest grant (`wasi = "minimal"`) lends ONLY the
/// Tier B allowlist (`io`/`clocks`/`random`/`cli`/`filesystem`) — never `sockets` or `http`, those
/// stay their own separate, allowlisted capabilities (ADR 000036 / 000060). `filter-tcp-gate`
/// imports `wasi:sockets`, so loading it with the fat-guest grant alone (no `outbound_tcp`
/// policy, so the host never links `wasi:sockets`) must still fail to instantiate — structural
/// deny, not merely a denied call. Requires `fat-guest` in addition to this file's `outbound-tcp`
/// gate, since it needs both `filter_tcp_gate_component()` (outbound-tcp) and
/// `LoadOptions::with_wasi_minimal` (fat-guest).
#[cfg(feature = "fat-guest")]
#[test]
fn the_wasi_minimal_grant_alone_does_not_satisfy_a_sockets_import() {
    use plecto_host::test_support::{TestSigner, bound_sbom, filter_tcp_gate_component};
    use plecto_host::{Host, SignedArtifact};

    let bytes = filter_tcp_gate_component();
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(&bytes).unwrap();
    let sbom = bound_sbom(&bytes);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let artifact = SignedArtifact {
        component_bytes: &bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };

    let err = host
        .load(
            "filter-tcp-gate",
            &artifact,
            LoadOptions::untrusted().with_wasi_minimal(),
        )
        .err()
        .expect(
            "filter-tcp-gate imports wasi:sockets, which the fat-guest grant does not lend — \
             load must fail closed",
        );
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("import") || msg.contains("wasi") || msg.contains("unknown"),
        "expected an unresolved-import style failure, got: {msg}"
    );
}
