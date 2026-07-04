//! Per-filter (namespace) accounting and caps for host-held state (CWE-770): see [`KvQuota`].

use std::collections::HashMap;

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

/// Per-filter (namespace) accounting and caps for host-held state (CWE-770). The host
/// charges every KV value, counter, and rate-limit bucket against the owning filter's namespace
/// and a host-wide ceiling, so an untrusted, multi-tenant filter cannot grow host memory (or the
/// redb file) without bound — the `StoreLimits` cap only bounds the guest's own linear memory,
/// not the host-side store. Enforced here at the capability boundary, keeping `KvBackend` generic.
/// One per `Host`, shared (`Arc`) across every filter's `HostState`.
pub(crate) struct KvQuota {
    inner: Mutex<QuotaInner>,
}

impl KvQuota {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(QuotaInner {
                ns: HashMap::new(),
                total_entries: 0,
                total_bytes: 0,
            }),
        }
    }

    /// Try to charge `(entries_delta, bytes_delta)` to namespace `ns`. A growth that would push
    /// the namespace or the host-wide total past a cap is rejected (returns `false`, the caller
    /// fails closed); a shrink (negative delta) always applies. Commits atomically under the lock.
    pub(crate) fn admit(&self, ns: &str, entries_delta: isize, bytes_delta: isize) -> bool {
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
            return false;
        }
        let usage = g.ns.entry(ns.to_string()).or_default();
        usage.entries = new_ns_entries.max(0) as usize;
        usage.bytes = new_ns_bytes.max(0) as usize;
        g.total_entries = new_total_entries.max(0) as usize;
        g.total_bytes = new_total_bytes.max(0) as usize;
        true
    }

    /// Release `(entries, bytes)` from `ns` (a delete). Never rejects.
    pub(crate) fn release(&self, ns: &str, entries: usize, bytes: usize) {
        self.admit(ns, -(entries as isize), -(bytes as isize));
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
}
