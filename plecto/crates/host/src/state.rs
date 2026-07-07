//! Per-request host state ([`HostState`]) and the host-API capability implementations
//! (deny-by-default: only these are lent to a filter, ADR 000006 / 000011).

use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
use wasmtime::component::Linker;
use wasmtime::{StoreLimits, StoreLimitsBuilder};

use crate::LogLevel;
use crate::bindings;
use crate::bindings::plecto::filter::{
    host_clock, host_config, host_counter, host_kv, host_log, host_ratelimit,
};
#[cfg(feature = "outbound-http")]
use crate::outbound_http;
#[cfg(feature = "outbound-tcp")]
use crate::outbound_tcp;
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
    /// Outbound capabilities (ADR 000036 HTTP / ADR 000060 TCP): the minimal WASI base ctx and
    /// resource table shared by both wirings. The relevant interfaces are added to the Linker only
    /// for filters with an outbound policy; for every other filter the guards below deny every
    /// call (belt-and-suspenders, so a stray call still fails closed).
    #[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
    wasi: wasmtime_wasi::WasiCtx,
    #[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
    table: wasmtime::component::ResourceTable,
    /// Outbound HTTP (ADR 000036): the `wasi:http` ctx and the SSRF-guarded send hooks.
    #[cfg(feature = "outbound-http")]
    http_ctx: wasmtime_wasi_http::WasiHttpCtx,
    #[cfg(feature = "outbound-http")]
    hooks: outbound_http::PlectoHttpHooks,
    /// Outbound TCP (ADR 000060): the per-Store guard (pinned set + connect budget) shared by the
    /// `socket_addr_check` closure inside `wasi` above and the host's ip-name-lookup impl.
    #[cfg(feature = "outbound-tcp")]
    tcp: outbound_tcp::TcpGuard,
    /// This filter's manifest-declared business config (`[filter.config]`, ADR 000066) — a
    /// read-only string map the filter reads back via `host-config`. The host never interprets it.
    config: Arc<BTreeMap<String, String>>,
}

/// The always-present [`HostState::new`] fields, grouped so the constructor's argument count
/// stays under clippy's threshold without an `#[allow]` — the outbound capabilities stay
/// separate, cfg-gated trailing params, since they don't exist in every build.
pub(crate) struct HostStateInit {
    pub(crate) kv: Arc<dyn KvBackend>,
    pub(crate) kv_prefix: String,
    pub(crate) max_memory_bytes: u64,
    pub(crate) ratelimit_bucket: Option<Bucket>,
    pub(crate) quota: Arc<KvQuota>,
    pub(crate) config: Arc<BTreeMap<String, String>>,
}

impl HostState {
    pub(crate) fn new(
        init: HostStateInit,
        #[cfg(feature = "outbound-http")] hooks: outbound_http::PlectoHttpHooks,
        #[cfg(feature = "outbound-tcp")] tcp: outbound_tcp::TcpGuard,
    ) -> Self {
        let HostStateInit {
            kv,
            kv_prefix,
            max_memory_bytes,
            ratelimit_bucket,
            quota,
            config,
        } = init;
        // The base WasiCtx: empty (no fs, no env, no preopens). With outbound TCP the guard
        // installs its socket_addr_check; otherwise the builder's deny-all socket default stands.
        #[cfg(feature = "outbound-tcp")]
        let wasi = tcp.wasi_ctx();
        #[cfg(all(feature = "outbound-http", not(feature = "outbound-tcp")))]
        let wasi = wasmtime_wasi::WasiCtxBuilder::new().build();
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
            config,
            #[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
            wasi,
            #[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
            table: wasmtime::component::ResourceTable::new(),
            #[cfg(feature = "outbound-http")]
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            #[cfg(feature = "outbound-http")]
            hooks,
            #[cfg(feature = "outbound-tcp")]
            tcp,
        }
    }

    /// Reset per-request state for a reused (trusted) instance. Clears the log buffer and
    /// re-snapshots the clock; the WASM instance's linear memory (init-derived) is untouched.
    pub(crate) fn begin_request(&mut self) {
        self.logs.clear();
        self.now_ms = wall_now_ms();
        // A reused instance's connect budget belongs to the request, not the instance.
        #[cfg(feature = "outbound-tcp")]
        self.tcp.begin_request();
    }

    /// The host's vetted `wasi:sockets/ip-name-lookup` view (ADR 000060): the Store's resource
    /// table plus this filter's guard.
    #[cfg(feature = "outbound-tcp")]
    pub(crate) fn tcp_lookup(&mut self) -> outbound_tcp::TcpLookupView<'_> {
        outbound_tcp::TcpLookupView {
            table: &mut self.table,
            guard: &self.tcp,
        }
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

