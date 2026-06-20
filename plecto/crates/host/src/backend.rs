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
fn apply_bucket(
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
fn decode_i64(bytes: &[u8]) -> i64 {
    let mut buf = [0u8; 8];
    let n = bytes.len().min(8);
    buf[..n].copy_from_slice(&bytes[..n]);
    i64::from_le_bytes(buf)
}

/// Decode bucket state `(tokens, last_refill_ms)` from 16 LE bytes (None if malformed).
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
        let next = decode_i64(map.get(key).map(Vec::as_slice).unwrap_or(&[])).saturating_add(delta);
        map.insert(key.to_vec(), next.to_le_bytes().to_vec());
        next
    }

    fn try_acquire(&self, key: &[u8], cost: u64, spec: Bucket, now_ms: u64) -> Acquire {
        let mut map = self.map.lock();
        let prev = map.get(key).and_then(|b| decode_bucket(b));
        let (next, result) = apply_bucket(prev, cost, spec, now_ms);
        map.insert(key.to_vec(), encode_bucket(next));
        result
    }
}

// --- redb backend (durable; ADR 000004) ---

const STATE_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("plecto_state");

/// redb-backed durable state. Atomicity comes from redb's single-writer write
/// transaction: each `increment` / `try_acquire` does its read-modify-write inside one
/// transaction (ADR 000004). redb is fully synchronous; ADR 000011's async-aware seam
/// is this `KvBackend` impl — when host calls go async, the commits move to a blocking
/// pool here without touching callers.
pub struct RedbBackend {
    db: Database,
}

impl RedbBackend {
    /// Open (or create) the redb database at `path`.
    pub fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        Ok(Self {
            db: Database::create(path)?,
        })
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

    fn set_inner(&self, key: &[u8], value: &[u8]) -> anyhow::Result<()> {
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(STATE_TABLE)?;
            table.insert(key, value)?;
        }
        wtxn.commit()?;
        Ok(())
    }

    fn delete_inner(&self, key: &[u8]) -> anyhow::Result<()> {
        let wtxn = self.db.begin_write()?;
        {
            let mut table = wtxn.open_table(STATE_TABLE)?;
            table.remove(key)?;
        }
        wtxn.commit()?;
        Ok(())
    }

    fn increment_inner(&self, key: &[u8], delta: i64) -> anyhow::Result<i64> {
        let wtxn = self.db.begin_write()?;
        let next = {
            let mut table = wtxn.open_table(STATE_TABLE)?;
            let cur = table.get(key)?.map(|g| decode_i64(g.value())).unwrap_or(0);
            let next = cur.saturating_add(delta);
            table.insert(key, next.to_le_bytes().as_slice())?;
            next
        };
        wtxn.commit()?;
        Ok(next)
    }

    fn try_acquire_inner(
        &self,
        key: &[u8],
        cost: u64,
        spec: Bucket,
        now_ms: u64,
    ) -> anyhow::Result<Acquire> {
        let wtxn = self.db.begin_write()?;
        let result = {
            let mut table = wtxn.open_table(STATE_TABLE)?;
            let prev = table.get(key)?.and_then(|g| decode_bucket(g.value()));
            let (next, result) = apply_bucket(prev, cost, spec, now_ms);
            table.insert(key, encode_bucket(next).as_slice())?;
            result
        };
        wtxn.commit()?;
        Ok(result)
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
        if let Err(e) = self.set_inner(key, &value) {
            tracing::error!(error = %e, "redb set failed; value dropped");
        }
    }

    fn delete(&self, key: &[u8]) {
        if let Err(e) = self.delete_inner(key) {
            tracing::error!(error = %e, "redb delete failed");
        }
    }

    fn increment(&self, key: &[u8], delta: i64) -> i64 {
        match self.increment_inner(key, delta) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "redb increment failed; returning 0");
                0
            }
        }
    }

    fn try_acquire(&self, key: &[u8], cost: u64, spec: Bucket, now_ms: u64) -> Acquire {
        match self.try_acquire_inner(key, cost, spec, now_ms) {
            Ok(r) => r,
            Err(e) => {
                // fail-closed: a limiter that cannot read its state denies (ADR 000004).
                tracing::error!(error = %e, "redb try_acquire failed; denying");
                Acquire {
                    allowed: false,
                    remaining: 0,
                    retry_after_ms: spec.refill_interval_ms,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn memory_backend_behaviour() {
        let b = MemoryBackend::default();
        kv_roundtrip(&b);
        counter_is_atomic_add_and_get(&b);
        token_bucket_drains_then_refills(&b);
    }

    #[test]
    fn redb_backend_behaviour() {
        let dir = tempfile::tempdir().unwrap();
        let b = RedbBackend::open(dir.path().join("state.redb")).unwrap();
        kv_roundtrip(&b);
        counter_is_atomic_add_and_get(&b);
        token_bucket_drains_then_refills(&b);
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
