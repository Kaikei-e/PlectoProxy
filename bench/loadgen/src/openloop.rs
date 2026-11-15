//! Open-loop HTTP/1.1 load with schedule-based latency (wrk2 / Gil Tene model).
//!
//! Arrivals are paced on a monotonic schedule at `--rate` req/s. Latency for each request is
//! measured from the **intended** send time, not from when the socket write actually happened —
//! so queueing under overload appears in the tail instead of being omitted (coordinated omission).
//! When the generator falls more than `--backlog-secs` behind the schedule, the overdue slot is
//! counted as `dropped` and not sent (open-loop shed), matching the honesty of k6's
//! `dropped_iterations` without depending on k6's VU ceiling.
//!
//! Schedule latency is decomposed into `send_lag` (actual send start minus scheduled slot) and
//! `wire` (actual send start to response complete). When `send_lag` dominates the tail, the tail
//! reflects generator backlog rather than server response time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use hdrhistogram::Histogram;
use tokio::time::Instant;

use crate::{BoxError, Target, connect, get_once, parse_target};

/// HDR bounds for schedule latency in µs: 1 µs .. 1 hour, 3 significant figures. Recording is a
/// few ns and the footprint is fixed, so window length / rate no longer scale generator memory.
const HIST_LOW_US: u64 = 1;
const HIST_HIGH_US: u64 = 3_600_000_000;
const HIST_SIGFIG: u8 = 3;

fn new_hist() -> Histogram<u64> {
    Histogram::new_with_bounds(HIST_LOW_US, HIST_HIGH_US, HIST_SIGFIG)
        .expect("static histogram bounds are valid")
}

fn dur_to_hist_us(d: Duration) -> u64 {
    let us = d.as_micros().min(u128::from(u32::MAX)) as u64;
    us.clamp(HIST_LOW_US, HIST_HIGH_US)
}

pub(crate) struct OpenloopArgs {
    pub(crate) target: String,
    pub(crate) rate: u64,
    pub(crate) duration: u64,
    pub(crate) warmup: u64,
    pub(crate) workers: u64,
    pub(crate) backlog_secs: u64,
    pub(crate) out: String,
    pub(crate) hist_out: Option<String>,
}

struct WorkerOut {
    hist: Histogram<u64>,
    lag_hist: Histogram<u64>,
    wire_hist: Histogram<u64>,
    ok: u64,
    fail: u64,
    dropped: u64,
}

struct Schedule {
    start: Instant,
    interval_ns: u64,
    total_slots: u64,
    warmup_slots: u64,
    backlog: Duration,
}

async fn openloop_worker(
    t: Target,
    next_seq: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
    sched: Schedule,
) -> WorkerOut {
    let Schedule {
        start,
        interval_ns,
        total_slots,
        warmup_slots,
        backlog,
    } = sched;
    let mut out = WorkerOut {
        hist: new_hist(),
        lag_hist: new_hist(),
        wire_hist: new_hist(),
        ok: 0,
        fail: 0,
        dropped: 0,
    };
    let mut conn = connect(&t).await.ok();

    loop {
        if done.load(Ordering::Relaxed) {
            break;
        }
        let seq = next_seq.fetch_add(1, Ordering::Relaxed);
        if seq >= total_slots {
            break;
        }

        let scheduled = start + Duration::from_nanos(interval_ns.saturating_mul(seq));
        let now = Instant::now();
        if now > scheduled + backlog {
            if seq >= warmup_slots {
                out.dropped += 1;
            }
            continue;
        }
        if now < scheduled {
            if scheduled.duration_since(now) > Duration::from_millis(2) {
                tokio::time::sleep_until(scheduled - Duration::from_millis(1)).await;
            }
            while Instant::now() < scheduled {
                tokio::task::yield_now().await;
            }
        }

        let sent = Instant::now();
        let res = match conn.as_mut() {
            Some(sender) => {
                tokio::time::timeout(Duration::from_secs(5), get_once(sender, &t)).await
            }
            None => Ok(Err("not connected".into())),
        };

        if seq < warmup_slots {
            if res.is_err() || matches!(&res, Ok(Err(_))) {
                conn = connect(&t).await.ok();
            }
            continue;
        }

        let end = Instant::now();
        let lat_us = dur_to_hist_us(end.duration_since(scheduled));
        let lag_us = dur_to_hist_us(sent.saturating_duration_since(scheduled));
        let wire_us = dur_to_hist_us(end.duration_since(sent));
        let _ = out.hist.record(lat_us);
        let _ = out.lag_hist.record(lag_us);
        let _ = out.wire_hist.record(wire_us);
        match res {
            Ok(Ok((status, _))) if (200..500).contains(&status) => {
                out.ok += 1;
            }
            Ok(Ok(_)) => {
                out.fail += 1;
            }
            Ok(Err(_)) | Err(_) => {
                out.fail += 1;
                conn = connect(&t).await.ok();
            }
        }
    }
    out
}

