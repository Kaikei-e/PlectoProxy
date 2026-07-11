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
    RequestTrace, ResponseOutcome,
};

use crate::body::{INBOUND_BODY_READ_TIMEOUT, MAX_REQUEST_BODY_BUFFER, buffer_request_body};
use crate::error::ServerError;
use crate::forward::{ForwardBody, ForwardOutcome, ForwardRequest, forward_with_retry};
use crate::headers::{
    copy_headers, copy_headers_direct, headers_to_vec, set_forwarded, to_http_request,
};
use crate::respond::{
    discard_upstream_body, fault, http_response, stream_response, stream_response_direct, synth,
    synth_retry_after, with_error_code,
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
    mut parts: hyper::http::request::Parts,
    body: ReqBody,
) -> Result<Response<ResponseBody>, ServerError> {
    let mut http_req = to_http_request(&parts, scheme);
    // `exact() == Some(0)` is hyper's framing-accurate "no body", computed up front before the
    // body moves: only a bodyless request can be an Upgrade handshake (ADR 000048), and bodyless
    // maps to the trivially replayable `ForwardBody::Bodyless` retry contract (ADR 000058) below.
    let bodyless = body.size_hint().exact() == Some(0);

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
            return Ok(with_error_code(
                synth(
                    StatusCode::BAD_REQUEST,
                    &fault::BAD_PATH,
                    b"bad request path",
                ),
                &plecto_control::PATH_NORMALIZATION_REJECTED,
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
        return Ok(with_error_code(
            synth_retry_after(
                StatusCode::TOO_MANY_REQUESTS,
                &fault::RATE_LIMITED,
                b"rate limit exceeded",
                retry_after_ms.div_ceil(1000),
            ),
            &plecto_control::QUOTA_EXCEEDED,
        ));
    }

    // HTTP/1.1 Upgrade opt-in (ADR 000048): tunnel only when the route declares the token, the
    // client genuinely asked (`Connection: upgrade` + an allowlisted `Upgrade`), the transport
    // can hand its connection over (h1 — hyper left an OnUpgrade on the request; h2/h3 never
    // do), and the handshake is bodyless. Anything else stays a plain HTTP request with the
    // Upgrade/Connection pair stripped as hop-by-hop, exactly as before.
    let upgrade = if bodyless {
        route.upgrade.as_ref().and_then(|cfg| {
            let header = crate::headers::upgrade_request_header(&parts.headers)?;
            let token = cfg.allowed_token(header)?.to_string();
            let on_upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>()?;
            Some((token, on_upgrade, cfg.idle_timeout()))
        })
    } else {
        None
    };

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
    // The retry loop's view of the body (ADR 000058): bodyless and buffered bodies are
    // replayable, an opaque streamed body (ADR 000013) moves into its single attempt.
    let mut real_body = if bodyless {
        ForwardBody::Bodyless
    } else {
        ForwardBody::OneShot(body)
    };

    // --- request-side body hook (ADR 000025): buffer the body (bounded) ONLY when a filter on the
    // route actually reads it — i.e. exports `on-request-body` (`reads_body`, ADR 000038). A route
    // with no body-reading filter (or a bodyless request) skips this entirely and keeps the body on
    // the zero-copy streaming path — the real fix for the body-tax (docs/servey). The chain runs on
    // the blocking pool (sync wasmtime, !Send Store), like the header chain.
    if route.reads_body
        && let Some(b) = real_body.take_oneshot()
    {
        // Bound concurrent buffered-body memory and the time spent reading one body
        // (slow-body slowloris): hold a buffer permit and read under a deadline. Over the
        // size cap → 413, over the time budget → 408 — both fail closed (never an unbounded buffer).
        // An acquire error (the semaphore closed) must fail closed too, not silently proceed
        // without a permit — that would bypass the concurrency cap entirely for this request.
        let _buf_permit = match state.body_buffer_limit.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                return Ok(synth(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &fault::BODY_BUFFER_UNAVAILABLE,
                    b"body buffer unavailable",
                ));
            }
        };
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
            // The buffered, filter-edited bytes are replayable by definition (ADR 000058):
            // `Vec<u8>` → `Bytes` is a move, and each retry attempt shares it by reference count.
            RequestBodyOutcome::Forward(edited) => {
                real_body = ForwardBody::Replayable(bytes::Bytes::from(edited));
            }
        }
    }

    // Circuit breaker (ADR 000028): take an in-flight slot under this upstream's `max_requests` cap
    // before forwarding. At the cap, shed load with a fast-fail 503 rather than queueing work onto a
    // saturated backend. One slot per request, held across the retry loop and released by RAII on
    // every return path; an unlimited breaker (the default) is a zero-cost no-op permit. The breaker
    // is overload protection, NOT a health signal — a shed request never demotes an instance.
    let permit = match group.try_acquire() {
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
        upgrade_token: upgrade.as_ref().map(|(t, _, _)| t.as_str()),
    };
    // The client for this group's security context (ADR 000042): the shared plain client, or the
    // pooled TLS client for its `[upstream.tls]` config. A cheap clone (shared pool inside).
    let client = state.clients.for_group(&group);
    let (mut upstream_resp, upstream_pick) = match forward_with_retry(
        &client,
        &state.metrics,
        &group,
        pick,
        hash_key,
        forward_req,
        real_body,
        timeout,
        overall_deadline,
        group.max_retries(),
    )
    .await
    {
        ForwardOutcome::Response(resp, pick) => (resp, pick),
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

    // --- upgrade switch (ADR 000048): a verified 101 splices the two connections into an opaque
    // tunnel; any other status falls through to the normal response path (the handshake was
    // simply refused — the upstream's answer is a legitimate response). ---
    if upstream_resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        let Some((token, downstream_on, idle)) = upgrade else {
            // A 101 the client never solicited (or an unlisted token): relaying it would hand
            // the upstream a raw byte stream the client cannot parse as HTTP — response
            // smuggling. Fail closed (RFC 9110 §7.8: no unrequested switch).
            return Ok(synth(
                StatusCode::BAD_GATEWAY,
                &fault::BAD_UPGRADE,
                b"unsolicited upgrade",
            ));
        };
        // The upstream must switch to the token we offered (case-insensitive, RFC 9110 §7.8).
        let token_ok = upstream_resp
            .headers()
            .get(hyper::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(&token)));
        if !token_ok {
            return Ok(synth(
                StatusCode::BAD_GATEWAY,
                &fault::BAD_UPGRADE,
                b"upgrade token mismatch",
            ));
        }
        let upstream_on = hyper::upgrade::on(&mut upstream_resp);
        // The 101 back to the client: the response-side chain still sees the handshake headers
        // (status/headers only), end-to-end headers (e.g. Sec-WebSocket-Accept) pass the
        // hop-by-hop strip, then the switch signal itself is re-issued deliberately.
        let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
        if route.has_filters {
            let http_resp = HttpResponse {
                status: StatusCode::SWITCHING_PROTOCOLS.as_u16(),
                headers: headers_to_vec(upstream_resp.headers()),
                body: Vec::new(),
            };
            let snap_resp = snapshot.clone();
            // `forward` moves into the closure: this branch always returns, so the normal
            // response path below (which also consumes it) is never reached after this.
            let outcome = tokio::task::spawn_blocking(move || {
                snap_resp.dispatch_response(idx, &forward, http_resp)
            })
            .await?;
            let edited = match outcome {
                // A response filter replaced the handshake (or trapped fail-closed): honour
                // its response and never splice — the upstream connection drops with
                // `upstream_on`.
                ResponseOutcome::Respond(resp) => return Ok(http_response(resp)),
                ResponseOutcome::Forward(edited) => edited,
            };
            if edited.status != StatusCode::SWITCHING_PROTOCOLS.as_u16() {
                // A response filter vetoed the switch by demoting the status: honour that
                // response (header-only) and never splice.
                return Ok(http_response(edited));
            }
            copy_headers(builder.headers_mut(), &edited.headers);
        } else {
            copy_headers_direct(builder.headers_mut(), upstream_resp.headers());
        }
        if let Some(h) = builder.headers_mut() {
            if let Ok(v) = hyper::header::HeaderValue::from_str(&token) {
                h.insert(hyper::header::UPGRADE, v);
            }
            h.insert(
                hyper::header::CONNECTION,
                hyper::header::HeaderValue::from_static("upgrade"),
            );
        }
        let resp101 = builder
            .body(crate::body::full(Vec::new()))
            .map_err(ServerError::from)?;
        let drain = state.drain.clone();
        let metrics = state.metrics.clone();
        let tunnel_active = crate::metrics::TunnelActive::new(metrics.clone());
        tokio::spawn(async move {
            // Long-lived tunnels stay inside the existing resource accounting (ADR 000048): the
            // breaker permit (ADR 000028), the LB pick guard (least-request in-flight) and the
            // `tunnels_active` gauge guard (ADR 000059) live exactly as long as the tunnel, and
            // the drain flag closes it at shutdown. The byte totals are recorded once, at close.
            let _permit = permit;
            let _pick = upstream_pick;
            let _active = tunnel_active;
            let (down, up) = crate::tunnel::run(downstream_on, upstream_on, idle, drain).await;
            metrics.add_tunnel_bytes(down, up);
        });
        return Ok(resp101);
    }

    // --- response side: the route's chain in reverse (status / headers only) ---
    let (uparts, ubody) = upstream_resp.into_parts();
    if !route.has_filters {
        // No chain to run: skip the blocking-pool hop and the contract projection; the hop-by-hop
        // strip still applies, directly on the original header bytes.
        let resp = stream_response_direct(uparts.status, &uparts.headers, ubody);
        return Ok(crate::compression::apply(resp, &route, &parts));
    }
    let http_resp = HttpResponse {
        status: uparts.status.as_u16(),
        headers: headers_to_vec(&uparts.headers),
        body: Vec::new(), // header-only: filters never see the streamed body
    };
    // The response chain sees the AS-FORWARDED request snapshot (ADR 000073): `forward` is the
    // request exactly as it left the request-side chain (filter edits applied, before the
    // egress hop-by-hop strip / path rewrite / traceparent injection), moved here for free —
    // no per-request copy is added to hold it.
    let snap_resp = snapshot.clone();
    let outcome =
        tokio::task::spawn_blocking(move || snap_resp.dispatch_response(idx, &forward, http_resp))
            .await?;

    // The typed successor of the old in-band signal (ADR 000073): `Forward` sends the edited
    // status + headers and streams the upstream body through; `Respond` is a synthesised
    // response — a filter's `replace` or the chain's fail-closed 5xx. The upstream body is
    // discarded without blocking the client (background drain up to a cap, else socket close)
    // so a replace does not permanently poison the upstream connection pool.
    match outcome {
        ResponseOutcome::Forward(edited) => {
            // Compression runs LAST — after the chain's header edits, on the streamed body the
            // chain never sees (ADR 000074: filters always see identity). A `Respond` below is
            // host-framed synthesis (small, buffered), deliberately never compressed.
            let resp = stream_response(edited.status, &edited.headers, ubody);
            Ok(crate::compression::apply(resp, &route, &parts))
        }
        ResponseOutcome::Respond(resp) => {
            discard_upstream_body(ubody);
            Ok(http_response(resp))
        }
    }
}
