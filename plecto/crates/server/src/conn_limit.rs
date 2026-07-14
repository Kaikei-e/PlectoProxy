//! Per-source-IP concurrent-connection admission control (docs/servey production hardening,
//! ADR 000027 amendment): a CWE-770/CWE-400-bounded complement to the global `MAX_CONNECTIONS`
//! semaphore (`lib.rs`). The global semaphore only caps the connection TOTAL — nothing stops one
//! source from acquiring every permit and starving every other client. This caps each source
//! independently, at [`crate::MAX_CONNECTIONS_PER_IP`].
//!
//! Same shape as `plecto_control::ratelimit`'s `client-ip` bucket table — a fixed number of
//! hashed slots, so memory is O(1) regardless of how many distinct source addresses connect — but
//! deliberately NOT shared with it: `plecto-control` is a published crate (ADR 000090/000091)
//! whose public surface is a semver commitment, and this is a different concern (transport-layer
//! connection admission, checked before any route is even known) from that module's
//! manifest-driven per-route rate-limit policy. Duplicating the ~20-line hashing helper costs far
//! less than a permanent new public API on a published crate.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Slots in the per-source-IP table. A power of two so `slot = hash & (N - 1)`; sized like
/// `plecto_control::ratelimit`'s `client-ip` table (~1 MiB fixed, independent of traffic) for the
/// same reason — routes needing this level of protection are the common case, not a rarity, so
/// the bound should stay generous.
const IP_SLOTS: usize = 1 << 16;

/// A fixed-size, hash-sharded counter table capping concurrently open connections per source IP.
/// `try_acquire` never allocates per distinct address — an attacker controlling many source
/// addresses cannot grow this table, only collide within it (bounded, shared collateral with
/// whatever else hashes into the same slot — never an OOM).
pub(crate) struct PerIpConnLimit {
    slots: Box<[AtomicU32]>,
    hasher: RandomState,
    limit: u32,
}

impl PerIpConnLimit {
    pub(crate) fn new(limit: u32) -> Self {
        Self {
            slots: (0..IP_SLOTS).map(|_| AtomicU32::new(0)).collect(),
            hasher: RandomState::new(),
            limit,
        }
    }

    /// Admit a new connection from `peer`, or refuse it if its slot already holds `limit`
    /// concurrent connections. `Some` carries a guard that releases the slot when the connection
    /// ends (on drop) — hold it for the connection's lifetime, alongside the global permit.
    pub(crate) fn try_acquire(limiter: &Arc<Self>, peer: IpAddr) -> Option<PerIpConnGuard> {
        let slot = slot_for_ip(peer, &limiter.hasher);
        let counter = limiter.slots.get(slot)?;
        let mut cur = counter.load(Ordering::Relaxed);
        loop {
            if cur >= limiter.limit {
                return None;
            }
            match counter.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    return Some(PerIpConnGuard {
                        limiter: limiter.clone(),
                        slot,
                    });
                }
                Err(actual) => cur = actual,
            }
        }
    }
}

/// Releases its source-IP slot when the connection it was issued for ends.
pub(crate) struct PerIpConnGuard {
    limiter: Arc<PerIpConnLimit>,
    slot: usize,
}

