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
//!    (or that the operator did not list literally) — resolution cannot be bypassed. Listening,
//!    accepting and every UDP use are denied outright; of the bind checks only the wildcard bind
//!    that `connect` itself implies passes (no listen, no UDP; the UDP interfaces are not even
//!    linked).
//! 3. **Resource bounds** — live socket ceilings of 64 per Store and 1,024 per Host (shared
//!    across filters and overlapping reloads), a per-request connect budget (`max_connections`, reset by
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
use wasmtime_wasi::p2::{DynPollable, Pollable, SocketError, subscribe};
use wasmtime_wasi::sockets::SocketAddrUse;

use crate::outbound::{AddrVerdict, OutboundTcpPolicy};
use crate::resolver::Resolver;

/// Cap on distinct pinned IPs per Store. Entries are host-vetted, but an allowlisted name that
/// rotates addresses could otherwise grow the map for the life of a pooled instance; at the cap,
/// resolution of NEW addresses fails (fail-closed) until the pool recycles the instance.
const MAX_PINNED_IPS: usize = 1024;
/// A single Store may keep at most this many native TCP socket descriptors alive. This is
/// deliberately independent from the per-request connect fan-out budget: trusted instances may
/// legitimately retain one Redis connection, but they must not accumulate descriptors forever.
const MAX_LIVE_TCP_SOCKETS_PER_STORE: u32 = 64;

/// Host-wide ceiling for native TCP descriptors lent to filters. It is shared by every filter
/// loaded through one [`crate::Host`], including a replacement while reload overlaps the old one.
pub(crate) const MAX_LIVE_TCP_SOCKETS_PER_HOST: u32 = 1024;

/// Host-owned descriptor accounting shared by every outbound-TCP filter. The counter is only
/// changed after a Store has reserved its own slot, so a noisy filter cannot exceed either bound.
pub(crate) struct TcpSocketQuota {
    used: AtomicU32,
    limit: u32,
}

impl TcpSocketQuota {
    pub(crate) fn new() -> Self {
        Self {
            used: AtomicU32::new(0),
            limit: MAX_LIVE_TCP_SOCKETS_PER_HOST,
        }
    }

    #[cfg(test)]
    fn with_limit(limit: u32) -> Self {
        Self {
            used: AtomicU32::new(0),
            limit,
        }
    }

    fn try_acquire(&self) -> bool {
        self.used
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < self.limit).then(|| used + 1)
            })
            .is_ok()
    }

    fn release(&self, count: u32) {
        if count == 0 {
            return;
        }
        // Every release follows a successful reservation. Do not underflow: a duplicate release
        // must never free another Store's descriptor slot.
        if self
            .used
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_sub(count)
            })
            .is_err()
        {
            tracing::error!(
                count,
                "outbound TCP socket quota release exceeded reservations"
            );
        }
    }

    #[cfg(test)]
    fn used(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }
}

/// Per-filter outbound TCP state, held by the loaded filter and shared across its requests.
pub(crate) struct OutboundTcpState {
    policy: Arc<OutboundTcpPolicy>,
    resolver: Arc<Resolver>,
    socket_quota: Arc<TcpSocketQuota>,
}

impl OutboundTcpState {
    pub(crate) fn new(
        policy: OutboundTcpPolicy,
        resolver: Resolver,
        socket_quota: Arc<TcpSocketQuota>,
    ) -> Self {
        Self {
            policy: Arc::new(policy),
            resolver: Arc::new(resolver),
            socket_quota,
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
                live_sockets: AtomicU32::new(0),
                socket_quota: self.socket_quota.clone(),
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
    /// Native TCP descriptors still owned by socket resources or P3 stream halves. Unlike
    /// `connects`, this spans requests on a trusted instance.
    live_sockets: AtomicU32,
    socket_quota: Arc<TcpSocketQuota>,
}

impl Drop for GuardInner {
    fn drop(&mut self) {
        // ResourceTable is dropped before HostState's guard field, so this is the final cleanup
        // for sockets that a guest retained until an instance was discarded or a deadline trapped.
        self.socket_quota
            .release(self.live_sockets.swap(0, Ordering::SeqCst));
    }
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

    /// Reserve one native descriptor before Wasmtime creates its TCP socket. The Store and host
    /// limits are separate so a single retained instance cannot consume the process-wide pool.
    fn try_acquire_live_socket(&self) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        if inner
            .live_sockets
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < MAX_LIVE_TCP_SOCKETS_PER_STORE).then(|| used + 1)
            })
            .is_err()
        {
            return false;
        }
        if inner.socket_quota.try_acquire() {
            return true;
        }
        inner.live_sockets.fetch_sub(1, Ordering::SeqCst);
        false
    }

    /// Return a reservation only after the native socket resource was successfully deleted.
    fn release_live_socket(&self) {
        let Some(inner) = &self.inner else {
            return;
        };
        if inner
            .live_sockets
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used > 0).then(|| used - 1)
            })
            .is_ok()
        {
            inner.socket_quota.release(1);
        }
    }

    /// Configure a shared `WasiCtxBuilder` for a Store guarded by this handle: TCP is enabled at
    /// all, every socket-address use funnels into [`TcpGuard::permits`], UDP is disabled outright,
    /// and the upstream ip-name-lookup permission stays at its deny default (the host's own lookup
    /// implementation replaces it). A builder (not a built `WasiCtx`) so `HostState::new` can
    /// compose this with the fat-guest stdio wiring (ADR 000063) on the same builder.
    pub(crate) fn configure_wasi_ctx(&self, builder: &mut wasmtime_wasi::WasiCtxBuilder) {
        if self.inner.is_some() {
            let guard = self.clone();
            builder.socket_addr_check(move |addr, addr_use| {
                let verdict = guard.permits(addr, addr_use);
                Box::pin(std::future::ready(verdict))
            });
            // Creating a TCP socket is a permission of its own, denied by default and checked
            // before any address is in play; the per-address gate above is what keeps it narrow.
            builder.allow_tcp(true);
            builder.allow_udp(false);
        }
        // No policy: the builder's defaults deny socket creation itself, TCP and UDP alike.
    }

    /// The connect gate. Pure decision logic (no I/O), so the deny paths are directly
    /// unit-testable: TCP connect only (plus the wildcard bind connect itself implies), the SSRF
    /// floor with private opt-in on the destination, an allowlist entry for the port whose host
    /// is the destination literal or pinned to it, and the per-request budget — consumed LAST,
    /// only by an otherwise-permitted connect.
    fn permits(&self, addr: SocketAddr, addr_use: SocketAddrUse) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        match addr_use {
            SocketAddrUse::TcpConnect => {}
            // A connect on an unbound socket asks permission for the implicit bind the OS is about
            // to perform, passing the wildcard address (`0.0.0.0:0` / `[::]:0`); denying it would
            // deny connect itself. Nothing else follows from the carve-out — a wildcard-bound
            // socket receives nothing without `listen`, which is its own check below.
            SocketAddrUse::TcpBind => return addr.ip().is_unspecified() && addr.port() == 0,
            // Listen and accept are now checks in their own right, and UDP stays off in every
            // form: the capability is outbound connect only.
            SocketAddrUse::TcpListen
            | SocketAddrUse::TcpAccept
            | SocketAddrUse::UdpBind
            | SocketAddrUse::UdpSend
            | SocketAddrUse::UdpReceive => return false,
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
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < inner.policy.max_connections).then(|| used + 1)
            })
            .is_ok()
    }
}

/// `HasData` marker for the two TCP interfaces whose resource lifetime Plecto accounts for.
/// Network creation and name lookup retain their existing upstream/custom markers; changing only
/// these interfaces keeps the capability surface exactly as before.
pub(crate) struct PlectoTcpSockets;

