//! DNS resolver seam shared by the outbound capabilities (ADR 000036 / 000060).
//!
//! Both outbound wirings resolve names ON THE HOST (never in the guest) so every resolved address
//! can be classified by the SSRF guard and pinned before any connect. `System` is production; the
//! `Static` map makes the resolve→classify→pin decision deterministic in tests — unit tests via
//! `cfg(test)`, and the feature-gated E2E suites via `test-support` (they need to point an
//! allowlisted NAME at a controlled address without real DNS).

use std::net::SocketAddr;

pub(crate) enum Resolver {
    System,
    #[cfg(any(test, feature = "test-support"))]
    Static(std::collections::HashMap<String, Vec<std::net::IpAddr>>),
}

impl Resolver {
    /// Resolve `host:port` to socket addresses. The underlying `io::Error` is preserved (DECREE
    /// §3: no `Result<_, ()>` swallowing) — the guest-visible mapping stays a generic DNS error
    /// code at the call sites, but the operator gets the real cause in the trace log there.
    pub(crate) async fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, std::io::Error> {
        match self {
            Resolver::System => tokio::net::lookup_host((host, port))
                .await
                .map(|it| it.collect()),
            #[cfg(any(test, feature = "test-support"))]
            Resolver::Static(map) => Ok(map
                .get(host)
                .map(|ips| ips.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
                .unwrap_or_default()),
        }
    }
}
