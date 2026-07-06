//! Pure retry/backoff/timeout DECISION logic (ADR 000023 / 000030 / 000031), extracted from the
//! imperative shell that drives it (`forward::forward_with_retry`). No I/O, no hyper types beyond
//! what it already owns — every function here is directly unit-testable with plain values.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyper::StatusCode;

/// A retryable upstream failure (ADR 000023). A timeout may already have been acted on by the
/// upstream; a connect failure never reached it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Failure {
    Timeout,
    Connect,
}

/// RFC 9110 §9.2.2 idempotent methods — safe to retry on a timeout. Matched case-sensitively
/// (standard methods are uppercase tokens); any other token is treated as non-idempotent.
pub(crate) fn is_idempotent(method: &str) -> bool {
    matches!(
        method,
        "GET" | "HEAD" | "PUT" | "DELETE" | "OPTIONS" | "TRACE"
    )
}

/// Whether a failed forward MAY be retried on another instance (ADR 000023) — independent of whether
/// a different instance is actually available (the caller checks that). A retry needs remaining
/// budget and a replayable body — bodyless, or buffered for the `on-request-body` hook (ADR
/// 000058); a timeout additionally needs an idempotent method, while a connect failure is safe for
/// any method (the upstream never received the request).
pub(crate) fn may_retry(failure: Failure, method: &str, replayable: bool, tries_left: u64) -> bool {
    replayable
        && tries_left > 0
        && match failure {
            Failure::Timeout => is_idempotent(method),
            Failure::Connect => true,
        }
}

/// The retriable gateway-class upstream statuses (ADR 000030): 502 / 503 / 504. A 5xx means the
/// upstream RECEIVED and processed the request, so retrying it is safe only for an idempotent method
/// (like a timeout) — and it never demotes the instance (5xx-driven ejection is outlier detection,
/// ADR 000032, a separate axis).
pub(crate) fn is_retriable_5xx(status: StatusCode) -> bool {
    // 502 Bad Gateway, 503 Service Unavailable, 504 Gateway Timeout — the gateway-error class.
    matches!(status.as_u16(), 502..=504)
}

/// Full-jitter exponential backoff base / cap in milliseconds (ADR 000030). Projected
/// Envoy-reference defaults, not tuned by measurement: a retry waits a uniform-random delay in
/// `[0, min(cap, base · 2^attempt)]`, so concurrent clients' retries spread out instead of
/// thundering onto a recovering upstream in lockstep.
const RETRY_BACKOFF_BASE_MS: u64 = 25;
const RETRY_BACKOFF_CAP_MS: u64 = 250;

/// Sleep a full-jitter exponential backoff before retry `attempt` (0-based, ADR 000030). The ceiling
/// doubles per attempt up to the cap; the actual wait is uniform in `[0, ceiling]`.
pub(crate) async fn backoff(attempt: u32) {
    let ceiling = RETRY_BACKOFF_BASE_MS
        .saturating_mul(1u64 << attempt.min(16))
        .min(RETRY_BACKOFF_CAP_MS);
    let delay = jitter(ceiling);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

/// A uniform-ish pseudo-random value in `[0, ceiling]` for retry jitter. Non-cryptographic (retry
/// timing is not a secret) — seeded from the wall-clock sub-second, which differs per call so
/// concurrent requests pick different waits. `ceiling == 0` yields 0 (no wait).
pub(crate) fn jitter(ceiling_ms: u64) -> u64 {
    if ceiling_ms == 0 {
        return 0;
    }
    let entropy = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    entropy % (ceiling_ms + 1)
}

/// The per-try timeout given the overall deadline (ADR 000031), or that the overall deadline has
/// already elapsed — a plain enum rather than a sentinel `Duration`, so the caller cannot
/// accidentally treat "elapsed" as "wait zero".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PerTryTimeout {
    Bounded(Duration),
    OverallDeadlineElapsed,
}

/// Pure: the timeout for the NEXT attempt given `now`, the overall deadline (if any), and the
/// per-try bound (ADR 000019 / 000031). `now` is an explicit argument (not `Instant::now()` read
/// inside) — the cheapest correct seam for testing this decision point without a `Clock` trait.
pub(crate) fn per_try_timeout(
    overall_deadline: Option<Instant>,
    per_try_bound: Duration,
    now: Instant,
) -> PerTryTimeout {
    match overall_deadline {
        Some(deadline) if now >= deadline => PerTryTimeout::OverallDeadlineElapsed,
        Some(deadline) => {
            let remaining = deadline - now;
            PerTryTimeout::Bounded(if per_try_bound.is_zero() {
                remaining
            } else {
                per_try_bound.min(remaining)
            })
        }
        None => PerTryTimeout::Bounded(per_try_bound),
    }
}

