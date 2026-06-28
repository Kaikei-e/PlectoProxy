//! Integration tests for the experimental streaming body-filter runtime (feature `streaming-body`,
//! OFF by default). Compiled only when the feature is on, so the default test run is unaffected.
//!
//! The runtime lends a MINIMAL WASI slice — `wasi:http/proxy` interfaces only (io / clocks / random
//! / stdio), never filesystem / sockets / environment / preopens (security audit F-002). That the
//! guest links and runs with ONLY that slice shows the runtime is not over-granting; a guest that
//! tried to import a denied interface would simply fail to link.
#![cfg(feature = "streaming-body")]

use plecto_host::{StreamingDecision, StreamingLimits, run_streaming_body};

const COMPONENT: &str = env!("FILTER_STREAMING_COMPONENT");

fn component_bytes() -> Vec<u8> {
    std::fs::read(COMPONENT).expect("read streaming guest component")
}

#[test]
fn streams_a_body_far_larger_than_the_guest_memory_cap() {
    // The decisive streaming property: feed 16 MiB through a guest capped at 2 MiB of linear memory.
    // A buffered `list<u8>` guest would need 16 MiB and OOM-trap under the cap; success proves the
    // guest pulled the body lazily — its memory stays flat in body size.
    let body = vec![0u8; 16 << 20];
    let limits = StreamingLimits {
        memory_cap: 2 << 20,
        deadline_ms: 10_000,
    };
    let decision = run_streaming_body(&component_bytes(), body, &limits).expect("streaming run");
    assert_eq!(
        decision,
        StreamingDecision::Continue,
        "a clean body 8x the guest memory cap streams through and is allowed"
    );
}

#[test]
fn short_circuits_on_a_body_marker_spanning_the_stream() {
    // The filter inspects the streamed body and rejects on a marker — exercising the short-circuit
    // branch over a stream (not a buffered list<u8>).
    let mut body = vec![b'x'; 4096];
    body.extend_from_slice(b"deny-body");
    body.extend(std::iter::repeat_n(b'y', 4096));
    let limits = StreamingLimits {
        memory_cap: 2 << 20,
        deadline_ms: 5_000,
    };
    let decision = run_streaming_body(&component_bytes(), body, &limits).expect("streaming run");
    match decision {
        StreamingDecision::ShortCircuit { status, body } => {
            assert_eq!(status, 403);
            assert_eq!(body, b"blocked streaming body");
        }
        other => panic!("expected a short-circuit, got {other:?}"),
    }
}

#[test]
fn allows_a_clean_body() {
    let limits = StreamingLimits {
        memory_cap: 2 << 20,
        deadline_ms: 5_000,
    };
    let decision = run_streaming_body(
        &component_bytes(),
        b"hello world, perfectly fine".to_vec(),
        &limits,
    )
    .expect("streaming run");
    assert_eq!(decision, StreamingDecision::Continue);
}
