//! The transport-agnostic transaction core: route → chain (request side) → forward → chain
//! (response side). HTTP/1.1, HTTP/2 and HTTP/3 all funnel through `proxy_core`; only the body
//! adapters differ. Bounded retry onto another instance (ADR 000023, hardened with jittered
//! backoff + retriable-5xx retry in ADR 000030) and the `on-request-body` hook (ADR 000025) live in
//! `forward`/`retry`; this module wires routing, rate-limiting, and the chain around that call.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use hyper::body::Body;
use hyper::{Response, StatusCode};
use plecto_control::otlp::SpanRecord;
use plecto_control::{
    ChainOutcome, HashInput, HashKeySource, HttpResponse, RateLimitDecision, RequestBodyOutcome,
    RequestTrace,
};

use crate::body::{
    INBOUND_BODY_READ_TIMEOUT, MAX_REQUEST_BODY_BUFFER, buffer_request_body, req_full,
};
use crate::error::ServerError;
use crate::forward::{ForwardOutcome, ForwardRequest, forward_with_retry};
use crate::headers::{headers_to_vec, set_forwarded, to_http_request};
use crate::respond::{
    fault, http_response, stream_response, stream_response_direct, synth, synth_retry_after,
};
use crate::{ReqBody, ResponseBody, ServerState, access_log};

/// The transport-agnostic transaction core (Stage A observability wrapper, ADR 000009). Every
/// transport funnels through here, so it is the one place to tally per-request metrics and emit the
/// access log: it times `proxy_core_inner`, records the RED signals, and — when enabled — logs the
/// request, then returns the inner result unchanged.
pub(crate) async fn proxy_core(
    state: Arc<ServerState>,
    scheme: &'static str,
    peer: SocketAddr,
    parts: hyper::http::request::Parts,
    body: ReqBody,
) -> Result<Response<ResponseBody>, ServerError> {
    /// Decrements the in-flight gauge on drop: hyper drops this future when the client
    /// connection dies mid-request (h2 RST_STREAM, disconnect), so a plain post-`.await`
    /// decrement would leak and the gauge would drift upward monotonically. The RED tally
    /// (`record_request`) is deliberately still skipped on cancellation — no response was sent.
    struct InFlight<'a>(&'a crate::metrics::ServerMetrics);
    impl Drop for InFlight<'_> {
        fn drop(&mut self) {
            self.0.dec_in_flight();
        }
    }

    let start = Instant::now();
    state.metrics.inc_in_flight();
    let in_flight = InFlight(&state.metrics);

    // Capture the access-log fields BEFORE the core consumes `parts`, and only when logging is on —
    // a disabled access log allocates nothing on the hot path.
    let access = state
        .control
        .access_log_enabled()
        .then(|| access_log::Access {
            method: parts.method.as_str().to_string(),
            authority: parts
                .uri
                .authority()
                .map(|a| a.to_string())
                .or_else(|| {
                    parts
                        .headers
                        .get(hyper::header::HOST)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                })
                .unwrap_or_default(),
            path: parts.uri.path().to_string(),
        });

    // Continue an inbound distributed trace (ADR 000009): if the caller sent a W3C `traceparent`,
    // parse it and pin the transaction to it so Plecto's spans JOIN the caller's trace — the
    // inbound span becomes the REMOTE PARENT of a locally-minted request span (ADR 000040).
    // Fail-soft: a missing / malformed header falls back to a new root, never a panic on
    // untrusted input (review f000005 P1#2; `from_traceparent` is the fail-soft parser).
    let trace = parts
        .headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .and_then(RequestTrace::from_traceparent)
        .unwrap_or_else(RequestTrace::root);

    // Request-span fields for OTLP export (ADR 000040), captured BEFORE the core consumes
    // `parts` and only when a configured exporter will actually see them — like the access log,
    // a disabled (or unsampled) transaction allocates nothing on the hot path.
    let otlp_request = state.otlp.as_ref().filter(|_| trace.is_sampled()).map(|_| {
        (
            parts.method.as_str().to_string(),
            parts.uri.path().to_string(),
            SystemTime::now(),
        )
    });

    let result = proxy_core_inner(state.clone(), scheme, peer, trace, parts, body).await;

    drop(in_flight);
    let status = match &result {
        Ok(resp) => resp.status().as_u16(),
        // an inner error is mapped to 502 by the caller (`dispatch::handle`), so record it as such.
        Err(_) => StatusCode::BAD_GATEWAY.as_u16(),
    };
    let elapsed = start.elapsed();
    state.metrics.record_request(status, elapsed);
    if let Some(access) = access {
        access_log::record(scheme, peer, &access, status, elapsed);
    }
    // One SERVER span per sampled transaction (ADR 000040): the root the filter spans (and the
    // upstream's own trace, via the propagated traceparent) nest under. Push is a bounded-queue
    // append — a slow collector can never back-pressure this path.
    if let (Some(buffer), Some((method, path, started))) = (state.otlp.as_ref(), otlp_request) {
        buffer.push(SpanRecord::request_span(
            &trace, &method, &path, scheme, status, started, elapsed,
        ));
    }
    result
}

