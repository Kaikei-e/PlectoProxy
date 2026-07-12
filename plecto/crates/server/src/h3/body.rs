//! Adapts an HTTP/3 request's recv stream into an `http_body::Body`, so the request body streams to
//! the upstream like any other inbound body. One copy per chunk into `Bytes` (the recv buffer's own
//! type is opaque); the body is otherwise opaque pass-through.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use hyper::body::{Body, Frame, SizeHint};

use crate::BoxError;

pub(super) struct H3ReqBody {
    recv: h3::server::RequestStream<h3_quinn::RecvStream, Bytes>,
    /// Bytes left per the request's declared `content-length`, or `None` when it sent none.
    /// Feeds `size_hint` so the transport-independent bodyless check (`exact() == Some(0)`,
    /// which gates upstream retry and the body-buffer path) works for h3 like it does for
    /// hyper's TCP `Incoming` — the trait's default hint is `(0, None)`, which reads as
    /// "maybe a body" and silently disabled retry for every h3 request.
    remaining: Option<u64>,
}

impl H3ReqBody {
    pub(super) fn new(
        recv: h3::server::RequestStream<h3_quinn::RecvStream, Bytes>,
        content_length: Option<u64>,
    ) -> Self {
        Self {
            recv,
            remaining: content_length,
        }
    }
}

impl Body for H3ReqBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        match this.recv.poll_recv_data(cx) {
            Poll::Ready(Ok(Some(mut buf))) => {
                let bytes = buf.copy_to_bytes(buf.remaining());
                if let Some(remaining) = &mut this.remaining {
                    match remaining.checked_sub(bytes.len() as u64) {
                        Some(rest) => *remaining = rest,
                        // More DATA than the declared content-length is a malformed request
                        // (RFC 9114 §4.1.2): surface a body error instead of silently forwarding
                        // the extra bytes under the original declaration (bp-rust §6 — no second
                        // framing interpretation).
                        None => {
                            return Poll::Ready(Some(Err(
                                "h3 request body exceeds its declared content-length".into(),
                            )));
                        }
                    }
                }
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Poll::Ready(Ok(None)) => Poll::Ready(None),
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(Box::new(e)))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self.remaining {
            Some(n) => SizeHint::with_exact(n),
            None => SizeHint::default(),
        }
    }
}
