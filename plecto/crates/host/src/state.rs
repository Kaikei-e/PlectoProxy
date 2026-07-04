//! Per-request host state ([`HostState`]) and the host-API capability implementations
//! (deny-by-default: only these are lent to a filter, ADR 000006 / 000011).

use std::sync::Arc;

#[cfg(feature = "outbound-http")]
use wasmtime::component::Linker;
use wasmtime::{StoreLimits, StoreLimitsBuilder};

use crate::LogLevel;
use crate::bindings;
use crate::bindings::plecto::filter::{
    host_clock, host_counter, host_kv, host_log, host_ratelimit,
};
#[cfg(feature = "outbound-http")]
use crate::outbound_http;
use crate::quota::KvQuota;
use crate::util::wall_now_ms;
use crate::{Bucket, KvBackend};

/// A log line captured from the host-log capability (test visibility / future tracing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: LogLevel,
    pub message: String,
}

/// Delimiter the host uses to namespace KV keys by filter identity. A filter can never
/// remove the host-applied prefix, so it cannot reach another filter's namespace —
/// capability isolation across a chain (ADR 000006 / 000011). Filter ids must not contain
/// this byte (enforced by `Host::load`).
pub(crate) const KV_NS_DELIM: char = '\u{1f}';

// Primitive sub-namespace tags, so a filter's kv "x", counter "x", and bucket "x" never
// collide in the shared backend keyspace.
const TAG_KV: u8 = b'k';
const TAG_COUNTER: u8 = b'c';
const TAG_RATELIMIT: u8 = b'r';

/// Largest value a filter may store under one KV key. A bigger `set` is dropped (fail-closed).
const MAX_KV_VALUE_BYTES: usize = 256 * 1024;
/// Largest filter-supplied key. A longer key is dropped (bounds the namespaced key itself).
const MAX_KV_KEY_BYTES: usize = 1024;
/// Per-request cap on host-log lines a filter may emit (CWE-770). The last slot is a
/// single truncation marker so overflow stays observable.
const MAX_LOG_LINES_PER_REQUEST: usize = 256;
/// Per-line cap on a host-log message; a longer message is truncated on a char boundary.
const MAX_LOG_MSG_BYTES: usize = 8 * 1024;

/// Per-instance cap on total table elements (review f000003 #2). `StoreLimits::memory_size`
/// bounds linear memory but NOT `table.grow`; a guest growing a huge funcref table could eat
/// host memory outside the linear-memory cap before the epoch deadline trips. This is generous
/// for any reasonable filter and bounds the pathological case — cheap defense-in-depth.
const MAX_TABLE_ELEMENTS: usize = 100_000;

