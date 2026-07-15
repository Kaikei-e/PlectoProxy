//! Host-held state backend (ADR 000004 / 000011).
//!
//! A stateless filter (Fork 4) keeps its *mutable* business state here: raw KV bytes,
//! atomic counters, and token-bucket rate limiters. `KvBackend` is the **seam** ADR
//! 000011 asks for: the host-API impls and the lifecycle never name a concrete store,
//! so swapping in-memory ↔ redb is local, and when wasmtime 46 makes host calls async
//! only the redb impl moves behind a blocking pool — callers stay put.
//!
//! Sync today (wasmtime 45 sync path). Locks are **non-poisoning** (`parking_lot`): a
//! panicking filter must not cascade a poisoned lock across every later request.
//!
//! Keys arrive already namespaced by filter identity + primitive tag (done in
//! `HostState`, ADR 000011); a backend treats them as opaque bytes and never inspects
//! the namespace.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use parking_lot::Mutex;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

/// A token-bucket specification (mirrors the WIT `host-ratelimit.bucket`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub capacity: u64,
    pub refill_tokens: u64,
    pub refill_interval_ms: u64,
}

/// The outcome of a token-bucket acquire (mirrors the WIT `host-ratelimit.acquire`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Acquire {
    pub allowed: bool,
    pub remaining: u64,
    pub retry_after_ms: u64,
}

/// The place a stateless filter's mutable state lives. Object-safe so the host can hold
/// `Arc<dyn KvBackend>` and pick the backend at construction. Every method is internally
/// synchronized and infallible from the filter's view — a backend error is logged and
/// resolved **fail-closed** (reads vanish, rate limits deny), never a panic on the data
/// plane (bp-rust).
pub trait KvBackend: Send + Sync {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn set(&self, key: &[u8], value: Vec<u8>);
    fn delete(&self, key: &[u8]);
    /// Atomic add-and-get. An unset counter starts at 0; `delta` is signed.
    fn increment(&self, key: &[u8], delta: i64) -> i64;
    /// Atomic token-bucket acquire against the `now_ms` request-clock snapshot. The
    /// refill + counting stay host-native (ADR 000005) — they never cross the WASM
    /// boundary; the filter only decided to consult the limiter.
    fn try_acquire(&self, key: &[u8], cost: u64, spec: Bucket, now_ms: u64) -> Acquire;
}

// --- pure token-bucket math (host-native, deterministic against `now_ms`) ---

/// Refill then consume. State is `(tokens, last_refill_ms)`; the host advances `last`
/// by whole intervals only, so no fractional tokens are lost between calls. Returns the
/// new state to persist and the acquire outcome.
///
/// Pure and storage-agnostic: it owns no state and does no I/O, so the same math drives both
/// the per-filter `host-ratelimit` capability (this module's backends) and the fast path's
/// native per-route rate limiter (ADR 000033, `plecto-control`). A zero state `(0, 0)` refills
/// from epoch and therefore reads as a full bucket on first use — a caller backing the state
/// with a zero-initialised table needs no separate "first sight" sentinel.
pub fn apply_bucket(
    state: Option<(u64, u64)>,
    cost: u64,
    spec: Bucket,
    now_ms: u64,
) -> ((u64, u64), Acquire) {
    let no_refill = spec.refill_interval_ms == 0 || spec.refill_tokens == 0;
    let (tokens, last_refill) = match state {
        // first sight of this bucket: start full as of now
        None => (spec.capacity, now_ms),
        Some((tokens, last)) if no_refill => (tokens.min(spec.capacity), last),
        Some((tokens, last)) => {
            let intervals = now_ms.saturating_sub(last) / spec.refill_interval_ms;
            let refilled = tokens
                .saturating_add(intervals.saturating_mul(spec.refill_tokens))
                .min(spec.capacity);
            let advanced = last.saturating_add(intervals.saturating_mul(spec.refill_interval_ms));
            (refilled, advanced)
        }
    };

    if tokens >= cost {
        let remaining = tokens - cost;
        (
            (remaining, last_refill),
            Acquire {
                allowed: true,
                remaining,
                retry_after_ms: 0,
            },
        )
    } else {
        // Retry-After is a deliberate over-estimate: it counts whole refill intervals and
        // ignores the fraction of the current interval already elapsed, so it can be late by up
        // to one interval. That is the conservative side for an advisory hint — it never invites
        // a retry that is too early. Tightening it would mean persisting sub-interval phase,
        // not worth it for an advisory value.
        let retry_after_ms = if no_refill {
            u64::MAX
        } else {
            let needed = cost - tokens;
            needed
                .div_ceil(spec.refill_tokens)
                .saturating_mul(spec.refill_interval_ms)
        };
        (
            (tokens, last_refill),
            Acquire {
                allowed: false,
                remaining: tokens,
                retry_after_ms,
            },
        )
    }
}

