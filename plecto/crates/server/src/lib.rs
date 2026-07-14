//! plecto-server — the M2 fast path (ADR 000013, TLS 000014, HTTP/2 000015, HTTP/3 000016).
//!
//! A tokio listener that turns Plecto from a library into an actual reverse proxy. It serves
//! HTTP/1.1 and HTTP/2 over TCP (hyper, ALPN-negotiated — ADR 000015) and HTTP/3 over QUIC (quinn +
//! the h3 crate, an independent UDP listener advertised via `Alt-Svc` — ADR 000016). All three
//! transports feed the same transaction core (`proxy_core`); only the body adapters differ.
//! Per request it: builds a header-only `HttpRequest`, asks the control plane which route
//! matches (host + path prefix), runs that route's filter chain, and either responds now (a
//! filter short-circuited / failed closed) or forwards the request to the route's upstream and
//! runs the response side of the chain on the way back.
//!
//! **sync↔async bridge (the §6.3 prerequisite).** Filter execution is synchronous and runs on a
//! wasmtime `Store` that is `!Send`, so it cannot cross an `.await`. Each chain dispatch is moved
//! to tokio's blocking pool via `spawn_blocking`; the M1 trusted instance pool handles instance
//! reuse and saturation there. Route matching is pure config lookup and stays on the async thread.
//!
//! **Request body: buffered ONLY when a filter reads it (ADR 000025 / 000038).** A route whose
//! filters all target the header-only `filter` world streams the request body straight to the
//! upstream (zero-copy); a route with a filter that exports `on-request-body` (`reads_body`) has the
//! body buffered (bounded) and run through the `on-request-body` chain. The response body always
//! streams straight back — filters see response headers / status only (they may synthesise a
//! short-circuit body of their own).

// Hot-path discipline (bp-rust): no unwrap/expect/panic/indexing on the data plane. Exempted
// under `cfg(test)` — this crate's own `#[cfg(test)] mod` blocks legitimately use them;
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

mod access_log;
mod admin;
mod body;
mod compression;
mod conn_limit;
mod dispatch;
mod dns;
mod error;
mod forward;
mod h3;
mod headers;
mod health;
mod listener;
mod metrics;
mod otlp;
mod proxy;
// `pub`: the out-of-workspace fuzz harness (`fuzz/`) drives the pure parser (ADR 000057). Not a
// semver surface — the crate is `publish = false`.
pub mod proxy_protocol;
mod respond;
mod retry;
mod tunnel;
mod upstream_client;

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::header::HeaderValue;
use hyper_util::client::legacy::connect::HttpConnector;
use plecto_control::Control;
use tokio::sync::Semaphore;

use crate::metrics::ServerMetrics;
use crate::upstream_client::UpstreamClients;

pub use listener::{DEFAULT_DRAIN_DEADLINE, serve, serve_with_shutdown};

