//! The OTLP export pump (ADR 000040): the async half behind the host's [`OtlpBuffer`]. A single
//! background task drains the buffer on a short tick, hand-encodes each batch (the host's
//! dependency-less encoder), and POSTs it to the collector as OTLP/HTTP binary protobuf. The
//! pump is pull-based — the data plane never waits on it; a slow or dead collector costs
//! dropped spans (counted), never latency.
//!
//! Spec-derived behaviour (OTLP 1.x + the OTel exporter/BSP specs):
//! - batch ≤ 512 spans, per-request timeout 10 s (the spec defaults);
//! - a 250 ms tick draining until empty stands in for the BSP's size-trigger — at queue
//!   capacity 2048 it sustains bursts a plain 5 s tick would drop;
//! - retry ONLY on 429/502/503/504, honouring `Retry-After` (delta-seconds), with jittered
//!   exponential backoff, at most 3 attempts; every other failure drops the batch (the spec
//!   forbids retrying, e.g. a 400) and tallies the loss;
//! - a populated `partial_success` is logged and never retried;
//! - shutdown flushes what is queued, single-attempt, under a bounded deadline (the spec's
//!   "Shutdown MUST include the effects of ForceFlush, within a timeout").

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use plecto_control::otlp::{OtlpBuffer, decode_export_partial_success, encode_traces_request};
use tokio::sync::watch;

use crate::listener::drained;
use crate::upstream_connector;

/// The Resource `service.name` this process exports under.
const SERVICE_NAME: &str = "plecto";
/// OTel BSP spec default (`OTEL_BSP_MAX_EXPORT_BATCH_SIZE`).
const MAX_EXPORT_BATCH: usize = 512;
/// OTel exporter spec default (`OTEL_EXPORTER_OTLP_TIMEOUT`).
const EXPORT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Drain cadence. Deliberately far below the BSP's 5 s schedule: without a size-trigger wired
/// through the runtime-free buffer, the short tick is what keeps a 2048-cap queue ahead of
/// realistic span rates (2048 spans / 250 ms ≈ 8k spans/s sustained before drops).
const TICK: Duration = Duration::from_millis(250);
/// Total attempts per batch (1 initial + 2 retries) on retryable failures.
const MAX_ATTEMPTS: u32 = 3;
const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_CAP: Duration = Duration::from_secs(5);
/// How long the shutdown flush may run before the remainder is dropped (counted). Kept well
/// under the connection drain deadline so telemetry never delays process exit meaningfully.
pub(crate) const SHUTDOWN_FLUSH_DEADLINE: Duration = Duration::from_secs(3);

type OtlpClient = Client<HttpConnector, Full<Bytes>>;

/// Resolve the operator's base endpoint into the traces URI, fail-soft: a bad or unsupported
/// endpoint disables export (logged) without touching the data plane.
fn traces_uri(endpoint: &str) -> Option<Uri> {
    if !endpoint.starts_with("http://") {
        tracing::error!(
            endpoint,
            "otlp_endpoint must be an http:// base URL (TLS export is not yet supported); \
             trace export disabled"
        );
        return None;
    }
    let base = endpoint.trim_end_matches('/');
    match format!("{base}/v1/traces").parse::<Uri>() {
        Ok(uri) => Some(uri),
        Err(e) => {
            tracing::error!(endpoint, error = %e, "invalid otlp_endpoint; trace export disabled");
            None
        }
    }
}

/// Run the export pump until the drain flag flips (graceful shutdown), then flush what is left
/// under [`SHUTDOWN_FLUSH_DEADLINE`]. Spawned by the listener iff `otlp_endpoint` is set.
pub(crate) async fn serve_otlp_export(
    buffer: Arc<OtlpBuffer>,
    endpoint: String,
    mut drain: watch::Receiver<bool>,
) {
    let Some(uri) = traces_uri(&endpoint) else {
        return;
    };
    // A dedicated small client: the upstream pool's tuning (idle caps per proxied host) has
    // nothing to do with the single collector connection this task keeps alive.
    let client: OtlpClient = Client::builder(TokioExecutor::new()).build(upstream_connector());
    tracing::info!(endpoint, "OTLP trace export running");

    loop {
        tokio::select! {
            _ = tokio::time::sleep(TICK) => {
                export_until_empty(&client, &uri, &buffer, MAX_ATTEMPTS).await;
            }
            _ = drained(&mut drain) => {
                let flush = export_until_empty(&client, &uri, &buffer, 1);
                if tokio::time::timeout(SHUTDOWN_FLUSH_DEADLINE, flush).await.is_err() {
                    let left = buffer.len() as u64;
                    buffer.record_dropped(left);
                    tracing::warn!(
                        dropped = left,
                        "OTLP shutdown flush deadline expired; dropping remaining spans"
                    );
                }
                return;
            }
        }
    }
}