fn quantile_ms(hist: &Histogram<u64>, q: f64) -> f64 {
    if hist.is_empty() {
        return 0.0;
    }
    hist.value_at_quantile(q) as f64 / 1000.0 // µs → ms
}

pub(crate) async fn run_openloop(a: OpenloopArgs) -> Result<(), BoxError> {
    if a.rate == 0 {
        return Err("openloop --rate must be > 0".into());
    }
    let t = parse_target(&a.target)?;
    let total_slots = a.rate.saturating_mul(a.warmup + a.duration);
    let warmup_slots = a.rate.saturating_mul(a.warmup);
    let interval_ns = 1_000_000_000u64 / a.rate;
    let backlog = Duration::from_secs(a.backlog_secs.max(1));
    let next_seq = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let start = Instant::now();

    let handles: Vec<_> = (0..a.workers)
        .map(|_| {
            tokio::spawn(openloop_worker(
                t.clone(),
                next_seq.clone(),
                done.clone(),
                Schedule {
                    start,
                    interval_ns,
                    total_slots,
                    warmup_slots,
                    backlog,
                },
            ))
        })
        .collect();

    let end = start + Duration::from_secs(a.warmup + a.duration);
    tokio::time::sleep_until(end).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    done.store(true, Ordering::Relaxed);

    let mut hist = new_hist();
    let mut lag_hist = new_hist();
    let mut wire_hist = new_hist();
    let mut ok = 0u64;
    let mut fail = 0u64;
    let mut dropped = 0u64;
    for h in handles {
        let w = h.await?;
        ok += w.ok;
        fail += w.fail;
        dropped += w.dropped;
        for (dst, src) in [
            (&mut hist, &w.hist),
            (&mut lag_hist, &w.lag_hist),
            (&mut wire_hist, &w.wire_hist),
        ] {
            dst.add(src)
                .map_err(|e| format!("merging worker histograms (identical bounds): {e}"))?;
        }
    }

    let measured = ok + fail;
    let dur = a.duration.max(1) as f64;
    let achieved = measured as f64 / dur;
    let failed_frac = if measured == 0 {
        0.0
    } else {
        fail as f64 / measured as f64
    };

    let out = json_summary(
        a.rate,
        achieved,
        measured,
        failed_frac,
        dropped,
        Percentiles {
            p50: quantile_ms(&hist, 0.50),
            p95: quantile_ms(&hist, 0.95),
            p99: quantile_ms(&hist, 0.99),
            p99_9: quantile_ms(&hist, 0.999),
            send_lag: TailPercentiles::from_hist(&lag_hist),
            wire: TailPercentiles::from_hist(&wire_hist),
        },
    );
    std::fs::write(&a.out, &out)?;
    print!("{out}");
    if let Some(path) = &a.hist_out {
        std::fs::write(path, hist_dump(&hist))?;
        println!("histogram dump ({} samples) -> {path}", hist.len());
    }
    Ok(())
}

/// Full-distribution text dump (CSV): one row per recorded HDR bucket, with the cumulative
/// quantile. This is what separates "the p99 moved because a second mode appeared" from "the one
/// mode's tail stretched" — a distinction a handful of percentile points cannot make.
fn hist_dump(hist: &Histogram<u64>) -> String {
    use std::fmt::Write as _;
    let total = hist.len().max(1);
    let mut cum = 0u64;
    let mut s = String::from("value_us,count,cum_count,quantile\n");
    for v in hist.iter_recorded() {
        cum += v.count_since_last_iteration();
        let _ = writeln!(
            s,
            "{},{},{},{:.6}",
            v.value_iterated_to(),
            v.count_at_value(),
            cum,
            cum as f64 / total as f64
        );
    }
    s
}