/// Cap glibc's per-thread malloc arenas at process start to bound RSS on many-core hosts.
///
/// glibc defaults to `8 × ncpu` arenas on 64-bit. Under a many-threaded proxy doing bursty
/// per-request allocations, freed memory lingers in each thread's arena instead of returning to the
/// OS, inflating RSS (measured ~2.5× at 1 MB bodies × 50 conns — docs/servey body-tax). This is a
/// defensive complement to the real fix (not buffering a body no filter reads); routes that
/// legitimately buffer still allocate, and this bounds their arena fragmentation.
///
/// `M_ARENA_MAX` only gates creation of NEW arenas and never reclaims existing ones, so this MUST
/// run before the runtime spawns its worker threads (call it first in `main`). Default cap **4** — a
/// portable, contention-safe value used across multithreaded services, chosen over the value that
/// minimised RSS on one host (1) precisely because Plecto is self-hosted on varied machines.
/// Override with `PLECTO_MALLOC_ARENA_MAX` (`0` leaves glibc's default in place). No-op off glibc.
pub fn cap_malloc_arenas() {
    let max = std::env::var("PLECTO_MALLOC_ARENA_MAX")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(4);
    apply_arena_cap(max);
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn apply_arena_cap(max: i32) {
    if max <= 0 {
        return; // 0 / negative: leave glibc's default (8 × ncpu) untouched.
    }
    // SAFETY: a plain libc call made single-threaded at startup, before any worker thread exists.
    // Returns 1 on success / 0 on failure; a best-effort tuning knob, so a rejection is ignored
    // rather than failing startup.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, max as core::ffi::c_int);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn apply_arena_cap(_max: i32) {}

/// A boxed, `Send` error — the unified error type for the boxed request/response bodies.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The response body the service yields: either a synthesised buffer (`Full`, for a short-circuit
/// or a fail-closed 5xx) or the upstream's streamed body (`Incoming`), unified behind one boxed
/// type so the service has a single return shape.
pub(crate) type ResponseBody = BoxBody<Bytes, BoxError>;

/// The request body forwarded to the upstream, boxed so one type covers every inbound transport:
/// the hyper `Incoming` (HTTP/1.1 + HTTP/2) and the QUIC/h3 recv stream (HTTP/3, ADR 000016). The
/// body streams straight through opaquely (header-only contract, ADR 000010) regardless of source.
pub(crate) type ReqBody = BoxBody<Bytes, BoxError>;

/// Global cap on concurrently-served connections across all transports (CWE-770). A permit
/// is acquired BEFORE each accept, so at saturation the listener stops pulling new connections off
/// the OS backlog (natural backpressure) instead of spawning per-connection tasks unboundedly.
pub(crate) const MAX_CONNECTIONS: usize = 10_000;

/// Per-source-IP cap on concurrently-served connections (CWE-770/CWE-400, docs/servey production
/// hardening — ADR 000027 amendment), enforced by [`conn_limit::PerIpConnLimit`] in both accept
/// loops (TCP `listener.rs`, QUIC `h3/endpoint.rs`). `MAX_CONNECTIONS` alone bounds the total but
/// not its distribution: without this, one source can hold every permit and starve every other
/// client. ~2.6% of `MAX_CONNECTIONS` — comfortably above any legitimate single-source workload
/// (even a large NAT/corporate gateway), while an attacker needs ~40 distinct source addresses
/// (or hash-colliding /64s, for IPv6) to exhaust the pool alone, instead of one.
pub(crate) const MAX_CONNECTIONS_PER_IP: u32 = 256;

/// Raise the process's soft `RLIMIT_NOFILE` to match the hard limit at startup (Unix only; a no-op
/// twin exists for other platforms below).
///
/// Most distros ship a low default soft limit (often 1024) while the hard limit is already
/// generous — an unprivileged process may always raise its own soft limit up to the hard limit
/// (POSIX `setrlimit(2)`, no `CAP_SYS_RESOURCE` needed). Without this, an accept loop asking for
/// `MAX_CONNECTIONS` sockets hits EMFILE at the OS default long before its own cap — and, by
/// design, a transient accept error is logged and the loop continues (`listener.rs`), so the
/// server silently stops admitting new connections instead of crashing loudly. Raising the HARD
/// limit itself needs a privilege Plecto does not request (self-hosting simplicity, deny-by-default
/// P4); if the hard limit is ALSO below what `MAX_CONNECTIONS` needs, this only warns — an
/// operator must raise it externally (systemd `LimitNOFILE=`, `docker run --ulimit nofile=...`, or
/// the container runtime's own ulimit configuration).
#[cfg(unix)]
pub fn raise_nofile_limit() {
    match unix_raise_nofile_limit() {
        Ok((soft, hard)) => {
            // 1 client fd + 1 upstream fd per proxied connection (the same accounting nginx
            // documents for a proxying worker), doubled for headroom: the admin/health listener,
            // DNS-refresh sockets, TLS resumption file handles, and connections mid-teardown that
            // have not yet released their `MAX_CONNECTIONS` permit.
            let wanted = MAX_CONNECTIONS as u64 * 4;
            if soft < wanted {
                tracing::warn!(
                    soft,
                    hard,
                    wanted,
                    "RLIMIT_NOFILE is below the recommended floor for MAX_CONNECTIONS; raise the \
                     HARD limit externally (systemd LimitNOFILE=, docker --ulimit nofile=, or \
                     ulimit -Hn) — Plecto does not request CAP_SYS_RESOURCE to do this itself"
                );
            } else {
                tracing::info!(soft, hard, "RLIMIT_NOFILE raised to the hard limit");
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to read/raise RLIMIT_NOFILE; leaving the OS default in place"
            );
        }
    }
}

/// No POSIX resource limits on this platform — nothing to raise.
#[cfg(not(unix))]
pub fn raise_nofile_limit() {}

#[cfg(unix)]
fn unix_raise_nofile_limit() -> std::io::Result<(u64, u64)> {
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rlim` is a valid, stack-local `libc::rlimit`; `getrlimit`/`setrlimit` only read/
    // write through the pointer we pass and signal failure via a `-1` return (checked below),
    // never touching memory beyond the struct.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let hard = rlim.rlim_max;
    if rlim.rlim_cur < hard {
        rlim.rlim_cur = hard;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // `rlim_t` is `u64` on every Unix target this crate builds for (Linux glibc/musl, macOS).
    Ok((rlim.rlim_cur, hard))
}

/// Per-connection cap on concurrent HTTP/2 streams (ADR 000015). A fixed, conservative bound (not
/// yet manifest-configurable): it stops a single h2 connection from monopolising the fixed-capacity
/// M1 instance pool (ADR 000012) with concurrent chain dispatches, and is defence-in-depth against
/// stream-flooding DoS (the h2 crate already mitigates Rapid Reset, CVE-2023-44487). 100 is the
/// RFC 9113 recommended floor; hyper's own default is version-dependent and not API-stable, so we
/// pin it explicitly.
pub(crate) const MAX_CONCURRENT_STREAMS: u32 = 100;

/// Shared per-server state: the control plane (filters, routes, reload), the upstream clients, and
/// the `Alt-Svc` header value advertising HTTP/3 (ADR 000016) — `Some` only when a QUIC listener is
/// bound, and added to TCP (HTTP/1.1 + HTTP/2) responses to steer capable clients to h3.
pub(crate) struct ServerState {
    control: Arc<Control>,
    /// Per-security-context upstream clients (ADR 000042): one plain HTTP/1.1 client plus one
    /// pooled TLS client per distinct `[upstream.tls]` config.
    clients: UpstreamClients,
    alt_svc: Option<HeaderValue>,
    /// Global connection cap across TCP + QUIC: a permit is held for each connection's
    /// lifetime, so the server never serves more than `MAX_CONNECTIONS` at once.
    conn_limit: Arc<Semaphore>,
    /// Per-source-IP connection cap across TCP + QUIC (`MAX_CONNECTIONS_PER_IP`): a slot is held
    /// for each connection's lifetime, so one source cannot exhaust `conn_limit` alone.
    per_ip_conn_limit: Arc<conn_limit::PerIpConnLimit>,
    /// Cap on concurrently-buffered request bodies for the `on-request-body` hook, bounding
    /// total buffered memory.
    body_buffer_limit: Arc<Semaphore>,
    /// Native data-plane metrics (Stage A observability, ADR 000009): RED signals tallied per
    /// request and served on the admin endpoint. Always recorded (cheap atomics); whether anyone
    /// can scrape them is gated by `[observability] admin_addr`.
    metrics: Arc<ServerMetrics>,
    /// The OTLP span buffer (ADR 000040), present iff `[observability] otlp_endpoint` is set:
    /// `proxy_core` pushes one SERVER request span per sampled transaction; the export pump
    /// (spawned by the listener) drains it.
    otlp: Option<Arc<plecto_control::otlp::OtlpBuffer>>,
    /// The graceful-shutdown drain flag (ADR 000039), cloned into every spawned upgrade tunnel
    /// (ADR 000048) so an indefinite tunnel closes at drain instead of outliving the server.
    drain: tokio::sync::watch::Receiver<bool>,
    /// The readiness flag (ADR 000059): `true` for the serving lifetime, flipped to `false` at
    /// the shutdown signal — BEFORE the drain starts — so `/readyz` tells the front load
    /// balancer to remove this replica while it still accepts connections (the readiness grace).
    ready: tokio::sync::watch::Receiver<bool>,
}

/// An upstream TCP connector with `TCP_NODELAY` set. A proxy must disable Nagle on its upstream
/// sockets: with Nagle on, a streamed request body sent in several writes stalls ~40 ms on the
/// peer's delayed-ACK timer (surfaced by the body benchmark as a p99 cliff on large streamed
/// bodies). Disabling Nagle on proxy/upstream sockets is standard practice across mature L7 proxies.
/// Both the forwarding clients and the health prober use it — plain, and wrapped by the rustls
/// connector for a TLS upstream (ADR 000042), which is why `enforce_http` is off (the wrapping
/// `HttpsConnector` dials `https://` URIs through it).
pub(crate) fn upstream_connector() -> HttpConnector {
    let mut c = HttpConnector::new();
    c.set_nodelay(true);
    c.enforce_http(false);
    c
}

#[cfg(test)]
mod alloc_tuning_tests {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn mallopt_arena_max_is_accepted_by_glibc() {
        // Guards the FFI constant + linkage: glibc returns 1 when it accepts the option.
        let rc = unsafe { libc::mallopt(libc::M_ARENA_MAX, 4) };
        assert_eq!(
            rc, 1,
            "glibc mallopt(M_ARENA_MAX, 4) should return 1 on success"
        );
    }

    #[test]
    fn cap_is_a_noop_when_disabled() {
        // 0 leaves glibc's default in place; must not panic (and compiles/no-ops off glibc).
        super::apply_arena_cap(0);
    }
}

#[cfg(all(test, unix))]
mod nofile_tests {
    #[test]
    fn raises_soft_limit_to_match_the_hard_limit() {
        // An unprivileged process may always raise its own soft limit up to the hard limit
        // (POSIX) — every sandbox this test runs in must allow it, so a failure here is real.
        let (soft, hard) =
            super::unix_raise_nofile_limit().expect("getrlimit/setrlimit must succeed");
        assert_eq!(
            soft, hard,
            "soft limit must be raised to match the hard limit"
        );

        // Calling it again is idempotent: the limit is already at its ceiling.
        let (soft2, hard2) =
            super::unix_raise_nofile_limit().expect("a second call must also succeed");
        assert_eq!((soft2, hard2), (soft, hard));
    }
}