/// One attempt's outcome, as far as the retry DECISION cares (not the full response/error — the
/// imperative shell keeps that separately for whichever branch it takes).
#[derive(Debug, Clone, Copy)]
pub(crate) enum AttemptOutcome {
    /// A response was received; is its status the retriable gateway-class (502/503/504, ADR
    /// 000030)?
    Response { retriable_5xx: bool },
    /// The per-try or overall deadline elapsed before a response (ADR 000019).
    TimedOut,
    /// The send itself failed. `connect`: the connection attempt failed (upstream never received
    /// the request, safe to retry for any method) vs. some other transport fault after connecting.
    Failed { connect: bool },
}

/// Pure: given this attempt's outcome and the retry policy inputs, should the shell even ATTEMPT a
/// retry? This deliberately does NOT decide "is an alternate instance available" — resolving that
/// means calling `plecto_control::UpstreamGroup::pick_excluding`, which has a side effect (it
/// advances the load-balancer's round-robin/least-request state), so the shell must call it lazily,
/// only after this policy check already says yes — never speculatively "just to check". Mirrors the
/// original code's short-circuit `&&` chain (retryable && may_retry && pick_excluding().is_some()):
/// this function is exactly the first two conjuncts.
pub(crate) fn should_attempt_retry(
    outcome: AttemptOutcome,
    method: &str,
    replayable: bool,
    tries_left: u64,
    overall_elapsed: bool,
) -> bool {
    if overall_elapsed {
        return false;
    }
    let (retryable, failure) = match outcome {
        AttemptOutcome::Response { retriable_5xx } => (retriable_5xx, Failure::Timeout),
        AttemptOutcome::TimedOut => (true, Failure::Timeout),
        AttemptOutcome::Failed { connect } => (connect, Failure::Connect),
    };
    retryable && may_retry(failure, method, replayable, tries_left)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retriable_5xx_is_the_gateway_class_only() {
        // ADR 000030: only 502/503/504 (gateway-error class) are retriable; a 500/501 (the origin's
        // own bug) and non-5xx are taken as-is, so a retry never replays a 500-producing request.
        for s in [502u16, 503, 504] {
            assert!(
                is_retriable_5xx(StatusCode::from_u16(s).unwrap()),
                "{s} is retriable"
            );
        }
        for s in [500u16, 501, 505, 200, 404, 429] {
            assert!(
                !is_retriable_5xx(StatusCode::from_u16(s).unwrap()),
                "{s} is not retriable"
            );
        }
    }

    #[test]
    fn jitter_stays_within_the_ceiling() {
        // Full-jitter (ADR 000030) must never exceed its ceiling; a zero ceiling never waits.
        assert_eq!(jitter(0), 0, "a zero ceiling yields no wait");
        for ceiling in [1u64, 25, 250, 1000] {
            for _ in 0..1000 {
                assert!(
                    jitter(ceiling) <= ceiling,
                    "jitter must stay within ceiling {ceiling}"
                );
            }
        }
    }

    #[test]
    fn idempotent_methods_per_rfc_9110() {
        for m in ["GET", "HEAD", "PUT", "DELETE", "OPTIONS", "TRACE"] {
            assert!(is_idempotent(m), "{m} is idempotent (RFC 9110 §9.2.2)");
        }
        for m in ["POST", "PATCH", "CONNECT", "get", ""] {
            assert!(!is_idempotent(m), "{m} is not idempotent");
        }
    }

    #[test]
    fn may_retry_gates_on_failure_method_replayability_and_budget() {
        // A timeout retries only for an idempotent method (the upstream may have acted).
        assert!(may_retry(Failure::Timeout, "GET", true, 1));
        assert!(!may_retry(Failure::Timeout, "POST", true, 1));
        // A connect failure never reached the upstream → safe for ANY method.
        assert!(may_retry(Failure::Connect, "POST", true, 1));
        assert!(may_retry(Failure::Connect, "GET", true, 1));
        // A one-shot streamed body can't be replayed → never retried, either failure (ADR 000058:
        // replayable covers bodyless AND buffered bodies; only the streamed one-shot is excluded).
        assert!(!may_retry(Failure::Timeout, "GET", false, 1));
        assert!(!may_retry(Failure::Connect, "POST", false, 1));
        // Exhausted budget → no retry.
        assert!(!may_retry(Failure::Timeout, "GET", true, 0));
        assert!(!may_retry(Failure::Connect, "GET", true, 0));
    }

    #[test]
    fn per_try_timeout_reports_overall_deadline_elapsed() {
        let now = Instant::now();
        let deadline = now - Duration::from_millis(1);
        assert_eq!(
            per_try_timeout(Some(deadline), Duration::from_secs(1), now),
            PerTryTimeout::OverallDeadlineElapsed
        );
    }

    #[test]
    fn per_try_timeout_bounds_by_the_tighter_of_per_try_and_remaining() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(50);
        // per-try bound (1s) is looser than the remaining budget (50ms) — remaining wins.
        assert_eq!(
            per_try_timeout(Some(deadline), Duration::from_secs(1), now),
            PerTryTimeout::Bounded(Duration::from_millis(50))
        );
        // per-try bound (10ms) is tighter than the remaining budget (50ms) — per-try wins.
        assert_eq!(
            per_try_timeout(Some(deadline), Duration::from_millis(10), now),
            PerTryTimeout::Bounded(Duration::from_millis(10))
        );
        // no overall deadline: the per-try bound applies unchanged, even if zero (opt-out).
        assert_eq!(
            per_try_timeout(None, Duration::from_millis(10), now),
            PerTryTimeout::Bounded(Duration::from_millis(10))
        );
    }

    #[test]
    fn should_attempt_retry_gates_on_outcome_method_replayability_budget_and_overall_deadline() {
        struct Case {
            name: &'static str,
            outcome: AttemptOutcome,
            method: &'static str,
            replayable: bool,
            tries_left: u64,
            overall_elapsed: bool,
            want: bool,
        }
        let cases = vec![
            Case {
                name: "retriable 5xx, idempotent -> attempt retry",
                outcome: AttemptOutcome::Response {
                    retriable_5xx: true,
                },
                method: "GET",
                replayable: true,
                tries_left: 1,
                overall_elapsed: false,
                want: true,
            },
            Case {
                name: "non-retriable status -> stop even with budget",
                outcome: AttemptOutcome::Response {
                    retriable_5xx: false,
                },
                method: "GET",
                replayable: true,
                tries_left: 1,
                overall_elapsed: false,
                want: false,
            },
            Case {
                name: "timeout, idempotent -> attempt retry",
                outcome: AttemptOutcome::TimedOut,
                method: "HEAD",
                replayable: true,
                tries_left: 1,
                overall_elapsed: false,
                want: true,
            },
            Case {
                name: "timeout, non-idempotent -> stop",
                outcome: AttemptOutcome::TimedOut,
                method: "POST",
                replayable: true,
                tries_left: 1,
                overall_elapsed: false,
                want: false,
            },
            Case {
                name: "overall deadline elapsed -> stop unconditionally",
                outcome: AttemptOutcome::TimedOut,
                method: "GET",
                replayable: true,
                tries_left: 5,
                overall_elapsed: true,
                want: false,
            },
            Case {
                name: "connect failure, non-idempotent method -> still attempts (any method safe)",
                outcome: AttemptOutcome::Failed { connect: true },
                method: "POST",
                replayable: true,
                tries_left: 1,
                overall_elapsed: false,
                want: true,
            },
            Case {
                name: "non-connect transport fault -> never retried",
                outcome: AttemptOutcome::Failed { connect: false },
                method: "GET",
                replayable: true,
                tries_left: 1,
                overall_elapsed: false,
                want: false,
            },
            Case {
                name: "one-shot streamed body -> never retried regardless of everything else",
                outcome: AttemptOutcome::Response {
                    retriable_5xx: true,
                },
                method: "GET",
                replayable: false,
                tries_left: 1,
                overall_elapsed: false,
                want: false,
            },
            Case {
                name: "exhausted retry budget -> stop",
                outcome: AttemptOutcome::TimedOut,
                method: "GET",
                replayable: true,
                tries_left: 0,
                overall_elapsed: false,
                want: false,
            },
        ];
        for case in cases {
            let got = should_attempt_retry(
                case.outcome,
                case.method,
                case.replayable,
                case.tries_left,
                case.overall_elapsed,
            );
            assert_eq!(got, case.want, "case: {}", case.name);
        }
    }
}
