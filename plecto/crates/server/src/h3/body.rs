//! Adapts an HTTP/3 request's recv stream into an `http_body::Body`, so the request body streams to
//! the upstream like any other inbound body. One copy per chunk into `Bytes` (the recv buffer's own
//! type is opaque); the body is otherwise opaque pass-through.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use hyper::body::{Body, Frame};

use crate::BoxError;

pub(super) struct H3ReqBody {
    recv: h3::server::RequestStream<h3_quinn::RecvStream, Bytes>,
}

impl H3ReqBody {
    pub(super) fn new(recv: h3::server::RequestStream<h3_quinn::RecvStream, Bytes>) -> Self {
        Self { recv }
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
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Poll::Ready(Ok(None)) => Poll::Ready(None),
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(Box::new(e)))),
            Poll::Pending => Poll::Pending,
        }
    }
}
