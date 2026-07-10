//! Polyglot conformance: the SAME contract assertions, one component per guest language.
//!
//! The `plecto:filter` world is language-neutral — anything that compiles to a WASM
//! component with zero WASI imports slots into the unchanged deny-by-default Linker.
//! Each component under test is the filter-hello conformance subset ported to another
//! language (MoonBit / JavaScript / C), built by its own toolchain (see
//! `examples/filters/filter-hello-*/build.sh`), NOT by build.rs. Running one assertion
//! suite across all of them is the falsifiable form of the polyglot claim.
//!
//! Gated behind `polyglot-conformance` so a plain `cargo test` never needs the non-Rust
//! toolchains. Component paths default to each example's `dist/`; override with
//! `PLECTO_POLYGLOT_COMPONENTS=/path/a.wasm:/path/b.wasm`.
#![cfg(feature = "polyglot-conformance")]

use plecto_host::test_support::{TestSigner, bound_sbom};
use plecto_host::{
    Header, Host, HttpRequest, HttpResponse, LoadOptions, LoadedFilter, LogLine,
    RequestBodyDecision, RequestDecision, RequestTrace, ResponseDecision, SignedArtifact,
};
use std::path::PathBuf;

const DEFAULT_COMPONENTS: &[&str] = &[
    "../../examples/filters/filter-hello-moonbit/dist/filter_hello_moonbit.wasm",
    "../../examples/filters/filter-hello-js/dist/filter_hello_js.wasm",
    "../../examples/filters/filter-hello-c/dist/filter_hello_c.wasm",
];

fn component_paths() -> Vec<PathBuf> {
    match std::env::var("PLECTO_POLYGLOT_COMPONENTS") {
        Ok(list) => list.split(':').map(PathBuf::from).collect(),
        Err(_) => DEFAULT_COMPONENTS
            .iter()
            .map(|rel| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel))
            .collect(),
    }
}

fn components() -> Vec<(String, Vec<u8>)> {
    component_paths()
        .into_iter()
        .map(|p| {
            let name = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("component")
                .to_string();
            let bytes = std::fs::read(&p).unwrap_or_else(|e| {
                panic!(
                    "read polyglot component {}: {e}\n\
                     build it first: examples/filters/filter-hello-*/build.sh \
                     (or point PLECTO_POLYGLOT_COMPONENTS at prebuilt components)",
                    p.display()
                )
            });
            (name, bytes)
        })
        .collect()
}

/// Sign the component with a fresh ephemeral key and load it through the real provenance
/// path (ADR 000006) — polyglot components get no special treatment at the gate.
fn signed_load(name: &str, bytes: &[u8], opts: LoadOptions) -> (Host, LoadedFilter) {
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(bytes).unwrap();
    let sbom = bound_sbom(bytes);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let artifact = SignedArtifact {
        component_bytes: bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };
    let filter = host
        .load(name, &artifact, opts)
        .unwrap_or_else(|e| panic!("{name} must satisfy plecto:filter@0.1.0: {e}"));
    (host, filter)
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
                value: v.as_bytes().to_vec(),
            })
            .collect(),
    }
}

fn on_req(f: &LoadedFilter, r: &HttpRequest) -> (RequestDecision, Vec<LogLine>) {
    f.on_request(r, &RequestTrace::root()).unwrap()
}

