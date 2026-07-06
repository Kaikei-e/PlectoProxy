//! The generic seam over "send one request upstream" (audunhalland pattern: a generic parameter
//! bounded by a trait — static dispatch, no `Box<dyn Trait>`). Production has one implementation
//! (`HyperUpstreamClient`); tests substitute a fake so the retry/backoff/circuit-breaker
//! orchestration in `forward::forward_with_retry` can run against a scripted client instead of a
//! real socket.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hyper::Request;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connect, HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioTimer};
use plecto_control::{TlsClientConfig, UpstreamGroup};

use crate::body::stream;
use crate::{BoxError, ReqBody, ResponseBody, upstream_connector};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendErrorKind {
    /// The connection attempt itself failed — the upstream never received the request, so a retry
    /// is safe for any method (ADR 000023).
    Connect,
    /// Some other transport fault after a connection was established.
    Other,
}

#[derive(Debug, thiserror::Error)]
#[error("upstream send failed: {source}")]
pub(crate) struct UpstreamSendError {
    pub(crate) kind: SendErrorKind,
    #[source]
    pub(crate) source: BoxError,
}

impl UpstreamSendError {
    pub(crate) fn is_connect(&self) -> bool {
        self.kind == SendErrorKind::Connect
    }
}

/// Send one request upstream and return its response with the body already boxed into
/// `ResponseBody` — the one leaf I/O boundary the retry/backoff/circuit-breaker DECISION logic
/// needs mocked. A native `async fn` in the trait suffices (every call site is monomorphized), so
/// there is no unavoidable dynamic-dispatch case to flag here.
pub(crate) trait UpstreamClient: Send + Sync {
    fn request(
        &self,
        req: Request<ReqBody>,
    ) -> impl std::future::Future<Output = Result<hyper::Response<ResponseBody>, UpstreamSendError>> + Send;
}

/// The production `UpstreamClient`: a pooling hyper-util legacy client per forward-leg security
/// context (ADR 000042). `Plain` speaks HTTP/1.1 over TCP (the pre-000042 behaviour, untouched
/// hot path); `Tls` re-encrypts with rustls and lets ALPN pick h2 / http/1.1 — the negotiated
/// protocol drives hyper's pool (an h2 origin multiplexes one shared connection). Cheap to
/// `Clone` (the pool is internally shared), so `UpstreamClients::for_group` hands out copies.
#[derive(Clone)]
pub(crate) enum HyperUpstreamClient {
    Plain(Client<HttpConnector, ReqBody>),
    Tls(Client<HttpsConnector<HttpConnector>, ReqBody>),
}

/// One pooled send, unified across connector types: box the streamed response body and classify
/// the failure (connect vs other) for the retry policy (ADR 000023).
async fn send_pooled<C>(
    client: &Client<C, ReqBody>,
    req: Request<ReqBody>,
) -> Result<hyper::Response<ResponseBody>, UpstreamSendError>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    match client.request(req).await {
        Ok(resp) => Ok(resp.map(stream)),
        Err(e) => Err(UpstreamSendError {
            kind: if e.is_connect() {
                SendErrorKind::Connect
            } else {
                SendErrorKind::Other
            },
            source: Box::new(e),
        }),
    }
}

impl UpstreamClient for HyperUpstreamClient {
    async fn request(
        &self,
        req: Request<ReqBody>,
    ) -> Result<hyper::Response<ResponseBody>, UpstreamSendError> {
        match self {
            HyperUpstreamClient::Plain(client) => send_pooled(client, req).await,
            HyperUpstreamClient::Tls(client) => send_pooled(client, req).await,
        }
    }
}

/// The shared pool settings for every upstream client (plain and TLS): the pool needs a timer for
/// idle expiry to be actively enforced (without one, `pool_idle_timeout` degrades to a lazy
/// checkout-time check), and the default max-idle-per-host is unbounded — cap what a burst can
/// strand.
fn pooled_builder() -> hyper_util::client::legacy::Builder {
    let mut builder = Client::builder(TokioExecutor::new());
    builder
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(32);
    builder
}

/// Build the TLS client for one `[upstream.tls]` config (ADR 000042): rustls over the same
/// nodelay TCP connector, `https_only` (this client only ever sees `https://` URIs — a scheme
/// bug fails closed instead of silently going plaintext). ALPN advertises `[h2, http/1.1]`
/// (control set it on the config; `enable_all_versions` keeps the connector in agreement), and
/// hyper's legacy client picks up the negotiated protocol per pooled connection.
///
/// `sni` (ADR 000050) overrides hyper-rustls' default server-name derivation (the forwarded URI's
/// host, which is the connected address — an IP when addresses are IP literals or DNS-expanded,
/// ADR 000044): when set, every TLS leg to this upstream uses `sni` for BOTH the SNI extension
/// and certificate-name verification instead.
fn build_tls_client(
    config: &TlsClientConfig,
    sni: Option<&rustls::pki_types::ServerName<'static>>,
) -> HyperUpstreamClient {
    let builder = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config.clone())
        .https_only();
    let builder = match sni {
        Some(name) => builder
            .with_server_name_resolver(hyper_rustls::FixedServerNameResolver::new(name.clone())),
        None => builder,
    };
    let connector = builder
        .enable_all_versions()
        .wrap_connector(upstream_connector());
    HyperUpstreamClient::Tls(pooled_builder().build(connector))
}

