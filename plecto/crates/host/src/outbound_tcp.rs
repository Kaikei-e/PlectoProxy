//! The SSRF-guarded outbound TCP wiring (ADR 000060): lends the `wasi:sockets` TCP-connect
//! vocabulary behind two seams the host owns.
//!
//! Unlike outbound HTTP — where `WasiHttpHooks::send_request` hands the host the whole
//! name→connect sequence — `wasi:sockets` splits it: the guest drives the connect, and the host's
//! say is (a) the `ip-name-lookup` implementation and (b) the `socket_addr_check` called on every
//! connect. This module owns both and ties them together with an IP pin:
//!
//! 1. **Name-resolution vetting** — the upstream `ip-name-lookup` has no hostname filter (only a
//!    boolean allow), so the host substitutes its own implementation: only allowlisted names
//!    resolve; the host resolves them itself, classifies EVERY resolved address with the shared
//!    SSRF guard ([`crate::outbound::classify`], the same floor as outbound HTTP), rejects the
//!    lookup wholesale if any address is blocked (a mixed / rebinding A-record set never leaks a
//!    partial result), and records the vetted addresses in the per-Store **pinned set**.
//! 2. **Connect vetting** — `socket_addr_check` (invoked by wasmtime-wasi on `TcpConnect`)
//!    requires the destination to pass the SSRF floor AND to match an allowlist entry for that
//!    port whose host is either the destination IP literal or a name the host itself pinned to
//!    that IP. A guest cannot connect to an address it did not obtain through the vetted lookup
//!    (or that the operator did not list literally) — resolution cannot be bypassed. `TcpBind`
//!    and every UDP use are denied outright (no listen, no UDP; the UDP interfaces are not even
//!    linked).
//! 3. **Resource bounds** — a per-request connect budget (`max_connections`, reset by
//!    `begin_request`; held connections on a pooled instance cost only their opening request) and
//!    a wall-clock deadline on each guest hook call (`io_deadline`, enforced in
//!    `WasmtimeRuntime::drive_call` — epoch interruption cannot reach a guest blocked in host
//!    socket I/O, and with raw TCP the host cannot bound individual reads the way outbound HTTP's
//!    `total_timeout` bounds one call).
//!
//! Every denial reaches the guest as a `wasi:sockets` `error-code`, never a silent success —
//! fail-closed.

use std::collections::{HashMap, HashSet};
use std::mem;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use wasmtime::component::{HasData, Resource, ResourceTable};
use wasmtime_wasi::p2::bindings::sockets::ip_name_lookup::{
    Host, HostResolveAddressStream, ResolveAddressStream,
};
use wasmtime_wasi::p2::bindings::sockets::network::{self, ErrorCode, IpAddress, Network};
use wasmtime_wasi::p2::{DynPollable, SocketError, subscribe};
use wasmtime_wasi::sockets::SocketAddrUse;

use crate::outbound::{AddrVerdict, OutboundTcpPolicy};
use crate::resolver::Resolver;

/// Cap on distinct pinned IPs per Store. Entries are host-vetted, but an allowlisted name that
/// rotates addresses could otherwise grow the map for the life of a pooled instance; at the cap,
/// resolution of NEW addresses fails (fail-closed) until the pool recycles the instance.
const MAX_PINNED_IPS: usize = 1024;

/// Per-filter outbound TCP state, held by the loaded filter and shared across its requests.
pub(crate) struct OutboundTcpState {
    policy: Arc<OutboundTcpPolicy>,
    resolver: Arc<Resolver>,
}

impl OutboundTcpState {
    pub(crate) fn new(policy: OutboundTcpPolicy, resolver: Resolver) -> Self {
        Self {
            policy: Arc::new(policy),
            resolver: Arc::new(resolver),
        }
    }

    /// The wall-clock ceiling for each guest hook call of this filter (`drive_call`).
    pub(crate) fn io_deadline(&self) -> Duration {
        self.policy.io_deadline
    }