impl Drop for PerIpConnGuard {
    fn drop(&mut self) {
        if let Some(counter) = self.limiter.slots.get(self.slot) {
            counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// The fixed 8-byte key a peer hashes on: v4 /32 (all four octets) or v6 /64 (the top eight
/// octets) — coarsened so a single host cannot evade its cap by rotating addresses within a /64
/// it controls (the common IPv6 end-site allocation unit). `plecto_control::ratelimit` applies
/// the identical coarsening to its `client-ip` rate-limit key, for the same reason; keeping both
/// limiters consistent means an operator reasons about one IPv6 threat model, not two. An
/// IPv4-mapped IPv6 peer (`::ffff:a.b.c.d`, how a dual-stack accept reports an IPv4 client)
/// collapses to its v4 form first.
fn ip_key_bytes(peer: IpAddr) -> [u8; 8] {
    let peer = match peer {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    };
    match peer {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            [o[0], o[1], o[2], o[3], 0, 0, 0, 0]
        }
        IpAddr::V6(v6) => {
            let o = v6.octets();
            [o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7]]
        }
    }
}

fn slot_for_ip(peer: IpAddr, hasher: &RandomState) -> usize {
    let mut h = hasher.build_hasher();
    h.write(&ip_key_bytes(peer));
    (h.finish() as usize) & (IP_SLOTS - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn admits_up_to_the_limit_then_refuses() {
        let limiter = Arc::new(PerIpConnLimit::new(2));
        let peer = ip("203.0.113.7");
        let g1 = PerIpConnLimit::try_acquire(&limiter, peer);
        let g2 = PerIpConnLimit::try_acquire(&limiter, peer);
        let g3 = PerIpConnLimit::try_acquire(&limiter, peer);
        assert!(g1.is_some(), "1st connection must be admitted");
        assert!(
            g2.is_some(),
            "2nd connection (at the limit) must be admitted"
        );
        assert!(g3.is_none(), "3rd connection must be refused (limit is 2)");
    }

    #[test]
    fn releasing_a_guard_frees_a_slot() {
        let limiter = Arc::new(PerIpConnLimit::new(1));
        let peer = ip("203.0.113.7");
        let g1 = PerIpConnLimit::try_acquire(&limiter, peer).expect("1st admitted");
        assert!(
            PerIpConnLimit::try_acquire(&limiter, peer).is_none(),
            "at the limit while g1 is held"
        );
        drop(g1);
        assert!(
            PerIpConnLimit::try_acquire(&limiter, peer).is_some(),
            "dropping the guard must free the slot"
        );
    }

    #[test]
    fn distinct_peers_get_independent_counters() {
        let limiter = Arc::new(PerIpConnLimit::new(1));
        let drained = ip("192.0.2.255");
        let _g = PerIpConnLimit::try_acquire(&limiter, drained).expect("drained peer admitted");
        assert!(PerIpConnLimit::try_acquire(&limiter, drained).is_none());

        let allowed = (0..=255u8)
            .map(|n| IpAddr::V4(Ipv4Addr::new(192, 0, 2, n)))
            .filter(|p| *p != drained)
            .filter(|p| PerIpConnLimit::try_acquire(&limiter, *p).is_some())
            .count();
        assert!(
            allowed >= 250,
            "distinct peers get independent slots (got {allowed}/255 admitted)"
        );
    }

    #[test]
    fn ipv4_mapped_v6_collapses_to_v4_key() {
        let v4 = ip("198.51.100.9");
        let mapped = IpAddr::V6(Ipv4Addr::new(198, 51, 100, 9).to_ipv6_mapped());
        assert_eq!(ip_key_bytes(v4), ip_key_bytes(mapped));
    }

    #[test]
    fn ipv6_key_is_the_64_prefix() {
        let a = IpAddr::V6("2001:db8:abcd:1::1".parse::<Ipv6Addr>().unwrap());
        let b = IpAddr::V6(
            "2001:db8:abcd:1:ffff:ffff:ffff:ffff"
                .parse::<Ipv6Addr>()
                .unwrap(),
        );
        let other = IpAddr::V6("2001:db8:abcd:2::1".parse::<Ipv6Addr>().unwrap());
        assert_eq!(ip_key_bytes(a), ip_key_bytes(b));
        assert_ne!(ip_key_bytes(a), ip_key_bytes(other));
    }

    #[test]
    fn ipv6_64_prefix_shares_one_slot_across_the_limit() {
        // Two addresses in the same /64 must share the cap: draining one exhausts the other's
        // budget too — this is the intended anti-rotation-evasion behaviour, not a bug.
        let limiter = Arc::new(PerIpConnLimit::new(1));
        let a = ip("2001:db8:abcd:1::1");
        let b = ip("2001:db8:abcd:1::2");
        let _g = PerIpConnLimit::try_acquire(&limiter, a).expect("first address admitted");
        assert!(
            PerIpConnLimit::try_acquire(&limiter, b).is_none(),
            "a same-/64 address must share the exhausted slot"
        );
    }
}
