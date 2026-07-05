//! Per-filter (namespace) accounting and caps for host-held state (CWE-770): see [`KvQuota`].

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;

use parking_lot::Mutex;

// --- host-state (KV / counter / ratelimit) quotas (CWE-770). The host charges every
// --- value, counter, and bucket against the owning filter's namespace and a global ceiling so
// --- an untrusted, multi-tenant filter cannot grow host memory/disk without bound. ---

/// Per-filter (namespace) cap on the number of distinct keys across kv + counter + ratelimit.
pub(crate) const MAX_NS_ENTRIES: usize = 100_000;
/// Per-filter (namespace) cap on total stored bytes (keys + values) across all primitives.
pub(crate) const MAX_NS_BYTES: usize = 64 << 20;
/// Host-wide cap on total entries across every filter (multi-tenant ceiling).
pub(crate) const MAX_TOTAL_ENTRIES: usize = 5_000_000;
/// Host-wide cap on total stored bytes across every filter (multi-tenant ceiling).
pub(crate) const MAX_TOTAL_BYTES: usize = 1 << 30;

#[derive(Default, Clone, Copy)]
struct NsUsage {
    entries: usize,
    bytes: usize,
}

struct QuotaInner {
    ns: HashMap<String, NsUsage>,
    total_entries: usize,
    total_bytes: usize,
}

/// Number of per-key stripe locks. A power of two (the stripe index is a masked hash). 64
/// stripes keeps two concurrent distinct keys' collision odds at ~1/64 while the array stays
/// a few cache lines — raising it is cheap if a host ever runs wide enough to contend here.
const STRIPE_COUNT: usize = 64;

/// Per-filter (namespace) accounting and caps for host-held state (CWE-770). The host
/// charges every KV value, counter, and rate-limit bucket against the owning filter's namespace
/// and a host-wide ceiling, so an untrusted, multi-tenant filter cannot grow host memory (or the
/// redb file) without bound — the `StoreLimits` cap only bounds the guest's own linear memory,
/// not the host-side store. Enforced here at the capability boundary, keeping `KvBackend` generic.
/// One per `Host`, shared (`Arc`) across every filter's `HostState`.
///
/// Two locks with a fixed order (stripe → tally, never the reverse):
/// - `stripes[hash(key) % N]` gives one key's read-decide-write sequence (backend I/O included)
///   exclusivity against the same key only — a stalled write (a redb disk commit) blocks its
///   stripe, not every host-state call in the process.
/// - `inner` guards only the namespace/global tallies. Its critical section is pure arithmetic —
///   **no backend I/O and no caller closures ever run under it** — so cap decisions stay exact
///   without becoming the process-wide convoy the pre-stripe design was.
pub(crate) struct KvQuota {
    inner: Mutex<QuotaInner>,
    stripes: Box<[Mutex<()>]>,
    /// Seeded per `KvQuota` (`RandomState`), so an untrusted filter cannot precompute keys that
    /// pile onto one stripe another tenant is using.
    hasher: RandomState,
}