    /// A fresh per-Store guard: its pinned set and connect budget belong to one instance.
    pub(crate) fn guard(&self) -> TcpGuard {
        TcpGuard {
            inner: Some(Arc::new(GuardInner {
                policy: self.policy.clone(),
                resolver: self.resolver.clone(),
                pinned: Mutex::new(HashMap::new()),
                connects: AtomicU32::new(0),
            })),
        }
    }
}

/// The per-Store guard shared between the `socket_addr_check` closure and the host's
/// ip-name-lookup implementation. `inner: None` denies everything — the handle installed for
/// filters without an outbound TCP policy (belt-and-suspenders: those filters link no
/// `wasi:sockets` and cannot reach this at all).
#[derive(Clone)]
pub(crate) struct TcpGuard {
    inner: Option<Arc<GuardInner>>,
}

struct GuardInner {
    policy: Arc<OutboundTcpPolicy>,
    resolver: Arc<Resolver>,
    /// The IP pin: addresses the host itself resolved (and classified) per allowlisted name that
    /// yielded them. Names are stored lowercased. Never reset within a Store's life — every entry
    /// is host-vetted, and growth is capped by [`MAX_PINNED_IPS`] + pool recycling.
    pinned: Mutex<HashMap<IpAddr, HashSet<String>>>,
    /// Connects consumed by the current request (reset by `begin_request`).
    connects: AtomicU32,
}

impl TcpGuard {
    /// The deny-everything guard for filters with no outbound TCP policy.
    pub(crate) fn deny_all() -> Self {
        Self { inner: None }
    }

    /// Reset the per-request connect budget (called from `HostState::begin_request`).
    pub(crate) fn begin_request(&self) {
        if let Some(inner) = &self.inner {
            inner.connects.store(0, Ordering::SeqCst);
        }
    }

    /// Configure a shared `WasiCtxBuilder` for a Store guarded by this handle: every
    /// socket-address use funnels into [`TcpGuard::permits`], UDP is disabled outright, and the
    /// upstream ip-name-lookup permission stays at its deny default (the host's own lookup
    /// implementation replaces it). A builder (not a built `WasiCtx`) so `HostState::new` can
    /// compose this with the fat-guest stdio wiring (ADR 000063) on the same builder.
    pub(crate) fn configure_wasi_ctx(&self, builder: &mut wasmtime_wasi::WasiCtxBuilder) {
        if self.inner.is_some() {
            let guard = self.clone();
            builder.socket_addr_check(move |addr, addr_use| {
                let verdict = guard.permits(addr, addr_use);
                Box::pin(std::future::ready(verdict))
            });
            builder.allow_udp(false);
        }
        // No policy: the builder's defaults already deny every socket address.
    }

    /// The connect gate. Pure decision logic (no I/O), so the deny paths are directly
    /// unit-testable: TCP connect only, SSRF floor + private opt-in on the destination, an
    /// allowlist entry for the port whose host is the destination literal or pinned to it, and
    /// the per-request budget — consumed LAST, only by an otherwise-permitted connect.
    fn permits(&self, addr: SocketAddr, addr_use: SocketAddrUse) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        match addr_use {
            SocketAddrUse::TcpConnect => {}
            // No bind (and thus no listen), no UDP in any form.
            SocketAddrUse::TcpBind
            | SocketAddrUse::UdpBind
            | SocketAddrUse::UdpConnect
            | SocketAddrUse::UdpOutgoingDatagram => return false,
        }
        let ip = addr.ip().to_canonical();
        if inner.policy.classify(ip) != AddrVerdict::Allowed {
            return false;
        }
        let matched = {
            let pinned = inner.pinned.lock();
            let names = pinned.get(&ip);
            inner.policy.allow.iter().any(|entry| {
                entry.port == addr.port()
                    && (entry
                        .host
                        .parse::<IpAddr>()
                        .is_ok_and(|lit| lit.to_canonical() == ip)
                        || names.is_some_and(|n| n.contains(&entry.host.to_ascii_lowercase())))
            })
        };
        if !matched {
            return false;
        }
        inner
            .connects
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < inner.policy.max_connections).then(|| used + 1)
            })
            .is_ok()
    }
}