/// Neutralize a guest-supplied log message (CWE-117): truncate to a byte cap on a char
/// boundary and replace control characters (CR/LF for log-line injection, C0/C1/ESC for terminal
/// ANSI) with the replacement char. The filter is untrusted and may embed `Authorization` header
/// bytes or escape sequences, so the host — the trust boundary — neutralizes before storing.
fn sanitize_log_message(mut message: String) -> String {
    if message.len() > MAX_LOG_MSG_BYTES {
        let mut end = MAX_LOG_MSG_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    if message.bytes().any(|b| b < 0x20 || b == 0x7f) {
        message = message
            .chars()
            .map(|c| if c.is_control() { '\u{fffd}' } else { c })
            .collect();
    }
    message
}

/// Per-request host state: the capability handles lent to a filter plus request-scoped
/// buffers. For untrusted filters a fresh one is built per request; for trusted filters
/// the same one is reused with `begin_request` resetting the per-request fields, while the
/// instance's init-derived linear memory persists (ADR 000011).
pub struct HostState {
    kv: Arc<dyn KvBackend>,
    /// Host-owned prefix (`"{filter_id}\u{1f}"`) applied to every key. The filter cannot
    /// observe or alter it.
    kv_prefix: String,
    /// Per-request host-log buffer. `pub(crate)`: `runtime.rs`'s `WasmtimeRuntime::take_logs`
    /// drains it directly after a guest call (structural cross-module access, no behavior change).
    pub(crate) logs: Vec<LogLine>,
    /// Wall-clock ms captured once at request start: a stable per-request snapshot.
    now_ms: u64,
    /// Linear-memory / table / instance caps for this Store (ADR 000006). Wired via
    /// `Store::limiter`; a grow past the cap is denied, bounding mis-allocation and runaway
    /// growth even on the untrusted on-demand engine (which has no pooling reservation).
    /// `pub(crate)`: `runtime.rs` wires `Store::limiter` to this field directly.
    pub(crate) limits: StoreLimits,
    /// This filter's host-configured token-bucket spec (manifest `[filter.ratelimit]`, ADR
    /// 000026). `None` = no bucket configured → `host-ratelimit/try-acquire` fails closed. The
    /// filter cannot supply or override it, so an untrusted filter cannot neuter its own limiter.
    ratelimit_bucket: Option<Bucket>,
    /// Shared per-namespace accounting + caps for host-held state. Charged on every
    /// `set` / `increment` / `try_acquire` that grows the store; over-quota writes fail closed.
    quota: Arc<KvQuota>,
    /// Outbound HTTP (ADR 000036): the minimal WASI base ctx, resource table, `wasi:http` ctx, and
    /// the SSRF-guarded hooks. `wasi:http` is added to the Linker only for filters with an outbound
    /// policy; for every other filter `hooks` denies every call (belt-and-suspenders, so a stray
    /// call still fails closed).
    #[cfg(feature = "outbound-http")]
    wasi: wasmtime_wasi::WasiCtx,
    #[cfg(feature = "outbound-http")]
    http_ctx: wasmtime_wasi_http::WasiHttpCtx,
    #[cfg(feature = "outbound-http")]
    table: wasmtime::component::ResourceTable,
    #[cfg(feature = "outbound-http")]
    hooks: outbound_http::PlectoHttpHooks,
}

impl HostState {
    pub(crate) fn new(
        kv: Arc<dyn KvBackend>,
        kv_prefix: String,
        max_memory_bytes: u64,
        ratelimit_bucket: Option<Bucket>,
        quota: Arc<KvQuota>,
        #[cfg(feature = "outbound-http")] hooks: outbound_http::PlectoHttpHooks,
    ) -> Self {
        Self {
            kv,
            kv_prefix,
            logs: Vec::new(),
            now_ms: wall_now_ms(),
            limits: StoreLimitsBuilder::new()
                .memory_size(max_memory_bytes as usize)
                .table_elements(MAX_TABLE_ELEMENTS)
                .build(),
            ratelimit_bucket,
            quota,
            #[cfg(feature = "outbound-http")]
            wasi: wasmtime_wasi::WasiCtxBuilder::new().build(),
            #[cfg(feature = "outbound-http")]
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            #[cfg(feature = "outbound-http")]
            table: wasmtime::component::ResourceTable::new(),
            #[cfg(feature = "outbound-http")]
            hooks,
        }
    }

    /// Reset per-request state for a reused (trusted) instance. Clears the log buffer and
    /// re-snapshots the clock; the WASM instance's linear memory (init-derived) is untouched.
    pub(crate) fn begin_request(&mut self) {
        self.logs.clear();
        self.now_ms = wall_now_ms();
    }

    /// Namespace a filter-supplied key into `{filter_id}\u{1f}{tag}\u{1f}{key}` bytes.
    fn ns_key(&self, tag: u8, key: &str) -> Vec<u8> {
        let mut k = Vec::with_capacity(self.kv_prefix.len() + 2 + key.len());
        k.extend_from_slice(self.kv_prefix.as_bytes());
        k.push(tag);
        k.push(KV_NS_DELIM as u8);
        k.extend_from_slice(key.as_bytes());
        k
    }
}

// --- host-API capability implementations (deny-by-default: only these are lent) ---

// Outbound HTTP (ADR 000036): the WASI base + wasi:http projections, added to the Linker only for
// filters with an outbound policy. They share one resource table.
#[cfg(feature = "outbound-http")]
impl wasmtime_wasi::WasiView for HostState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

#[cfg(feature = "outbound-http")]
impl wasmtime_wasi_http::p2::WasiHttpView for HostState {
    fn http(&mut self) -> wasmtime_wasi_http::p2::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::p2::WasiHttpCtxView {
            ctx: &mut self.http_ctx,
            table: &mut self.table,
            hooks: &mut self.hooks,
        }
    }
}