/// Decode a stored counter (tolerant of short/empty values: missing == 0).
#[allow(clippy::indexing_slicing)] // n = bytes.len().min(8), so buf[..n]/bytes[..n] are always in-bounds
fn decode_i64(bytes: &[u8]) -> i64 {
    let mut buf = [0u8; 8];
    let n = bytes.len().min(8);
    buf[..n].copy_from_slice(&bytes[..n]);
    i64::from_le_bytes(buf)
}

/// Decode bucket state `(tokens, last_refill_ms)` from 16 LE bytes (None if malformed).
#[allow(clippy::indexing_slicing)] // length is checked (== 16) three lines above
fn decode_bucket(bytes: &[u8]) -> Option<(u64, u64)> {
    if bytes.len() != 16 {
        return None;
    }
    let tokens = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
    let last = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
    Some((tokens, last))
}

fn encode_bucket(state: (u64, u64)) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&state.0.to_le_bytes());
    out.extend_from_slice(&state.1.to_le_bytes());
    out
}

/// Map stored bucket bytes to the state `apply_bucket` consumes, **fail-closed on corruption**.
/// `None` (key absent) is a legitimate first sight → start full. Present-but-malformed bytes must
/// NOT decode to a full bucket (that is fail-OPEN, inconsistent with the limiter's fail-closed
/// stance); treat corruption as an empty bucket so the call is denied and the limiter self-heals
/// via refill.
fn bucket_input(raw: Option<&[u8]>, now_ms: u64) -> Option<(u64, u64)> {
    raw.map(|bytes| decode_bucket(bytes).unwrap_or((0, now_ms)))
}

// --- in-memory backend (default; tests and single-process runs) ---

/// Process-lifetime, in-memory backend. The default until a durable store is configured.
#[derive(Default)]
pub struct MemoryBackend {
    map: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl KvBackend for MemoryBackend {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.lock().get(key).cloned()
    }

    fn set(&self, key: &[u8], value: Vec<u8>) {
        self.map.lock().insert(key.to_vec(), value);
    }

    fn delete(&self, key: &[u8]) {
        self.map.lock().remove(key);
    }

    fn increment(&self, key: &[u8], delta: i64) -> i64 {
        let mut map = self.map.lock();
        let cur = decode_i64(map.get(key).map(Vec::as_slice).unwrap_or(&[]));
        // delta == 0 is a counter read (host-counter.get): return the current value without
        // creating or rewriting the key (mirrors the redb read-txn branch).
        if delta == 0 {
            return cur;
        }
        let next = cur.saturating_add(delta);
        map.insert(key.to_vec(), next.to_le_bytes().to_vec());
        next
    }

    fn try_acquire(&self, key: &[u8], cost: u64, spec: Bucket, now_ms: u64) -> Acquire {
        let mut map = self.map.lock();
        let prev = bucket_input(map.get(key).map(Vec::as_slice), now_ms);
        let (next, result) = apply_bucket(prev, cost, spec, now_ms);
        map.insert(key.to_vec(), encode_bucket(next));
        result
    }
}

// --- redb backend (durable; ADR 000004) ---

const STATE_TABLE: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("plecto_state");

/// Cap on redb's lazily-filled page cache. The library default (1 GiB) is sized for a
/// process that IS the database; embedded stores conventionally cap far lower (SQLite ~2 MiB,
/// RocksDB 32 MiB, Kafka Streams 50 MiB per store). The cache only evicts at the cap, so a
/// long-lived proxy would otherwise grow toward the full gigabyte; 64 MiB still dwarfs the
/// working set of 8–16-byte counters and bucket states.
const REDB_CACHE_BYTES: usize = 64 << 20;