struct TailPercentiles {
    p50: f64,
    p99: f64,
    p99_9: f64,
}

impl TailPercentiles {
    fn from_hist(hist: &Histogram<u64>) -> Self {
        Self {
            p50: quantile_ms(hist, 0.50),
            p99: quantile_ms(hist, 0.99),
            p99_9: quantile_ms(hist, 0.999),
        }
    }
}

struct Percentiles {
    p50: f64,
    p95: f64,
    p99: f64,
    p99_9: f64,
    send_lag: TailPercentiles,
    wire: TailPercentiles,
}

/// Minimal JSON writer — avoids a serde dependency (bench tooling stays offline-buildable from
/// the already-vendored crate graph in Cargo.lock).
fn json_summary(
    target_rps: u64,
    achieved_rps: f64,
    reqs: u64,
    failed: f64,
    dropped: u64,
    p: Percentiles,
) -> String {
    format!(
        "{{\n  \"generator\": \"plecto-loadgen\",\n  \"method\": \"constant-arrival-rate+schedule-latency\",\n  \
         \"target_rps\": {target_rps},\n  \"achieved_rps\": {achieved_rps:.6},\n  \"reqs\": {reqs},\n  \
         \"failed\": {failed:.8},\n  \"dropped\": {dropped},\n  \
         \"p50\": {p50:.6},\n  \"p95\": {p95:.6},\n  \"p99\": {p99:.6},\n  \"p99_9\": {p99_9:.6},\n  \
         \"send_lag_p50\": {send_lag_p50:.6},\n  \"send_lag_p99\": {send_lag_p99:.6},\n  \
         \"send_lag_p99_9\": {send_lag_p99_9:.6},\n  \
         \"wire_p50\": {wire_p50:.6},\n  \"wire_p99\": {wire_p99:.6},\n  \"wire_p99_9\": {wire_p99_9:.6}\n}}\n",
        p50 = p.p50,
        p95 = p.p95,
        p99 = p.p99,
        p99_9 = p.p99_9,
        send_lag_p50 = p.send_lag.p50,
        send_lag_p99 = p.send_lag.p99,
        send_lag_p99_9 = p.send_lag.p99_9,
        wire_p50 = p.wire.p50,
        wire_p99 = p.wire.p99,
        wire_p99_9 = p.wire.p99_9,
    )
}

#[cfg(test)]
mod tests {
    use super::{Percentiles, TailPercentiles, json_summary};

    #[test]
    fn json_summary_appends_send_lag_and_wire_after_schedule_latency() {
        let out = json_summary(
            1000,
            998.5,
            9985,
            0.0012,
            15,
            Percentiles {
                p50: 1.25,
                p95: 3.5,
                p99: 12.0,
                p99_9: 25.5,
                send_lag: TailPercentiles {
                    p50: 0.1,
                    p99: 9.0,
                    p99_9: 21.25,
                },
                wire: TailPercentiles {
                    p50: 1.15,
                    p99: 3.0,
                    p99_9: 4.25,
                },
            },
        );

        let expected_lines = [
            "  \"generator\": \"plecto-loadgen\",",
            "  \"method\": \"constant-arrival-rate+schedule-latency\",",
            "  \"target_rps\": 1000,",
            "  \"achieved_rps\": 998.500000,",
            "  \"reqs\": 9985,",
            "  \"failed\": 0.00120000,",
            "  \"dropped\": 15,",
            "  \"p50\": 1.250000,",
            "  \"p95\": 3.500000,",
            "  \"p99\": 12.000000,",
            "  \"p99_9\": 25.500000,",
            "  \"send_lag_p50\": 0.100000,",
            "  \"send_lag_p99\": 9.000000,",
            "  \"send_lag_p99_9\": 21.250000,",
            "  \"wire_p50\": 1.150000,",
            "  \"wire_p99\": 3.000000,",
            "  \"wire_p99_9\": 4.250000",
        ];
        let mut expected = String::from("{\n");
        for line in expected_lines {
            expected.push_str(line);
            expected.push('\n');
        }
        expected.push_str("}\n");
        assert_eq!(out, expected);
    }
}
