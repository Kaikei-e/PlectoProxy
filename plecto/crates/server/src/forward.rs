//! The imperative shell that drives `retry::should_attempt_retry` in a loop: send one attempt,
//! resolve an alternate instance if policy says to retry, back off, repeat. Generic over
//! `C: UpstreamClient` (audunhalland pattern — static dispatch), so this can be exercised against a
//! `FakeUpstreamClient` without a real socket.

use std::time::{Duration, Instant};

use bytes::Bytes;
use hyper::header::HeaderValue;
use hyper::{HeaderMap, Request};
use plecto_control::{HashInput, UpstreamGroup};

use crate::headers::copy_headers_preserving;
use crate::metrics::ServerMetrics;
use crate::retry::{self, AttemptOutcome};
use crate::upstream_client::{UpstreamClient, UpstreamSendError};
use crate::{ReqBody, ResponseBody};

/// What `forward_with_retry` settled on, once the retry loop is done. The caller (`proxy_core_inner`)
/// already knows how to turn each of these into the right response / propagated error — this enum
/// only reports WHICH of those cases happened, not how to render it.
pub(crate) enum ForwardOutcome {
    /// The upstream answered. Carries the winning attempt's `Pick` so an upgrade tunnel
    /// (ADR 000048) can keep the least-request in-flight guard alive for the tunnel's lifetime;
    /// every other caller just drops it with the outcome.
    Response(hyper::Response<ResponseBody>, plecto_control::Pick),
    /// The overall request deadline (ADR 000031) elapsed, either before an attempt or while
    /// waiting on one.
    OverallTimeout,
    /// A per-try timeout (ADR 000019) elapsed and no further retry was taken.
    PerTryTimeout,
    /// The send itself failed on the final (non-retried) attempt.
    SendFailed(UpstreamSendError),
    /// Building the upstream request failed (a malformed method/URI/header).
    BuildFailed(hyper::http::Error),
}

/// The request body as the retry loop sees it (ADR 000058): whether an attempt's body can be
/// rebuilt for a re-send decides — together with the failure kind and method idempotency — whether
/// a retry is even considered. Bodyless and buffered bodies are replayable; an opaque streamed
/// body is not (it moves into its single attempt and is gone).
pub(crate) enum ForwardBody {
    /// No body at all (`size_hint().exact() == Some(0)`): each attempt sends a fresh empty body.
    Bodyless,
    /// An opaque streamed body (ADR 000013) — sent exactly once, never replayed.
    OneShot(ReqBody),
    /// A body already buffered for the `on-request-body` hook (ADR 000025 / 000038): each attempt
    /// rebuilds a `Full` from a `Bytes` clone — a reference-count bump, never a memory copy.
    Replayable(Bytes),
}

impl ForwardBody {
    /// Whether a failed attempt's body could be re-sent (ADR 000058): everything except the
    /// streamed one-shot. The retry DECISION still layers failure kind / idempotency / budget on
    /// top of this (`retry::should_attempt_retry`).
    fn replayable(&self) -> bool {
        match self {
            ForwardBody::Bodyless | ForwardBody::Replayable(_) => true,
            ForwardBody::OneShot(_) => false,
        }
    }

    /// Move a `OneShot` stream out for buffering (the `on-request-body` hook, ADR 000025),
    /// leaving `Bodyless` behind — the caller replaces that with `Replayable` once the hook
    /// forwards the edited bytes. `None` for the other variants (nothing to buffer).
    pub(crate) fn take_oneshot(&mut self) -> Option<ReqBody> {
        match self {
            ForwardBody::OneShot(_) => match std::mem::replace(self, ForwardBody::Bodyless) {
                ForwardBody::OneShot(body) => Some(body),
                ForwardBody::Bodyless | ForwardBody::Replayable(_) => None,
            },
            ForwardBody::Bodyless | ForwardBody::Replayable(_) => None,
        }
    }

    /// The body for the NEXT attempt. A `OneShot` moves out on the first call (subsequent calls
    /// yield an empty body, but a one-shot attempt is never retried, so none happen).
    fn attempt_body(&mut self) -> ReqBody {
        match self {
            ForwardBody::Bodyless => crate::body::empty_req(),
            ForwardBody::OneShot(body) => std::mem::replace(body, crate::body::empty_req()),
            ForwardBody::Replayable(bytes) => crate::body::req_full(bytes.clone()),
        }
    }
}

