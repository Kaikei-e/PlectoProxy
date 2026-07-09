//! The trusted instance pool (ADR 000012) and the lifecycle-dispatch logic layered on it: one
//! cohesive "pool lifecycle" responsibility, from checkout through recycling and the pool-wide
//! trap circuit-breaker.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

#[cfg(test)]
use crate::NoopSink;
use crate::runtime::FilterRuntime;
use crate::util::wall_now_ms;
use crate::{Isolation, LogLine, RunError, TelemetrySink};
#[cfg(test)]
use anyhow::Result;

/// The result of one guest hook call: on `Err`, the `RunError` is paired with whatever host-log
/// lines (including fat-guest stdio, ADR 000063) were recovered from the failed instance before
/// it was discarded — a trap's own diagnostic output would otherwise be lost along with it.
pub(crate) type HookResult<T> = std::result::Result<(T, Vec<LogLine>), (RunError, Vec<LogLine>)>;

/// Shared, isolation-independent load result. Generic over the `FilterRuntime` seam so the pool /
/// lifecycle-dispatch logic here is unit-testable against a fake runtime — production always
/// resolves `R = WasmtimeRuntime` (`LoadedFilter` is a concrete, non-generic struct built that way).
pub(crate) struct LoadedInner<R: FilterRuntime> {
    pub(crate) runtime: R,
    /// The filter id (span name + telemetry attribute, ADR 000009).
    pub(crate) filter_id: String,
    /// Where this filter's per-execution spans go (cloned from the `Host` at load).
    pub(crate) sink: Arc<dyn TelemetrySink>,
    pub(crate) isolation: Isolation,
    /// Circuit breaker for the untrusted lifecycle (`run_fresh`), mirroring `TrustedPool`'s
    /// pool-wide breaker but scoped to this filter: every untrusted call pays a fresh
    /// `instantiate_initialized()` (init under the generous `init_deadline_ms`, not the tight
    /// per-request one) with no pool to amortize across. Without this, a filter whose `init`
    /// deterministically traps forces the host to re-pay that full init budget on every single
    /// incoming request forever — bounded per-call by the epoch deadline, but with zero backoff
    /// across calls, exactly the repeated-cost DoS shape the trusted pool's breaker exists to stop.
    untrusted_breaker: Mutex<TrapBreaker>,
}

/// Consecutive traps before a circuit breaker opens a cooldown; shared shape for both the
/// pool-wide trusted breaker and the per-filter untrusted breaker.
#[derive(Default)]
struct TrapBreaker {
    consecutive_traps: u32,
    cooldown_until_ms: u64,
}

impl TrapBreaker {
    fn record_trap(&mut self) {
        self.consecutive_traps = self.consecutive_traps.saturating_add(1);
        if self.consecutive_traps >= UNTRUSTED_TRAP_BREAKER_THRESHOLD {
            self.cooldown_until_ms = wall_now_ms().saturating_add(UNTRUSTED_TRAP_COOLDOWN_MS);
        }
    }

    fn clear(&mut self) {
        self.consecutive_traps = 0;
        self.cooldown_until_ms = 0;
    }
}

/// Consecutive untrusted-lifecycle failures (instantiate/init OR the call itself trapping)
/// before the per-filter breaker opens a cooldown. Same threshold as the trusted pool's breaker
/// — a handful of traps still self-heal (the next call tries fresh); only a deterministically
/// broken filter reaches it.
const UNTRUSTED_TRAP_BREAKER_THRESHOLD: u32 = 3;
/// How long the untrusted breaker stays open once tripped: during it, calls fail closed cheaply
/// (`RunError::Unavailable`) without paying `instantiate_initialized()` at all.
const UNTRUSTED_TRAP_COOLDOWN_MS: u64 = 500;

/// Releases a reserved/held `live` pool slot on unwind. Armed while a checked-out instance (or a
/// reserved build slot) is outside the pool's bookkeeping; the normal return paths disarm it and
/// do their own accounting. Without this, a panic inside a guest call (a host-function bug, a
/// wasmtime-internal panic) would drop the instance without decrementing `live` or waking a
/// waiter — repeated panics would permanently shrink the pool's effective capacity.
struct LiveSlotGuard<'a, I> {
    pool: &'a TrustedPool<I>,
    armed: bool,
}

impl<I> Drop for LiveSlotGuard<'_, I> {
    fn drop(&mut self) {
        if self.armed {
            {
                let mut g = self.pool.inner.lock();
                g.live = g.live.saturating_sub(1);
            }
            self.pool.available.notify_one();
        }
    }
}