// Outbound capabilities (ADR 000036 / 000060): the WASI base + per-capability projections, added
// to the Linker only for filters with an outbound policy. They share one resource table.
#[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
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
/// those capabilities stay denied (security audit F-002; mirrors the streaming path). Outbound TCP
/// filters (ADR 000060) get their `wasi:sockets` slice added separately, behind their own guard.
#[cfg(any(feature = "outbound-http", feature = "outbound-tcp"))]
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
        let stripe_key = nskey.clone();
        let kv_read = self.kv.clone();
        let kv_write = self.kv.clone();
        let read_nskey = nskey.clone();
        let key_len = key.len();
        let value_len = value.len();
        // Charge the byte delta vs. any existing value (a new key also charges its key bytes + 1
        // entry), then write — atomically with the quota decision (`KvQuota::charge_and_apply`),
        // so a concurrent `set` on the same key (the pool runs many instances of one filter at
        // once) cannot race the read-old-value step against this call's write.
        self.quota.charge_and_apply(
            &self.kv_prefix,
            &stripe_key,
            move || match kv_read.get(&read_nskey).map(|v| v.len()) {
                None => (1isize, (key_len + value_len) as isize),
                Some(old) => (0isize, value_len as isize - old as isize),
            },
            move || kv_write.set(&nskey, value),
        );
    }
    fn delete(&mut self, key: String) {
        let nskey = self.ns_key(TAG_KV, &key);
        let stripe_key = nskey.clone();
        let kv_read = self.kv.clone();
        let kv_write = self.kv.clone();
        let read_nskey = nskey.clone();
        let key_len = key.len();
        // Read-then-release must be atomic with the quota decision too: two concurrent deletes
        // of the same key must not both observe `Some(old)` and both release — the second must
        // see the first's delete has already happened and release nothing (see
        // `KvQuota::charge_and_apply` doc for the race this closes).
        self.quota.charge_and_apply(
            &self.kv_prefix,
            &stripe_key,
            move || match kv_read.get(&read_nskey).map(|v| v.len()) {
                Some(old) => (-1isize, -((key_len + old) as isize)),
                None => (0isize, 0isize),
            },
            move || kv_write.delete(&nskey),
        );
    }
}

impl host_counter::Host for HostState {
    fn increment(&mut self, key: String, delta: i64) -> i64 {
        let nskey = self.ns_key(TAG_COUNTER, &key);
        // A zero delta is a pure read (host-counter.get); it neither creates a key nor is charged.
        if delta == 0 {
            return self.kv.increment(&nskey, 0);
        }
        let stripe_key = nskey.clone();
        let kv_read = self.kv.clone();
        let kv_write = self.kv.clone();
        let read_nskey = nskey.clone();
        let key_len = key.len();
        // A counter is a fixed 8-byte value: only a NEW key grows the store, so charge one entry
        // when first created and fail closed (report the current value, do not create) over quota
        // — atomically with the increment itself (`KvQuota::charge_and_apply`), so two concurrent
        // first-writes to the same new key cannot both observe "absent" and both charge an entry
        // for what ends up being one logical key (the pool runs many concurrent instances of the
        // same filter, all sharing this backend + quota).
        self.quota
            .charge_and_apply(
                &self.kv_prefix,
                &stripe_key,
                move || {
                    if kv_read.get(&read_nskey).is_none() {
                        (1isize, (key_len + 8) as isize)
                    } else {
                        (0isize, 0isize)
                    }
                },
                move || kv_write.increment(&nskey, delta),
            )
            .unwrap_or(0)
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
        let stripe_key = nskey.clone();
        let kv_read = self.kv.clone();
        let kv_write = self.kv.clone();
        let read_nskey = nskey.clone();
        let key_len = key.len();
        let now_ms = self.now_ms;
        // A bucket is a fixed 16-byte value: charge one entry when first created. Over quota a
        // filter cannot mint unbounded distinct-key buckets — deny (fail-closed), do not create.
        // Reserving the entry and acquiring the bucket happen atomically
        // (`KvQuota::charge_and_apply`), so two concurrent first-acquires on the same new key
        // cannot both observe "absent" and both charge an entry for one logical bucket.
        let result = self.quota.charge_and_apply(
            &self.kv_prefix,
            &stripe_key,
            move || {
                if kv_read.get(&read_nskey).is_none() {
                    (1isize, (key_len + 16) as isize)
                } else {
                    (0isize, 0isize)
                }
            },
            move || kv_write.try_acquire(&nskey, cost, spec, now_ms),
        );
        match result {
            Some(r) => host_ratelimit::Acquire {
                allowed: r.allowed,
                remaining: r.remaining,
                retry_after_ms: r.retry_after_ms,
            },
            None => host_ratelimit::Acquire {
                allowed: false,
                remaining: 0,
                retry_after_ms: 0,
            },
        }
    }
}

