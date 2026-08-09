//! Body adapters and the buffering behind the body hooks — `on-request-body` (ADR 000025) and
//! `on-response-body` (ADR 000098). The fast path boxes every inbound body into `ReqBody` and every
//! response body into `ResponseBody` so one type covers all three transports; the buffering bounds
//! memory and time for both hooks, against one shared byte budget.

use std::convert::Infallible;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use tokio::sync::OwnedSemaphorePermit;

use crate::{BoxError, ReqBody, ResponseBody};

/// The cap on a request body buffered for the `on-request-body` hook (ADR 000025). Buffer-then-
/// decide must bound memory: an unbounded buffer is a trivial OOM DoS, so a body larger than this
/// fails closed (413) rather than being read into RAM. A per-route override is a follow-up; the
/// constant keeps v1 safe. Header-only / bodyless requests never reach this path.
pub(crate) const MAX_REQUEST_BODY_BUFFER: usize = 16 << 20; // 16 MiB

/// The whole process's budget for bodies held in memory for a body hook, counted in BYTES and
/// shared by BOTH directions (ADR 000098). It replaces the old per-direction count of concurrent
/// buffers: with a second buffering plane, "N buffers" would have meant `N × cap × 2 directions`
/// of resident memory hiding behind one number, and each direction's cap is configurable
/// independently. One byte budget collapses that product into the single figure an operator can
/// reason about. 1 GiB is the ceiling the request side already nominally had
/// (64 buffers × 16 MiB); a request reserves its cap and a response its route's, each held for as
/// long as the bytes stay resident.
pub(crate) const MAX_INFLIGHT_BODY_BUFFER_BYTES: usize = 1 << 30;

/// How long the server spends reading a buffered request body before failing closed 408 (slow-body
/// slowloris): the body hook buffers, and an un-timed read would await trickled frames forever.
pub(crate) const INBOUND_BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A buffered body boxed into `ResponseBody` (its `Infallible` error widened to the boxed type).
pub(crate) fn full(bytes: Vec<u8>) -> ResponseBody {
    Full::new(Bytes::from(bytes))
        .map_err(|e: Infallible| -> BoxError { match e {} })
        .boxed_unsync()
}

/// The upstream's streamed body boxed into `ResponseBody`.
pub(crate) fn stream(body: Incoming) -> ResponseBody {
    body.map_err(|e| -> BoxError { Box::new(e) }).boxed_unsync()
}

/// Box a hyper `Incoming` inbound body into the transport-agnostic `ReqBody`.
pub(crate) fn box_incoming(body: Incoming) -> ReqBody {
    body.map_err(|e| -> BoxError { Box::new(e) }).boxed()
}

/// An empty `ReqBody` — used to re-send a bodyless request to another instance on retry (ADR
/// 000023), since the opaque streamed body (ADR 000013) cannot be replayed.
pub(crate) fn empty_req() -> ReqBody {
    Empty::<Bytes>::new()
        .map_err(|e: Infallible| -> BoxError { match e {} })
        .boxed()
}

/// What buffering a request body settled on: the caller maps each case to its own fail-closed
/// status (over-cap → 413, read fault → 400) instead of conflating a client abort with an
/// oversized body (they were previously one `None`).
pub(crate) enum BufferOutcome {
    Buffered(Vec<u8>),
    /// The body exceeded the cap; nothing over `max` was ever resident (bp-rust: DoS-aware).
    TooLarge,
    /// A frame-read fault (client abort / transport error) before the body completed.
    ReadError,
}

/// Buffer a request body for the `on-request-body` hook (ADR 000025), capped at `max` bytes.
/// Streams frame-by-frame so an over-cap body is rejected without first reading it all into
/// memory (data-plane no-panic / DoS-aware, bp-rust). The size hint seeds the buffer's capacity
/// (clamped to the cap — the hint is client-supplied and untrusted).
pub(crate) async fn buffer_request_body(mut body: ReqBody, max: usize) -> BufferOutcome {
    use hyper::body::Body;
    let hint = usize::try_from(body.size_hint().lower()).unwrap_or(usize::MAX);
    let mut buf = Vec::with_capacity(hint.min(max));
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return BufferOutcome::ReadError;
        };
        if let Ok(data) = frame.into_data() {
            if buf.len() + data.len() > max {
                return BufferOutcome::TooLarge;
            }
            buf.extend_from_slice(&data);
        }
    }
    BufferOutcome::Buffered(buf)
}