impl<R: FilterRuntime> LoadedInner<R> {
    /// Build a `LoadedInner` with a fresh (untripped) untrusted-lifecycle breaker.
    pub(crate) fn new(
        runtime: R,
        filter_id: String,
        sink: Arc<dyn TelemetrySink>,
        isolation: Isolation,
    ) -> Self {
        Self {
            runtime,
            filter_id,
            sink,
            isolation,
            untrusted_breaker: Mutex::new(TrapBreaker::default()),
        }
    }

    /// Check out a trusted instance from the pool (ADR 000012): reuse an idle one, lazily build
    /// a fresh one while under `cap`, or — when every instance is checked out — wait up to the
    /// pool's `checkout_timeout` for one to free and then fail **closed** (`Unavailable`).
    /// Also fails closed fast while the pool-wide breaker's cooldown is open. wasmtime's pooling
    /// allocator has no internal wait queue, so this bounded wait is the host-side backpressure
    /// its docs call for.
    fn checkout(
        &self,
        pool: &TrustedPool<R::Instance>,
    ) -> std::result::Result<PooledInstance<R::Instance>, RunError> {
        // The decision made under the lock; acted on (build / return) after releasing it.
        enum Step<I> {
            Use(PooledInstance<I>),
            Build,
            Retry,
        }
        loop {
            let step = {
                let mut g = pool.inner.lock();
                if wall_now_ms() < g.cooldown_until_ms {
                    return Err(RunError::Unavailable);
                }
                if let Some(p) = g.idle.pop() {
                    Step::Use(p)
                } else if g.live < pool.cap {
                    g.live += 1; // reserve the slot before the (slow) build, done outside the lock
                    Step::Build
                } else if pool
                    .available
                    .wait_for(&mut g, pool.checkout_timeout)
                    .timed_out()
                {
                    // saturated and nothing freed in time → shed load, fail closed.
                    return Err(RunError::Unavailable);
                } else {
                    Step::Retry
                }
            };
            match step {
                Step::Use(p) => return Ok(p),
                Step::Build => {
                    // The guard rolls back the reserved slot (and wakes a waiter that may now
                    // build) on error OR unwind; success disarms it — the caller now owns the slot.
                    let mut guard = LiveSlotGuard { pool, armed: true };
                    match self.runtime.instantiate_initialized() {
                        Ok(instance) => {
                            guard.armed = false;
                            return Ok(PooledInstance {
                                instance,
                                served: 0,
                            });
                        }
                        Err(e) => return Err(RunError::Instantiate(e)),
                    }
                }
                Step::Retry => continue,
            }
        }
    }

    /// The untrusted lifecycle: instantiate fresh + init, run one call, map errors, take logs.
    /// Exactly mirrors `run_pooled`'s post-call bookkeeping — the only difference between the two
    /// lifecycles is whether the instance comes from the pool or is built fresh right here.
    /// Guarded by a per-filter breaker (`untrusted_breaker`): a deterministically failing filter
    /// fails closed cheaply during its cooldown instead of re-paying `instantiate_initialized()`
    /// (the generous init budget) on every single incoming request.
    fn run_fresh<T>(
        &self,
        call: impl FnOnce(&mut R::Instance) -> wasmtime::Result<T>,
    ) -> HookResult<T> {
        if wall_now_ms() < self.untrusted_breaker.lock().cooldown_until_ms {
            return Err((RunError::Unavailable, Vec::new()));
        }
        let mut inst = match self.runtime.instantiate_initialized() {
            Ok(inst) => inst,
            Err(e) => {
                self.untrusted_breaker.lock().record_trap();
                // Nothing instantiated → no Store, no logs to recover.
                return Err((RunError::Instantiate(e), Vec::new()));
            }
        };
        self.runtime.set_request_deadline(&mut inst);
        match call(&mut inst) {
            Ok(value) => {
                let logs = self.runtime.take_logs(&mut inst);
                self.untrusted_breaker.lock().clear();
                Ok((value, logs))
            }
            Err(e) => {
                // The instance is discarded either way (its linear memory is undefined after a
                // trap), so recover whatever host-log/stdio output it produced before failing —
                // ADR 000063's whole point is that a trapping guest's own diagnostic output
                // (e.g. a TinyGo panic on stderr) still reaches the span this request emits.
                let logs = self.runtime.take_logs_after_trap(&mut inst);
                self.untrusted_breaker.lock().record_trap();
                Err((RunError::from_call(e), logs))
            }
        }
    }