/// Cap on consecutive combining rounds one caller performs before releasing the lock, so
/// sustained write pressure cannot conscript a single caller as combiner indefinitely.
/// 16 rounds bounds the conscription to roughly a millisecond of commits while keeping
/// timeout-based re-election (the slow path) rare.
const MAX_COMBINE_ROUNDS: u32 = 16;

/// Every `DURABLE_FLUSH_EVERY`th hot-path commit is upgraded from `Durability::None` to
/// `Immediate`. redb frees pages only at a durable commit, so an unbounded None-only run
/// (a counter/ratelimit-heavy workload with no kv writes) would grow the file without bound.
/// 1024 amortises the fsync to noise on the hot path while bounding both the file growth
/// between durable commits and the window of updates a crash can lose.
const DURABLE_FLUSH_EVERY: u64 = 1024;

/// redb-backed durable state. Atomicity comes from redb's single-writer write
/// transaction: each op's read-modify-write is applied inside one (ADR 000004). redb is
/// fully synchronous; ADR 000011's async-aware seam is this `KvBackend` impl — callers
/// never see the store or the batching behind it.
///
/// Writes go through a **group commit** (DeWitt et al. 1984) shaped as **flat combining**
/// (Hendler et al., SPAA'10): a caller queues its op, then competes for the combiner
/// lock; the winner drains everything queued and applies the whole batch inside ONE
/// write transaction, answering every waiter before releasing. redb serializes writers
/// globally (`begin_write` blocks), so per-op transactions would serialize every filter
/// and route on N× the begin/commit cost; combining keeps the single writer but pays
/// that cost once per batch. The batch is self-clocking — no timer, no resident thread:
/// an uncontended caller drains only its own op and runs it inline (zero thread handoff,
/// per-op cost identical to a plain transaction), while contended callers batch exactly
/// what accumulated while the previous combiner held the lock.
pub struct RedbBackend {
    db: Database,
    flush_every: u64,
    /// Hot-path (non-durable) ops committed since the last durable commit. Mutated only
    /// while holding the `combine` lock; atomics so tests observe them from outside.
    non_durable_run: AtomicU64,
    /// Durable commits forced by the cadence (observability for the flush tests).
    forced_flushes: AtomicU64,
    /// Write queue, open for the backend's whole life (the paired receiver lives inside
    /// `combine`, so the channel never disconnects while a caller can reach it).
    jobs: mpsc::Sender<WriteJob>,
    /// The combiner election: holding this lock IS being the combiner — it grants
    /// exclusive drain access to the queue. Non-poisoning (parking_lot): if a combiner
    /// died mid-batch its drained jobs' reply channels disconnect, so every waiter
    /// resolves fail-closed instead of inheriting a poisoned lock.
    combine: Mutex<mpsc::Receiver<WriteJob>>,
}

/// One queued write, applied by the combiner inside the next batch's transaction.
enum WriteOp {
    Set {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    Delete {
        key: Vec<u8>,
    },
    Increment {
        key: Vec<u8>,
        delta: i64,
    },
    TryAcquire {
        key: Vec<u8>,
        cost: u64,
        spec: Bucket,
        now_ms: u64,
    },
}

impl WriteOp {
    /// Durable KV (`set`/`delete`) keeps its per-call `Immediate` guarantee: one in a batch
    /// upgrades the whole commit, and the co-batched hot ops become durable for free.
    fn needs_durable_commit(&self) -> bool {
        matches!(self, WriteOp::Set { .. } | WriteOp::Delete { .. })
    }
}

/// A completed op's result, sent back over the job's reply channel only after its batch
/// commits — an outcome must never be observable before it is committed.
enum WriteOutcome {
    Done,
    Counter(i64),
    Acquired(Acquire),
}

struct WriteJob {
    op: WriteOp,
    /// One-shot reply: the caller blocks on the paired receiver until the batch commits, so
    /// in-flight jobs are bounded by the number of calling threads — the queue needs no cap.
    reply: mpsc::Sender<anyhow::Result<WriteOutcome>>,
}

impl RedbBackend {
    /// Open (or create) the redb database at `path`.
    pub fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        Self::open_inner(path, DURABLE_FLUSH_EVERY)
    }

    #[cfg(test)]
    fn open_with_flush_every(
        path: impl AsRef<std::path::Path>,
        flush_every: u64,
    ) -> anyhow::Result<Self> {
        Self::open_inner(path, flush_every)
    }

