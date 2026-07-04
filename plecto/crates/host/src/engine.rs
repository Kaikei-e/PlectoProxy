//! Engine construction (ADR 000006 metering) and the epoch ticker that drives deadlines.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig};

/// Pooling-engine per-kind slot budget (memories / tables / instances), shared by every
/// trusted filter's pool (ADR 000012). VA-reservation cost only (slots × `max_memory_size`).
const TRUSTED_POOL_SLOTS: usize = 256;

/// Hard ceiling on a trusted pool, matched to the pooling engine's per-kind slot budget so a
/// single filter cannot, by itself, demand more instances than the engine reserved.
pub(crate) const TRUSTED_POOL_MAX: usize = TRUSTED_POOL_SLOTS;

pub(crate) enum Allocation {
    Pooling,
    OnDemand,
}

pub(crate) fn build_engine(alloc: Allocation) -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    // Outbound HTTP (ADR 000036) lends the async `wasi:http` interfaces; enable the Component Model
    // async ABI so they link. Off by default; a non-async guest is unaffected by the flag.
    #[cfg(feature = "outbound-http")]
    config.wasm_component_model_async(true);
    // epoch interruption: the low-overhead deadline mechanism for the data plane (ADR 000006;
    // epoch over fuel — lighter, no determinism requirement here). A background ticker
    // advances the epoch; each Store sets a deadline before every guest call so a runaway
    // filter traps instead of hanging the worker (fail-closed).
    config.epoch_interruption(true);
    // M3 Stage 1 (ADR 000021): the host runs the guest on wasmtime fibers via `call_async` and
    // bridges it to its still-sync public API with `block_on` (the server-side spawn_blocking
    // removal is Stage 2). wasmtime 46 needs no `Config::async_support` toggle (it is deprecated /
    // a no-op) — the async path is selected by the bindgen `exports: async` config plus
    // `instantiate_async` / `call_async`. `memory_init_cow` stays at its default (enabled): every
    // instance gets its own copy-on-write heap image — safe against CVE-2022-39393 (ADR 000006).
    if let Allocation::Pooling = alloc {
        let mut pool = PoolingAllocationConfig::default();
        // Global per-kind slot budget for ALL trusted filters' pools combined (ADR 000012). The
        // pool reserves virtual address space up front (slots × max_memory_size), so this caps
        // VA reservation, not resident memory. `TRUSTED_POOL_MAX` bounds any single filter's
        // pool below this; the manifest registry (ADR 000007) will apportion the budget across
        // filters when the fast-path server lands. Exhaustion is a hard error (no internal
        // queue), surfaced as a fail-closed `RunError::Instantiate`.
        let slots = TRUSTED_POOL_SLOTS as u32;
        pool.total_memories(slots);
        pool.total_tables(slots);
        pool.total_core_instances(slots);
        pool.total_component_instances(slots);
        pool.max_memory_size(64 << 20); // 64 MiB per linear memory
        config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    }
    Ok(Engine::new(&config)?)
}

/// Granularity of the epoch ticker. Deadlines are expressed in milliseconds and converted
/// 1:1 to epoch increments, so the effective deadline resolution is one tick.
const EPOCH_TICK: Duration = Duration::from_millis(1);

/// Background thread that advances each engine's epoch counter so per-`Store` deadlines fire
/// (ADR 000006 metering). Without it `set_epoch_deadline` never trips. Stops and joins on
/// `Host` drop. One ticker per `Host`; it drives both engines (each has its own counter).
pub(crate) struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EpochTicker {
    pub(crate) fn spawn(engines: Vec<Engine>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(EPOCH_TICK);
                for e in &engines {
                    e.increment_epoch();
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