/// The transaction core proper: route → chain (request side) → forward → chain (response side).
/// Takes the request head + a boxed body (so HTTP/1.1, HTTP/2 and HTTP/3 all share it) and returns
/// the client-visible response. Errors here are upstream/transport failures the caller maps to 502.
async fn proxy_core_inner(
    state: Arc<ServerState>,
    scheme: &'static str,
    peer: SocketAddr,
    trace: RequestTrace,
    parts: hyper::http::request::Parts,
    body: ReqBody,
) -> Result<Response<ResponseBody>, ServerError> {
    let mut http_req = to_http_request(&parts, scheme);

    // Normalize the request path once at ingress (CWE-22 Path Traversal / CWE-436
    // Interpretation Conflict): route selection, the filter chain, and the forwarded path then all
    // use the SAME normalized path, so the upstream cannot re-derive a stricter path than the
    // (possibly laxer, unfiltered) route we selected — closing the per-route-filter bypass. An
    // ambiguous (encoded-separator) or root-escaping path is rejected fail-closed.
    match plecto_control::normalize_path(&http_req.path) {
        // Borrowed = already normalized (the common case); only a rewritten path is stored back.
        Some(std::borrow::Cow::Owned(path)) => http_req.path = path,
        Some(std::borrow::Cow::Borrowed(_)) => {}
        None => {
            return Ok(synth(
                StatusCode::BAD_REQUEST,
                &fault::BAD_PATH,
                b"bad request path",
            ));
        }
    }

    // Client-IP propagation, edge model (ADR 000018 / review f000005 P2#3): strip any inbound
    // `X-Forwarded-*` / `Forwarded` (which an untrusted client can forge) and set them afresh from
    // the connection's real peer + scheme. Done BEFORE the chain so an IP-based rate-limit / auth
    // filter sees a value it can trust; the corrected headers then forward to the upstream.
    set_forwarded(&mut http_req.headers, peer.ip(), scheme);

    // One snapshot pins config + trace for the whole transaction (a concurrent reload cannot
    // desync the request and response halves); cloning it is a cheap Arc + trace-id clone. The
    // trace itself was resolved by `proxy_core`, which also emits the request span (ADR 000040).
    let snapshot = state.control.snapshot_with_trace(trace);
    // Match against the full request — host, path, method, headers, query (ADR 000034); the most
    // specific route wins. `http_req` carries the forwarded-header-corrected inbound request.
    let Some(route) = snapshot.find_route(&http_req) else {
        return Ok(synth(StatusCode::NOT_FOUND, &fault::NO_ROUTE, b"no route"));
    };
    let idx = route.index;

    // Native rate limit (ADR 000033): a coarse token-bucket baseline consulted at the front door —
    // BEFORE the route's filter chain — so a flood is shed without spending any WASM CPU. The
    // peer-keyed (or route-keyed) bucket math is host-native and never crosses the WASM boundary.
    // Over the limit fails closed with 429 + `Retry-After`, distinct from the breaker's 503
    // (`circuit-open`, upstream saturated): this is the client over its inbound rate floor. The
    // per-filter `host-ratelimit` capability (ADR 000026) is a separate, policy-shaped limiter.
    if let RateLimitDecision::Limit { retry_after_ms } = route.check_rate_limit(peer.ip()) {
        state.metrics.inc_rate_limited();
        return Ok(synth_retry_after(
            StatusCode::TOO_MANY_REQUESTS,
            &fault::RATE_LIMITED,
            b"rate limit exceeded",
            retry_after_ms.div_ceil(1000),
        ));
    }

    // --- request side: the route's chain on the blocking pool (sync wasmtime, !Send Store).
    // A route with no filters skips the hop entirely — an empty chain is the identity, and the
    // blocking-pool handoff (~µs each way) would be the pure-proxy path's single largest tax. ---
    let forward = if route.has_filters {
        let snap_req = snapshot.clone();
        match tokio::task::spawn_blocking(move || snap_req.dispatch_request(idx, http_req)).await? {
            ChainOutcome::Respond(resp) => return Ok(http_response(resp)),
            ChainOutcome::Forward(req) => req,
        }
    } else {
        http_req
    };

    // Weighted traffic split (ADR 000034): pick which backend upstream group to forward to, in the
    // route's weighted proportion, skipping any backend with no eligible instance (renormalize over
    // healthy). `None` = no backend has a healthy instance → fail closed 503 (the no-healthy fault,
    // ADR 000017 / 000024). The chosen group then drives the existing instance LB / retry / breaker /
    // health below; a single-upstream route is a one-element split, so this is uniform.
    let Some(group) = route.pick_upstream() else {
        return Ok(synth(
            StatusCode::SERVICE_UNAVAILABLE,
            &fault::NO_HEALTHY_UPSTREAM,
            b"no healthy upstream",
        ));
    };

    // Maglev consistent-hashing key (ADR 000035): for a `maglev` upstream, project the request's
    // hash key — a named header's value (borrowed bytes) or the connection peer's IP (hashed as
    // canonical octets, NOT a spoofable forwarding header). `None` for the other algorithms, or when
    // the configured header is absent (the group then falls back to round-robin). Borrowed from
    // `parts`, which outlives the retry loop, and `Copy`, so each attempt reuses it unchanged.
    let hash_key: Option<HashInput> = group.hash_key_source().and_then(|src| match src {
        HashKeySource::Header(name) => parts
            .headers
            .get(name)
            .map(|v| HashInput::Bytes(v.as_bytes())),
        HashKeySource::SourceIp => Some(HashInput::Ip(peer.ip())),
    });

    // --- forward to a healthy instance, with bounded retry onto ANOTHER instance on a retryable
    // failure (ADR 000019 timeout / 000023 retry). The per-attempt invariants are computed once. ---
    let upstream_path = route.rewrite_path(&forward.path);
    let timeout = group.request_timeout();
    // Overall request deadline across all attempts + backoff (ADR 000031); `None` = no overall bound
    // (only the per-try `timeout` applies). Pinned once, so the budget shrinks as retries consume it.
    let overall = group.overall_timeout();
    let overall_deadline = (!overall.is_zero()).then(|| Instant::now() + overall);
    // Only a bodyless request can be retried without buffering: the opaque streamed body
    // (ADR 000013) can't be replayed. `exact() == Some(0)` is hyper's framing-accurate "no body".
    let bodyless = body.size_hint().exact() == Some(0);
    let mut real_body = Some(body);

    // --- request-side body hook (ADR 000025): buffer the body (bounded) ONLY when a filter on the
    // route actually reads it — i.e. exports `on-request-body` (`reads_body`, ADR 000038). A route
    // with no body-reading filter (or a bodyless request) skips this entirely and keeps the body on
    // the zero-copy streaming path — the real fix for the body-tax (docs/servey). The chain runs on
    // the blocking pool (sync wasmtime, !Send Store), like the header chain.
    if route.reads_body
        && !bodyless
        && let Some(b) = real_body.take()
    {
        // Bound concurrent buffered-body memory and the time spent reading one body
        // (slow-body slowloris): hold a buffer permit and read under a deadline. Over the
        // size cap → 413, over the time budget → 408 — both fail closed (never an unbounded buffer).
        let _buf_permit = state.body_buffer_limit.clone().acquire_owned().await.ok();
        let buffered = match tokio::time::timeout(
            INBOUND_BODY_READ_TIMEOUT,
            buffer_request_body(b, MAX_REQUEST_BODY_BUFFER),
        )
        .await
        {
            Ok(Some(buf)) => buf,
            Ok(None) => {
                return Ok(synth(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    &fault::BODY_TOO_LARGE,
                    b"request body too large",
                ));
            }
            Err(_) => {
                return Ok(synth(
                    StatusCode::REQUEST_TIMEOUT,
                    &fault::BODY_TIMEOUT,
                    b"request body read timeout",
                ));
            }
        };
        let snap_body = snapshot.clone();
        match tokio::task::spawn_blocking(move || snap_body.dispatch_request_body(idx, buffered))
            .await?
        {
            RequestBodyOutcome::Respond(resp) => return Ok(http_response(resp)),
            RequestBodyOutcome::Forward(edited) => real_body = Some(req_full(edited)),
        }
    }

    // Circuit breaker (ADR 000028): take an in-flight slot under this upstream's `max_requests` cap
    // before forwarding. At the cap, shed load with a fast-fail 503 rather than queueing work onto a
    // saturated backend. One slot per request, held across the retry loop and released by RAII on
    // every return path; an unlimited breaker (the default) is a zero-cost no-op permit. The breaker
    // is overload protection, NOT a health signal — a shed request never demotes an instance.
    let _permit = match group.try_acquire() {
        Some(permit) => permit,
        None => {
            state.metrics.inc_circuit_open();
            return Ok(synth(
                StatusCode::SERVICE_UNAVAILABLE,
                &fault::CIRCUIT_OPEN,
                b"upstream overloaded",
            ));
        }
    };

    // First pick per the upstream's LB algorithm (ADR 000035): round-robin, least-request (P2C), or
    // maglev (by `hash_key`). Fail closed (503) if no instance is eligible (ADR 000017). The `Pick`
    // carries the least-request load guard (a no-op otherwise) and is held across the retry loop, so
    // an instance's active-request count is decremented on every exit and on each retry hand-off.
    let Some(pick) = group.pick(hash_key) else {
        return Ok(synth(
            StatusCode::SERVICE_UNAVAILABLE,
            &fault::NO_HEALTHY_UPSTREAM,
            b"no healthy upstream",
        ));
    };

    let forward_req = ForwardRequest {
        method: forward.method.as_str(),
        chain_headers: &forward.headers,
        original_headers: &parts.headers,
        upstream_path: &upstream_path,
        traceparent: &snapshot.traceparent(),
    };
    let upstream_resp = match forward_with_retry(
        &state.client,
        &state.metrics,
        &group,
        pick,
        hash_key,
        forward_req,
        bodyless,
        real_body,
        timeout,
        overall_deadline,
        group.max_retries(),
    )
    .await
    {
        ForwardOutcome::Response(resp) => resp,
        ForwardOutcome::OverallTimeout => {
            return Ok(synth(
                StatusCode::GATEWAY_TIMEOUT,
                &fault::REQUEST_TIMEOUT,
                b"request timeout",
            ));
        }
        ForwardOutcome::PerTryTimeout => {
            return Ok(synth(
                StatusCode::GATEWAY_TIMEOUT,
                &fault::UPSTREAM_TIMEOUT,
                b"upstream timeout",
            ));
        }
        ForwardOutcome::SendFailed(e) => return Err(e.into()),
        ForwardOutcome::BuildFailed(e) => return Err(e.into()),
    };

    // --- response side: the route's chain in reverse (status / headers only) ---
    let (uparts, ubody) = upstream_resp.into_parts();
    if !route.has_filters {
        // No chain to run: skip the blocking-pool hop and the contract projection; the hop-by-hop
        // strip still applies, directly on the original header bytes.
        return Ok(stream_response_direct(
            uparts.status,
            &uparts.headers,
            ubody,
        ));
    }
    let http_resp = HttpResponse {
        status: uparts.status.as_u16(),
        headers: headers_to_vec(&uparts.headers),
        body: Vec::new(), // header-only: filters never see the streamed body
    };
    let snap_resp = snapshot.clone();
    let edited =
        tokio::task::spawn_blocking(move || snap_resp.dispatch_response(idx, http_resp)).await?;

    // A response filter that trapped fail-closed yields a synthetic 5xx WITH a body; only then is
    // `body` non-empty (a normal response-edit can set status/headers but not a body). So: a
    // non-empty body means "use the synthetic response, drop the upstream stream"; an empty body
    // means "send the edited status + headers and stream the upstream body through".
    if edited.body.is_empty() {
        Ok(stream_response(
            edited.status,
            &edited.headers,
            &uparts.headers,
            ubody,
        ))
    } else {
        Ok(http_response(edited))
    }
}