impl host_config::Host for HostState {
    fn get(&mut self, key: String) -> Option<String> {
        self.config.get(&key).cloned()
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

    /// A [`HostStateInit`] with test-friendly defaults: a fresh in-memory backend, no rate-limit
    /// bucket, a fresh quota, and no business config. Callers override individual fields.
    fn init_for(prefix: &str) -> HostStateInit {
        HostStateInit {
            kv: Arc::new(MemoryBackend::default()),
            kv_prefix: prefix.to_string(),
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            ratelimit_bucket: None,
            quota: Arc::new(KvQuota::new()),
            config: Arc::new(BTreeMap::new()),
        }
    }

    fn state(prefix: &str) -> HostState {
        HostState::new(
            init_for(prefix),
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
            #[cfg(feature = "outbound-tcp")]
            crate::outbound_tcp::TcpGuard::deny_all(),
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
            HostStateInit {
                kv: shared.clone(),
                ..init_for("filter-a\u{1f}")
            },
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
            #[cfg(feature = "outbound-tcp")]
            crate::outbound_tcp::TcpGuard::deny_all(),
        );
        let mut b = HostState::new(
            HostStateInit {
                kv: shared.clone(),
                ..init_for("filter-b\u{1f}")
            },
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
            #[cfg(feature = "outbound-tcp")]
            crate::outbound_tcp::TcpGuard::deny_all(),
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
            HostStateInit {
                kv: shared.clone(),
                ..init_for("filter-a\u{1f}")
            },
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
            #[cfg(feature = "outbound-tcp")]
            crate::outbound_tcp::TcpGuard::deny_all(),
        );
        let mut b = HostState::new(
            HostStateInit {
                kv: shared.clone(),
                ..init_for("filter-b\u{1f}")
            },
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
            #[cfg(feature = "outbound-tcp")]
            crate::outbound_tcp::TcpGuard::deny_all(),
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
            HostStateInit {
                kv: shared.clone(),
                ratelimit_bucket: Some(one_token_no_refill()),
                ..init_for("filter-a\u{1f}")
            },
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
            #[cfg(feature = "outbound-tcp")]
            crate::outbound_tcp::TcpGuard::deny_all(),
        );
        let mut b = HostState::new(
            HostStateInit {
                kv: shared.clone(),
                ratelimit_bucket: Some(one_token_no_refill()),
                ..init_for("filter-b\u{1f}")
            },
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
            #[cfg(feature = "outbound-tcp")]
            crate::outbound_tcp::TcpGuard::deny_all(),
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
    fn concurrent_delete_of_the_same_key_releases_quota_exactly_once() {
        // Regression test: the trusted pool runs many concurrent instances of one filter, all
        // sharing one backend + one KvQuota. A get-then-delete-then-release sequence done as
        // three independent lock acquisitions lets N concurrent deletes of the SAME key all
        // observe `Some(old)` and all release budget for a key that only existed once —
        // permanently under-counting real usage. `KvQuota::charge_and_apply` closes this by
        // making the whole read-decide-release sequence one atomic unit per call.
        use std::sync::Barrier;
        use std::thread;

        let shared: Arc<dyn KvBackend> = Arc::new(MemoryBackend::default());
        let quota = Arc::new(KvQuota::new());
        let prefix = "f\u{1f}".to_string();

        let mut seed = HostState::new(
            HostStateInit {
                kv: shared.clone(),
                quota: quota.clone(),
                ..init_for(&prefix)
            },
            #[cfg(feature = "outbound-http")]
            outbound_http::PlectoHttpHooks::deny_all(),
            #[cfg(feature = "outbound-tcp")]
            crate::outbound_tcp::TcpGuard::deny_all(),
        );
        // Two distinct keys in the same namespace: "k1" (raced on below) and "k2" (untouched —
        // its surviving budget is the tell-tale that a double-release didn't over-free the ns).
        KvHost::set(&mut seed, "k1".into(), vec![0u8; 100]);
        KvHost::set(&mut seed, "k2".into(), vec![0u8; 100]);
        assert_eq!(
            quota.usage_for_test(&prefix),
            (2, 2 * (2 + 100)),
            "both keys charged once each"
        );

        const RACERS: usize = 8;
        let barrier = Arc::new(Barrier::new(RACERS));
        let handles: Vec<_> = (0..RACERS)
            .map(|_| {
                let kv = shared.clone();
                let q = quota.clone();
                let p = prefix.clone();
                let b = barrier.clone();
                thread::spawn(move || {
                    let mut s = HostState::new(
                        HostStateInit {
                            kv,
                            quota: q,
                            ..init_for(&p)
                        },
                        #[cfg(feature = "outbound-http")]
                        outbound_http::PlectoHttpHooks::deny_all(),
                        #[cfg(feature = "outbound-tcp")]
                        crate::outbound_tcp::TcpGuard::deny_all(),
                    );
                    b.wait();
                    KvHost::delete(&mut s, "k1".into());
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        // Exactly one release happened for "k1"; "k2" is untouched — the namespace must show
        // precisely k2's own accounting, not zeroed-out-by-clamping evidence of over-release.
        assert_eq!(
            quota.usage_for_test(&prefix),
            (1, 2 + 100),
            "k1's single release must not consume k2's untouched budget"
        );
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