/// A buffered request body (post `on-request-body` hook, ADR 000025) boxed into `ReqBody` — one
/// attempt's view of a replayable body (ADR 000058). Takes `Bytes` so each retry attempt shares
/// the same buffer by reference count instead of copying it.
pub(crate) fn req_full(bytes: Bytes) -> ReqBody {
    Full::new(bytes)
        .map_err(|e: Infallible| -> BoxError { match e {} })
        .boxed()
}

/// How long the server spends reading a buffered response body before failing closed (ADR 000098).
/// The response plane needs its own bound for the same reason the request plane does: the headers
/// are being held, so a trickling upstream would hold them forever.
pub(crate) const UPSTREAM_BODY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// What buffering an upstream response body settled on (ADR 000098). Unlike the request side, an
/// over-cap body is not automatically fatal — the route's `over_cap` mode may still want to
/// forward it — so this hands back the bytes already read AND the untouched remainder rather than
/// discarding both.
pub(crate) enum ResponseBufferOutcome {
    Buffered(Vec<u8>),
    /// The body exceeded the cap: `head` is exactly `max` bytes, `rest` everything after them.
    OverCap {
        head: Vec<u8>,
        rest: ResponseBody,
    },
    /// A frame-read fault (upstream abort / transport error) before the body completed.
    ReadError,
}

/// Buffer an upstream response body for the `on-response-body` hook (ADR 000098), capped at `max`
/// bytes. Streams frame-by-frame, and on the first frame that crosses the cap it splits that frame
/// rather than reading further, so nothing beyond `max` is ever accumulated.
pub(crate) async fn buffer_response_body(
    mut body: ResponseBody,
    max: usize,
) -> ResponseBufferOutcome {
    use hyper::body::Body;
    let hint = usize::try_from(body.size_hint().lower()).unwrap_or(usize::MAX);
    let mut buf = Vec::with_capacity(hint.min(max));
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return ResponseBufferOutcome::ReadError;
        };
        // Trailers are out of scope for this hook (ADR 000098): they pass through untouched, so a
        // non-data frame is simply not part of what the filter inspects.
        if let Ok(data) = frame.into_data() {
            let room = max.saturating_sub(buf.len());
            if data.len() > room {
                let mut data = data;
                let head_tail = data.split_to(room);
                buf.extend_from_slice(&head_tail);
                return ResponseBufferOutcome::OverCap {
                    head: buf,
                    rest: prefixed(data, body),
                };
            }
            buf.extend_from_slice(&data);
        }
    }
    ResponseBufferOutcome::Buffered(buf)
}

/// Re-attach already-read bytes to the front of the stream they came from — how an over-cap body
/// is forwarded uninspected without the host having to hold all of it.
pub(crate) fn prefixed(prefix: Bytes, rest: ResponseBody) -> ResponseBody {
    PrefixedBody {
        prefix: Some(prefix),
        rest,
    }
    .boxed_unsync()
}

/// Keep a byte-budget reservation alive for exactly as long as the bytes it covers are resident.
/// Releasing it when buffering returns would free the budget while the response is still queued
/// for the wire — the same lifetime mistake the request plane's permit comment names.
pub(crate) fn hold_budget(body: ResponseBody, permit: OwnedSemaphorePermit) -> ResponseBody {
    BudgetedBody {
        inner: body,
        _permit: permit,
    }
    .boxed_unsync()
}

struct BudgetedBody {
    inner: ResponseBody,
    _permit: OwnedSemaphorePermit,
}

impl hyper::body::Body for BudgetedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, BoxError>>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

/// A [`ResponseBody`] that yields `prefix` first, then everything `rest` has left.
struct PrefixedBody {
    prefix: Option<Bytes>,
    rest: ResponseBody,
}