impl HasData for PlectoTcpSockets {
    type Data<'a> = TcpSocketsView<'a>;
}

/// A narrow forwarding view over Wasmtime's socket host implementation. Socket creation reserves
/// a host-owned descriptor lease. P2 streams are resource children; P3 streams share an RAII
/// lease with the socket, so deleting a socket cannot make its still-live streams free.
pub(crate) struct TcpSocketsView<'a> {
    pub(crate) sockets: wasmtime_wasi::sockets::WasiSocketsCtxView<'a>,
    pub(crate) guard: TcpGuard,
    pub(crate) p3_leases: &'a mut HashMap<u32, Arc<P3SocketLease>>,
}

/// Shared by the P3 socket resource and its send/receive completion consumers. Wasmtime's
/// completion resolves only once its native stream half is closed, including cancellation.
/// Keeping the consumer host-owned prevents a guest from releasing the lease by dropping the
/// guest-visible completion future early.
pub(crate) struct P3SocketLease(TcpGuard);

impl Drop for P3SocketLease {
    fn drop(&mut self) {
        self.0.release_live_socket();
    }
}

type P3StreamResult = Result<(), wasmtime_wasi::p3::bindings::sockets::types::ErrorCode>;

struct P3StreamCompletion {
    result: Option<tokio::sync::oneshot::Sender<P3StreamResult>>,
    _lease: Arc<P3SocketLease>,
}

impl<T: 'static> wasmtime::component::FutureConsumer<T> for P3StreamCompletion {
    type Item = P3StreamResult;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        mut store: wasmtime::StoreContextMut<'_, T>,
        mut source: wasmtime::component::Source<'_, Self::Item>,
        _finish: bool,
    ) -> std::task::Poll<wasmtime::Result<()>> {
        let mut value = None;
        source.read(&mut store, &mut value)?;
        if let (Some(tx), Some(value)) = (self.result.take(), value) {
            let _ = tx.send(value);
        }
        std::task::Poll::Ready(Ok(()))
    }
}

fn retain_p3_stream_lease<T: 'static>(
    mut store: wasmtime::StoreContextMut<'_, T>,
    completion: wasmtime::component::FutureReader<P3StreamResult>,
    lease: Arc<P3SocketLease>,
) -> wasmtime::Result<wasmtime::component::FutureReader<P3StreamResult>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    completion.pipe(
        &mut store,
        P3StreamCompletion {
            result: Some(tx),
            _lease: lease,
        },
    )?;
    wasmtime::component::FutureReader::new(&mut store, async move {
        rx.await.map_err(wasmtime::Error::from)
    })
}

impl wasmtime_wasi::p2::bindings::sockets::tcp_create_socket::Host for TcpSocketsView<'_> {
    fn create_tcp_socket(
        &mut self,
        address_family: wasmtime_wasi::p2::bindings::sockets::network::IpAddressFamily,
    ) -> Result<
        Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        wasmtime_wasi::p2::SocketError,
    > {
        if !self.guard.try_acquire_live_socket() {
            return Err(
                wasmtime_wasi::p2::bindings::sockets::network::ErrorCode::NewSocketLimit.into(),
            );
        }
        let result = <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp_create_socket::Host>::create_tcp_socket(
            &mut self.sockets,
            address_family,
        );
        if result.is_err() {
            self.guard.release_live_socket();
        }
        result
    }
}

impl wasmtime_wasi::p2::bindings::sockets::tcp::Host for TcpSocketsView<'_> {}

// The TCP WIT interfaces `use` network's error conversion type, so their linker requires this
// projection even though `wasi:sockets/network` itself remains wired through `WasiSockets`.
impl wasmtime_wasi::p2::bindings::sockets::network::Host for TcpSocketsView<'_> {
    fn convert_error_code(
        &mut self,
        error: wasmtime_wasi::p2::SocketError,
    ) -> wasmtime::Result<wasmtime_wasi::p2::bindings::sockets::network::ErrorCode> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::network::Host>::convert_error_code(&mut self.sockets, error)
    }

    fn network_error_code(
        &mut self,
        err: Resource<wasmtime::Error>,
    ) -> wasmtime::Result<Option<wasmtime_wasi::p2::bindings::sockets::network::ErrorCode>> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::network::Host>::network_error_code(&mut self.sockets, err)
    }
}

impl wasmtime_wasi::p2::bindings::sockets::network::HostNetwork for TcpSocketsView<'_> {
    fn drop(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::network::Network>,
    ) -> wasmtime::Result<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::network::HostNetwork>::drop(&mut self.sockets, this)
    }
}

