//! Fuzz the pure PROXY v2 parser (ADR 000057): the listener feeds it bytes straight off an
//! untrusted socket, so its invariant is totality — any input returns `Ok`/`Err`, never
//! panics, never over-reads (P11: no panic on the data plane).

#![no_main]

use libfuzzer_sys::fuzz_target;
use plecto_server::proxy_protocol::{Parsed, parse_proxy_v2};

fuzz_target!(|data: &[u8]| {
    if let Ok(Parsed::Complete { consumed, .. }) = parse_proxy_v2(data) {
        // A completed header never claims bytes beyond the input.
        assert!(consumed <= data.len());
    }
});