/// `HasData` marker selecting [`TcpLookupView`] as the ip-name-lookup host data.
pub(crate) struct PlectoTcpLookup;

impl HasData for PlectoTcpLookup {
    type Data<'a> = TcpLookupView<'a>;
}

/// The host's own `wasi:sockets/ip-name-lookup` implementation (vetted resolution). Borrows the
/// Store's resource table plus its guard from `HostState`.
pub(crate) struct TcpLookupView<'a> {
    pub(crate) table: &'a mut ResourceTable,
    pub(crate) guard: &'a TcpGuard,
}

impl Host for TcpLookupView<'_> {
    fn resolve_addresses(
        &mut self,
        _network: Resource<Network>,
        name: String,
    ) -> Result<Resource<ResolveAddressStream>, SocketError> {
        let Some(inner) = self.guard.inner.as_ref() else {
            return Err(ErrorCode::PermanentResolverFailure.into());
        };
        let lower = name.to_ascii_lowercase();
        // Deny-by-default BEFORE any DNS: a name off the allowlist never resolves.
        if !inner.policy.allows_name(&lower) {
            return Err(ErrorCode::PermanentResolverFailure.into());
        }
        let inner = inner.clone();
        let task = wasmtime_wasi::runtime::spawn(async move {
            let addrs = inner.resolver.resolve(&lower, 0).await.map_err(|e| {
                tracing::debug!(host = %lower, error = %e, "outbound-tcp DNS resolution failed");
                SocketError::from(ErrorCode::NameUnresolvable)
            })?;
            if addrs.is_empty() {
                return Err(ErrorCode::NameUnresolvable.into());
            }
            let ips: Vec<IpAddr> = addrs.iter().map(|a| a.ip().to_canonical()).collect();
            // Classify EVERY resolved address; a single blocked one rejects the whole lookup
            // (the DNS-rebinding / mixed-record guard, same strictness as outbound HTTP).
            if ips
                .iter()
                .any(|ip| inner.policy.classify(*ip) != AddrVerdict::Allowed)
            {
                return Err(ErrorCode::PermanentResolverFailure.into());
            }
            // Pin: record every vetted address under the name that produced it, so the connect
            // gate can require "an IP this host resolved for that allowlisted name".
            {
                let mut pinned = inner.pinned.lock();
                for ip in &ips {
                    if !pinned.contains_key(ip) && pinned.len() >= MAX_PINNED_IPS {
                        return Err(ErrorCode::PermanentResolverFailure.into());
                    }
                    pinned.entry(*ip).or_default().insert(lower.clone());
                }
            }
            Ok(ips.into_iter().map(IpAddress::from).collect::<Vec<_>>())
        });
        let resource = self.table.push(ResolveAddressStream::Waiting(task))?;
        Ok(resource)
    }
}

// The ip-name-lookup interface `use`s types from wasi:sockets/network, so its `add_to_linker`
// requires the same data type to carry the network Host glue (error conversion + the `network`
// resource drop). Mirrors the upstream `WasiSocketsCtxView` impls over the shared table.
impl network::Host for TcpLookupView<'_> {
    fn convert_error_code(&mut self, error: SocketError) -> wasmtime::Result<ErrorCode> {
        error.downcast()
    }

    fn network_error_code(
        &mut self,
        err: Resource<wasmtime::Error>,
    ) -> wasmtime::Result<Option<ErrorCode>> {
        let err = self.table.get(&err)?;
        if let Some(err) = err.downcast_ref::<std::io::Error>() {
            return Ok(Some(ErrorCode::from(err)));
        }
        Ok(None)
    }
}

impl network::HostNetwork for TcpLookupView<'_> {
    fn drop(&mut self, this: Resource<Network>) -> wasmtime::Result<()> {
        self.table.delete(this)?;
        Ok(())
    }
}

