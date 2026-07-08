//! Tier B polyglot conformance (ADR 000063): `filter-hello-go`, the filter-hello conformance
//! subset ported to Go/TinyGo — the first "fat guest" (a runtime that assumes WASI is present,
//! confirmed against TinyGo 0.41.1 by its unconditional `wasi:cli`/`wasi:filesystem` imports even
//! for a program touching neither).
//!
//! Kept OUT of `tests/polyglot.rs`'s `components()` loop deliberately: that loop shares ONE
//! `LoadOptions` across every fixture specifically to keep proving the Tier A fixtures stay
//! zero-WASI (ADR 000055's falsifiability principle) — folding a fixture that NEEDS
//! `.with_wasi_minimal()` into that loop would either break the Tier A fixtures' options or force
//! every Tier A fixture to also take the fat-guest grant, diluting exactly the claim that loop
//! exists to prove. So Tier B gets its own file, gated behind BOTH `polyglot-conformance` (so a
//! plain `cargo test` never needs a non-Rust toolchain) AND `fat-guest` (so it never needs
//! `LoadOptions::with_wasi_minimal`, which does not exist otherwise).
#![cfg(all(feature = "polyglot-conformance", feature = "fat-guest"))]

use std::path::PathBuf;
use std::sync::Arc;

use plecto_host::test_support::{TestSigner, bound_sbom};
use plecto_host::{
    Header, Host, HttpRequest, InMemorySink, LoadOptions, LoadedFilter, LogLevel, LogLine,
    RequestBodyDecision, RequestDecision, RequestTrace, RunError, SignedArtifact, TelemetrySink,
};

const GO_COMPONENT: &str = "../../examples/filters/filter-hello-go/dist/filter_hello_go.wasm";

fn component_path() -> PathBuf {
    match std::env::var("PLECTO_POLYGLOT_GO_COMPONENT") {
        Ok(p) => PathBuf::from(p),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(GO_COMPONENT),
    }
}

fn component_bytes() -> Vec<u8> {
    let p = component_path();
    std::fs::read(&p).unwrap_or_else(|e| {
        panic!(
            "read Tier B component {}: {e}\n\
             build it first: examples/filters/filter-hello-go/build.sh \
             (or point PLECTO_POLYGLOT_GO_COMPONENT at a prebuilt component)",
            p.display()
        )
    })
}

/// Sign + load through the real provenance path (ADR 000006) — same as `tests/polyglot.rs`.
fn signed_load(bytes: &[u8], opts: LoadOptions) -> Result<(Host, LoadedFilter), anyhow::Error> {
    signed_load_with_sink(bytes, opts, None)
}

/// Like [`signed_load`], but wires `sink` (when present) into the `Host` before loading — for
/// tests that need to inspect the span a failing call emits (ADR 000063 F2/F5: the recovered
/// stdio logs a trap must not lose).
fn signed_load_with_sink(
    bytes: &[u8],
    opts: LoadOptions,
    sink: Option<Arc<dyn TelemetrySink>>,
) -> Result<(Host, LoadedFilter), anyhow::Error> {
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(bytes).unwrap();
    let sbom = bound_sbom(bytes);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let mut host = Host::new(signer.trust_policy().unwrap()).unwrap();
    if let Some(sink) = sink {
        host = host.with_telemetry_sink(sink);
    }
    let artifact = SignedArtifact {
        component_bytes: bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };
    host.load("filter-hello-go", &artifact, opts)
        .map(|filter| (host, filter))
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
fn without_the_wasi_minimal_grant_the_fat_guest_fails_to_link() {
    // Deny-by-default (ADR 000063 Decision 4): a fat guest's unresolved wasi:cli/wasi:filesystem
    // imports must fail instantiation structurally when the filter never declared `wasi =
    // "minimal"` — not merely be denied at call time.
    let bytes = component_bytes();
    let err = signed_load(&bytes, LoadOptions::untrusted())
        .err()
        .expect("a fat guest without the wasi_minimal grant must fail to load");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("import")
            || msg.to_lowercase().contains("wasi")
            || msg.to_lowercase().contains("unknown"),
        "expected an unresolved-import style failure, got: {msg}"
    );
}

