//! filter-streaming — an experimental `plecto:filter-streaming` body filter that consumes the
//! request body as an async `stream<u8>` (no whole-body buffer) and decides. It pulls the body in
//! fixed 64 KiB chunks into a reused buffer, scanning for a `deny-body` marker (which may straddle a
//! chunk boundary), then returns short-circuit 403 on a hit or continue otherwise. Guest linear
//! memory stays flat in body size — the property the buffered `list<u8>` contract cannot have.
//!
//! Built for wasm32-wasip2 and run by plecto-host's feature-gated `streaming-body` runtime; it is
//! NOT part of the default build (the shipped `plecto:filter@0.1.0` contract is untouched).
#![allow(clippy::all)]

use wit_bindgen::StreamResult;

wit_bindgen::generate!({
    path: "../../../wit-streaming",
    world: "streaming-filter",
    async: true,
});

use crate::plecto::filter_streaming::types::{Header, HttpResponse};
use exports::plecto::filter_streaming::body_filter::{Guest, RequestBodyDecision};

struct StreamingFilter;

const MARKER: &[u8] = b"deny-body";

impl Guest for StreamingFilter {
    async fn process_body(input: wit_bindgen::StreamReader<u8>) -> RequestBodyDecision {
        const CHUNK: usize = 64 * 1024;
        let mut reader = input;
        let mut buf: Vec<u8> = Vec::with_capacity(CHUNK);
        // Carry the trailing MARKER-1 bytes so a marker straddling a chunk boundary is still caught.
        let mut carry: Vec<u8> = Vec::new();
        let mut denied = false;
        loop {
            buf.clear(); // reset len, keep capacity → read refills the same spare 64 KiB
            let (status, returned) = reader.read(buf).await;
            buf = returned;
            match status {
                StreamResult::Complete(n) => {
                    if n > 0 && !denied {
                        let mut window = Vec::with_capacity(carry.len() + buf.len());
                        window.extend_from_slice(&carry);
                        window.extend_from_slice(&buf);
                        if window
                            .windows(MARKER.len())
                            .any(|w| w.eq_ignore_ascii_case(MARKER))
                        {
                            denied = true;
                        }
                        let keep = MARKER.len().saturating_sub(1);
                        let start = window.len().saturating_sub(keep);
                        carry = window[start..].to_vec();
                    }
                    // keep draining to let the stream complete cleanly even after a decision
                }
                StreamResult::Dropped => break,
                StreamResult::Cancelled => break,
            }
        }
        if denied {
            RequestBodyDecision::ShortCircuit(HttpResponse {
                status: 403,
                headers: vec![Header {
                    name: "x-plecto".to_string(),
                    value: "blocked-stream-body".to_string(),
                }],
                body: b"blocked streaming body".to_vec(),
            })
        } else {
            RequestBodyDecision::Continue
        }
    }
}

export!(StreamingFilter);