    fn open_inner(path: impl AsRef<std::path::Path>, flush_every: u64) -> anyhow::Result<Self> {
        let db = redb::Builder::new()
            .set_cache_size(REDB_CACHE_BYTES)
            .create(path)?;
        let (jobs, queue) = mpsc::channel();
        Ok(Self {
            db,
            flush_every,
            non_durable_run: AtomicU64::new(0),
            forced_flushes: AtomicU64::new(0),
            jobs,
            combine: Mutex::new(queue),
        })
    }

    #[cfg(test)]
    fn forced_flushes(&self) -> u64 {
        self.forced_flushes.load(Ordering::Relaxed)
    }

    fn get_inner(&self, key: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        let rtxn = self.db.begin_read()?;
        let table = match rtxn.open_table(STATE_TABLE) {
            Ok(t) => t,
            // no writer has created the table yet → empty
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(table.get(key)?.map(|g| g.value().to_vec()))
    }

    /// Queue one write, then either combine (apply everything queued, ours included) or
    /// wait for a concurrent combiner to commit ours. An error is the batch's transaction
    /// failing; it resolves fail-closed at the trait impl (reads vanish, limiters deny).
    ///
    /// Waiters park on their OWN reply channel, never on the combiner lock: replying
    /// wakes every serviced caller in parallel, where queueing on the lock would wake
    /// them one serial handoff at a time and hand the throughput right back.
    fn submit(&self, op: WriteOp) -> anyhow::Result<WriteOutcome> {
        let (reply, outcome) = mpsc::channel();
        // Unreachable: `self.combine` owns the receiver, so the channel outlives every
        // caller — but the data plane never panics on it (bp-rust).
        self.jobs
            .send(WriteJob { op, reply })
            .map_err(|_| anyhow::anyhow!("kv write queue closed"))?;
        loop {
            match outcome.try_recv() {
                Ok(result) => return result,
                Err(mpsc::TryRecvError::Empty) => {}
                // A combiner died mid-batch and dropped our job — fail closed.
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("kv combiner dropped a queued write")
                }
            }
            let Some(queue) = self.combine.try_lock() else {
                // Another caller is combining, and its next drain will pick our job up.
                // The timeout covers one race — the combiner drained empty and released
                // without seeing our job — by re-entering the election above.
                match outcome.recv_timeout(std::time::Duration::from_micros(100)) {
                    Ok(result) => return result,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        anyhow::bail!("kv combiner dropped a queued write")
                    }
                }
            };
            // We are the combiner. Drain-apply-reply until a drain comes up empty: a job
            // queued during our commit parks on its reply channel (not the lock), so
            // leaving it behind would strand it for a full timeout round. Cap the rounds
            // so one caller cannot be conscripted indefinitely under sustained load —
            // past the cap the residual jobs' owners re-elect via the timeout above.
            for _ in 0..MAX_COMBINE_ROUNDS {
                let mut batch = Vec::new();
                while let Ok(job) = queue.try_recv() {
                    batch.push(job);
                }
                if batch.is_empty() {
                    break;
                }
                match self.apply_batch(&batch) {
                    Ok(outcomes) => {
                        for (job, out) in batch.iter().zip(outcomes) {
                            let _ = job.reply.send(Ok(out));
                        }
                    }
                    Err(e) => {
                        // The whole batch rode one transaction, so all of it fails
                        // together; each caller resolves its own op fail-closed.
                        tracing::error!(error = %e, ops = batch.len(), "redb batch commit failed");
                        for job in &batch {
                            let _ = job
                                .reply
                                .send(Err(anyhow::anyhow!("batch commit failed: {e:#}")));
                        }
                    }
                }
            }
            // Our own op rode the first round (we enqueued before winning the election),
            // so the next `try_recv` returns it.
        }
    }

    /// The durability of one combined commit. A batch carrying durable KV (`set`/`delete`)
    /// commits `Immediate` — the guarantee those ops always had — and restarts the hot-path
    /// cadence (every earlier non-durable commit becomes durable with it). A hot-only batch
    /// (counters / buckets — ephemera, ADR 000005) skips the fsync until the run reaches
    /// `flush_every`, then one `Immediate` commit bounds file growth and the crash-loss
    /// window; the run counts OPS, not commits, so batching does not stretch that bound.
    fn batch_durability(&self, batch: &[WriteJob]) -> redb::Durability {
        if batch.iter().any(|j| j.op.needs_durable_commit()) {
            self.non_durable_run.store(0, Ordering::Relaxed);
            return redb::Durability::Immediate;
        }
        let ops = batch.len() as u64;
        let run = self.non_durable_run.fetch_add(ops, Ordering::Relaxed) + ops;
        if run >= self.flush_every {
            self.non_durable_run.store(0, Ordering::Relaxed);
            self.forced_flushes.fetch_add(1, Ordering::Relaxed);
            redb::Durability::Immediate
        } else {
            redb::Durability::None
        }
    }

    /// Apply one batch inside a single write transaction. Outcomes are returned, not sent —
    /// replies must not leave before the commit they depend on.
    fn apply_batch(&self, batch: &[WriteJob]) -> anyhow::Result<Vec<WriteOutcome>> {
        let mut wtxn = self.db.begin_write()?;
        // set_durability only errors if a persistent savepoint changed in this txn; we never
        // use savepoints.
        wtxn.set_durability(self.batch_durability(batch))?;
        let mut outcomes = Vec::with_capacity(batch.len());
        {
            let mut table = wtxn.open_table(STATE_TABLE)?;
            for job in batch {
                outcomes.push(apply_op(&mut table, &job.op)?);
            }
        }
        wtxn.commit()?;
        Ok(outcomes)
    }
}