impl wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket for TcpSocketsView<'_> {
    async fn start_bind(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        network: Resource<wasmtime_wasi::p2::bindings::sockets::network::Network>,
        local_address: wasmtime_wasi::p2::bindings::sockets::network::IpSocketAddress,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::start_bind(&mut self.sockets, this, network, local_address).await
    }

    fn finish_bind(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::finish_bind(&mut self.sockets, this)
    }

    fn start_connect(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        network: Resource<wasmtime_wasi::p2::bindings::sockets::network::Network>,
        remote_address: wasmtime_wasi::p2::bindings::sockets::network::IpSocketAddress,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::start_connect(&mut self.sockets, this, network, remote_address)
    }

    fn finish_connect(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<(
        Resource<wasmtime_wasi::p2::bindings::sockets::tcp::InputStream>,
        Resource<wasmtime_wasi::p2::bindings::sockets::tcp::OutputStream>,
    )> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::finish_connect(&mut self.sockets, this)
    }

    async fn start_listen(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::start_listen(&mut self.sockets, this).await
    }

    fn finish_listen(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::finish_listen(&mut self.sockets, this)
    }

    fn accept(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<(
        Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        Resource<wasmtime_wasi::p2::bindings::sockets::tcp::InputStream>,
        Resource<wasmtime_wasi::p2::bindings::sockets::tcp::OutputStream>,
    )> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::accept(&mut self.sockets, this)
    }

    fn local_address(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<
        wasmtime_wasi::p2::bindings::sockets::network::IpSocketAddress,
    > {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::local_address(&mut self.sockets, this)
    }
    fn remote_address(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<
        wasmtime_wasi::p2::bindings::sockets::network::IpSocketAddress,
    > {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::remote_address(&mut self.sockets, this)
    }
    fn is_listening(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime::Result<bool> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::is_listening(&mut self.sockets, this)
    }
    fn address_family(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime::Result<wasmtime_wasi::p2::bindings::sockets::network::IpAddressFamily> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::address_family(&mut self.sockets, this)
    }
    fn set_listen_backlog_size(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        value: u64,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::set_listen_backlog_size(&mut self.sockets, this, value)
    }
    fn keep_alive_enabled(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<bool> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::keep_alive_enabled(&mut self.sockets, this)
    }
    fn set_keep_alive_enabled(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        value: bool,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::set_keep_alive_enabled(&mut self.sockets, this, value)
    }
    fn keep_alive_idle_time(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<u64> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::keep_alive_idle_time(&mut self.sockets, this)
    }
    fn set_keep_alive_idle_time(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        value: u64,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::set_keep_alive_idle_time(&mut self.sockets, this, value)
    }
    fn keep_alive_interval(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<u64> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::keep_alive_interval(&mut self.sockets, this)
    }
    fn set_keep_alive_interval(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        value: u64,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::set_keep_alive_interval(&mut self.sockets, this, value)
    }
    fn keep_alive_count(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<u32> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::keep_alive_count(&mut self.sockets, this)
    }
    fn set_keep_alive_count(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        value: u32,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::set_keep_alive_count(&mut self.sockets, this, value)
    }
    fn hop_limit(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<u8> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::hop_limit(&mut self.sockets, this)
    }
    fn set_hop_limit(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        value: u8,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::set_hop_limit(&mut self.sockets, this, value)
    }
    fn receive_buffer_size(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<u64> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::receive_buffer_size(&mut self.sockets, this)
    }
    fn set_receive_buffer_size(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        value: u64,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::set_receive_buffer_size(&mut self.sockets, this, value)
    }
    fn send_buffer_size(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime_wasi::p2::SocketResult<u64> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::send_buffer_size(&mut self.sockets, this)
    }
    fn set_send_buffer_size(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        value: u64,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::set_send_buffer_size(&mut self.sockets, this, value)
    }
    fn subscribe(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime::Result<Resource<wasmtime_wasi::p2::DynPollable>> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::subscribe(&mut self.sockets, this)
    }
    fn shutdown(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
        shutdown_type: wasmtime_wasi::p2::bindings::sockets::tcp::ShutdownType,
    ) -> wasmtime_wasi::p2::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::shutdown(&mut self.sockets, this, shutdown_type)
    }

    fn drop(
        &mut self,
        this: Resource<wasmtime_wasi::p2::bindings::sockets::tcp::TcpSocket>,
    ) -> wasmtime::Result<()> {
        // Wasmtime's `finish_connect` creates input/output streams as ResourceTable children of
        // this socket. Its delegated delete therefore fails with `HasChildren` while either
        // stream remains live; returning the lease only below preserves the native FD lifetime.
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket>::drop(&mut self.sockets, this)?;
        self.guard.release_live_socket();
        Ok(())
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

/// Error classification returned by [`resolve_vetted`], mapped into `p2` and `p3` `ErrorCode`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VettedResolveError {
    PermanentResolverFailure,
    NameUnresolvable,
}

/// Core vetted DNS resolution shared by `wasi:sockets/ip-name-lookup` `@0.2.x` (`p2`) and
/// `@0.3.0` (`p3`):
/// 1. Deny-by-default allowlist check before any DNS query.
/// 2. Host-side resolution (`inner.resolver`).
/// 3. SSRF classification of EVERY resolved IP (a single blocked address rejects the entire lookup).
/// 4. Per-Store IP pinning under `MAX_PINNED_IPS`.
async fn resolve_vetted(
    inner: Option<Arc<GuardInner>>,
    name: String,
) -> Result<Vec<IpAddr>, VettedResolveError> {
    let Some(inner) = inner else {
        return Err(VettedResolveError::PermanentResolverFailure);
    };
    let lower = name.to_ascii_lowercase();
    if !inner.policy.allows_name(&lower) {
        return Err(VettedResolveError::PermanentResolverFailure);
    }
    let addrs = inner.resolver.resolve(&lower, 0).await.map_err(|e| {
        tracing::debug!(host = %lower, error = %e, "outbound-tcp DNS resolution failed");
        VettedResolveError::NameUnresolvable
    })?;
    if addrs.is_empty() {
        return Err(VettedResolveError::NameUnresolvable);
    }
    let ips: Vec<IpAddr> = addrs.iter().map(|a| a.ip().to_canonical()).collect();
    // Classify EVERY resolved address; a single blocked one rejects the whole lookup
    // (the DNS-rebinding / mixed-record guard, same strictness as outbound HTTP).
    if ips
        .iter()
        .any(|ip| inner.policy.classify(*ip) != AddrVerdict::Allowed)
    {
        return Err(VettedResolveError::PermanentResolverFailure);
    }
    // Pin: record every vetted address under the name that produced it, so the connect
    // gate can require "an IP this host resolved for that allowlisted name".
    {
        let mut pinned = inner.pinned.lock();
        for ip in &ips {
            if !pinned.contains_key(ip) && pinned.len() >= MAX_PINNED_IPS {
                return Err(VettedResolveError::PermanentResolverFailure);
            }
            pinned.entry(*ip).or_default().insert(lower.clone());
        }
    }
    Ok(ips)
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
        let inner = Some(inner.clone());
        let task = wasmtime_wasi::runtime::spawn(async move {
            match resolve_vetted(inner, lower).await {
                Ok(ips) => Ok(ips.into_iter().map(IpAddress::from).collect::<Vec<_>>()),
                Err(VettedResolveError::PermanentResolverFailure) => {
                    Err(ErrorCode::PermanentResolverFailure.into())
                }
                Err(VettedResolveError::NameUnresolvable) => {
                    Err(ErrorCode::NameUnresolvable.into())
                }
            }
        });
        let resource = self.table.push(PlectoResolveStream::Waiting(task))?;
        Ok(Resource::new_own(resource.rep()))
    }
}

/// Host-side state behind a guest `resolve-address-stream` handle. wasmtime 48 made the upstream
/// `ResolveAddressStream` unconstructible outside wasmtime-wasi, so the table holds this type
/// instead; the generated bindings only ever carry the handle's rep, and `Resource::new_own`
/// maps between the two typed views of it.
pub(crate) enum PlectoResolveStream {
    Waiting(wasmtime_wasi::runtime::AbortOnDropJoinHandle<Result<Vec<IpAddress>, SocketError>>),
    Done(Result<std::vec::IntoIter<IpAddress>, SocketError>),
}

