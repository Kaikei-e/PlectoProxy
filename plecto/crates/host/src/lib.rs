//! plecto-host — embeds wasmtime to run `plecto:filter` components (ADR 000001 / 000002).
//!
//! v0.1.0 slice (ADR 000010): load a filter component, run its **sync, header-only**
//! hooks, and return the typed `decision`. The `Linker` is **deny-by-default** — it
//! lends ONLY the plecto host-API (log / clock / kv). No WASI, network, filesystem,
//! or sockets are reachable by a filter (ADR 000006). Pooling, epoch metering, OCI
//! signature verification, and the redb KV backend are deferred to ADR 000004 / 6 / 7.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "filter",
    });
}

// One canonical set of contract types for callers and tests.
pub use bindings::plecto::filter::host_log::Level as LogLevel;
pub use bindings::plecto::filter::types::{
    Header, HttpRequest, HttpResponse, RequestDecision, RequestEdit, ResponseDecision, ResponseEdit,
};
use bindings::{Filter, FilterPre};

/// A log line captured from the host-log capability (test visibility / future tracing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: LogLevel,
    pub message: String,
}

/// Shared, host-held state — the place filter state lives (filters are stateless, Fork 4).
/// In-memory now; the redb backend arrives with ADR 000004.
type Kv = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// Per-request host state. Holds the capability handles lent to a filter and the
/// request-scoped log buffer. Created fresh per request (Store-per-request, Fork 4).
pub struct HostState {
    kv: Kv,
    logs: Vec<LogLine>,
    now_ms: u64,
}

impl HostState {
    fn new(kv: Kv) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            kv,
            logs: Vec::new(),
            now_ms,
        }
    }
}

// --- host-API capability implementations (deny-by-default: only these are lent) ---

// `types` is a type-only interface (no functions); the generated `Host` trait is empty.
impl bindings::plecto::filter::types::Host for HostState {}

impl bindings::plecto::filter::host_log::Host for HostState {
    fn log(&mut self, level: LogLevel, message: String) {
        self.logs.push(LogLine { level, message });
    }
}

impl bindings::plecto::filter::host_clock::Host for HostState {
    fn now_ms(&mut self) -> u64 {
        self.now_ms
    }
}

impl bindings::plecto::filter::host_kv::Host for HostState {
    fn get(&mut self, key: String) -> Option<Vec<u8>> {
        self.kv
            .lock()
            .expect("kv mutex poisoned")
            .get(&key)
            .cloned()
    }

    fn set(&mut self, key: String, value: Vec<u8>) {
        self.kv
            .lock()
            .expect("kv mutex poisoned")
            .insert(key, value);
    }

    fn delete(&mut self, key: String) {
        self.kv.lock().expect("kv mutex poisoned").remove(&key);
    }
}

/// The wasmtime host: a shared `Engine` plus host-held state. One per process/worker.
pub struct Host {
    engine: Engine,
    kv: Kv,
}

impl Host {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        // Sync path: we deliberately do NOT enable async_support or
        // component-model-async on wasmtime 45 (ADR 000010).
        let engine = Engine::new(&config)?;
        Ok(Self {
            engine,
            kv: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Pre-instantiate a filter component: type-check and resolve imports up front
    /// (`InstancePre`, wasmtime-host skill / ADR 000004). The returned handle is
    /// reusable; per-worker pooling is deferred to ADR 000004.
    pub fn load(&self, component_bytes: &[u8]) -> Result<LoadedFilter> {
        let component = Component::from_binary(&self.engine, component_bytes)?;
        let mut linker = Linker::<HostState>::new(&self.engine);
        // deny-by-default: lend ONLY the plecto host-API. No WASI is added.
        Filter::add_to_linker::<_, wasmtime::component::HasSelf<HostState>>(
            &mut linker,
            |s: &mut HostState| s,
        )?;
        let pre = FilterPre::new(linker.instantiate_pre(&component)?)?;
        Ok(LoadedFilter {
            engine: self.engine.clone(),
            kv: self.kv.clone(),
            pre,
        })
    }
}

/// A pre-instantiated filter, ready to run per request.
pub struct LoadedFilter {
    engine: Engine,
    kv: Kv,
    pre: FilterPre<HostState>,
}

impl LoadedFilter {
    /// Run the request-side hook for one request. Returns the typed decision plus
    /// any log lines the filter emitted (captured via the host-log capability).
    pub fn on_request(&self, req: &HttpRequest) -> Result<(RequestDecision, Vec<LogLine>)> {
        let mut store = Store::new(&self.engine, HostState::new(self.kv.clone()));
        let filter = self.pre.instantiate(&mut store)?;
        filter.call_init(&mut store)?;
        let decision = filter.call_on_request(&mut store, req)?;
        let logs = std::mem::take(&mut store.data_mut().logs);
        Ok((decision, logs))
    }

    /// Run the response-side hook for one response.
    pub fn on_response(&self, resp: &HttpResponse) -> Result<(ResponseDecision, Vec<LogLine>)> {
        let mut store = Store::new(&self.engine, HostState::new(self.kv.clone()));
        let filter = self.pre.instantiate(&mut store)?;
        filter.call_init(&mut store)?;
        let decision = filter.call_on_response(&mut store, resp)?;
        let logs = std::mem::take(&mut store.data_mut().logs);
        Ok((decision, logs))
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the deny-by-default host-API implementations (ADR 000006).
    use super::*;
    use bindings::plecto::filter::host_clock::Host as ClockHost;
    use bindings::plecto::filter::host_kv::Host as KvHost;
    use bindings::plecto::filter::host_log::Host as LogHost;

    fn state() -> HostState {
        HostState::new(Arc::new(Mutex::new(HashMap::new())))
    }

    #[test]
    fn kv_get_set_delete_roundtrip() {
        let mut s = state();
        assert_eq!(KvHost::get(&mut s, "k".into()), None);
        KvHost::set(&mut s, "k".into(), b"v".to_vec());
        assert_eq!(KvHost::get(&mut s, "k".into()), Some(b"v".to_vec()));
        KvHost::delete(&mut s, "k".into());
        assert_eq!(KvHost::get(&mut s, "k".into()), None);
    }

    #[test]
    fn log_captures_lines() {
        let mut s = state();
        LogHost::log(&mut s, LogLevel::Info, "hello".into());
        assert_eq!(s.logs.len(), 1);
        assert_eq!(s.logs[0].message, "hello");
    }

    #[test]
    fn clock_returns_nonzero_wall_time() {
        let mut s = state();
        assert!(ClockHost::now_ms(&mut s) > 0);
    }
}