    /// Run one request through the trusted pool (ADR 000012): check out an instance, run `call`
    /// under the per-request deadline, then check it back in — returning it to `idle`, recycling
    /// it once it has served `max_requests_per_instance` (so init re-runs, bounding linear-memory
    /// state accumulation, §6.6), or discarding it on a trap. The circuit breaker is **pool-wide**
    /// (review f000003 #5, generalised): a deterministically-trapping filter trips the whole pool
    /// once rather than forcing every instance to the threshold independently. A trapped
    /// instance's memory is undefined, so the discard is per-instance.
    fn run_pooled<T>(
        &self,
        pool: &TrustedPool<R::Instance>,
        call: impl FnOnce(&mut R::Instance) -> wasmtime::Result<T>,
    ) -> HookResult<T> {
        // Nothing instantiated yet on a checkout failure → no Store, no logs to recover.
        let mut pooled = self.checkout(pool).map_err(|e| (e, Vec::new()))?;
        // Armed across the guest call: a panic unwinding out of `call` must still release the
        // `live` slot and wake a waiter. Both normal arms below disarm and do their own
        // bookkeeping (return-to-idle / recycle / discard).
        let mut slot = LiveSlotGuard { pool, armed: true };

        self.runtime.begin_request(&mut pooled.instance);
        self.runtime.set_request_deadline(&mut pooled.instance);
        let result = call(&mut pooled.instance);

        match result {
            Ok(value) => {
                let logs = self.runtime.take_logs(&mut pooled.instance);
                slot.armed = false;
                pooled.served = pooled.served.saturating_add(1);
                if pooled.served >= pool.max_requests_per_instance {
                    // Recycle: drop the Store (returning the slot + freeing memory) BEFORE the
                    // logical `live` decrement, so the physical instance count never transiently
                    // exceeds `cap`. The next checkout lazily rebuilds (re-init).
                    drop(pooled);
                    let mut g = pool.inner.lock();
                    g.clear_breaker();
                    g.live = g.live.saturating_sub(1);
                } else {
                    let mut g = pool.inner.lock();
                    g.clear_breaker();
                    g.idle.push(pooled);
                }
                pool.available.notify_one();
                Ok((value, logs))
            }
            Err(e) => {
                // Trap → this instance's linear memory is undefined → discard it. Recover its
                // logs (including any unterminated stdio partial line, ADR 000063) BEFORE the
                // discard, then bump the pool-wide breaker; past the threshold open a short
                // cooldown so a deterministically-trapping filter fails closed cheaply.
                let logs = self.runtime.take_logs_after_trap(&mut pooled.instance);
                slot.armed = false;
                drop(pooled);
                let mut g = pool.inner.lock();
                g.live = g.live.saturating_sub(1);
                g.consecutive_traps = g.consecutive_traps.saturating_add(1);
                if g.consecutive_traps >= TRUSTED_TRAP_BREAKER_THRESHOLD {
                    g.cooldown_until_ms = wall_now_ms().saturating_add(TRUSTED_TRAP_COOLDOWN_MS);
                }
                drop(g);
                pool.available.notify_one();
                Err((RunError::from_call(e), logs))
            }
        }
    }

    /// Shared executor for on_request / on_request_body / on_response: the ONLY difference
    /// between the three call sites is `call` itself. `trusted: Option<&TrustedPool<_>>` IS the
    /// lifecycle decision already (an `Option`, exhaustively matched below) — no separate
    /// `Lifecycle` enum is introduced, since that would just restate the `Option` with no new
    /// information.
    /// The Err side carries the logs recovered from the failed instance alongside the
    /// `RunError` (ADR 000063): a trap's own diagnostic output (e.g. a guest panic) is otherwise
    /// lost the moment the instance is discarded. `filter.rs` feeds these into the span it emits
    /// for the failing call, then drops them from the public `Result` it returns (unchanged
    /// contract: a `RunError` alone).
    pub(crate) fn run_hook<T>(
        &self,
        trusted: Option<&TrustedPool<R::Instance>>,
        call: impl FnOnce(&mut R::Instance) -> wasmtime::Result<T>,
    ) -> HookResult<T> {
        match trusted {
            Some(pool) => self.run_pooled(pool, call),
            None => self.run_fresh(call),
        }
    }
}