/// The per-security-context client registry (ADR 000042): one plain client for every
/// `http` upstream, plus one pooled TLS client per DISTINCT `[upstream.tls]` config, built
/// lazily and keyed on the config `Arc`'s identity — which control keeps stable across reloads
/// while the section is unchanged, so reloads never cold-start the connection pool.
pub(crate) struct UpstreamClients {
    plain: HyperUpstreamClient,
    tls: parking_lot::RwLock<HashMap<usize, HyperUpstreamClient>>,
}

/// Cap on retained TLS clients. Reached only by pathological reload churn over many distinct
/// `[upstream.tls]` configs; clearing rebuilds pools lazily (a reconnect blip, never an error).
const MAX_TLS_CLIENTS: usize = 64;

impl UpstreamClients {
    pub(crate) fn new() -> Self {
        Self {
            plain: HyperUpstreamClient::Plain(pooled_builder().build(upstream_connector())),
            tls: parking_lot::RwLock::new(HashMap::new()),
        }
    }

    /// The client for `group`'s security context: the shared plain client, or the pooled TLS
    /// client for its `[upstream.tls]` config (built on first use). Read-locked on the hot path;
    /// the write lock is taken only to build a missing client (first request after start/reload).
    pub(crate) fn for_group(&self, group: &UpstreamGroup) -> HyperUpstreamClient {
        let Some(config) = group.tls_client_config() else {
            return self.plain.clone();
        };
        let key = Arc::as_ptr(config) as usize;
        if let Some(client) = self.tls.read().get(&key) {
            return client.clone();
        }
        let built = build_tls_client(config, group.tls_sni());
        let mut map = self.tls.write();
        if map.len() >= MAX_TLS_CLIENTS {
            map.clear();
        }
        map.entry(key).or_insert(built).clone()
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Mutex;

    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};

    use super::{Request, ResponseBody, SendErrorKind, UpstreamClient, UpstreamSendError};
    use crate::{BoxError, ReqBody};

    /// A scripted response for [`FakeUpstreamClient`]: a status to answer with (body empty — tests
    /// only assert on status/retry behavior, not body content), a send failure, or a delay long
    /// enough to simulate a per-try timeout under `tokio::time::timeout`.
    #[derive(Clone, Copy)]
    pub(crate) enum Scripted {
        Status(u16),
        SendError(SendErrorKind),
        Hang(std::time::Duration),
    }

    /// A `FilterRuntime`-style test double for `UpstreamClient`: no socket at all. Each call to
    /// `request` pops the next scripted outcome (repeating the last one once the script runs out),
    /// so a test can assert exactly how many attempts a retry sequence made — and, via `bodies`,
    /// exactly which body bytes each attempt carried (a replayable retry must re-send them intact,
    /// ADR 000058).
    pub(crate) struct FakeUpstreamClient {
        script: Mutex<Vec<Scripted>>,
        calls: Mutex<u32>,
        bodies: Mutex<Vec<Bytes>>,
    }

    impl FakeUpstreamClient {
        pub(crate) fn new(script: Vec<Scripted>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: Mutex::new(0),
                bodies: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }

        /// The request body each attempt sent, in attempt order.
        pub(crate) fn bodies(&self) -> Vec<Bytes> {
            self.bodies.lock().unwrap().clone()
        }
    }

    impl UpstreamClient for FakeUpstreamClient {
        async fn request(
            &self,
            req: Request<ReqBody>,
        ) -> Result<hyper::Response<ResponseBody>, UpstreamSendError> {
            *self.calls.lock().unwrap() += 1;
            let body = req
                .into_body()
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            self.bodies.lock().unwrap().push(body);
            let next = {
                let mut script = self.script.lock().unwrap();
                if script.len() > 1 {
                    script.remove(0)
                } else {
                    // repeat the last scripted outcome once the script is exhausted
                    script.first().copied().unwrap_or(Scripted::Status(200))
                }
            };
            match next {
                Scripted::Status(status) => Ok(hyper::Response::builder()
                    .status(status)
                    .body(
                        Full::new(Bytes::new())
                            .map_err(|e: std::convert::Infallible| -> BoxError { match e {} })
                            .boxed(),
                    )
                    .unwrap()),
                Scripted::SendError(kind) => Err(UpstreamSendError {
                    kind,
                    source: "simulated send failure".into(),
                }),
                Scripted::Hang(dur) => {
                    tokio::time::sleep(dur).await;
                    Ok(hyper::Response::builder()
                        .status(200)
                        .body(
                            Full::new(Bytes::new())
                                .map_err(|e: std::convert::Infallible| -> BoxError { match e {} })
                                .boxed(),
                        )
                        .unwrap())
                }
            }
        }
    }
}