impl HostResolveAddressStream for TcpLookupView<'_> {
    fn resolve_next_address(
        &mut self,
        resource: Resource<ResolveAddressStream>,
    ) -> Result<Option<IpAddress>, SocketError> {
        let stream: &mut ResolveAddressStream = self.table.get_mut(&resource)?;
        loop {
            match stream {
                ResolveAddressStream::Waiting(future) => {
                    match wasmtime_wasi::runtime::poll_noop(Pin::new(future)) {
                        Some(result) => {
                            *stream = ResolveAddressStream::Done(result.map(|v| v.into_iter()));
                        }
                        None => return Err(ErrorCode::WouldBlock.into()),
                    }
                }
                ResolveAddressStream::Done(Ok(iter)) => return Ok(iter.next()),
                ResolveAddressStream::Done(slot @ Err(_)) => {
                    // Surface the error once; later polls see an exhausted (empty) stream. The
                    // Ok arm is unreachable given the match guard but stays panic-free (bp-rust:
                    // no data-plane panics).
                    return match mem::replace(slot, Ok(Vec::new().into_iter())) {
                        Err(e) => Err(e),
                        Ok(_) => Ok(None),
                    };
                }
            }
        }
    }

    fn subscribe(
        &mut self,
        resource: Resource<ResolveAddressStream>,
    ) -> wasmtime::Result<Resource<DynPollable>> {
        subscribe(self.table, resource)
    }

    fn drop(&mut self, resource: Resource<ResolveAddressStream>) -> wasmtime::Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;
    use std::time::Duration;

    use crate::outbound::TcpAllowEntry;

    fn policy(
        allow: Vec<TcpAllowEntry>,
        allow_private: Vec<&str>,
        budget: u32,
    ) -> OutboundTcpPolicy {
        OutboundTcpPolicy {
            allow,
            allow_private: allow_private.iter().map(|c| c.parse().unwrap()).collect(),
            max_connections: budget,
            io_deadline: Duration::from_secs(5),
        }
    }

    fn entry(host: &str, port: u16) -> TcpAllowEntry {
        TcpAllowEntry {
            host: host.to_string(),
            port,
        }
    }

    fn guard(policy: OutboundTcpPolicy) -> TcpGuard {
        OutboundTcpState::new(policy, Resolver::Static(StdHashMap::new())).guard()
    }

    /// Test seam: record `ip` as host-resolved for `name`, as the vetted lookup would.
    fn pin(g: &TcpGuard, name: &str, ip: IpAddr) {
        g.inner
            .as_ref()
            .unwrap()
            .pinned
            .lock()
            .entry(ip)
            .or_default()
            .insert(name.to_ascii_lowercase());
    }

    fn addr(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), port)
    }

    #[test]
    fn deny_all_guard_permits_nothing() {
        let g = TcpGuard::deny_all();
        assert!(!g.permits(addr("93.184.216.34", 443), SocketAddrUse::TcpConnect));
    }

    #[test]
    fn bind_and_udp_uses_are_denied_even_for_an_allowed_destination() {
        // The capability is active TCP connect ONLY (ADR 000060): no listen, no UDP — even to an
        // address that would pass every connect check.
        let g = guard(policy(vec![entry("8.8.8.8", 853)], vec![], 4));
        let dest = addr("8.8.8.8", 853);
        assert!(g.permits(dest, SocketAddrUse::TcpConnect));
        for blocked in [
            SocketAddrUse::TcpBind,
            SocketAddrUse::UdpBind,
            SocketAddrUse::UdpConnect,
            SocketAddrUse::UdpOutgoingDatagram,
        ] {
            assert!(!g.permits(dest, blocked), "{blocked:?} must be denied");
        }
    }

    #[test]
    fn reserved_floor_cannot_be_opted_into_at_connect() {
        // Loopback/metadata stay blocked even with the widest possible opt-in AND a literal
        // allowlist entry — the floor is not negotiable (same invariant as outbound-http).
        let g = guard(policy(
            vec![entry("127.0.0.1", 6379), entry("169.254.169.254", 80)],
            vec!["0.0.0.0/0"],
            4,
        ));
        assert!(!g.permits(addr("127.0.0.1", 6379), SocketAddrUse::TcpConnect));
        assert!(!g.permits(addr("169.254.169.254", 80), SocketAddrUse::TcpConnect));
    }

    #[test]
    fn ip_literal_entry_matches_including_v4_mapped_smuggling() {
        let g = guard(policy(vec![entry("8.8.8.8", 853)], vec![], 4));
        assert!(g.permits(addr("8.8.8.8", 853), SocketAddrUse::TcpConnect));
        // same address wrapped as v4-mapped v6 canonicalizes back to the entry
        assert!(g.permits(addr("::ffff:8.8.8.8", 853), SocketAddrUse::TcpConnect));
        // unlisted address / wrong port stay denied
        assert!(!g.permits(addr("8.8.4.4", 853), SocketAddrUse::TcpConnect));
        assert!(!g.permits(addr("8.8.8.8", 443), SocketAddrUse::TcpConnect));
    }

    #[test]
    fn named_entry_requires_the_host_side_pin() {
        // A destination reached by NAME is only connectable at an IP this host itself resolved
        // for that name — a guest cannot conjure an address and dial it (resolution cannot be
        // bypassed).
        let g = guard(policy(vec![entry("redis.internal", 6379)], vec![], 4));
        let dest = addr("93.184.216.34", 6379);
        assert!(
            !g.permits(dest, SocketAddrUse::TcpConnect),
            "unpinned: denied"
        );
        pin(&g, "REDIS.internal", "93.184.216.34".parse().unwrap());
        assert!(
            g.permits(dest, SocketAddrUse::TcpConnect),
            "pinned: allowed"
        );
    }

    #[test]
    fn pin_does_not_cross_pair_with_another_entrys_port() {
        // An IP pinned for entry A must not open entry B's port: the (host, port) pair is the
        // allowlist unit, and the pin is per-name.
        let g = guard(policy(
            vec![
                entry("redis.internal", 6379),
                entry("memcached.internal", 11211),
            ],
            vec![],
            4,
        ));
        let redis_ip: IpAddr = "93.184.216.34".parse().unwrap();
        pin(&g, "redis.internal", redis_ip);
        assert!(g.permits(SocketAddr::new(redis_ip, 6379), SocketAddrUse::TcpConnect));
        assert!(
            !g.permits(SocketAddr::new(redis_ip, 11211), SocketAddrUse::TcpConnect),
            "redis's pinned IP must not open memcached's port"
        );
    }

    #[test]
    fn private_destination_needs_the_cidr_optin() {
        let denied = guard(policy(vec![entry("10.1.2.3", 6379)], vec![], 4));
        assert!(!denied.permits(addr("10.1.2.3", 6379), SocketAddrUse::TcpConnect));
        let allowed = guard(policy(
            vec![entry("10.1.2.3", 6379)],
            vec!["10.1.0.0/16"],
            4,
        ));
        assert!(allowed.permits(addr("10.1.2.3", 6379), SocketAddrUse::TcpConnect));
    }

    #[test]
    fn connect_budget_is_consumed_only_by_permitted_connects_and_resets_per_request() {
        let g = guard(policy(vec![entry("8.8.8.8", 853)], vec![], 2));
        let dest = addr("8.8.8.8", 853);
        // denied attempts do not consume the budget
        for _ in 0..10 {
            assert!(!g.permits(addr("8.8.4.4", 853), SocketAddrUse::TcpConnect));
        }
        assert!(g.permits(dest, SocketAddrUse::TcpConnect));
        assert!(g.permits(dest, SocketAddrUse::TcpConnect));
        assert!(
            !g.permits(dest, SocketAddrUse::TcpConnect),
            "the third connect exceeds the per-request budget"
        );
        g.begin_request();
        assert!(
            g.permits(dest, SocketAddrUse::TcpConnect),
            "a new request starts with a fresh budget"
        );
    }
}