/// Add the wasi:cli interfaces the std guest's runtime imports beyond the proxy slice (environment /
/// exit / terminal-*), each inert under the empty `WasiCtx`. Adds NO filesystem and NO sockets, so
/// those capabilities stay denied (security audit F-002; mirrors the streaming path).
#[cfg(feature = "outbound-http")]
pub(crate) fn add_cli_runtime(linker: &mut Linker<HostState>) -> wasmtime::Result<()> {
    use wasmtime_wasi::cli::{WasiCli, WasiCliView};
    use wasmtime_wasi::p2::bindings::cli;
    let getter = <HostState as WasiCliView>::cli;
    cli::environment::add_to_linker::<HostState, WasiCli>(linker, getter)?;
    cli::exit::add_to_linker::<HostState, WasiCli>(linker, getter)?;
    cli::terminal_input::add_to_linker::<HostState, WasiCli>(linker, getter)?;
    cli::terminal_output::add_to_linker::<HostState, WasiCli>(linker, getter)?;
    cli::terminal_stdin::add_to_linker::<HostState, WasiCli>(linker, getter)?;
    cli::terminal_stdout::add_to_linker::<HostState, WasiCli>(linker, getter)?;
    cli::terminal_stderr::add_to_linker::<HostState, WasiCli>(linker, getter)?;
    Ok(())
}

// `types` is a type-only interface (no functions); the generated `Host` trait is empty.
impl bindings::plecto::filter::types::Host for HostState {}

impl host_log::Host for HostState {
    fn log(&mut self, level: LogLevel, message: String) {
        // Bound per-request log volume and neutralize control bytes: a guest can
        // loop `log` until its deadline, so cap the line count (reserving the last slot for one
        // truncation marker) and sanitize each message before it is stored.
        match self.logs.len() {
            n if n < MAX_LOG_LINES_PER_REQUEST - 1 => self.logs.push(LogLine {
                level,
                message: sanitize_log_message(message),
            }),
            n if n == MAX_LOG_LINES_PER_REQUEST - 1 => self.logs.push(LogLine {
                level: LogLevel::Warn,
                message: "… host-log truncated (per-request line cap reached)".to_string(),
            }),
            _ => {}
        }
    }
}

impl host_clock::Host for HostState {
    fn now_ms(&mut self) -> u64 {
        self.now_ms
    }
}

impl host_kv::Host for HostState {
    fn get(&mut self, key: String) -> Option<Vec<u8>> {
        self.kv.get(&self.ns_key(TAG_KV, &key))
    }
    fn set(&mut self, key: String, value: Vec<u8>) {
        // Per-key size limits + per-namespace/global quota. Over-limit writes are dropped
        // (fail-closed): from the filter's view the host-API is infallible ("reads vanish").
        if key.len() > MAX_KV_KEY_BYTES || value.len() > MAX_KV_VALUE_BYTES {
            return;
        }
        let nskey = self.ns_key(TAG_KV, &key);
        // Charge the byte delta vs. any existing value (a new key also charges its key bytes + 1
        // entry). The read-before-write keeps the byte accounting exact for variable-size values.
        let (entries_delta, bytes_delta) = match self.kv.get(&nskey).map(|v| v.len()) {
            None => (1isize, (key.len() + value.len()) as isize),
            Some(old) => (0isize, value.len() as isize - old as isize),
        };
        if !self
            .quota
            .admit(&self.kv_prefix, entries_delta, bytes_delta)
        {
            return;
        }
        self.kv.set(&nskey, value);
    }
    fn delete(&mut self, key: String) {
        let nskey = self.ns_key(TAG_KV, &key);
        if let Some(old) = self.kv.get(&nskey).map(|v| v.len()) {
            self.kv.delete(&nskey);
            self.quota.release(&self.kv_prefix, 1, key.len() + old);
        }
    }
}

