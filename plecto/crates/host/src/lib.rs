//! plecto-host — embeds wasmtime to run `plecto:filter` components (ADR 000001 / 000002).
//!
//! ADR 000004 slice: the filter **runtime model**. The host branches on trust at load
//! (ADR 000011's knot, made concrete):
//!   - **trusted** filters get a fixed-capacity **pool** of reusable instances on a
//!     **pooling-allocator** engine, checked out per request (ADR 000012). `init` runs once
//!     *per instance* — Tenet 4 pays off (init-derived state stays resident). The pool is
//!     lazily filled: a single thread only ever needs one instance, so init stays once; under
//!     concurrency the pool builds more (up to its cap), which is where the pooling allocator
//!     finally earns its keep. Saturation (every instance checked out) waits a bounded time
//!     then fails **closed** (`Unavailable`), and an instance is recycled after serving a
//!     configured number of requests to bound linear-memory state accumulation (§6.6).
//!     Binding the pool to the tokio/quinn fast path (blocking pool vs fiber) is M2's job.
//!   - **untrusted** filters get a **fresh instance per request** on an on-demand engine,
//!     so linear memory is zeroized **by construction** (no slot reuse → CVE-2022-39393
//!     surface absent, ADR 000006). The cost is `init` every request — the deliberate
//!     trade of isolation (ADR 000011).
//!
//! State lives behind a `KvBackend` (in-memory or redb) — filters are stateless (Fork 4),
//! keys are host-namespaced per filter identity + primitive (ADR 000011). The `Linker`
//! stays **deny-by-default**: it lends ONLY the plecto host-API (log / clock / kv /
//! counter / ratelimit). No WASI, network, filesystem, or sockets (ADR 000006).

// Hot-path discipline (bp-rust): no unwrap/expect/panic/indexing on the data plane. Exempted
// under `cfg(test)` — this crate's own `#[cfg(test)] mod tests` blocks legitimately use them;
// `tests/*.rs` integration tests are separate crates and are never subject to this attribute.
#![cfg_attr(
    not(test),
    warn(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod backend;
mod observe;
pub mod otlp;
// Experimental streaming body filter (direction_0003 gates 1+2), OFF by default. A descendant of the
// crate root, so it reuses the private `EpochTicker` metering; the shipped path is untouched.
#[cfg(feature = "streaming-body")]
mod streaming;
#[cfg(feature = "streaming-body")]
pub use streaming::{StreamingDecision, StreamingLimits, run_streaming_body};
// Outbound capabilities for filters (ADR 000036 HTTP / ADR 000060 TCP), each OFF by default.
// `outbound` is the pure allowlist + SSRF policy both share (one `classify` = one floor); the
// wasmtime wirings that enforce it are `outbound_http` / `outbound_tcp` (per-feature gates), and
// `resolver` is the host-side DNS seam they share.
#[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
mod outbound;
#[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
pub use outbound::AddrVerdict;
#[cfg(feature = "outbound-http")]
pub use outbound::{AllowEntry, OutboundPolicy, Scheme};
#[cfg(feature = "outbound-tcp")]
pub use outbound::{OutboundTcpPolicy, TcpAllowEntry};
#[cfg(feature = "outbound-http")]
mod outbound_http;
#[cfg(feature = "outbound-tcp")]
mod outbound_tcp;
#[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
mod resolver;
// Fat guest (ADR 000063): the minimal-WASI stdio bridge, OFF by default.
#[cfg(feature = "fat-guest")]
mod stdio;

// Generic conformance battery (ADR 000065): the `plecto conformance` CLI surface. Production
// code, distinct from `tests/polyglot.rs`'s fixture-specific internal regression suite.
mod conformance;
mod contract;
// DevSigner (ADR 000065): the persistent, project-local signing key for `plecto dev` /
// `plecto conformance`. Production code (unlike `test_support`) — it links into a plain
// `plecto-server` build, not just behind `test-support`.
mod dev_signer;
mod engine;
mod errors;
mod filter;
mod host;
mod options;
mod pool;
mod quota;
mod runtime;
mod state;
mod trust;
mod util;
// Test / dev signing support — **NOT production provenance**; see `test_support.rs`. The
// `clippy::expect_used` allow was on the module itself (not a single item), so it moves here
// with the module declaration.
#[doc(hidden)]
#[cfg(feature = "test-support")]
#[allow(clippy::expect_used)]
// dev/test-only fixture loader (test-support feature); not data-plane code
pub mod test_support;

pub use backend::{Acquire, Bucket, KvBackend, MemoryBackend, RedbBackend, apply_bucket};
pub use conformance::{ConformanceCheck, ConformanceReport, check as run_conformance};
pub use contract::{ContractVersion, header};
pub use dev_signer::{DEV_KEY_MARKER, DevKeyError, DevSigner, bound_sbom, public_key_path_for};
pub use observe::{
    FanOutSink, FilterSpan, Hook, InMemorySink, MetricsSink, MetricsSnapshot, NoopSink,
    RequestTrace, SpanOutcome, TelemetrySink,
};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "filter",
        // M3 Stage 1 (ADR 000021): the guest's exported hooks (init / on-request / on-response) run
        // via call_async on wasmtime fibers — the prerequisite for future WASI async host calls. The
        // trivial plecto host-API IMPORTS stay sync (they never block, so they don't need to be
        // async). Body / stream<u8> contract stays frozen until Stage 2.
        exports: { default: async },
    });
}

// One canonical set of contract types for callers and tests.
pub use bindings::plecto::filter::host_log::Level as LogLevel;
pub use bindings::plecto::filter::types::{
    Header, HttpRequest, HttpResponse, RequestBodyDecision, RequestDecision, RequestEdit,
    ResponseDecision, ResponseEdit,
};

pub use errors::{LoadError, RunError};
pub use filter::LoadedFilter;
pub use host::Host;
pub use options::{Isolation, LoadOptions};
pub use state::{HostState, LogLine};
pub use trust::{SignedArtifact, TrustPolicy};