fn apply_op(
    table: &mut redb::Table<'_, &'static [u8], &'static [u8]>,
    op: &WriteOp,
) -> anyhow::Result<WriteOutcome> {
    match op {
        WriteOp::Set { key, value } => {
            table.insert(key.as_slice(), value.as_slice())?;
            Ok(WriteOutcome::Done)
        }
        WriteOp::Delete { key } => {
            table.remove(key.as_slice())?;
            Ok(WriteOutcome::Done)
        }
        WriteOp::Increment { key, delta } => {
            let cur = table
                .get(key.as_slice())?
                .map(|g| decode_i64(g.value()))
                .unwrap_or(0);
            let next = cur.saturating_add(*delta);
            table.insert(key.as_slice(), next.to_le_bytes().as_slice())?;
            Ok(WriteOutcome::Counter(next))
        }
        WriteOp::TryAcquire {
            key,
            cost,
            spec,
            now_ms,
        } => {
            let prev = {
                let guard = table.get(key.as_slice())?;
                bucket_input(guard.as_ref().map(|g| g.value()), *now_ms)
            };
            let (next, result) = apply_bucket(prev, *cost, *spec, *now_ms);
            table.insert(key.as_slice(), encode_bucket(next).as_slice())?;
            Ok(WriteOutcome::Acquired(result))
        }
    }
}