impl host_counter::Host for HostState {
    fn increment(&mut self, key: String, delta: i64) -> i64 {
        let nskey = self.ns_key(TAG_COUNTER, &key);
        // A zero delta is a pure read (host-counter.get); it neither creates a key nor is charged.
        if delta == 0 {
            return self.kv.increment(&nskey, 0);
        }
        // A counter is a fixed 8-byte value: only a NEW key grows the store, so charge one entry
        // when first created and fail closed (report the current value, do not create) over quota.
        if self.kv.get(&nskey).is_none()
            && !self
                .quota
                .admit(&self.kv_prefix, 1, (key.len() + 8) as isize)
        {
            return 0;
        }
        self.kv.increment(&nskey, delta)
    }
    fn get(&mut self, key: String) -> i64 {
        // increment-by-zero is an atomic read of the current value (and the canonical
        // wasi:keyvalue/atomics idiom); keeps the counter encoding inside the backend.
        self.kv.increment(&self.ns_key(TAG_COUNTER, &key), 0)
    }
}

impl host_ratelimit::Host for HostState {
    fn try_acquire(&mut self, key: String, cost: u64) -> host_ratelimit::Acquire {
        // The bucket spec is host-configured per filter (manifest, ADR 000026); the filter cannot
        // supply or override it. A filter with no configured bucket is denied (fail-closed) — it
        // cannot opt out of its limiter.
        let Some(spec) = self.ratelimit_bucket else {
            return host_ratelimit::Acquire {
                allowed: false,
                remaining: 0,
                retry_after_ms: 0,
            };
        };
        let nskey = self.ns_key(TAG_RATELIMIT, &key);
        // A bucket is a fixed 16-byte value: charge one entry when first created. Over quota a
        // filter cannot mint unbounded distinct-key buckets — deny (fail-closed), do not create.
        if self.kv.get(&nskey).is_none()
            && !self
                .quota
                .admit(&self.kv_prefix, 1, (key.len() + 16) as isize)
        {
            return host_ratelimit::Acquire {
                allowed: false,
                remaining: 0,
                retry_after_ms: 0,
            };
        }
        let r = self.kv.try_acquire(&nskey, cost, spec, self.now_ms);
        host_ratelimit::Acquire {
            allowed: r.allowed,
            remaining: r.remaining,
            retry_after_ms: r.retry_after_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the deny-by-default host-API implementations (ADR 000006 / 000011).
    use super::*;
    use crate::MemoryBackend;
    use crate::options::DEFAULT_MAX_MEMORY_BYTES;
    use host_clock::Host as ClockHost;
    use host_counter::Host as CounterHost;
    use host_kv::Host as KvHost;
    use host_log::Host as LogHost;

    fn state(prefix: &str) -> HostState {
        HostState::new(
            Arc::new(MemoryBackend::default()),
            prefix.to_string(),
            DEFAULT_MAX_MEMORY_BYTES,
            None,
            Arc::new(KvQuota::new()),
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
        )
    }

    #[test]
    fn kv_get_set_delete_roundtrip() {
        let mut s = state("test\u{1f}");
        assert_eq!(KvHost::get(&mut s, "k".into()), None);
        KvHost::set(&mut s, "k".into(), b"v".to_vec());
        assert_eq!(KvHost::get(&mut s, "k".into()), Some(b"v".to_vec()));
        KvHost::delete(&mut s, "k".into());
        assert_eq!(KvHost::get(&mut s, "k".into()), None);
    }

    #[test]
    fn kv_is_namespaced_per_filter() {
        // Two filters sharing one backing store must not see each other's keys
        // (capability isolation across a chain, ADR 000006 / 000011).
        let shared: Arc<dyn KvBackend> = Arc::new(MemoryBackend::default());
        let mut a = HostState::new(
            shared.clone(),
            "filter-a\u{1f}".to_string(),
            DEFAULT_MAX_MEMORY_BYTES,
            None,
            Arc::new(KvQuota::new()),
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
        );
        let mut b = HostState::new(
            shared.clone(),
            "filter-b\u{1f}".to_string(),
            DEFAULT_MAX_MEMORY_BYTES,
            None,
            Arc::new(KvQuota::new()),
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
        );

        KvHost::set(&mut a, "count".into(), b"1".to_vec());
        assert_eq!(
            KvHost::get(&mut b, "count".into()),
            None,
            "b must not see a"
        );
        assert_eq!(KvHost::get(&mut a, "count".into()), Some(b"1".to_vec()));

        // a key that embeds the delimiter still cannot escape a's namespace
        KvHost::set(&mut a, format!("x{}count", '\u{1f}'), b"evil".to_vec());
        assert_eq!(KvHost::get(&mut b, "count".into()), None);
    }

    #[test]
    fn counter_is_namespaced_per_filter() {
        // The counter primitive shares the backend keyspace with kv/ratelimit, so its per-filter
        // isolation must hold too: one filter's `requests` counter must be invisible to another
        // (cross-tenant leakage, CWE-200). Only the `_KV_` test covered this before.
        let shared: Arc<dyn KvBackend> = Arc::new(MemoryBackend::default());
        let mut a = HostState::new(
            shared.clone(),
            "filter-a\u{1f}".to_string(),
            DEFAULT_MAX_MEMORY_BYTES,
            None,
            Arc::new(KvQuota::new()),
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
        );
        let mut b = HostState::new(
            shared.clone(),
            "filter-b\u{1f}".to_string(),
            DEFAULT_MAX_MEMORY_BYTES,
            None,
            Arc::new(KvQuota::new()),
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
        );

        assert_eq!(CounterHost::increment(&mut a, "hits".into(), 5), 5);
        assert_eq!(
            CounterHost::get(&mut b, "hits".into()),
            0,
            "b must not observe a's counter"
        );
        assert_eq!(
            CounterHost::increment(&mut b, "hits".into(), 1),
            1,
            "b's counter is independent of a's"
        );
        assert_eq!(
            CounterHost::get(&mut a, "hits".into()),
            5,
            "a's counter is untouched by b"
        );
    }

    #[test]
    fn ratelimit_bucket_is_namespaced_per_filter() {
        // A rate limiter is only a security control if one filter cannot drain — or be throttled
        // by — another filter's bucket under the same key. The token bucket lives in the shared
        // backend under a per-filter namespace; prove two filters' identical keys are independent.
        use host_ratelimit::Host as RateLimitHost;
        fn one_token_no_refill() -> Bucket {
            Bucket {
                capacity: 1,
                refill_tokens: 0,
                refill_interval_ms: 0,
            }
        }

        // The bucket spec is host-configured (ADR 000026), so each filter's HostState carries it.
        let shared: Arc<dyn KvBackend> = Arc::new(MemoryBackend::default());
        let mut a = HostState::new(
            shared.clone(),
            "filter-a\u{1f}".to_string(),
            DEFAULT_MAX_MEMORY_BYTES,
            Some(one_token_no_refill()),
            Arc::new(KvQuota::new()),
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
        );
        let mut b = HostState::new(
            shared.clone(),
            "filter-b\u{1f}".to_string(),
            DEFAULT_MAX_MEMORY_BYTES,
            Some(one_token_no_refill()),
            Arc::new(KvQuota::new()),
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
        );

        // a drains its single-token bucket on key "k".
        assert!(RateLimitHost::try_acquire(&mut a, "k".into(), 1).allowed);
        assert!(
            !RateLimitHost::try_acquire(&mut a, "k".into(), 1).allowed,
            "a's bucket is now empty"
        );
        // b's bucket under the SAME key is a different namespace → still full.
        assert!(
            RateLimitHost::try_acquire(&mut b, "k".into(), 1).allowed,
            "b's limiter must not share a's drained bucket"
        );
    }

    #[test]
    fn kv_and_counter_do_not_collide() {
        // Same logical key under different primitives must stay distinct (tag sub-namespace).
        let mut s = state("f\u{1f}");
        KvHost::set(&mut s, "x".into(), b"bytes".to_vec());
        assert_eq!(CounterHost::increment(&mut s, "x".into(), 7), 7);
        assert_eq!(KvHost::get(&mut s, "x".into()), Some(b"bytes".to_vec()));
        assert_eq!(CounterHost::get(&mut s, "x".into()), 7);
    }

    #[test]
    fn counter_increment_and_read() {
        let mut s = state("f\u{1f}");
        assert_eq!(CounterHost::get(&mut s, "n".into()), 0);
        assert_eq!(CounterHost::increment(&mut s, "n".into(), 1), 1);
        assert_eq!(CounterHost::increment(&mut s, "n".into(), 2), 3);
        assert_eq!(CounterHost::get(&mut s, "n".into()), 3);
    }

    #[test]
    fn log_captures_lines() {
        let mut s = state("test\u{1f}");
        LogHost::log(&mut s, LogLevel::Info, "hello".into());
        assert_eq!(s.logs.len(), 1);
        assert_eq!(s.logs[0].message, "hello");
    }

    #[test]
    fn begin_request_resets_logs_keeps_namespace() {
        let mut s = state("test\u{1f}");
        LogHost::log(&mut s, LogLevel::Info, "first".into());
        s.begin_request();
        assert!(s.logs.is_empty(), "logs reset for the next request");
    }

    #[test]
    fn clock_returns_nonzero_wall_time() {
        let mut s = state("test\u{1f}");
        assert!(ClockHost::now_ms(&mut s) > 0);
    }

    #[test]
    fn kv_value_over_cap_is_dropped_fail_closed() {
        // a value past the per-key cap is dropped, not stored (host-API is infallible from
        // the filter's view). A within-cap value stores normally.
        let mut s = state("f\u{1f}");
        KvHost::set(&mut s, "big".into(), vec![0u8; MAX_KV_VALUE_BYTES + 1]);
        assert_eq!(
            KvHost::get(&mut s, "big".into()),
            None,
            "an over-cap value is dropped"
        );
        KvHost::set(&mut s, "ok".into(), vec![0u8; 128]);
        assert_eq!(KvHost::get(&mut s, "ok".into()), Some(vec![0u8; 128]));
    }

    #[test]
    fn host_log_is_capped_and_sanitized() {
        // control bytes are neutralized (no CRLF log-line injection / ANSI), and
        // the per-request line count is bounded with a single truncation marker.
        let mut s = state("f\u{1f}");
        LogHost::log(&mut s, LogLevel::Info, "a\r\nInjected: x\u{1b}[31m".into());
        assert!(
            !s.logs[0].message.contains('\r') && !s.logs[0].message.contains('\n'),
            "CR/LF are neutralized (no log-line injection)"
        );
        assert!(
            !s.logs[0].message.contains('\u{1b}'),
            "the ANSI escape is neutralized"
        );

        // a long message is truncated to the byte cap.
        let mut s2 = state("f\u{1f}");
        LogHost::log(&mut s2, LogLevel::Info, "x".repeat(MAX_LOG_MSG_BYTES * 2));
        assert!(s2.logs[0].message.len() <= MAX_LOG_MSG_BYTES);

        // the per-request line count is bounded, last slot is a truncation marker.
        let mut s3 = state("f\u{1f}");
        for i in 0..(MAX_LOG_LINES_PER_REQUEST + 50) {
            LogHost::log(&mut s3, LogLevel::Info, format!("line {i}"));
        }
        assert_eq!(s3.logs.len(), MAX_LOG_LINES_PER_REQUEST);
        assert!(
            s3.logs.last().unwrap().message.contains("truncated"),
            "the final retained line is the truncation marker"
        );
    }
}