impl KvQuota {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(QuotaInner {
                ns: HashMap::new(),
                total_entries: 0,
                total_bytes: 0,
            }),
            stripes: (0..STRIPE_COUNT).map(|_| Mutex::new(())).collect(),
            hasher: RandomState::new(),
        }
    }

    // In-bounds by construction: the index is masked to STRIPE_COUNT-1 and `stripes` is built
    // with exactly STRIPE_COUNT elements (same pattern as maglev's table lookup).
    #[allow(clippy::indexing_slicing)]
    fn stripe(&self, key: &[u8]) -> &Mutex<()> {
        &self.stripes[(self.hasher.hash_one(key) as usize) & (STRIPE_COUNT - 1)]
    }

    /// Try to charge `(entries_delta, bytes_delta)` to namespace `ns`. A growth that would push
    /// the namespace or the host-wide total past a cap is rejected (returns `false`, the caller
    /// fails closed); a shrink (negative delta) always applies. Commits atomically under the lock.
    /// Test-only: production callers go through `charge_and_apply` directly so the backend
    /// read-modify-write and the quota decision are one atomic unit (see its doc for why a plain
    /// admit-then-write two-step is a real race under concurrent same-key access).
    #[cfg(test)]
    pub(crate) fn admit(&self, ns: &str, entries_delta: isize, bytes_delta: isize) -> bool {
        self.charge_and_apply(ns, ns.as_bytes(), || (entries_delta, bytes_delta), || ())
            .is_some()
    }

    /// Whether two keys land on the same stripe lock (the hash seed is per-instance, so tests
    /// that need two *non-colliding* keys must probe rather than hard-code them).
    #[cfg(test)]
    pub(crate) fn same_stripe_for_test(&self, a: &[u8], b: &[u8]) -> bool {
        std::ptr::eq(self.stripe(a), self.stripe(b))
    }

    /// Release `(entries, bytes)` from `ns` (a delete). Never rejects. Test-only, see `admit`.
    #[cfg(test)]
    pub(crate) fn release(&self, ns: &str, entries: usize, bytes: usize) {
        self.admit(ns, -(entries as isize), -(bytes as isize));
    }

    /// Current committed `(entries, bytes)` for `ns`. Test-only introspection.
    #[cfg(test)]
    pub(crate) fn usage_for_test(&self, ns: &str) -> (usize, usize) {
        let g = self.inner.lock();
        let u = g.ns.get(ns).copied().unwrap_or_default();
        (u.entries, u.bytes)
    }

    /// Atomically read-decide-apply one state mutation for `key` in namespace `ns`.
    /// `read_and_delta` inspects the current backend value and returns the
    /// `(entries_delta, bytes_delta)` the mutation would cost; `apply` performs the actual
    /// backend write (or delete) and produces the return value. Both closures run under
    /// **`key`'s stripe lock**, so the whole read-decide-write sequence is one atomic unit with
    /// respect to every other `charge_and_apply` call *on the same key* — which is the unit the
    /// race needs (see below). Calls on different keys proceed in parallel (different stripes),
    /// so a stalled backend write — a redb disk commit — blocks its own stripe, not every
    /// host-state call in the process. The tally update itself takes the quota lock, whose
    /// critical section is pure arithmetic (never the closures, never backend I/O); lock order
    /// is fixed stripe → tally and the tally lock is released before `apply` runs.
    ///
    /// The race this closes: the trusted pool runs many concurrent instances of the same
    /// filter, all sharing one `KvQuota` and one backend. A separate get-then-admit-then-write
    /// (three independent lock acquisitions) lets two concurrent calls on the same key both
    /// observe the pre-mutation state — e.g. two concurrent `delete`s on the same key both read
    /// `Some(old)` and both call `release`, double-releasing budget the key was only ever charged
    /// once for, permanently under-counting real usage and eroding the CWE-770 cap this module
    /// exists to enforce. Serializing per key makes the second concurrent caller observe the
    /// first's effect (an already-deleted key, an already-updated value) instead of racing it.
    ///
    /// Returns `None` if the mutation is rejected over quota (`apply` is not called).
    pub(crate) fn charge_and_apply<T>(
        &self,
        ns: &str,
        key: &[u8],
        read_and_delta: impl FnOnce() -> (isize, isize),
        apply: impl FnOnce() -> T,
    ) -> Option<T> {
        let _key_guard = self.stripe(key).lock();
        let (entries_delta, bytes_delta) = read_and_delta();
        {
            let mut g = self.inner.lock();
            let cur = g.ns.get(ns).copied().unwrap_or_default();
            let new_ns_entries = cur.entries as isize + entries_delta;
            let new_ns_bytes = cur.bytes as isize + bytes_delta;
            let new_total_entries = g.total_entries as isize + entries_delta;
            let new_total_bytes = g.total_bytes as isize + bytes_delta;
            // Only growth can violate a cap; a shrink (delete / smaller value) always applies.
            if (entries_delta > 0
                && (new_ns_entries as usize > MAX_NS_ENTRIES
                    || new_total_entries as usize > MAX_TOTAL_ENTRIES))
                || (bytes_delta > 0
                    && (new_ns_bytes as usize > MAX_NS_BYTES
                        || new_total_bytes as usize > MAX_TOTAL_BYTES))
            {
                return None;
            }
            // Only allocate the owned `String` key on a namespace's first-ever charge; every
            // later call for the same (already-resident) filter namespace is a borrowed lookup.
            let usage = match g.ns.get_mut(ns) {
                Some(u) => u,
                None => g.ns.entry(ns.to_string()).or_default(),
            };
            usage.entries = new_ns_entries.max(0) as usize;
            usage.bytes = new_ns_bytes.max(0) as usize;
            g.total_entries = new_total_entries.max(0) as usize;
            g.total_bytes = new_total_bytes.max(0) as usize;
        }
        Some(apply())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_admit_rejects_growth_past_caps_and_allows_shrink() {
        // KvQuota accounting — a growth past a per-namespace or global cap is rejected; a
        // shrink (negative delta) always applies and frees the budget for a later growth.
        let q = KvQuota::new();
        assert!(q.admit("ns", 1, 100), "a small entry fits");
        assert!(
            !q.admit("ns", 1, MAX_NS_BYTES as isize),
            "a value that would exceed the per-namespace byte cap is rejected"
        );
        assert!(
            !q.admit("ns2", 1, MAX_TOTAL_BYTES as isize),
            "a value that would exceed the host-wide byte cap is rejected"
        );
        // a shrink always applies (release path), and never rejects.
        q.release("ns", 1, 100);
        assert!(
            q.admit("ns", 1, 100),
            "freed budget is reusable after a release"
        );
    }

    #[test]
    fn a_stalled_apply_on_one_key_does_not_block_other_keys() {
        // charge_and_apply runs the backend write (`apply`) under the lock that makes the
        // read-decide-write sequence atomic. That atomicity is only needed PER KEY — but a
        // single process-wide lock over-delivers it: a slow backend write on one key (a redb
        // disk commit, a parked thread) stalls every host-state call of every filter. This
        // test pins the intended scope: while one call's `apply` is parked, a call for a
        // DIFFERENT key must still complete.
        use std::sync::{Arc, Barrier, mpsc};
        use std::thread;
        use std::time::Duration;

        let q = Arc::new(KvQuota::new());
        // The stripe hash is seeded per instance, so probe for a key that provably does not
        // share the stalled key's stripe — hard-coding two keys would make this flaky ~1/64.
        let stalled_key = b"key-a".to_vec();
        let other_key = (0..)
            .map(|i| format!("key-b{i}").into_bytes())
            .find(|k| !q.same_stripe_for_test(&stalled_key, k))
            .unwrap();
        // Two barriers handshake with the stalled call: `entered` proves its `apply` is
        // running (holding whatever lock the implementation wraps around it); `release` lets
        // it finish.
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));

        let stalled = {
            let q = q.clone();
            let entered = entered.clone();
            let release = release.clone();
            thread::spawn(move || {
                q.charge_and_apply(
                    "ns-a",
                    &stalled_key,
                    || (1, 100),
                    || {
                        entered.wait();
                        release.wait();
                    },
                );
            })
        };
        entered.wait();

        // With the stalled apply still parked, a different key's mutation must go through.
        let (tx, rx) = mpsc::channel();
        let other = {
            let q = q.clone();
            thread::spawn(move || {
                q.charge_and_apply("ns-b", &other_key, || (1, 100), || ());
                let _ = tx.send(());
            })
        };
        let completed = rx.recv_timeout(Duration::from_secs(2));

        // Unblock the stalled call before asserting, so a failing run tears down cleanly.
        release.wait();
        stalled.join().unwrap();
        other.join().unwrap();

        assert!(
            completed.is_ok(),
            "a stalled backend write on one key must not block an unrelated key's call"
        );
    }
}