/// One forward attempt's static inputs (the parts of the request that don't change across
/// retries) — grouped so `forward_with_retry` isn't a wall of positional parameters.
pub(crate) struct ForwardRequest<'a> {
    pub(crate) method: &'a str,
    /// The chain-edited headers (contract `string` values).
    pub(crate) chain_headers: &'a [plecto_control::Header],
    /// The original inbound headers, so a pass-through header forwards byte-for-byte (P3#6).
    pub(crate) original_headers: &'a HeaderMap,
    pub(crate) upstream_path: &'a str,
    pub(crate) traceparent: &'a str,
    /// `Some(token)` when this is an allowlisted Upgrade handshake (ADR 000048): the exact
    /// (lower-cased) token to re-issue toward the upstream. `None` for every plain request.
    pub(crate) upgrade_token: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_with_retry<C: UpstreamClient>(
    client: &C,
    metrics: &ServerMetrics,
    group: &UpstreamGroup,
    mut pick: plecto_control::Pick,
    hash_key: Option<HashInput<'_>>,
    forward: ForwardRequest<'_>,
    mut body: ForwardBody,
    per_try_bound: Duration,
    overall_deadline: Option<Instant>,
    max_retries: u64,
) -> ForwardOutcome {
    let replayable = body.replayable();
    let mut tries_left = max_retries;
    let mut attempt: u32 = 0;

    loop {
        let per_try = match retry::per_try_timeout(overall_deadline, per_try_bound, Instant::now())
        {
            retry::PerTryTimeout::OverallDeadlineElapsed => return ForwardOutcome::OverallTimeout,
            retry::PerTryTimeout::Bounded(d) => d,
        };

        let attempt_body = body.attempt_body();
        // The scheme is the GROUP's (ADR 000042): `https` re-encrypts via the TLS client the
        // caller selected for this group, `http` keeps the plain pre-000042 leg.
        let uri = format!(
            "{}://{}{}",
            group.scheme(),
            pick.address(),
            forward.upstream_path
        );
        let mut builder = Request::builder().method(forward.method).uri(uri);
        copy_headers_preserving(
            builder.headers_mut(),
            forward.chain_headers,
            forward.original_headers,
        );
        if let Some(h) = builder.headers_mut()
            && let Ok(v) = HeaderValue::from_str(forward.traceparent)
        {
            h.insert("traceparent", v);
        }
        // TE: trailers pass-through (ADR 000042): on a TLS (h2-capable) leg, re-issue exactly
        // `te: trailers` when the CLIENT asked for trailers — gRPC uses it to detect incompatible
        // proxies (RFC 9113 §8.2.2 allows TE in h2 only with this value). The general hop-by-hop
        // strip removed the inbound header, so this is a controlled re-issue, never a forward of
        // arbitrary TE values; an `http` (h1-only) leg keeps stripping it, so a gRPC call to an
        // h1 upstream fails visibly rather than half-working without trailers.
        if group.scheme() == "https"
            && crate::headers::te_requests_trailers(forward.original_headers)
            && let Some(h) = builder.headers_mut()
        {
            h.insert(hyper::header::TE, HeaderValue::from_static("trailers"));
        }
        // Upgrade controlled re-issue (ADR 000048), the TE-shaped pattern above: the general
        // hop-by-hop strip removed the inbound Upgrade/Connection pair; on an upgrade-declared
        // route re-issue exactly the ONE allowlisted token — never a forward of arbitrary
        // Upgrade values (the h2c-smuggling guard lives in the allowlist, not here).
        if let Some(token) = forward.upgrade_token
            && let Some(h) = builder.headers_mut()
            && let Ok(v) = HeaderValue::from_str(token)
        {
            h.insert(hyper::header::UPGRADE, v);
            h.insert(
                hyper::header::CONNECTION,
                HeaderValue::from_static("upgrade"),
            );
        }
        let upstream_req = match builder.body(attempt_body) {
            Ok(req) => req,
            Err(e) => return ForwardOutcome::BuildFailed(e),
        };

        let send = client.request(upstream_req);
        let outcome = if per_try.is_zero() {
            Some(send.await)
        } else {
            tokio::time::timeout(per_try, send).await.ok()
        };

        let overall_elapsed = overall_deadline.is_some_and(|d| Instant::now() >= d);

        match outcome {
            Some(Ok(resp)) => {
                // Feed outlier detection (ADR 000032): a gateway-class 5xx the instance RETURNED
                // is a misbehaviour signal (a retried-around 5xx still counts). NOT a retry
                // decision by itself — that's should_attempt_retry below.
                let retriable_5xx = retry::is_retriable_5xx(resp.status());
                if group.record_outcome(pick.instance(), retriable_5xx) {
                    metrics.inc_outlier_ejection();
                }
                // `pick_excluding` has a side effect (it advances the load-balancer's
                // round-robin/least-request state), so it is called ONLY once policy already says
                // a retry should be attempted — never speculatively, to "just check" an alternate
                // exists.
                let should_retry = retry::should_attempt_retry(
                    AttemptOutcome::Response { retriable_5xx },
                    forward.method,
                    replayable,
                    tries_left,
                    overall_elapsed,
                );
                if should_retry
                    && let Some(next_pick) = group.pick_excluding(pick.instance(), hash_key)
                {
                    metrics.inc_retries();
                    retry::backoff(attempt).await;
                    attempt += 1;
                    tries_left -= 1;
                    pick = next_pick;
                    continue;
                }
                return ForwardOutcome::Response(resp, pick);
            }
            None => {
                if overall_elapsed {
                    return ForwardOutcome::OverallTimeout;
                }
                let should_retry = retry::should_attempt_retry(
                    AttemptOutcome::TimedOut,
                    forward.method,
                    replayable,
                    tries_left,
                    overall_elapsed,
                );
                if should_retry
                    && let Some(next_pick) = group.pick_excluding(pick.instance(), hash_key)
                {
                    metrics.inc_retries();
                    retry::backoff(attempt).await;
                    attempt += 1;
                    tries_left -= 1;
                    pick = next_pick;
                    continue;
                }
                return ForwardOutcome::PerTryTimeout;
            }
            Some(Err(e)) => {
                // A connect failure passively ejects (ADR 000017) — the upstream never received
                // the request, so it is not a health signal in the outlier sense, but the passive
                // failure counter still reacts. A non-connect fault is neither.
                let connect = e.is_connect();
                if connect {
                    pick.record_passive_failure();
                }
                let should_retry = retry::should_attempt_retry(
                    AttemptOutcome::Failed { connect },
                    forward.method,
                    replayable,
                    tries_left,
                    overall_elapsed,
                );
                if should_retry
                    && let Some(next_pick) = group.pick_excluding(pick.instance(), hash_key)
                {
                    metrics.inc_retries();
                    retry::backoff(attempt).await;
                    attempt += 1;
                    tries_left -= 1;
                    pick = next_pick;
                    continue;
                }
                return ForwardOutcome::SendFailed(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::ServerMetrics;
    use crate::upstream_client::SendErrorKind;
    use crate::upstream_client::fake::{FakeUpstreamClient, Scripted};

    /// A real `plecto_control::UpstreamGroup` with `addrs.len()` healthy instances, reconciled
    /// through the same `UpstreamRegistry` production code uses — no fake/mock LB state, only the
    /// upstream CLIENT is faked. Each instance is promoted healthy with one probe success
    /// (`healthy_threshold = 1`), mirroring the cold-start pattern `plecto-control`'s own tests use.
    fn test_group(
        addrs: &[&str],
        max_retries: u64,
    ) -> std::sync::Arc<plecto_control::UpstreamGroup> {
        let addr_list = addrs
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let toml = format!(
            r#"
            [[upstream]]
            name = "test"
            addresses = [{addr_list}]
            max_retries = {max_retries}
            [upstream.health]
            path = "/healthz"
            healthy_threshold = 1
            "#
        );
        let manifest = plecto_control::Manifest::from_toml(&toml).unwrap();
        let registry = plecto_control::UpstreamRegistry::new();
        registry
            .reconcile(&manifest.upstreams, std::path::Path::new("."))
            .unwrap();
        let group = registry.group("test").unwrap();
        for inst in &group.endpoints().instances {
            inst.record_probe_success();
        }
        group
    }

    fn req<'a>(method: &'a str, headers: &'a HeaderMap) -> ForwardRequest<'a> {
        ForwardRequest {
            method,
            chain_headers: &[],
            original_headers: headers,
            upstream_path: "/",
            traceparent: "test-trace",
            upgrade_token: None,
        }
    }

    #[tokio::test]
    async fn retries_onto_another_attempt_after_a_retriable_5xx_then_succeeds() {
        let group = test_group(&["127.0.0.1:1", "127.0.0.1:2"], 2);
        let pick = group.pick(None).unwrap();
        let client = FakeUpstreamClient::new(vec![
            Scripted::Status(503),
            Scripted::Status(503),
            Scripted::Status(200),
        ]);
        let metrics = ServerMetrics::new();
        let headers = HeaderMap::new();

        let outcome = forward_with_retry(
            &client,
            &metrics,
            &group,
            pick,
            None,
            req("GET", &headers),
            ForwardBody::Bodyless,
            Duration::from_secs(5),
            None,
            2,
        )
        .await;

        match outcome {
            ForwardOutcome::Response(resp, _) => assert_eq!(resp.status(), 200),
            _ => panic!("expected the retry sequence to end in a 200 response"),
        }
        assert_eq!(
            client.calls(),
            3,
            "two retriable 5xxs then a success = 3 attempts"
        );
    }

    #[tokio::test]
    async fn non_idempotent_post_is_not_retried_after_a_per_try_timeout() {
        let group = test_group(&["127.0.0.1:1", "127.0.0.1:2"], 2);
        let pick = group.pick(None).unwrap();
        let client = FakeUpstreamClient::new(vec![Scripted::Hang(Duration::from_millis(200))]);
        let metrics = ServerMetrics::new();
        let headers = HeaderMap::new();

        let outcome = forward_with_retry(
            &client,
            &metrics,
            &group,
            pick,
            None,
            req("POST", &headers),
            ForwardBody::Bodyless,
            Duration::from_millis(20),
            None,
            2,
        )
        .await;

        assert!(
            matches!(outcome, ForwardOutcome::PerTryTimeout),
            "expected a per-try timeout outcome"
        );
        assert_eq!(
            client.calls(),
            1,
            "a non-idempotent method must not be retried after a timeout"
        );
    }

    #[tokio::test]
    async fn overall_deadline_already_elapsed_fails_closed_without_a_send() {
        let group = test_group(&["127.0.0.1:1"], 2);
        let pick = group.pick(None).unwrap();
        let client = FakeUpstreamClient::new(vec![Scripted::Status(200)]);
        let metrics = ServerMetrics::new();
        let headers = HeaderMap::new();
        let elapsed_deadline = Some(Instant::now() - Duration::from_millis(1));

        let outcome = forward_with_retry(
            &client,
            &metrics,
            &group,
            pick,
            None,
            req("GET", &headers),
            ForwardBody::Bodyless,
            Duration::from_secs(5),
            elapsed_deadline,
            2,
        )
        .await;

        assert!(matches!(outcome, ForwardOutcome::OverallTimeout));
        assert_eq!(
            client.calls(),
            0,
            "an already-elapsed overall deadline must not attempt any send"
        );
    }

    #[tokio::test]
    async fn connect_failure_is_retried_even_for_a_non_idempotent_method() {
        let group = test_group(&["127.0.0.1:1", "127.0.0.1:2"], 1);
        let pick = group.pick(None).unwrap();
        let client = FakeUpstreamClient::new(vec![
            Scripted::SendError(SendErrorKind::Connect),
            Scripted::Status(200),
        ]);
        let metrics = ServerMetrics::new();
        let headers = HeaderMap::new();

        let outcome = forward_with_retry(
            &client,
            &metrics,
            &group,
            pick,
            None,
            req("POST", &headers),
            ForwardBody::Bodyless,
            Duration::from_secs(5),
            None,
            1,
        )
        .await;

        match outcome {
            ForwardOutcome::Response(resp, _) => assert_eq!(resp.status(), 200),
            _ => panic!("expected success after retrying the connect failure"),
        }
        assert_eq!(
            client.calls(),
            2,
            "a connect failure (upstream never received the request) is safe to retry for any method"
        );
    }

    #[tokio::test]
    async fn replayable_body_is_resent_intact_on_a_connect_failure() {
        // ADR 000058: a buffered body is replayable, so a connect failure — which never reached
        // the upstream — retries onto another instance for ANY method, and the retried attempt
        // must carry the exact same bytes.
        let group = test_group(&["127.0.0.1:1", "127.0.0.1:2"], 1);
        let pick = group.pick(None).unwrap();
        let client = FakeUpstreamClient::new(vec![
            Scripted::SendError(SendErrorKind::Connect),
            Scripted::Status(200),
        ]);
        let metrics = ServerMetrics::new();
        let headers = HeaderMap::new();

        let outcome = forward_with_retry(
            &client,
            &metrics,
            &group,
            pick,
            None,
            req("POST", &headers),
            ForwardBody::Replayable(Bytes::from_static(b"buffered payload")),
            Duration::from_secs(5),
            None,
            1,
        )
        .await;

        match outcome {
            ForwardOutcome::Response(resp, _) => assert_eq!(resp.status(), 200),
            _ => panic!("expected the connect failure to be retried onto the other instance"),
        }
        assert_eq!(client.calls(), 2);
        assert_eq!(
            client.bodies(),
            vec![Bytes::from_static(b"buffered payload"); 2],
            "every attempt must re-send the buffered body intact"
        );
    }

    #[tokio::test]
    async fn replayable_idempotent_request_is_retried_on_a_retriable_5xx() {
        // The decision table is unchanged (ADR 000058): a retriable 5xx retries only an
        // idempotent method — but a buffered PUT body no longer disqualifies the request.
        let group = test_group(&["127.0.0.1:1", "127.0.0.1:2"], 1);
        let pick = group.pick(None).unwrap();
        let client = FakeUpstreamClient::new(vec![Scripted::Status(503), Scripted::Status(200)]);
        let metrics = ServerMetrics::new();
        let headers = HeaderMap::new();

        let outcome = forward_with_retry(
            &client,
            &metrics,
            &group,
            pick,
            None,
            req("PUT", &headers),
            ForwardBody::Replayable(Bytes::from_static(b"idempotent payload")),
            Duration::from_secs(5),
            None,
            1,
        )
        .await;

        match outcome {
            ForwardOutcome::Response(resp, _) => assert_eq!(resp.status(), 200),
            _ => panic!("expected the 503 to be retried onto the healthy instance"),
        }
        assert_eq!(client.calls(), 2);
    }

    #[tokio::test]
    async fn replayable_non_idempotent_request_is_not_retried_on_a_timeout() {
        // Replayability and idempotency are separate questions (ADR 000058): a timeout may
        // already have been acted on by the upstream, so a buffered POST still never retries.
        let group = test_group(&["127.0.0.1:1", "127.0.0.1:2"], 2);
        let pick = group.pick(None).unwrap();
        let client = FakeUpstreamClient::new(vec![Scripted::Hang(Duration::from_millis(200))]);
        let metrics = ServerMetrics::new();
        let headers = HeaderMap::new();

        let outcome = forward_with_retry(
            &client,
            &metrics,
            &group,
            pick,
            None,
            req("POST", &headers),
            ForwardBody::Replayable(Bytes::from_static(b"post payload")),
            Duration::from_millis(20),
            None,
            2,
        )
        .await;

        assert!(matches!(outcome, ForwardOutcome::PerTryTimeout));
        assert_eq!(
            client.calls(),
            1,
            "a non-idempotent method must not be retried after a timeout, replayable or not"
        );
    }

    #[tokio::test]
    async fn replayable_retry_shares_the_bounded_budget() {
        // A replayable re-send stays inside the existing bounded-retry frame (ADR 000058
        // consequences): max_retries attempts of backoff-and-retry, then the failure surfaces.
        let group = test_group(&["127.0.0.1:1", "127.0.0.1:2"], 2);
        let pick = group.pick(None).unwrap();
        let client = FakeUpstreamClient::new(vec![Scripted::Status(503)]);
        let metrics = ServerMetrics::new();
        let headers = HeaderMap::new();

        let outcome = forward_with_retry(
            &client,
            &metrics,
            &group,
            pick,
            None,
            req("PUT", &headers),
            ForwardBody::Replayable(Bytes::from_static(b"exhausted payload")),
            Duration::from_secs(5),
            None,
            2,
        )
        .await;

        match outcome {
            ForwardOutcome::Response(resp, _) => assert_eq!(
                resp.status(),
                503,
                "once the budget is spent the last 503 surfaces"
            ),
            _ => panic!("expected the final 503 response"),
        }
        assert_eq!(
            client.calls(),
            3,
            "max_retries = 2 bounds a replayable request to 3 attempts total"
        );
    }

    #[tokio::test]
    async fn oneshot_streamed_body_is_never_retried() {
        // ADR 000058 changes nothing for the streamed path: a one-shot body moves into its single
        // attempt, so even a connect failure surfaces instead of retrying.
        let group = test_group(&["127.0.0.1:1", "127.0.0.1:2"], 2);
        let pick = group.pick(None).unwrap();
        let client = FakeUpstreamClient::new(vec![
            Scripted::SendError(SendErrorKind::Connect),
            Scripted::Status(200),
        ]);
        let metrics = ServerMetrics::new();
        let headers = HeaderMap::new();

        let outcome = forward_with_retry(
            &client,
            &metrics,
            &group,
            pick,
            None,
            req("POST", &headers),
            ForwardBody::OneShot(crate::body::req_full(Bytes::from_static(b"streamed"))),
            Duration::from_secs(5),
            None,
            2,
        )
        .await;

        assert!(
            matches!(outcome, ForwardOutcome::SendFailed(_)),
            "a streamed body must surface the connect failure unretried"
        );
        assert_eq!(client.calls(), 1);
    }
}