#[test]
fn with_the_wasi_minimal_grant_the_fat_guest_satisfies_the_conformance_subset() {
    let bytes = component_bytes();
    let (_host, filter) = signed_load(&bytes, LoadOptions::untrusted().with_wasi_minimal())
        .expect("filter-hello-go must load once wasi = \"minimal\" is granted");

    assert!(
        filter.reads_body(),
        "filter-hello-go targets world filter-body-go, so reads_body() must be true"
    );

    let (decision, logs) = filter
        .on_request(&request(&[]), &RequestTrace::root())
        .unwrap();
    assert!(
        matches!(decision, RequestDecision::Continue),
        "an unblocked request should continue"
    );
    assert!(
        logs.iter().any(|l| l.message.contains("on-request")),
        "host-log must carry the guest's log line"
    );

    let (decision, _logs) = filter
        .on_request(&request(&[("x-plecto-block", "1")]), &RequestTrace::root())
        .unwrap();
    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(resp.status, 403, "blocked request must get 403");
            assert!(
                resp.headers
                    .iter()
                    .any(|h| h.name == "x-plecto" && h.value == "blocked"),
                "short-circuit must carry the filter's header"
            );
        }
        _ => panic!("expected short-circuit for a blocked request"),
    }

    let (decision, _logs) = filter
        .on_request(
            &request(&[("x-plecto-addheader", "1")]),
            &RequestTrace::root(),
        )
        .unwrap();
    match decision {
        RequestDecision::Modified(edit) => {
            assert!(
                edit.set_headers
                    .iter()
                    .any(|h| h.name == "x-plecto-added" && h.value == "1"),
                "the edit must add x-plecto-added: 1"
            );
        }
        _ => panic!("expected a modified decision"),
    }

    let (decision, _logs) = filter
        .on_request_body(b"hello world", &RequestTrace::root())
        .unwrap();
    match decision {
        RequestBodyDecision::Continue(body) => {
            assert_eq!(
                body,
                b"HELLO WORLD".to_vec(),
                "body must round-trip uppercased"
            )
        }
        RequestBodyDecision::ShortCircuit(_) => panic!("expected continue with transformed body"),
    }
}

#[test]
fn the_isolation_lifecycle_holds_for_the_fat_guest() {
    let bytes = component_bytes();

    let (_host, filter) = signed_load(&bytes, LoadOptions::trusted().with_wasi_minimal())
        .expect("filter-hello-go must load trusted with the wasi_minimal grant");
    for _ in 0..3 {
        let (_decision, logs) = filter
            .on_request(&request(&[]), &RequestTrace::root())
            .unwrap();
        let inits: u64 = logs
            .iter()
            .find_map(|l| {
                l.message
                    .strip_prefix("init-calls=")
                    .and_then(|n| n.parse().ok())
            })
            .expect("filter-hello-go must log init-calls=N every request");
        assert_eq!(inits, 1, "trusted init must run exactly once");
    }
}

#[test]
fn the_real_tinygo_guests_stdout_and_stderr_are_bridged_into_host_log() {
    // The point of ADR 000063's stdio bridge, proven against a REAL TinyGo-compiled guest (not a
    // hand-rolled Rust fixture): stdout -> debug, stderr -> warn, both land in the SAME
    // `Vec<LogLine>` host-log itself feeds (so they show up in the same OTLP trace as the
    // request, ADR 000063's grill-session decision).
    let bytes = component_bytes();
    let (_host, filter) = signed_load(&bytes, LoadOptions::untrusted().with_wasi_minimal())
        .expect("filter-hello-go must load with the wasi_minimal grant");

    let (_decision, logs) = filter
        .on_request(&request(&[]), &RequestTrace::root())
        .unwrap();

    let stdout_line = find_line(&logs, "filter-hello-go: stdout probe");
    assert_eq!(
        stdout_line.level,
        LogLevel::Debug,
        "TinyGo's stdout is bridged at debug level"
    );
    let stderr_line = find_line(&logs, "filter-hello-go: stderr probe");
    assert_eq!(
        stderr_line.level,
        LogLevel::Warn,
        "TinyGo's stderr is bridged at warn level"
    );
}

fn find_line<'a>(logs: &'a [LogLine], needle: &str) -> &'a LogLine {
    logs.iter()
        .find(|l| l.message.contains(needle))
        .unwrap_or_else(|| {
            panic!(
                "expected a bridged stdio line containing {needle:?}; got: {:#?}",
                logs.iter().map(|l| &l.message).collect::<Vec<_>>()
            )
        })
}

#[test]
fn a_trapping_tinygo_guests_unterminated_stderr_still_reaches_the_span() {
    // Regression for ADR 000063 F2/F5: a trap discards the instance, so the host must recover
    // whatever stdio the guest produced BEFORE the trap — including an unterminated line (no
    // trailing '\n') — instead of losing it along with the instance. The public `on_request`
    // Result still carries no logs on `Err` (unchanged contract); the recovery is only
    // observable via the span the host emits, so this test wires an `InMemorySink` to inspect it.
    let bytes = component_bytes();
    let sink = Arc::new(InMemorySink::new());
    let (_host, filter) = signed_load_with_sink(
        &bytes,
        LoadOptions::untrusted().with_wasi_minimal(),
        Some(sink.clone()),
    )
    .expect("filter-hello-go must load with the wasi_minimal grant");

    let err = filter
        .on_request(&request(&[("x-plecto-panic", "1")]), &RequestTrace::root())
        .expect_err("the guest must trap on x-plecto-panic");
    assert!(
        matches!(err, RunError::Trap(_)),
        "expected a Trap, got {err:?}"
    );

    let spans = sink.spans();
    let span = spans
        .last()
        .expect("the failing call must still emit a span");
    assert!(
        span.events
            .iter()
            .any(|e| e.name.contains("filter-hello-go: panic probe")),
        "the guest's unterminated stderr line before the trap must reach the span's events; got: {:#?}",
        span.events.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
}