fn init_calls(name: &str, logs: &[LogLine]) -> u64 {
    logs.iter()
        .find_map(|l| {
            l.message
                .strip_prefix("init-calls=")
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or_else(|| panic!("{name} must log init-calls=N every request"))
}

#[test]
fn every_language_satisfies_the_world_and_reads_body() {
    for (name, bytes) in components() {
        let (_host, filter) = signed_load(&name, &bytes, LoadOptions::untrusted());
        assert!(
            filter.reads_body(),
            "{name} targets world filter-body, so reads_body() must be true"
        );
    }
}

#[test]
fn every_language_continues_a_plain_request() {
    for (name, bytes) in components() {
        let (_host, filter) = signed_load(&name, &bytes, LoadOptions::untrusted());
        let (decision, logs) = on_req(&filter, &request(&[]));
        assert!(
            matches!(decision, RequestDecision::Continue),
            "{name}: an unblocked request should continue"
        );
        assert!(
            logs.iter().any(|l| l.message.contains("on-request")),
            "{name}: host-log must carry the guest's log line"
        );
    }
}

#[test]
fn every_language_short_circuits_the_block_header() {
    for (name, bytes) in components() {
        let (_host, filter) = signed_load(&name, &bytes, LoadOptions::untrusted());
        let (decision, _logs) = on_req(&filter, &request(&[("x-plecto-block", "1")]));
        match decision {
            RequestDecision::ShortCircuit(resp) => {
                assert_eq!(resp.status, 403, "{name}: blocked request must get 403");
                assert!(
                    resp.headers
                        .iter()
                        .any(|h| h.name == "x-plecto" && h.value.as_slice() == b"blocked"),
                    "{name}: short-circuit must carry the filter's header"
                );
            }
            _ => panic!("{name}: expected short-circuit for a blocked request"),
        }
    }
}

#[test]
fn every_language_returns_a_typed_request_edit() {
    for (name, bytes) in components() {
        let (_host, filter) = signed_load(&name, &bytes, LoadOptions::untrusted());
        let (decision, _logs) = on_req(&filter, &request(&[("x-plecto-addheader", "1")]));
        match decision {
            RequestDecision::Modified(edit) => {
                assert!(
                    edit.set_headers
                        .iter()
                        .any(|h| h.name == "x-plecto-added" && h.value.as_slice() == b"1"),
                    "{name}: the edit must add x-plecto-added: 1"
                );
                assert!(edit.remove_headers.is_empty());
            }
            _ => panic!("{name}: expected a modified decision"),
        }
    }
}

#[test]
fn every_language_drains_the_host_native_bucket() {
    // Capacity 2, host-configured (ADR 000026): two pass, the third is denied 429 with a
    // retry-after — the SAME host-native bucket semantics regardless of guest language.
    for (name, bytes) in components() {
        let (_host, filter) = signed_load(
            &name,
            &bytes,
            LoadOptions::trusted().with_ratelimit_bucket(2, 1, 60_000),
        );
        let limited = request(&[("x-plecto-ratelimit", "1")]);
        for n in 1..=2 {
            let (decision, _logs) = on_req(&filter, &limited);
            assert!(
                matches!(decision, RequestDecision::Continue),
                "{name}: request {n} within capacity should continue"
            );
        }
        let (decision, _logs) = on_req(&filter, &limited);
        match decision {
            RequestDecision::ShortCircuit(resp) => {
                assert_eq!(resp.status, 429, "{name}: exhausted bucket must get 429");
                assert!(
                    resp.headers.iter().any(|h| h.name == "retry-after-ms"),
                    "{name}: 429 must advertise a retry-after"
                );
            }
            _ => panic!("{name}: expected short-circuit once the bucket is empty"),
        }
    }
}

#[test]
fn every_language_transforms_and_blocks_the_body() {
    for (name, bytes) in components() {
        let (_host, filter) = signed_load(&name, &bytes, LoadOptions::untrusted());

        let (decision, _logs) = filter
            .on_request_body(b"hello world", &RequestTrace::root())
            .unwrap();
        match decision {
            RequestBodyDecision::Continue(body) => assert_eq!(
                body,
                b"HELLO WORLD".to_vec(),
                "{name}: body must round-trip uppercased"
            ),
            RequestBodyDecision::ShortCircuit(_) => {
                panic!("{name}: expected continue with transformed body")
            }
        }

        let (decision, _logs) = filter
            .on_request_body(b"please deny-body now", &RequestTrace::root())
            .unwrap();
        match decision {
            RequestBodyDecision::ShortCircuit(resp) => {
                assert_eq!(resp.status, 403, "{name}: marker body must be blocked 403")
            }
            RequestBodyDecision::Continue(_) => {
                panic!("{name}: expected short-circuit on the marker body")
            }
        }
    }
}

#[test]
fn every_language_edits_the_response_only_when_asked() {
    for (name, bytes) in components() {
        let (_host, filter) = signed_load(&name, &bytes, LoadOptions::untrusted());

        let plain = HttpResponse {
            status: 200,
            headers: vec![],
            body: vec![],
        };
        let (decision, _logs) = filter.on_response(&plain, &RequestTrace::root()).unwrap();
        assert!(
            matches!(decision, ResponseDecision::Continue),
            "{name}: a plain response should continue"
        );

        let marked = HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "x-plecto-respedit".to_string(),
                value: b"1".to_vec(),
            }],
            body: vec![],
        };
        let (decision, _logs) = filter.on_response(&marked, &RequestTrace::root()).unwrap();
        match decision {
            ResponseDecision::Modified(edit) => {
                assert!(
                    edit.set_headers
                        .iter()
                        .any(|h| h.name == "x-plecto-respadded" && h.value.as_slice() == b"1"),
                    "{name}: the edit must add x-plecto-respadded: 1"
                );
                assert!(edit.set_status.is_none());
            }
            _ => panic!("{name}: expected a modified response decision"),
        }
    }
}

#[test]
fn every_language_observes_the_isolation_lifecycle() {
    // The init-once (trusted) vs fresh-per-request (untrusted) lifecycle (ADR 000004/000011)
    // is host policy, so it must hold identically for every guest language.
    for (name, bytes) in components() {
        let (_host, filter) = signed_load(&name, &bytes, LoadOptions::trusted());
        for _ in 0..3 {
            let (_decision, logs) = on_req(&filter, &request(&[]));
            assert_eq!(
                init_calls(&name, &logs),
                1,
                "{name}: trusted init must run exactly once"
            );
        }

        let (_host, filter) = signed_load(&name, &bytes, LoadOptions::untrusted());
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (_decision, logs) = on_req(&filter, &request(&[]));
            seen.push(init_calls(&name, &logs));
        }
        assert_eq!(
            seen,
            vec![1, 2, 3],
            "{name}: untrusted init must run every request"
        );
    }
}