/// Consecutive trusted-pool traps before the circuit-breaker opens a cooldown (review f000003
/// #5, now pool-wide — ADR 000012). The first few traps still self-heal (a fresh instance is
/// built on the next checkout); only a deterministically-trapping filter reaches the threshold.
const TRUSTED_TRAP_BREAKER_THRESHOLD: u32 = 3;
/// How long the breaker stays open once tripped: during it, trusted checkouts fail closed
/// cheaply (`RunError::Unavailable`) without rebuilding. After it, the next checkout retries.
const TRUSTED_TRAP_COOLDOWN_MS: u64 = 500;

/// An instance in the trusted pool, plus how many requests it has served since it was last
/// (re)initialized — the counter that drives recycling (ADR 000012 / §6.6). Generic over the
/// `FilterRuntime::Instance` type so the pool is testable against a fake instance.
pub(crate) struct PooledInstance<I> {
    instance: I,
    served: u64,
}

/// The trusted pool's mutable interior, behind one lock (ADR 000012). `idle` holds warm
/// instances ready to check out; `live` counts every instance that currently exists (idle +
/// checked-out + being-built), bounding lazy fill to the pool `cap`. The circuit breaker is
/// **pool-wide**: a deterministically-trapping filter trips the whole pool once, not each
/// instance independently.
struct PoolInner<I> {
    idle: Vec<PooledInstance<I>>,
    live: usize,
    consecutive_traps: u32,
    cooldown_until_ms: u64,
}

impl<I> PoolInner<I> {
    /// Clear the breaker after a successful call (a healthy request resets the trap streak).
    fn clear_breaker(&mut self) {
        self.consecutive_traps = 0;
        self.cooldown_until_ms = 0;
    }
}

/// A fixed-capacity pool of reusable trusted instances (ADR 000012). Replaces the v0.1
/// single-instance-behind-one-`Mutex` placeholder (concurrency=1). Checkout reuses an idle
/// instance, lazily builds one while under `cap`, or waits up to `checkout_timeout` then fails
/// closed; `available` is signalled whenever an instance is returned or a slot is freed.
pub(crate) struct TrustedPool<I> {
    inner: Mutex<PoolInner<I>>,
    available: Condvar,
    cap: usize,
    checkout_timeout: Duration,
    max_requests_per_instance: u64,
}

impl<I> TrustedPool<I> {
    /// Build a pool seeded with one eager, already-initialized instance (so a single-threaded
    /// caller reuses it and `init` stays once). `cap` is the caller's clamped pool size.
    pub(crate) fn new(
        cap: usize,
        checkout_timeout: Duration,
        max_requests_per_instance: u64,
        first: I,
    ) -> Self {
        Self {
            inner: Mutex::new(PoolInner {
                idle: vec![PooledInstance {
                    instance: first,
                    served: 0,
                }],
                live: 1,
                consecutive_traps: 0,
                cooldown_until_ms: 0,
            }),
            available: Condvar::new(),
            cap,
            checkout_timeout,
            max_requests_per_instance,
        }
    }
}

