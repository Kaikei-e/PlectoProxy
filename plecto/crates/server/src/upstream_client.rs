//! The generic seam over "send one request upstream" (audunhalland pattern: a generic parameter
//! bounded by a trait — static dispatch, no `Box<dyn Trait>`). Production has one implementation
//! (`HyperUpstreamClient`); tests substitute a fake so the retry/backoff/circuit-breaker
//! orchestration in `forward::forward_with_retry` can run against a scripted client instead of a
//! real socket.

use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;

use crate::body::stream;
use crate::{BoxError, ReqBody, ResponseBody};

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

/// The production `UpstreamClient`: the pooling hyper-util legacy client (plain HTTP/1.1 to the
/// upstream; connection reuse for free).
pub(crate) struct HyperUpstreamClient(Client<HttpConnector, ReqBody>);

impl HyperUpstreamClient {
    pub(crate) fn new(client: Client<HttpConnector, ReqBody>) -> Self {
        Self(client)
    }
}

impl UpstreamClient for HyperUpstreamClient {
    async fn request(
        &self,
        req: Request<ReqBody>,
    ) -> Result<hyper::Response<ResponseBody>, UpstreamSendError> {
        match self.0.request(req).await {
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
    /// so a test can assert exactly how many attempts a retry sequence made.
    pub(crate) struct FakeUpstreamClient {
        script: Mutex<Vec<Scripted>>,
        calls: Mutex<u32>,
    }

    impl FakeUpstreamClient {
        pub(crate) fn new(script: Vec<Scripted>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: Mutex::new(0),
            }
        }

        pub(crate) fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl UpstreamClient for FakeUpstreamClient {
        async fn request(
            &self,
            _req: Request<ReqBody>,
        ) -> Result<hyper::Response<ResponseBody>, UpstreamSendError> {
            *self.calls.lock().unwrap() += 1;
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
