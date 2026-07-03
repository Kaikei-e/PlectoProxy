//! The imperative shell that drives `retry::should_attempt_retry` in a loop: send one attempt,
//! resolve an alternate instance if policy says to retry, back off, repeat. Generic over
//! `C: UpstreamClient` (audunhalland pattern — static dispatch), so this can be exercised against a
//! `FakeUpstreamClient` without a real socket.

use std::time::{Duration, Instant};

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
    Response(hyper::Response<ResponseBody>),
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
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_with_retry<C: UpstreamClient>(
    client: &C,
    metrics: &ServerMetrics,
    group: &UpstreamGroup,
    mut pick: plecto_control::Pick,
    hash_key: Option<HashInput<'_>>,
    forward: ForwardRequest<'_>,
    bodyless: bool,
    mut body: Option<ReqBody>,
    per_try_bound: Duration,
    overall_deadline: Option<Instant>,
    max_retries: u64,
) -> ForwardOutcome {
    let mut tries_left = max_retries;
    let mut attempt: u32 = 0;

    loop {
        let per_try = match retry::per_try_timeout(overall_deadline, per_try_bound, Instant::now())
        {
            retry::PerTryTimeout::OverallDeadlineElapsed => return ForwardOutcome::OverallTimeout,
            retry::PerTryTimeout::Bounded(d) => d,
        };

        // A bodyless request re-sends an empty body to each instance; a bodied one moves its
        // single streamed body and (since a bodied request is never retryable) is sent once.
        let attempt_body = if bodyless {
            crate::body::empty_req()
        } else {
            body.take().unwrap_or_else(crate::body::empty_req)
        };
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
                    bodyless,
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
                return ForwardOutcome::Response(resp);
            }
            None => {
                if overall_elapsed {
                    return ForwardOutcome::OverallTimeout;
                }
                let should_retry = retry::should_attempt_retry(
                    AttemptOutcome::TimedOut,
                    forward.method,
                    bodyless,
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
                    bodyless,
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
        for inst in &group.instances {
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
            true,
            None,
            Duration::from_secs(5),
            None,
            2,
        )
        .await;

        match outcome {
            ForwardOutcome::Response(resp) => assert_eq!(resp.status(), 200),
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
            true,
            None,
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
            true,
            None,
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
            true,
            None,
            Duration::from_secs(5),
            None,
            1,
        )
        .await;

        match outcome {
            ForwardOutcome::Response(resp) => assert_eq!(resp.status(), 200),
            _ => panic!("expected success after retrying the connect failure"),
        }
        assert_eq!(
            client.calls(),
            2,
            "a connect failure (upstream never received the request) is safe to retry for any method"
        );
    }
}
