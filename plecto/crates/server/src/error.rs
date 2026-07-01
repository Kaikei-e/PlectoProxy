//! Typed fast-path errors (bp-rust: domain errors are `thiserror` enums, not `anyhow`, in library
//! code — `anyhow` stays at `main.rs`'s binary entrypoint).

use crate::upstream_client::UpstreamSendError;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ServerError {
    #[error("failed to bind the fast-path listener: {0}")]
    Bind(#[source] std::io::Error),
    #[error("failed to build the upstream request: {0}")]
    RequestBuild(#[from] hyper::http::Error),
    #[error("chain dispatch task failed: {0}")]
    ChainJoin(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Upstream(#[from] UpstreamSendError),
    /// HTTP/3 (QUIC) transport setup/accept failure. `h3`/`quinn`'s error types don't uniformly
    /// implement `std::error::Error + Send + Sync + 'static` for a clean `#[from]`, so this one
    /// variant stays a boxed `dyn Error` — an explicit, narrowly-scoped exception rather than
    /// falling back to `anyhow` for the whole enum.
    #[error("HTTP/3 (QUIC) transport error: {0}")]
    Http3(#[source] Box<dyn std::error::Error + Send + Sync>),
}