/// Drain and export batch-by-batch until the buffer is empty (one tick may move several
/// batches — the catch-up that stands in for a size-trigger).
async fn export_until_empty(
    client: &OtlpClient,
    uri: &Uri,
    buffer: &OtlpBuffer,
    max_attempts: u32,
) {
    loop {
        let batch = buffer.drain(MAX_EXPORT_BATCH);
        if batch.is_empty() {
            return;
        }
        let len = batch.len() as u64;
        let bytes = Bytes::from(encode_traces_request(SERVICE_NAME, &batch));
        if !export_batch(client, uri, bytes, max_attempts).await {
            buffer.record_dropped(len);
        }
    }
}

/// POST one encoded batch, with the spec's retry discipline. Returns whether the collector
/// accepted it (a partial rejection still counts as delivered — the spec forbids retrying it).
async fn export_batch(client: &OtlpClient, uri: &Uri, bytes: Bytes, max_attempts: u32) -> bool {
    for attempt in 0..max_attempts {
        let request = Request::post(uri.clone())
            .header(hyper::header::CONTENT_TYPE, "application/x-protobuf")
            .header(
                hyper::header::USER_AGENT,
                concat!("plecto-otlp-exporter/", env!("CARGO_PKG_VERSION")),
            )
            .body(Full::new(bytes.clone()));
        let Ok(request) = request else {
            // The URI was validated at startup; an unbuildable request cannot heal by retrying.
            tracing::error!("failed to build OTLP export request; dropping batch");
            return false;
        };

        let response = tokio::time::timeout(EXPORT_REQUEST_TIMEOUT, client.request(request)).await;
        let retry_after = match response {
            Ok(Ok(resp)) => {
                let status = resp.status();
                if status == StatusCode::OK {
                    let body = resp.into_body().collect().await.map(|b| b.to_bytes());
                    if let Ok(body) = body
                        && let Some(partial) = decode_export_partial_success(&body)
                    {
                        tracing::warn!(
                            rejected = partial.rejected_spans,
                            message = %partial.error_message,
                            "OTLP collector partially rejected a batch (not retried, per spec)"
                        );
                    }
                    return true;
                }
                if !is_retryable(status) {
                    tracing::warn!(%status, "OTLP export rejected; dropping batch (not retryable)");
                    return false;
                }
                let after = retry_after_seconds(&resp);
                tracing::debug!(%status, attempt, "OTLP export throttled/unavailable");
                after
            }
            Ok(Err(e)) => {
                // Transport failure (refused / reset / DNS): the spec says retry with backoff.
                tracing::debug!(error = %e, attempt, "OTLP export transport failure");
                None
            }
            Err(_) => {
                tracing::debug!(attempt, "OTLP export request timed out");
                None
            }
        };

        if attempt + 1 < max_attempts {
            tokio::time::sleep(retry_after.unwrap_or_else(|| backoff(attempt))).await;
        }
    }
    tracing::warn!(
        attempts = max_attempts,
        "OTLP export gave up; dropping batch"
    );
    false
}

/// The OTLP/HTTP spec's exhaustive retryable set; every other status MUST NOT be retried.
fn is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 502 | 503 | 504)
}

/// `Retry-After` in its delta-seconds form (the HTTP-date form falls back to backoff), capped so
/// a hostile/buggy collector cannot park the pump.
fn retry_after_seconds<B>(resp: &hyper::Response<B>) -> Option<Duration> {
    let secs: u64 = resp
        .headers()
        .get(hyper::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs).min(BACKOFF_CAP))
}

/// Exponential backoff with cheap jitter (sub-second clock noise — telemetry retry spacing
/// needs decorrelation, not cryptographic randomness).
fn backoff(attempt: u32) -> Duration {
    let base = BACKOFF_BASE.saturating_mul(1 << attempt.min(4));
    let jitter_ms = u64::from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
            % 250,
    );
    (base + Duration::from_millis(jitter_ms)).min(BACKOFF_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traces_uri_appends_the_signal_path_to_the_base() {
        let uri = traces_uri("http://127.0.0.1:4318").expect("plain base parses");
        assert_eq!(uri.to_string(), "http://127.0.0.1:4318/v1/traces");
        let trailing = traces_uri("http://collector:4318/").expect("trailing slash is normalised");
        assert_eq!(trailing.to_string(), "http://collector:4318/v1/traces");
    }

    #[test]
    fn traces_uri_rejects_non_http_endpoints() {
        assert!(
            traces_uri("https://collector:4318").is_none(),
            "TLS export unsupported"
        );
        assert!(traces_uri("collector:4318").is_none(), "scheme is required");
        assert!(traces_uri("http://bad url").is_none());
    }

    #[test]
    fn retryable_statuses_match_the_spec_exactly() {
        for code in [429u16, 502, 503, 504] {
            assert!(is_retryable(StatusCode::from_u16(code).unwrap()), "{code}");
        }
        for code in [400u16, 401, 403, 404, 500, 501] {
            assert!(!is_retryable(StatusCode::from_u16(code).unwrap()), "{code}");
        }
    }

    #[test]
    fn backoff_grows_and_stays_capped() {
        assert!(backoff(0) >= BACKOFF_BASE);
        assert!(backoff(10) <= BACKOFF_CAP);
    }
}