#[wasmtime_wasi::async_trait]
impl Pollable for PlectoResolveStream {
    async fn ready(&mut self) {
        if let PlectoResolveStream::Waiting(task) = self {
            let result = (&mut *task).await;
            *self = PlectoResolveStream::Done(result.map(Vec::into_iter));
        }
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
        let stream: &mut PlectoResolveStream =
            self.table.get_mut(&Resource::new_borrow(resource.rep()))?;
        loop {
            match stream {
                PlectoResolveStream::Waiting(future) => {
                    match wasmtime_wasi::runtime::poll_noop(Pin::new(future)) {
                        Some(result) => {
                            *stream = PlectoResolveStream::Done(result.map(Vec::into_iter));
                        }
                        None => return Err(ErrorCode::WouldBlock.into()),
                    }
                }
                PlectoResolveStream::Done(Ok(iter)) => return Ok(iter.next()),
                PlectoResolveStream::Done(slot @ Err(_)) => {
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
        // A borrowed view: handing `subscribe` an OWNED handle would transfer ownership to the
        // pollable, which then deletes the stream entry when the guest drops the pollable — and
        // the guest's own stream drop would trap on the vanished entry.
        subscribe(
            self.table,
            Resource::<PlectoResolveStream>::new_borrow(resource.rep()),
        )
    }

    async fn drop(&mut self, resource: Resource<ResolveAddressStream>) -> wasmtime::Result<()> {
        self.table
            .delete::<PlectoResolveStream>(Resource::new_own(resource.rep()))?;
        Ok(())
    }
}

// --- WASI 0.3 (`wasi:sockets@0.3.0`) host implementations ---
//
// In WASI 0.3, `wasi:sockets` consolidated into two interfaces:
// 1. `wasi:sockets/ip-name-lookup@0.3.0` (`resolve-addresses: async func(name: string)`)
// 2. `wasi:sockets/types@0.3.0` (`resource tcp-socket` + `resource udp-socket`)
//
// Both route through the exact same `TcpGuard` / `resolve_vetted` / `TcpSocketQuota` enforcement as
// the `p2` bindings above: vetted DNS resolution with IP pinning, SSRF classification, per-Store and
// per-Host live socket limits, and unconditional denial of UDP (`AccessDenied`), `listen`, and
// non-wildcard `bind`.

impl wasmtime_wasi::p3::bindings::sockets::ip_name_lookup::Host for TcpLookupView<'_> {}

impl<U> wasmtime_wasi::p3::bindings::sockets::ip_name_lookup::HostWithStore<U> for PlectoTcpLookup {
    async fn resolve_addresses(
        store: &wasmtime::component::Accessor<U, Self>,
        name: String,
    ) -> wasmtime::Result<
        Result<
            Vec<wasmtime_wasi::p3::bindings::sockets::types::IpAddress>,
            wasmtime_wasi::p3::bindings::sockets::ip_name_lookup::ErrorCode,
        >,
    > {
        use wasmtime_wasi::p3::bindings::sockets::ip_name_lookup::ErrorCode as P3LookupErrorCode;
        use wasmtime_wasi::p3::bindings::sockets::types::IpAddress as P3IpAddress;

        let inner = store.with(|mut view| view.get().guard.inner.clone());
        Ok(match resolve_vetted(inner, name).await {
            Ok(ips) => Ok(ips.into_iter().map(P3IpAddress::from).collect()),
            Err(VettedResolveError::PermanentResolverFailure) => {
                Err(P3LookupErrorCode::PermanentResolverFailure)
            }
            Err(VettedResolveError::NameUnresolvable) => Err(P3LookupErrorCode::NameUnresolvable),
        })
    }
}

impl wasmtime_wasi::p3::bindings::sockets::types::Host for TcpSocketsView<'_> {
    fn convert_error_code(
        &mut self,
        error: wasmtime_wasi::p3::sockets::SocketError,
    ) -> wasmtime::Result<wasmtime_wasi::p3::bindings::sockets::types::ErrorCode> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::Host>::convert_error_code(
            &mut self.sockets,
            error,
        )
    }
}

impl wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket for TcpSocketsView<'_> {
    fn create(
        &mut self,
        address_family: wasmtime_wasi::p3::bindings::sockets::types::IpAddressFamily,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<
        Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    > {
        if !self.guard.try_acquire_live_socket() {
            // WASI 0.3 `wasi:sockets/types@0.3.0` removed the `new-socket-limit` variant from
            // `error-code`; resource-exhaustion on socket creation surfaces as `out-of-memory`.
            return Err(wasmtime_wasi::p3::bindings::sockets::types::ErrorCode::OutOfMemory.into());
        }
        let result = <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::create(
            &mut self.sockets,
            address_family,
        );
        match result {
            Ok(socket) => {
                self.p3_leases
                    .insert(socket.rep(), Arc::new(P3SocketLease(self.guard.clone())));
                Ok(socket)
            }
            Err(err) => {
                self.guard.release_live_socket();
                Err(err)
            }
        }
    }

    async fn bind(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        local_address: wasmtime_wasi::p3::bindings::sockets::types::IpSocketAddress,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::bind(
            &mut self.sockets,
            socket,
            local_address,
        )
        .await
    }

    fn get_local_address(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<
        wasmtime_wasi::p3::bindings::sockets::types::IpSocketAddress,
    > {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_local_address(
            &mut self.sockets,
            socket,
        )
    }

    fn get_remote_address(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<
        wasmtime_wasi::p3::bindings::sockets::types::IpSocketAddress,
    > {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_remote_address(
            &mut self.sockets,
            socket,
        )
    }

    fn get_is_listening(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime::Result<bool> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_is_listening(
            &mut self.sockets,
            socket,
        )
    }

    fn get_address_family(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime::Result<wasmtime_wasi::p3::bindings::sockets::types::IpAddressFamily> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_address_family(
            &mut self.sockets,
            socket,
        )
    }

    fn set_listen_backlog_size(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        value: u64,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::set_listen_backlog_size(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn get_keep_alive_enabled(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<bool> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_keep_alive_enabled(
            &mut self.sockets,
            socket,
        )
    }

    fn set_keep_alive_enabled(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        value: bool,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::set_keep_alive_enabled(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn get_keep_alive_idle_time(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<
        wasmtime_wasi::p3::bindings::sockets::types::Duration,
    > {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_keep_alive_idle_time(
            &mut self.sockets,
            socket,
        )
    }

    fn set_keep_alive_idle_time(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        value: wasmtime_wasi::p3::bindings::sockets::types::Duration,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::set_keep_alive_idle_time(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn get_keep_alive_interval(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<
        wasmtime_wasi::p3::bindings::sockets::types::Duration,
    > {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_keep_alive_interval(
            &mut self.sockets,
            socket,
        )
    }

    fn set_keep_alive_interval(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        value: wasmtime_wasi::p3::bindings::sockets::types::Duration,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::set_keep_alive_interval(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn get_keep_alive_count(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<u32> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_keep_alive_count(
            &mut self.sockets,
            socket,
        )
    }

    fn set_keep_alive_count(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        value: u32,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::set_keep_alive_count(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn get_hop_limit(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<u8> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_hop_limit(
            &mut self.sockets,
            socket,
        )
    }

    fn set_hop_limit(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        value: u8,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::set_hop_limit(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn get_receive_buffer_size(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<u64> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_receive_buffer_size(
            &mut self.sockets,
            socket,
        )
    }

    fn set_receive_buffer_size(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        value: u64,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::set_receive_buffer_size(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn get_send_buffer_size(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<u64> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::get_send_buffer_size(
            &mut self.sockets,
            socket,
        )
    }

    fn set_send_buffer_size(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        value: u64,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::set_send_buffer_size(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn drop(
        &mut self,
        sock: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime::Result<()> {
        let rep = sock.rep();
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocket>::drop(
            &mut self.sockets,
            sock,
        )?;
        self.p3_leases.remove(&rep);
        Ok(())
    }
}

impl<T: wasmtime_wasi::sockets::WasiSocketsView + Send>
    wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocketWithStore<T> for PlectoTcpSockets
{
    async fn connect(
        store: &wasmtime::component::Accessor<T, Self>,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        remote_address: wasmtime_wasi::p3::bindings::sockets::types::IpSocketAddress,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        let accessor = store.with_getter::<wasmtime_wasi::sockets::WasiSockets>(T::sockets);
        <wasmtime_wasi::sockets::WasiSockets as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocketWithStore<T>>::connect(
            &accessor,
            socket,
            remote_address,
        )
        .await
    }

    async fn listen(
        mut store: wasmtime::component::Access<'_, T, Self>,
        socket_resource: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<
        wasmtime::component::StreamReader<
            Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        >,
    > {
        use wasmtime::AsContextMut as _;
        let access = wasmtime::component::Access::<T, wasmtime_wasi::sockets::WasiSockets>::new(
            store.as_context_mut(),
            T::sockets,
        );
        <wasmtime_wasi::sockets::WasiSockets as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocketWithStore<T>>::listen(
            access,
            socket_resource,
        )
        .await
    }

    fn send(
        mut store: wasmtime::component::Access<'_, T, Self>,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
        data: wasmtime::component::StreamReader<u8>,
    ) -> wasmtime::Result<
        wasmtime::component::FutureReader<
            Result<(), wasmtime_wasi::p3::bindings::sockets::types::ErrorCode>,
        >,
    > {
        use wasmtime::AsContextMut as _;
        let lease = store
            .get()
            .p3_leases
            .get(&socket.rep())
            .cloned()
            .ok_or_else(|| wasmtime::Error::msg("missing P3 TCP socket lease"))?;
        let access = wasmtime::component::Access::<T, wasmtime_wasi::sockets::WasiSockets>::new(
            store.as_context_mut(),
            T::sockets,
        );
        let completion = <wasmtime_wasi::sockets::WasiSockets as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocketWithStore<T>>::send(
            access,
            socket,
            data,
        )?;
        retain_p3_stream_lease(store.as_context_mut(), completion, lease)
    }

    fn receive(
        mut store: wasmtime::component::Access<'_, T, Self>,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::TcpSocket>,
    ) -> wasmtime::Result<(
        wasmtime::component::StreamReader<u8>,
        wasmtime::component::FutureReader<
            Result<(), wasmtime_wasi::p3::bindings::sockets::types::ErrorCode>,
        >,
    )> {
        use wasmtime::AsContextMut as _;
        let lease = store
            .get()
            .p3_leases
            .get(&socket.rep())
            .cloned()
            .ok_or_else(|| wasmtime::Error::msg("missing P3 TCP socket lease"))?;
        let access = wasmtime::component::Access::<T, wasmtime_wasi::sockets::WasiSockets>::new(
            store.as_context_mut(),
            T::sockets,
        );
        let (mut stream, completion) = <wasmtime_wasi::sockets::WasiSockets as wasmtime_wasi::p3::bindings::sockets::types::HostTcpSocketWithStore<T>>::receive(
            access,
            socket,
        )?;
        match retain_p3_stream_lease(store.as_context_mut(), completion, lease) {
            Ok(completion) => Ok((stream, completion)),
            Err(err) => {
                stream.close(&mut store)?;
                Err(err)
            }
        }
    }
}

// In WASI 0.3, `udp-socket` is part of the same `wasi:sockets/types@0.3.0` interface as
// `tcp-socket`. Plecto's outbound-TCP capability never permits UDP (ADR 000060): `create` fails
// closed with `AccessDenied` before touching the OS, and `WasiCtxBuilder::allow_udp(false)` +
// `TcpGuard::permits` deny every UDP operation as defence in depth.
impl wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket for TcpSocketsView<'_> {
    async fn create(
        &mut self,
        _address_family: wasmtime_wasi::p3::bindings::sockets::types::IpAddressFamily,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<
        Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
    > {
        Err(wasmtime_wasi::p3::bindings::sockets::types::ErrorCode::AccessDenied.into())
    }

    async fn bind(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
        local_address: wasmtime_wasi::p3::bindings::sockets::types::IpSocketAddress,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::bind(
            &mut self.sockets,
            socket,
            local_address,
        )
        .await
    }

    async fn connect(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
        remote_address: wasmtime_wasi::p3::bindings::sockets::types::IpSocketAddress,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::connect(
            &mut self.sockets,
            socket,
            remote_address,
        )
        .await
    }

    fn disconnect(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::disconnect(
            &mut self.sockets,
            socket,
        )
    }

    fn get_local_address(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<
        wasmtime_wasi::p3::bindings::sockets::types::IpSocketAddress,
    > {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::get_local_address(
            &mut self.sockets,
            socket,
        )
    }

    fn get_remote_address(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<
        wasmtime_wasi::p3::bindings::sockets::types::IpSocketAddress,
    > {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::get_remote_address(
            &mut self.sockets,
            socket,
        )
    }

    fn get_address_family(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
    ) -> wasmtime::Result<wasmtime_wasi::p3::bindings::sockets::types::IpAddressFamily> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::get_address_family(
            &mut self.sockets,
            socket,
        )
    }

    fn get_unicast_hop_limit(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<u8> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::get_unicast_hop_limit(
            &mut self.sockets,
            socket,
        )
    }

    fn set_unicast_hop_limit(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
        value: u8,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::set_unicast_hop_limit(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn get_receive_buffer_size(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<u64> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::get_receive_buffer_size(
            &mut self.sockets,
            socket,
        )
    }

    fn set_receive_buffer_size(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
        value: u64,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::set_receive_buffer_size(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn get_send_buffer_size(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<u64> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::get_send_buffer_size(
            &mut self.sockets,
            socket,
        )
    }

    fn set_send_buffer_size(
        &mut self,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
        value: u64,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::set_send_buffer_size(
            &mut self.sockets,
            socket,
            value,
        )
    }

    fn drop(
        &mut self,
        sock: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
    ) -> wasmtime::Result<()> {
        <wasmtime_wasi::sockets::WasiSocketsCtxView<'_> as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocket>::drop(
            &mut self.sockets,
            sock,
        )
    }
}

impl<T: wasmtime_wasi::sockets::WasiSocketsView + Send>
    wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocketWithStore<T> for PlectoTcpSockets
{
    async fn send(
        store: &wasmtime::component::Accessor<T, Self>,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
        data: Vec<u8>,
        remote_address: Option<wasmtime_wasi::p3::bindings::sockets::types::IpSocketAddress>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<()> {
        let accessor = store.with_getter::<wasmtime_wasi::sockets::WasiSockets>(T::sockets);
        <wasmtime_wasi::sockets::WasiSockets as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocketWithStore<T>>::send(
            &accessor,
            socket,
            data,
            remote_address,
        )
        .await
    }

    async fn receive(
        store: &wasmtime::component::Accessor<T, Self>,
        socket: Resource<wasmtime_wasi::p3::bindings::sockets::types::UdpSocket>,
    ) -> wasmtime_wasi::p3::sockets::SocketResult<(
        Vec<u8>,
        wasmtime_wasi::p3::bindings::sockets::types::IpSocketAddress,
    )> {
        let accessor = store.with_getter::<wasmtime_wasi::sockets::WasiSockets>(T::sockets);
        <wasmtime_wasi::sockets::WasiSockets as wasmtime_wasi::p3::bindings::sockets::types::HostUdpSocketWithStore<T>>::receive(
            &accessor,
            socket,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap as StdHashMap};
    use std::time::Duration;

    use crate::MemoryBackend;
    use crate::options::DEFAULT_MAX_MEMORY_BYTES;
    use crate::outbound::TcpAllowEntry;
    use crate::quota::KvQuota;
    use crate::state::{HostState, HostStateInit};
    use wasmtime_wasi::p2::bindings::sockets::network::IpAddressFamily;
    use wasmtime_wasi::p2::bindings::sockets::tcp::HostTcpSocket;
    use wasmtime_wasi::p2::bindings::sockets::tcp_create_socket::Host as TcpCreateHost;
    use wasmtime_wasi::sockets::WasiSocketsView;

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
        guard_with_quota(policy, Arc::new(TcpSocketQuota::new()))
    }

    fn guard_with_quota(policy: OutboundTcpPolicy, quota: Arc<TcpSocketQuota>) -> TcpGuard {
        OutboundTcpState::new(policy, Resolver::Static(StdHashMap::new()), quota).guard()
    }

    fn state_with_tcp(guard: TcpGuard) -> HostState {
        HostState::new(
            HostStateInit {
                kv: Arc::new(MemoryBackend::default()),
                kv_prefix: "tcp-test".to_string(),
                max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
                ratelimit_bucket: None,
                quota: Arc::new(KvQuota::new()),
                config: Arc::new(BTreeMap::new()),
                #[cfg(feature = "fat-guest")]
                wasi_minimal: false,
            },
            #[cfg(feature = "outbound-http")]
            crate::outbound_http::PlectoHttpHooks::deny_all(),
            guard,
        )
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
        // not even the wildcard bind the connect carve-out lets through for a policied guard
        assert!(!g.permits(addr("0.0.0.0", 0), SocketAddrUse::TcpBind));
    }

    #[test]
    fn listen_accept_bind_and_udp_uses_are_denied_even_for_an_allowed_destination() {
        // The capability is active TCP connect ONLY (ADR 000060): no listen, no accept, no UDP —
        // even to an address that would pass every connect check.
        let g = guard(policy(vec![entry("8.8.8.8", 853)], vec![], 4));
        let dest = addr("8.8.8.8", 853);
        assert!(g.permits(dest, SocketAddrUse::TcpConnect));
        for blocked in [
            SocketAddrUse::TcpBind,
            SocketAddrUse::TcpListen,
            SocketAddrUse::TcpAccept,
            SocketAddrUse::UdpBind,
            SocketAddrUse::UdpSend,
            SocketAddrUse::UdpReceive,
        ] {
            assert!(!g.permits(dest, blocked), "{blocked:?} must be denied");
        }
    }

    #[test]
    fn only_the_wildcard_bind_that_connect_implies_is_permitted() {
        // connect() on an unbound socket asks permission for the implicit bind first, with the
        // wildcard address; that one must pass or nothing connects. Any address a guest could
        // pick for itself does not — and no bind check spends the connect budget.
        let g = guard(policy(vec![entry("8.8.8.8", 853)], vec![], 1));
        for _ in 0..8 {
            assert!(g.permits(addr("0.0.0.0", 0), SocketAddrUse::TcpBind));
            assert!(g.permits(addr("::", 0), SocketAddrUse::TcpBind));
        }
        assert!(!g.permits(addr("0.0.0.0", 8080), SocketAddrUse::TcpBind));
        assert!(!g.permits(addr("127.0.0.1", 0), SocketAddrUse::TcpBind));
        assert!(
            g.permits(addr("8.8.8.8", 853), SocketAddrUse::TcpConnect),
            "the single-connect budget survived the bind checks"
        );
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

    #[test]
    fn live_socket_budget_spans_requests_and_a_successful_drop_reuses_its_slot() {
        let g = guard(policy(vec![entry("8.8.8.8", 853)], vec![], 1));
        for _ in 0..MAX_LIVE_TCP_SOCKETS_PER_STORE {
            assert!(g.try_acquire_live_socket());
            g.begin_request();
        }
        assert!(!g.try_acquire_live_socket(), "Store cap must span requests");
        g.release_live_socket();
        assert!(
            g.try_acquire_live_socket(),
            "a dropped socket returns its slot"
        );
    }

    #[test]
    fn host_socket_quota_is_shared_between_stores_and_store_drop_returns_leases() {
        let quota = Arc::new(TcpSocketQuota::with_limit(2));
        let one = guard_with_quota(
            policy(vec![entry("8.8.8.8", 853)], vec![], 1),
            quota.clone(),
        );
        let two = guard_with_quota(
            policy(vec![entry("8.8.8.8", 853)], vec![], 1),
            quota.clone(),
        );
        assert!(one.try_acquire_live_socket());
        assert!(two.try_acquire_live_socket());
        assert_eq!(quota.used(), 2);
        assert!(
            !one.try_acquire_live_socket(),
            "shared host cap rejects excess"
        );
        drop(one);
        assert_eq!(
            quota.used(),
            1,
            "discarded Store returns all retained sockets"
        );
        assert!(
            two.try_acquire_live_socket(),
            "a released host slot is reusable"
        );
    }

    #[test]
    fn failed_or_duplicate_release_cannot_free_another_stores_slot() {
        let quota = Arc::new(TcpSocketQuota::with_limit(1));
        let one = guard_with_quota(
            policy(vec![entry("8.8.8.8", 853)], vec![], 1),
            quota.clone(),
        );
        let two = guard_with_quota(
            policy(vec![entry("8.8.8.8", 853)], vec![], 1),
            quota.clone(),
        );
        assert!(one.try_acquire_live_socket());
        // This models a failed ResourceTable delete (invalid handle / live child): no release.
        assert!(!two.try_acquire_live_socket());
        two.release_live_socket();
        assert!(
            !two.try_acquire_live_socket(),
            "unreserved release must not underflow host quota"
        );
        one.release_live_socket();
        assert!(two.try_acquire_live_socket());
    }

    #[test]
    fn native_socket_creation_failure_rolls_back_the_reserved_lease() {
        let quota = Arc::new(TcpSocketQuota::with_limit(1));
        let guard = guard_with_quota(
            policy(vec![entry("8.8.8.8", 853)], vec![], 1),
            quota.clone(),
        );
        // Build an inert WASI context, then use a valid accounting guard. The upstream creator
        // deterministically returns access-denied before any OS socket call, exercising rollback.
        let mut state = state_with_tcp(TcpGuard::deny_all());
        let sockets = <HostState as WasiSocketsView>::sockets(&mut state);
        let mut p3_leases = HashMap::new();
        let mut sockets = TcpSocketsView {
            sockets,
            guard,
            p3_leases: &mut p3_leases,
        };
        assert!(sockets.create_tcp_socket(IpAddressFamily::Ipv4).is_err());
        assert_eq!(
            quota.used(),
            0,
            "failed native creation must roll back its lease"
        );
    }

    #[test]
    fn failed_socket_drop_with_a_live_child_keeps_the_lease_until_real_drop() {
        let quota = Arc::new(TcpSocketQuota::with_limit(2));
        let guard = guard_with_quota(
            policy(vec![entry("8.8.8.8", 853)], vec![], 1),
            quota.clone(),
        );
        let mut state = state_with_tcp(guard);
        let mut sockets = state.tcp_sockets();
        let socket = sockets
            .create_tcp_socket(IpAddressFamily::Ipv4)
            .expect("create native test socket");
        assert_eq!(quota.used(), 1);
        // Wasmtime's `finish_connect` registers its input/output streams as children in exactly
        // this way. `HasChildren` must keep the native descriptor lease until those streams drop.
        let child = sockets
            .sockets
            .table
            .push_child((), &socket)
            .expect("add synthetic stream child");
        assert!(sockets.drop(Resource::new_own(socket.rep())).is_err());
        assert_eq!(quota.used(), 1, "failed native delete must not release");
        sockets
            .sockets
            .table
            .delete(child)
            .expect("drop child first");
        sockets
            .drop(socket)
            .expect("socket delete succeeds after children drop");
        assert_eq!(quota.used(), 0, "successful native delete returns lease");
    }

    #[test]
    fn dropping_a_store_returns_retained_native_socket_leases() {
        let quota = Arc::new(TcpSocketQuota::with_limit(1));
        let guard = guard_with_quota(
            policy(vec![entry("8.8.8.8", 853)], vec![], 1),
            quota.clone(),
        );
        let mut state = state_with_tcp(guard);
        {
            let mut sockets = state.tcp_sockets();
            sockets
                .create_tcp_socket(IpAddressFamily::Ipv4)
                .expect("create native socket retained until Store drop");
            assert_eq!(quota.used(), 1);
        }
        // HostState drops WasiCtx, then ResourceTable, then TcpGuard. GuardInner's final Drop
        // must return the lease that the guest intentionally retained.
        drop(state);
        assert_eq!(quota.used(), 0);
    }

    #[test]
    fn p3_tcp_socket_create_and_drop_enforce_quota_and_udp_is_denied() {
        use wasmtime_wasi::p3::bindings::sockets::types::{
            ErrorCode as P3ErrorCode, Host as P3TypesHost, HostTcpSocket as P3HostTcpSocket,
            HostUdpSocket as P3HostUdpSocket, IpAddressFamily as P3IpAddressFamily,
        };

        let quota = Arc::new(TcpSocketQuota::with_limit(1));
        let guard = guard_with_quota(
            policy(vec![entry("8.8.8.8", 853)], vec![], 1),
            quota.clone(),
        );
        let mut state = state_with_tcp(guard);
        let mut sockets = state.tcp_sockets();

        // UDP creation is unconditionally denied with AccessDenied in WASI 0.3.
        let udp_err = pollster::block_on(<TcpSocketsView<'_> as P3HostUdpSocket>::create(
            &mut sockets,
            P3IpAddressFamily::Ipv4,
        ))
        .expect_err("UDP socket creation must be denied");
        let code = <TcpSocketsView<'_> as P3TypesHost>::convert_error_code(&mut sockets, udp_err)
            .expect("convert error code");
        assert!(matches!(code, P3ErrorCode::AccessDenied));
        assert_eq!(quota.used(), 0, "denied UDP must not touch TCP quota");

        // First p3 TCP socket succeeds and reserves the 1-slot host quota.
        let sock1 =
            <TcpSocketsView<'_> as P3HostTcpSocket>::create(&mut sockets, P3IpAddressFamily::Ipv4)
                .expect("create first p3 TCP socket");
        assert_eq!(quota.used(), 1);

        // Second p3 TCP socket exceeds the quota and fails with OutOfMemory.
        let limit_err =
            <TcpSocketsView<'_> as P3HostTcpSocket>::create(&mut sockets, P3IpAddressFamily::Ipv4)
                .expect_err("second p3 TCP socket must hit quota");
        let code = <TcpSocketsView<'_> as P3TypesHost>::convert_error_code(&mut sockets, limit_err)
            .expect("convert error code");
        assert!(matches!(code, P3ErrorCode::OutOfMemory));

        // Dropping the p3 TCP socket releases the slot.
        <TcpSocketsView<'_> as P3HostTcpSocket>::drop(&mut sockets, sock1)
            .expect("drop first p3 TCP socket");
        assert_eq!(quota.used(), 0);
    }

    #[test]
    fn resolve_vetted_enforces_allowlist_ssrf_and_pins_resolved_ips() {
        let mut static_map = StdHashMap::new();
        static_map.insert(
            "allowed.internal".to_string(),
            vec!["93.184.216.34".parse().unwrap()],
        );
        static_map.insert(
            "mixed.internal".to_string(),
            vec![
                "93.184.216.34".parse().unwrap(),
                "127.0.0.1".parse().unwrap(),
            ],
        );
        let state = OutboundTcpState::new(
            policy(
                vec![
                    entry("allowed.internal", 6379),
                    entry("mixed.internal", 6379),
                ],
                vec![],
                4,
            ),
            Resolver::Static(static_map),
            Arc::new(TcpSocketQuota::new()),
        );
        let g = state.guard();

        // Unlisted hostname fails before resolution.
        let unlisted = pollster::block_on(resolve_vetted(
            g.inner.clone(),
            "unlisted.internal".to_string(),
        ));
        assert_eq!(unlisted, Err(VettedResolveError::PermanentResolverFailure));

        // Mixed public + loopback record set is rejected wholesale and pins nothing.
        let mixed = pollster::block_on(resolve_vetted(
            g.inner.clone(),
            "mixed.internal".to_string(),
        ));
        assert_eq!(mixed, Err(VettedResolveError::PermanentResolverFailure));
        assert!(
            !g.permits(addr("93.184.216.34", 6379), SocketAddrUse::TcpConnect),
            "rejected mixed lookup must not pin the public half"
        );

        // Allowed public hostname resolves and pins its IP for subsequent connect.
        let resolved = pollster::block_on(resolve_vetted(
            g.inner.clone(),
            "ALLOWED.Internal".to_string(),
        ))
        .expect("allowed name resolves");
        assert_eq!(resolved, vec!["93.184.216.34".parse::<IpAddr>().unwrap()]);
        assert!(
            g.permits(addr("93.184.216.34", 6379), SocketAddrUse::TcpConnect),
            "vetted lookup pins the resolved IP for connect"
        );
    }

    #[tokio::test]
    async fn p3_streams_keep_the_socket_quota_after_resource_and_completion_drop() {
        use crate::engine::{Allocation, build_engine};
        use wasmtime::component::{Access, StreamReader};
        use wasmtime::{AsContextMut as _, Store};
        use wasmtime_wasi::p3::bindings::sockets::types::{
            HostTcpSocket as P3Socket, HostTcpSocketWithStore as P3SocketWithStore,
            IpAddressFamily as P3Family, IpSocketAddress as P3Address,
            Ipv4SocketAddress as P3V4Address,
        };

        // Hold the send stream open until explicitly released; no unbounded buffering or I/O.
        struct PendingBytes(tokio::sync::oneshot::Receiver<()>);
        impl<T> wasmtime::component::StreamProducer<T> for PendingBytes {
            type Item = u8;
            type Buffer = Option<u8>;
            fn poll_produce<'a>(
                mut self: Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                _store: wasmtime::StoreContextMut<'a, T>,
                _dst: wasmtime::component::Destination<'a, u8, Option<u8>>,
                finish: bool,
            ) -> std::task::Poll<wasmtime::Result<wasmtime::component::StreamResult>> {
                use std::task::Poll;
                use wasmtime::component::StreamResult;
                match std::future::Future::poll(Pin::new(&mut self.0), cx) {
                    Poll::Ready(_) => Poll::Ready(Ok(StreamResult::Dropped)),
                    Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
                    Poll::Pending => Poll::Pending,
                }
            }
        }

        let route = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
        route.connect("192.0.2.1:80").await.unwrap();
        let ip = route.local_addr().unwrap().ip();
        let IpAddr::V4(v4) = ip else {
            panic!("expected IPv4 route")
        };
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let cidr = format!("{ip}/32");

        for discard_store in [false, true] {
            let quota = Arc::new(TcpSocketQuota::with_limit(1));
            let guard = guard_with_quota(
                policy(vec![entry(&ip.to_string(), port)], vec![&cidr], 4),
                quota.clone(),
            );
            let mut store = Store::new(
                &build_engine(Allocation::OnDemand).unwrap(),
                state_with_tcp(guard.clone()),
            );
            let socket =
                P3Socket::create(&mut store.data_mut().tcp_sockets(), P3Family::Ipv4).unwrap();
            let rep = socket.rep();
            store
                .run_concurrent(async |accessor| {
                    let accessor = accessor.with_getter::<PlectoTcpSockets>(HostState::tcp_sockets);
                    <PlectoTcpSockets as P3SocketWithStore<HostState>>::connect(
                        &accessor,
                        socket,
                        P3Address::Ipv4(P3V4Address {
                            port,
                            address: v4.octets().into(),
                        }),
                    )
                    .await
                })
                .await
                .unwrap()
                .unwrap();

            let access = Access::<HostState, PlectoTcpSockets>::new(
                store.as_context_mut(),
                HostState::tcp_sockets,
            );
            let (mut received, mut receive_done) = <PlectoTcpSockets as P3SocketWithStore<
                HostState,
            >>::receive(
                access, Resource::new_borrow(rep)
            )
            .unwrap();
            let (finish_send, rx) = tokio::sync::oneshot::channel();
            let data = StreamReader::new(&mut store, PendingBytes(rx)).unwrap();
            let access = Access::<HostState, PlectoTcpSockets>::new(
                store.as_context_mut(),
                HostState::tcp_sockets,
            );
            let mut send_done = <PlectoTcpSockets as P3SocketWithStore<HostState>>::send(
                access,
                Resource::new_borrow(rep),
                data,
            )
            .unwrap();
            // Dropping completion futures must not cancel the host's accounting consumers.
            receive_done.close(&mut store).unwrap();
            send_done.close(&mut store).unwrap();
            P3Socket::drop(&mut store.data_mut().tcp_sockets(), Resource::new_own(rep)).unwrap();
            guard.begin_request();
            assert_eq!(quota.used(), 1);
            assert!(P3Socket::create(&mut store.data_mut().tcp_sockets(), P3Family::Ipv4).is_err());
            let mut other =
                state_with_tcp(guard_with_quota(policy(vec![], vec![], 1), quota.clone()));
            assert!(
                P3Socket::create(&mut other.tcp_sockets(), P3Family::Ipv4).is_err(),
                "host quota spans Stores"
            );

            if discard_store {
                drop(store);
                assert_eq!(
                    quota.used(),
                    0,
                    "discarding a Store reclaims stream reservations"
                );
                continue;
            }
            received.close(&mut store).unwrap();
            store
                .run_concurrent(async |_| tokio::task::yield_now().await)
                .await
                .unwrap();
            assert_eq!(quota.used(), 1, "the send half still owns its FD");
            finish_send.send(()).unwrap();
            store
                .run_concurrent(async |_| {
                    tokio::time::timeout(Duration::from_secs(1), async {
                        while quota.used() != 0 {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("closing both stream halves releases the quota");
                })
                .await
                .unwrap();
            let replacement =
                P3Socket::create(&mut store.data_mut().tcp_sockets(), P3Family::Ipv4).unwrap();
            P3Socket::drop(&mut store.data_mut().tcp_sockets(), replacement).unwrap();
            assert_eq!(quota.used(), 0);
        }
    }

    #[tokio::test]
    async fn p3_lookup_and_tcp_socket_connect_enforce_pin_budget_and_deny_listen() {
        use crate::engine::{Allocation, build_engine};
        use wasmtime::{AsContextMut as _, Store};
        use wasmtime_wasi::p3::bindings::sockets::ip_name_lookup::{
            ErrorCode as P3LookupErrorCode, HostWithStore as P3LookupHostWithStore,
        };
        use wasmtime_wasi::p3::bindings::sockets::types::{
            ErrorCode as P3SocketErrorCode, Host as P3TypesHost, HostTcpSocket as P3HostTcpSocket,
            HostTcpSocketWithStore as P3HostTcpSocketWithStore, IpAddress as P3IpAddress,
            IpAddressFamily as P3IpAddressFamily, IpSocketAddress as P3IpSocketAddress,
            Ipv4SocketAddress as P3Ipv4SocketAddress,
        };

        let route = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
        route.connect("192.0.2.1:80").await.unwrap();
        let ip = route.local_addr().unwrap().ip();
        let IpAddr::V4(v4) = ip else {
            return;
        };
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, 0))
            .await
            .unwrap();
        let bound_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { while let Ok((_sock, _)) = listener.accept().await {} });

        let mut static_map = StdHashMap::new();
        static_map.insert("backend.internal".to_string(), vec![ip]);
        let cidr = format!("{ip}/32");
        let tcp_state = OutboundTcpState::new(
            policy(
                vec![entry("backend.internal", bound_port)],
                vec![cidr.as_str()],
                1, // 1 connect per request
            ),
            Resolver::Static(static_map),
            Arc::new(TcpSocketQuota::new()),
        );
        let engine = build_engine(Allocation::OnDemand).unwrap();
        let mut store = Store::new(&engine, state_with_tcp(tcp_state.guard()));

        let remote = P3IpSocketAddress::Ipv4(P3Ipv4SocketAddress {
            port: bound_port,
            address: v4.octets().into(),
        });

        // 1. Connecting before resolving `backend.internal` is denied (IP not pinned yet).
        let unpinned_sock = <TcpSocketsView<'_> as P3HostTcpSocket>::create(
            &mut store.data_mut().tcp_sockets(),
            P3IpAddressFamily::Ipv4,
        )
        .unwrap();
        let unpinned_err = store
            .run_concurrent(async move |accessor| {
                let accessor = accessor.with_getter::<PlectoTcpSockets>(HostState::tcp_sockets);
                <PlectoTcpSockets as P3HostTcpSocketWithStore<HostState>>::connect(
                    &accessor,
                    unpinned_sock,
                    remote,
                )
                .await
            })
            .await
            .unwrap()
            .expect_err("connect before resolve must be denied");
        let code = <TcpSocketsView<'_> as P3TypesHost>::convert_error_code(
            &mut store.data_mut().tcp_sockets(),
            unpinned_err,
        )
        .unwrap();
        assert!(matches!(code, P3SocketErrorCode::AccessDenied));

        // 2. Unlisted hostname resolution via `wasi:sockets/ip-name-lookup@0.3.0` is denied.
        let unlisted_res = store
            .run_concurrent(async move |accessor| {
                let accessor = accessor.with_getter::<PlectoTcpLookup>(HostState::tcp_lookup);
                <PlectoTcpLookup as P3LookupHostWithStore<HostState>>::resolve_addresses(
                    &accessor,
                    "evil.internal".to_string(),
                )
                .await
            })
            .await
            .unwrap();
        assert!(matches!(
            unlisted_res,
            Ok(Err(P3LookupErrorCode::PermanentResolverFailure))
        ));

        // 3. Vetted resolution via `wasi:sockets/ip-name-lookup@0.3.0` succeeds and pins the IP.
        let resolved = store
            .run_concurrent(async move |accessor| {
                let accessor = accessor.with_getter::<PlectoTcpLookup>(HostState::tcp_lookup);
                <PlectoTcpLookup as P3LookupHostWithStore<HostState>>::resolve_addresses(
                    &accessor,
                    "backend.internal".to_string(),
                )
                .await
            })
            .await
            .unwrap()
            .unwrap()
            .expect("allowed p3 resolve_addresses succeeds");
        assert!(matches!(
            resolved.as_slice(),
            [P3IpAddress::Ipv4(octets)] if *octets == v4.octets().into()
        ));

        // 4. First connect to the pinned IP succeeds (`max_connections = 1`).
        let sock1 = <TcpSocketsView<'_> as P3HostTcpSocket>::create(
            &mut store.data_mut().tcp_sockets(),
            P3IpAddressFamily::Ipv4,
        )
        .unwrap();
        store
            .run_concurrent(async move |accessor| {
                let accessor = accessor.with_getter::<PlectoTcpSockets>(HostState::tcp_sockets);
                <PlectoTcpSockets as P3HostTcpSocketWithStore<HostState>>::connect(
                    &accessor, sock1, remote,
                )
                .await
            })
            .await
            .unwrap()
            .expect("first p3 connect to pinned IP succeeds");

        // 5. Second connect in the same request exceeds the 1-connect budget and fails closed.
        let sock2 = <TcpSocketsView<'_> as P3HostTcpSocket>::create(
            &mut store.data_mut().tcp_sockets(),
            P3IpAddressFamily::Ipv4,
        )
        .unwrap();
        let budget_err = store
            .run_concurrent(async move |accessor| {
                let accessor = accessor.with_getter::<PlectoTcpSockets>(HostState::tcp_sockets);
                <PlectoTcpSockets as P3HostTcpSocketWithStore<HostState>>::connect(
                    &accessor, sock2, remote,
                )
                .await
            })
            .await
            .unwrap()
            .expect_err("second p3 connect must be denied by per-request budget");
        let code = <TcpSocketsView<'_> as P3TypesHost>::convert_error_code(
            &mut store.data_mut().tcp_sockets(),
            budget_err,
        )
        .unwrap();
        assert!(matches!(code, P3SocketErrorCode::AccessDenied));

        // 6. `tcp-socket.listen` on WASI 0.3 is unconditionally denied (`AccessDenied`).
        let listen_sock = <TcpSocketsView<'_> as P3HostTcpSocket>::create(
            &mut store.data_mut().tcp_sockets(),
            P3IpAddressFamily::Ipv4,
        )
        .unwrap();
        let access = wasmtime::component::Access::<HostState, PlectoTcpSockets>::new(
            store.as_context_mut(),
            HostState::tcp_sockets,
        );
        let listen_err =
            <PlectoTcpSockets as P3HostTcpSocketWithStore<HostState>>::listen(access, listen_sock)
                .await
                .expect_err("p3 tcp-socket.listen must be denied");
        let code = <TcpSocketsView<'_> as P3TypesHost>::convert_error_code(
            &mut store.data_mut().tcp_sockets(),
            listen_err,
        )
        .unwrap();
        assert!(matches!(code, P3SocketErrorCode::AccessDenied));
    }
}