/// Unit tests for the pool / lifecycle-dispatch DECISION logic in `LoadedInner`/`TrustedPool`
/// against a `FakeRuntime` — no wasmtime engine, component, or Store involved at all. Exercises
/// checkout/recycle/circuit-breaker/lifecycle semantics that `tests/pool.rs`'s real-component
/// tests also cover, but with precise, deterministic control over instantiation counts and
/// failures that a real wasm component cannot give.
#[cfg(test)]
mod pool_tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use super::*;

    struct FakeInstance {
        id: u64,
    }

    /// A `FilterRuntime` with no wasmtime engine at all: `instantiate_initialized` just hands out
    /// an incrementing id, so tests can assert exactly how many times an instance was (re)built.
    /// `take_logs`/`take_logs_after_trap` return distinct marker lines (not just `Vec::new()`) so
    /// a test can assert WHICH ONE `run_fresh`/`run_pooled` actually called, not merely that some
    /// `Vec<LogLine>` came back.
    struct FakeRuntime {
        next_id: AtomicU64,
        instantiate_calls: AtomicUsize,
    }

    impl FakeRuntime {
        fn new() -> Self {
            Self {
                next_id: AtomicU64::new(0),
                instantiate_calls: AtomicUsize::new(0),
            }
        }

        fn instantiate_calls(&self) -> usize {
            self.instantiate_calls.load(Ordering::SeqCst)
        }
    }

    fn marker_log(message: &str) -> Vec<LogLine> {
        vec![LogLine {
            level: crate::LogLevel::Debug,
            message: message.to_string(),
        }]
    }

    impl FilterRuntime for FakeRuntime {
        type Instance = FakeInstance;

        fn instantiate_initialized(&self) -> Result<FakeInstance> {
            self.instantiate_calls.fetch_add(1, Ordering::SeqCst);
            Ok(FakeInstance {
                id: self.next_id.fetch_add(1, Ordering::SeqCst),
            })
        }

        fn begin_request(&self, _instance: &mut FakeInstance) {}
        fn set_request_deadline(&self, _instance: &mut FakeInstance) {}
        fn take_logs(&self, _instance: &mut FakeInstance) -> Vec<LogLine> {
            marker_log("take_logs")
        }
        fn take_logs_after_trap(&self, _instance: &mut FakeInstance) -> Vec<LogLine> {
            marker_log("take_logs_after_trap")
        }
    }

    fn fake_inner(runtime: FakeRuntime) -> LoadedInner<FakeRuntime> {
        LoadedInner::new(
            runtime,
            "test".to_string(),
            Arc::new(NoopSink),
            Isolation::Trusted,
        )
    }

    #[test]
    fn trusted_pool_lazily_fills_and_reuses_idle_instance() {
        let runtime = FakeRuntime::new();
        let first = runtime.instantiate_initialized().unwrap();
        let pool = TrustedPool::new(2, Duration::from_millis(50), 1000, first);
        let inner = fake_inner(runtime);

        let a = inner
            .run_hook(Some(&pool), |inst: &mut FakeInstance| {
                Ok::<_, wasmtime::Error>(inst.id)
            })
            .unwrap();
        let b = inner
            .run_hook(Some(&pool), |inst: &mut FakeInstance| {
                Ok::<_, wasmtime::Error>(inst.id)
            })
            .unwrap();

        assert_eq!(
            a.0, b.0,
            "a single-threaded caller reuses the same instance"
        );
        assert_eq!(
            inner.runtime.instantiate_calls(),
            1,
            "only the eager initial build — no rebuild needed to reuse an idle instance"
        );
    }

    #[test]
    fn trusted_pool_checkout_waits_then_fails_closed_when_saturated() {
        let runtime = FakeRuntime::new();
        let first = runtime.instantiate_initialized().unwrap();
        let pool = TrustedPool::new(1, Duration::from_millis(20), 1000, first);
        let inner = fake_inner(runtime);

        let _held = inner.checkout(&pool).expect("first checkout succeeds");
        let failed_closed = matches!(inner.checkout(&pool), Err(RunError::Unavailable));

        assert!(
            failed_closed,
            "a saturated pool should time out and fail closed"
        );
    }

    #[test]
    fn trusted_pool_recycles_after_max_requests_per_instance() {
        let runtime = FakeRuntime::new();
        let first = runtime.instantiate_initialized().unwrap();
        let pool = TrustedPool::new(4, Duration::from_millis(50), 2, first);
        let inner = fake_inner(runtime);

        for _ in 0..2 {
            inner
                .run_hook(Some(&pool), |inst: &mut FakeInstance| {
                    Ok::<_, wasmtime::Error>(inst.id)
                })
                .unwrap();
        }
        // the 2nd call above hit max_requests_per_instance and recycled the instance, so this
        // 3rd call must rebuild — one more instantiate than the initial eager build.
        inner
            .run_hook(Some(&pool), |inst: &mut FakeInstance| {
                Ok::<_, wasmtime::Error>(inst.id)
            })
            .unwrap();

        assert_eq!(
            inner.runtime.instantiate_calls(),
            2,
            "instance recycles (rebuilds) after serving max_requests_per_instance"
        );
    }

    #[test]
    fn trusted_pool_opens_circuit_breaker_after_consecutive_traps_and_cools_down_then_self_heals() {
        let runtime = FakeRuntime::new();
        let first = runtime.instantiate_initialized().unwrap();
        let pool = TrustedPool::new(4, Duration::from_millis(50), 1000, first);
        let inner = fake_inner(runtime);

        for _ in 0..TRUSTED_TRAP_BREAKER_THRESHOLD {
            let result = inner.run_hook(Some(&pool), |_inst: &mut FakeInstance| {
                wasmtime::Result::<()>::Err(wasmtime::Error::msg("simulated trap"))
            });
            assert!(
                matches!(result, Err((RunError::Trap(_), _))),
                "expected a Trap before the breaker opens, got {result:?}"
            );
        }

        // breaker open: `call` must not even be invoked while the cooldown is in effect.
        let result = inner.run_hook(
            Some(&pool),
            |_inst: &mut FakeInstance| -> wasmtime::Result<()> {
                panic!("must not be called while the circuit breaker is open")
            },
        );
        assert!(
            matches!(result, Err((RunError::Unavailable, _))),
            "the pool-wide breaker should be open"
        );

        std::thread::sleep(Duration::from_millis(TRUSTED_TRAP_COOLDOWN_MS + 20));
        let result = inner.run_hook(Some(&pool), |inst: &mut FakeInstance| {
            Ok::<_, wasmtime::Error>(inst.id)
        });
        assert!(
            result.is_ok(),
            "the pool should self-heal (rebuild) once the cooldown elapses, got {result:?}"
        );
    }

    #[test]
    fn untrusted_lifecycle_instantiates_fresh_every_call() {
        let runtime = FakeRuntime::new();
        let inner = fake_inner(runtime);

        for _ in 0..3 {
            inner
                .run_hook(None, |inst: &mut FakeInstance| {
                    Ok::<_, wasmtime::Error>(inst.id)
                })
                .unwrap();
        }

        assert_eq!(
            inner.runtime.instantiate_calls(),
            3,
            "the untrusted lifecycle instantiates fresh + re-inits on every single call"
        );
    }

    #[test]
    fn untrusted_lifecycle_opens_circuit_breaker_after_consecutive_traps_and_cools_down_then_self_heals()
     {
        // Regression test: without a breaker, a filter whose call deterministically traps would
        // force `run_fresh` to re-pay `instantiate_initialized()` (the generous init budget) on
        // every single request forever. The per-filter breaker must fail closed cheaply instead,
        // without even attempting to instantiate, once tripped — mirroring the trusted pool's
        // pool-wide breaker test above.
        let runtime = FakeRuntime::new();
        let inner = fake_inner(runtime);

        for _ in 0..UNTRUSTED_TRAP_BREAKER_THRESHOLD {
            let result = inner.run_hook(None, |_inst: &mut FakeInstance| {
                wasmtime::Result::<()>::Err(wasmtime::Error::msg("simulated trap"))
            });
            assert!(
                matches!(result, Err((RunError::Trap(_), _))),
                "expected a Trap before the breaker opens, got {result:?}"
            );
        }

        let calls_before_open = inner.runtime.instantiate_calls();
        let result = inner.run_hook(None, |_inst: &mut FakeInstance| -> wasmtime::Result<()> {
            panic!("must not be called while the untrusted breaker is open")
        });
        assert!(
            matches!(result, Err((RunError::Unavailable, _))),
            "the per-filter untrusted breaker should be open"
        );
        assert_eq!(
            inner.runtime.instantiate_calls(),
            calls_before_open,
            "a call during the cooldown must not pay instantiate_initialized at all"
        );

        std::thread::sleep(Duration::from_millis(UNTRUSTED_TRAP_COOLDOWN_MS + 20));
        let result = inner.run_hook(None, |inst: &mut FakeInstance| {
            Ok::<_, wasmtime::Error>(inst.id)
        });
        assert!(
            result.is_ok(),
            "the untrusted lifecycle should self-heal once the cooldown elapses, got {result:?}"
        );
    }

    #[test]
    fn run_fresh_recovers_final_logs_on_success_not_just_on_trap() {
        // Regression (staff review of ADR 000063): a fresh/untrusted instance is discarded after
        // ONE call regardless of whether it traps — so an unterminated stdio partial line (e.g. a
        // guest that writes "done" with no trailing '\n' then returns normally) must be recovered
        // on the Ok arm exactly like the Err arm already does, not silently dropped along with
        // the discarded instance.
        let runtime = FakeRuntime::new();
        let inner = fake_inner(runtime);

        let (_value, logs) = inner
            .run_hook(None, |inst: &mut FakeInstance| {
                Ok::<_, wasmtime::Error>(inst.id)
            })
            .expect("a non-trapping call succeeds");

        assert_eq!(
            logs,
            marker_log("take_logs_after_trap"),
            "run_fresh's Ok arm must use the final drain (partial-line flush), \
             not the plain mid-lifetime drain — the instance is discarded either way"
        );
    }
}