impl hyper::body::Body for PrefixedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if let Some(prefix) = this.prefix.take() {
            return std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(prefix))));
        }
        std::pin::Pin::new(&mut this.rest).poll_frame(cx)
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        let mut hint = self.rest.size_hint();
        if let Some(prefix) = &self.prefix {
            let extra = prefix.len() as u64;
            let upper = hint.upper().map(|u| u.saturating_add(extra));
            hint = match upper {
                Some(upper) => {
                    let mut h = hyper::body::SizeHint::new();
                    h.set_lower(hint.lower().saturating_add(extra));
                    h.set_upper(upper);
                    h
                }
                None => {
                    let mut h = hyper::body::SizeHint::new();
                    h.set_lower(hint.lower().saturating_add(extra));
                    h
                }
            };
        }
        hint
    }

    fn is_end_stream(&self) -> bool {
        self.prefix.is_none() && self.rest.is_end_stream()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response body delivered as several frames, so the cap can be crossed mid-frame.
    struct Frames(std::collections::VecDeque<Bytes>);

    impl hyper::body::Body for Frames {
        type Data = Bytes;
        type Error = BoxError;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, BoxError>>> {
            std::task::Poll::Ready(
                self.get_mut()
                    .0
                    .pop_front()
                    .map(|b| Ok(hyper::body::Frame::data(b))),
            )
        }
    }

    fn framed(chunks: Vec<&'static [u8]>) -> ResponseBody {
        Frames(chunks.into_iter().map(Bytes::from_static).collect()).boxed_unsync()
    }

    async fn collect(body: ResponseBody) -> Vec<u8> {
        BodyExt::collect(body).await.unwrap().to_bytes().to_vec()
    }

    #[tokio::test]
    async fn a_body_within_the_cap_is_buffered_whole() {
        match buffer_response_body(framed(vec![b"hello ", b"world"]), 64).await {
            ResponseBufferOutcome::Buffered(body) => assert_eq!(body, b"hello world".to_vec()),
            _ => panic!("expected a whole buffer"),
        }
    }

    #[tokio::test]
    async fn an_over_cap_body_splits_at_the_cap_and_keeps_the_remainder_streamable() {
        // Nothing beyond the cap is ever accumulated: the frame that crosses it is split, and the
        // leftover is handed back attached to the untouched rest of the stream — which is what
        // lets `passthrough` / `process-partial` forward the response without holding all of it.
        match buffer_response_body(framed(vec![b"0123456789", b"abcdefghij"]), 12).await {
            ResponseBufferOutcome::OverCap { head, rest } => {
                assert_eq!(head, b"0123456789ab".to_vec());
                assert_eq!(collect(rest).await, b"cdefghij".to_vec());
            }
            _ => panic!("expected an over-cap split"),
        }
    }

    #[tokio::test]
    async fn re_attached_bytes_lead_the_stream_they_came_from() {
        let body = prefixed(Bytes::from_static(b"head"), framed(vec![b"-tail"]));
        assert_eq!(collect(body).await, b"head-tail".to_vec());
    }

    #[tokio::test]
    async fn the_byte_budget_admits_concurrent_buffers_up_to_its_total_and_no_further() {
        // ADR 000098: one budget, both directions. A reservation is the buffer's cap, held for as
        // long as the bytes are resident, so what the number bounds is total resident bytes rather
        // than a count of buffers whose sizes are configured elsewhere.
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1024));
        let first = budget.clone().acquire_many_owned(768).await.unwrap();
        let second = budget.clone().acquire_many_owned(256).await.unwrap();
        assert!(
            budget.clone().try_acquire_many_owned(1).is_err(),
            "the budget is spent"
        );
        drop(second);
        assert!(
            budget.clone().try_acquire_many_owned(256).is_ok(),
            "releasing one buffer's bytes frees exactly those bytes for the other direction"
        );
        drop(first);
    }

    #[tokio::test]
    async fn a_held_budget_outlives_the_buffering_call_and_frees_with_the_bytes() {
        // Releasing the reservation when buffering returns would free the budget while the
        // response is still queued for the wire.
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(16));
        let permit = budget.clone().acquire_many_owned(16).await.unwrap();
        let body = hold_budget(framed(vec![b"bytes"]), permit);
        assert!(budget.clone().try_acquire_many_owned(1).is_err());
        assert_eq!(collect(body).await, b"bytes".to_vec());
        assert!(
            budget.try_acquire_many_owned(16).is_ok(),
            "dropping the body releases its reservation"
        );
    }
}