impl KvBackend for RedbBackend {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        match self.get_inner(key) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "redb get failed; treating key as absent");
                None
            }
        }
    }

    fn set(&self, key: &[u8], value: Vec<u8>) {
        if let Err(e) = self.submit(WriteOp::Set {
            key: key.to_vec(),
            value,
        }) {
            tracing::error!(error = %e, "redb set failed; value dropped");
        }
    }

    fn delete(&self, key: &[u8]) {
        if let Err(e) = self.submit(WriteOp::Delete { key: key.to_vec() }) {
            tracing::error!(error = %e, "redb delete failed");
        }
    }

    fn increment(&self, key: &[u8], delta: i64) -> i64 {
        // delta == 0 is the canonical counter READ (host-counter.get). Serve it from an MVCC
        // read txn so it never queues behind the combiner or pays a commit (ADR 000004 /
        // 000005 hot path). Only a real mutation takes the write path.
        if delta == 0 {
            return match self.get_inner(key) {
                Ok(opt) => decode_i64(opt.as_deref().unwrap_or_default()),
                Err(e) => {
                    tracing::error!(error = %e, "redb increment failed; returning 0");
                    0
                }
            };
        }
        match self.submit(WriteOp::Increment {
            key: key.to_vec(),
            delta,
        }) {
            Ok(WriteOutcome::Counter(v)) => v,
            Ok(WriteOutcome::Done | WriteOutcome::Acquired(_)) => {
                tracing::error!("redb increment got a mismatched outcome; returning 0");
                0
            }
            Err(e) => {
                tracing::error!(error = %e, "redb increment failed; returning 0");
                0
            }
        }
    }

    fn try_acquire(&self, key: &[u8], cost: u64, spec: Bucket, now_ms: u64) -> Acquire {
        // fail-closed: a limiter that cannot read its state denies (ADR 000004).
        let denied = Acquire {
            allowed: false,
            remaining: 0,
            retry_after_ms: spec.refill_interval_ms,
        };
        match self.submit(WriteOp::TryAcquire {
            key: key.to_vec(),
            cost,
            spec,
            now_ms,
        }) {
            Ok(WriteOutcome::Acquired(r)) => r,
            Ok(WriteOutcome::Done | WriteOutcome::Counter(_)) => {
                tracing::error!("redb try_acquire got a mismatched outcome; denying");
                denied
            }
            Err(e) => {
                tracing::error!(error = %e, "redb try_acquire failed; denying");
                denied
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// The same behaviour suite must hold for every backend (the seam, ADR 000011).
    fn kv_roundtrip(backend: &dyn KvBackend) {
        assert_eq!(backend.get(b"k"), None);
        backend.set(b"k", b"v".to_vec());
        assert_eq!(backend.get(b"k"), Some(b"v".to_vec()));
        backend.delete(b"k");
        assert_eq!(backend.get(b"k"), None);
    }

    fn counter_is_atomic_add_and_get(backend: &dyn KvBackend) {
        assert_eq!(backend.increment(b"c", 1), 1);
        assert_eq!(backend.increment(b"c", 4), 5);
        assert_eq!(backend.increment(b"c", -2), 3);
        // get reads the same encoding the counter wrote
        assert_eq!(decode_i64(&backend.get(b"c").unwrap()), 3);
    }

    fn token_bucket_drains_then_refills(backend: &dyn KvBackend) {
        let spec = Bucket {
            capacity: 2,
            refill_tokens: 1,
            refill_interval_ms: 1000,
        };
        // capacity 2 → two acquires allowed, third denied (no time passed)
        assert!(backend.try_acquire(b"rl", 1, spec, 0).allowed);
        assert!(backend.try_acquire(b"rl", 1, spec, 0).allowed);
        let denied = backend.try_acquire(b"rl", 1, spec, 0);
        assert!(!denied.allowed);
        assert_eq!(denied.remaining, 0);
        assert_eq!(denied.retry_after_ms, 1000, "1 token needs one interval");
        // after one interval, one token refills → allowed again
        assert!(backend.try_acquire(b"rl", 1, spec, 1000).allowed);
    }

    fn counter_read_via_zero_delta_does_not_create_key(backend: &dyn KvBackend) {
        // host-counter.get maps to increment(key, 0); a pure read must not create the key — it
        // takes the redb read-txn / memory read branch, off the single-writer write path.
        assert_eq!(backend.increment(b"zc", 0), 0, "unset counter reads as 0");
        assert_eq!(
            backend.get(b"zc"),
            None,
            "a zero-delta read must not create the counter key"
        );
        assert_eq!(backend.increment(b"zc", 5), 5);
        assert_eq!(
            backend.increment(b"zc", 0),
            5,
            "zero-delta still reads the live value"
        );
    }

    fn token_bucket_corrupt_state_fails_closed(backend: &dyn KvBackend) {
        // A malformed stored bucket must DENY (fail-closed), never reset to full (fail-open).
        let spec = Bucket {
            capacity: 5,
            refill_tokens: 1,
            refill_interval_ms: 1000,
        };
        backend.set(b"cb", vec![0xff; 3]); // not 16 bytes → corrupt
        assert!(
            !backend.try_acquire(b"cb", 1, spec, 0).allowed,
            "corrupt bucket must fail closed, not start full"
        );
        // and it self-heals: after one interval a refilled token is granted
        assert!(backend.try_acquire(b"cb", 1, spec, 1000).allowed);
    }

    #[test]
    fn memory_backend_behaviour() {
        let b = MemoryBackend::default();
        kv_roundtrip(&b);
        counter_is_atomic_add_and_get(&b);
        counter_read_via_zero_delta_does_not_create_key(&b);
        token_bucket_drains_then_refills(&b);
        token_bucket_corrupt_state_fails_closed(&b);
    }

    #[test]
    fn redb_backend_behaviour() {
        let dir = tempfile::tempdir().unwrap();
        let b = RedbBackend::open(dir.path().join("state.redb")).unwrap();
        kv_roundtrip(&b);
        counter_is_atomic_add_and_get(&b);
        counter_read_via_zero_delta_does_not_create_key(&b);
        token_bucket_drains_then_refills(&b);
        token_bucket_corrupt_state_fails_closed(&b);
    }

    #[test]
    fn redb_periodic_durable_flush_caps_a_non_durable_run() {
        // Hot-path commits (increment / try_acquire) skip the per-commit fsync
        // (Durability::None), but redb frees pages only at a durable commit — an unbounded
        // None-only run (a counter/ratelimit-heavy workload with no kv writes) would grow the
        // file without bound. Every `flush_every`th hot-path commit must be durable.
        let dir = tempfile::tempdir().unwrap();
        let b = RedbBackend::open_with_flush_every(dir.path().join("state.redb"), 4).unwrap();

        let spec = Bucket {
            capacity: 1000,
            refill_tokens: 0,
            refill_interval_ms: 0,
        };
        b.increment(b"c", 1);
        b.increment(b"c", 1);
        b.increment(b"c", 0); // a read (delta 0) commits nothing and must not advance the run
        b.try_acquire(b"rl", 1, spec, 0);
        assert_eq!(
            b.forced_flushes(),
            0,
            "three hot-path commits stay non-durable"
        );

        b.try_acquire(b"rl", 1, spec, 0);
        assert_eq!(
            b.forced_flushes(),
            1,
            "the 4th commit in a run is upgraded to durable"
        );

        for _ in 0..4 {
            b.increment(b"c", 1);
        }
        assert_eq!(b.forced_flushes(), 2, "the cadence repeats");
    }

    #[test]
    fn redb_durable_kv_write_resets_the_flush_cadence() {
        // set/delete commit with Durability::Immediate, which already makes every earlier
        // non-durable commit durable (and frees its pages) — the flush cadence restarts
        // instead of forcing a redundant flush shortly after.
        let dir = tempfile::tempdir().unwrap();
        let b = RedbBackend::open_with_flush_every(dir.path().join("state.redb"), 4).unwrap();

        b.increment(b"c", 1);
        b.increment(b"c", 1);
        b.increment(b"c", 1);
        b.set(b"k", b"v".to_vec()); // durable; the run restarts
        b.increment(b"c", 1);
        b.increment(b"c", 1);
        b.increment(b"c", 1);
        assert_eq!(
            b.forced_flushes(),
            0,
            "a durable set resets the non-durable run"
        );
        b.increment(b"c", 1);
        assert_eq!(b.forced_flushes(), 1);
    }

    #[test]
    fn redb_concurrent_writes_are_exact_under_combining() {
        // 8 threads hammer one counter and one bucket through the combiner; group commit
        // must not lose or double any read-modify-write. The counter must total exactly,
        // and a capacity-100 no-refill bucket must admit exactly 100 of the 400 acquires.
        let dir = tempfile::tempdir().unwrap();
        let b = Arc::new(RedbBackend::open(dir.path().join("state.redb")).unwrap());
        let spec = Bucket {
            capacity: 100,
            refill_tokens: 0,
            refill_interval_ms: 0,
        };
        let admitted = Arc::new(AtomicU64::new(0));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let b = Arc::clone(&b);
                let admitted = Arc::clone(&admitted);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        b.increment(b"c", 1);
                        if b.try_acquire(b"rl", 1, spec, 0).allowed {
                            admitted.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(b.increment(b"c", 0), 400, "no increment lost or doubled");
        assert_eq!(
            admitted.load(Ordering::Relaxed),
            100,
            "the bucket admits exactly its capacity"
        );
    }

    #[test]
    fn redb_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        {
            let b = RedbBackend::open(&path).unwrap();
            assert_eq!(b.increment(b"hits", 3), 3);
        }
        // reopening the same file recovers durable state (ADR 000004 durability)
        let b = RedbBackend::open(&path).unwrap();
        assert_eq!(b.increment(b"hits", 1), 4);
    }

    #[test]
    fn token_bucket_cost_zero_always_allowed() {
        let spec = Bucket {
            capacity: 1,
            refill_tokens: 1,
            refill_interval_ms: 1000,
        };
        let (_state, r) = apply_bucket(Some((0, 0)), 0, spec, 0);
        assert!(r.allowed, "a zero-cost acquire never blocks");
    }
}
