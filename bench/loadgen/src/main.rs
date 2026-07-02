//! plecto-loadgen — load generators for run-perf.sh's rr / ejection phases.
//!
//! Replaces the Python drivers: their GIL-bound worker threads saturated below the proxy's
//! ceiling, so the generator melted first and its own queueing bled into the measured timeline.
//!
//!   plecto-loadgen rr --target http://127.0.0.1:28080/ --total 120000 --workers 48 --out rr.csv
//!   plecto-loadgen ejection --target http://127.0.0.1:28080/ --rate 4000 --duration 75 \
//!       --workers 64 --toggle a=URL b=URL c=URL --out timeline.csv --events-out events.csv
//!
//! `rr` fires N keep-alive GETs and tallies the `X-Instance` header (round-robin split to
//! single-request precision). `ejection` holds a fixed open-loop arrival rate while a controller
//! drives the fault timeline (15 s eject b / 30 rejoin b / 45 eject all / 60 restore all / 75 end)
//! and buckets per-second per-instance served counts plus the 503/error rate; `--warmup` seconds
//! of unrecorded load precede t=0 so the timeline starts at steady state. `hold` opens N idle
//! keep-alive connections for the footprint phase's RSS read.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::time::Instant;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
struct Target {
    addr: String,
    authority: String,
    path: String,
}

fn parse_target(url: &str) -> Result<Target, BoxError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("target must be an http:// URL: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    Ok(Target {
        addr,
        authority: authority.to_string(),
        path: path.to_string(),
    })
}

async fn connect(t: &Target) -> Result<SendRequest<Empty<Bytes>>, BoxError> {
    let stream = TcpStream::connect(&t.addr).await?;
    stream.set_nodelay(true)?;
    let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

/// One GET on the kept-alive connection -> (status, `X-Instance` header if non-empty).
async fn get_once(
    sender: &mut SendRequest<Empty<Bytes>>,
    t: &Target,
) -> Result<(u16, Option<String>), BoxError> {
    sender.ready().await?;
    let req = Request::get(t.path.as_str())
        .header(hyper::header::HOST, t.authority.as_str())
        .body(Empty::<Bytes>::new())?;
    let mut resp = sender.send_request(req).await?;
    let status = resp.status().as_u16();
    let inst = resp
        .headers()
        .get("x-instance")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    while let Some(frame) = resp.frame().await {
        frame?;
    }
    Ok((status, inst))
}

// ---------------------------------------------------------------------------- rr

struct RrArgs {
    target: String,
    total: u64,
    workers: u64,
    out: String,
}

async fn rr_worker(t: Target, n: u64) -> HashMap<String, u64> {
    let mut local: HashMap<String, u64> = HashMap::new();
    let mut conn = connect(&t).await.ok();
    for _ in 0..n {
        let res = match conn.as_mut() {
            Some(sender) => get_once(sender, &t).await,
            None => Err("not connected".into()),
        };
        match res {
            Ok((status, inst)) => {
                let key = inst
                    .unwrap_or_else(|| if status >= 500 { "FAIL" } else { "other" }.to_string());
                *local.entry(key).or_insert(0) += 1;
            }
            Err(_) => {
                *local.entry("error".to_string()).or_insert(0) += 1;
                conn = connect(&t).await.ok();
            }
        }
    }
    local
}

async fn run_rr(a: RrArgs) -> Result<(), BoxError> {
    let t = parse_target(&a.target)?;
    let per = a.total / a.workers;
    let handles: Vec<_> = (0..a.workers)
        .map(|_| tokio::spawn(rr_worker(t.clone(), per)))
        .collect();
    let mut tally: HashMap<String, u64> = HashMap::new();
    for h in handles {
        for (k, v) in h.await? {
            *tally.entry(k).or_insert(0) += v;
        }
    }

    // rr.csv expects instances a/b/c; emit them in order, then any extras (FAIL/error) for honesty.
    let mut ordered: Vec<String> = ["a", "b", "c"]
        .into_iter()
        .filter(|k| tally.contains_key(*k))
        .map(str::to_owned)
        .collect();
    let mut extras: Vec<String> = tally
        .keys()
        .filter(|k| !matches!(k.as_str(), "a" | "b" | "c"))
        .cloned()
        .collect();
    extras.sort_unstable();
    ordered.extend(extras);

    let mut csv = String::from("instance,count\n");
    for k in &ordered {
        let _ = writeln!(csv, "{k},{}", tally[k]);
    }
    std::fs::write(&a.out, csv)?;

    let total: u64 = tally.values().sum();
    let parts: Vec<String> = ordered
        .iter()
        .map(|k| {
            format!(
                "{k}={} ({:.2}%)",
                tally[k],
                100.0 * tally[k] as f64 / total.max(1) as f64
            )
        })
        .collect();
    println!("rr: {total} responses -> {}", parts.join(", "));
    Ok(())
}

// ---------------------------------------------------------------------------- ejection

struct EjectionArgs {
    target: String,
    rate: u64,
    duration: u64,
    warmup: u64,
    workers: u64,
    toggles: HashMap<String, String>,
    out: String,
    events_out: String,
}

async fn ejection_worker(
    t: Target,
    tokens: Arc<AtomicI64>,
    done: Arc<AtomicBool>,
    start: Instant,
    warmup: u64,
) -> HashMap<u64, HashMap<String, u64>> {
    let mut local: HashMap<u64, HashMap<String, u64>> = HashMap::new();
    let mut conn = connect(&t).await.ok();
    while !done.load(Ordering::Relaxed) {
        if tokens.fetch_sub(1, Ordering::Relaxed) <= 0 {
            tokens.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_micros(500)).await;
            continue;
        }
        let res = match conn.as_mut() {
            Some(sender) => {
                tokio::time::timeout(Duration::from_secs(5), get_once(sender, &t)).await
            }
            None => Ok(Err("not connected".into())),
        };
        // t=0 is the end of the warmup window: warmup requests fire but are not recorded,
        // so the timeline starts at steady state.
        let Some(sec) = start.elapsed().as_secs().checked_sub(warmup) else {
            if res.is_err() || matches!(&res, Ok(Err(_))) {
                conn = connect(&t).await.ok();
            }
            continue;
        };
        let bucket = local.entry(sec).or_default();
        match res {
            Ok(Ok((status, Some(inst)))) if status < 500 => {
                *bucket.entry(inst).or_insert(0) += 1;
            }
            Ok(Ok(_)) => {
                *bucket.entry("failed".to_string()).or_insert(0) += 1;
            }
            Ok(Err(_)) | Err(_) => {
                *bucket.entry("failed".to_string()).or_insert(0) += 1;
                conn = connect(&t).await.ok();
            }
        }
    }
    local
}

async fn toggle(url: &str) {
    let hit = async {
        let t = parse_target(url)?;
        let mut sender = connect(&t).await?;
        get_once(&mut sender, &t).await
    };
    let _ = tokio::time::timeout(Duration::from_secs(2), hit).await;
}

async fn controller(
    toggles: HashMap<String, String>,
    start: Instant,
    warmup: u64,
) -> Vec<(u64, String)> {
    let plan: [(u64, &[&str], &str); 4] = [
        (15, &["b"], "eject b"),
        (30, &["b"], "rejoin b"),
        (45, &["a", "b", "c"], "eject all"),
        (60, &["a", "b", "c"], "restore all"),
    ];
    let mut events = Vec::new();
    for (delay, keys, label) in plan {
        tokio::time::sleep_until(start + Duration::from_secs(warmup + delay)).await;
        for k in keys {
            if let Some(url) = toggles.get(*k) {
                toggle(url).await;
            }
        }
        events.push((delay, label.to_string()));
    }
    events
}

async fn run_ejection(a: EjectionArgs) -> Result<(), BoxError> {
    let t = parse_target(&a.target)?;
    let tokens = Arc::new(AtomicI64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let start = Instant::now();

    let handles: Vec<_> = (0..a.workers)
        .map(|_| {
            tokio::spawn(ejection_worker(
                t.clone(),
                tokens.clone(),
                done.clone(),
                start,
                a.warmup,
            ))
        })
        .collect();
    let ctl = tokio::spawn(controller(a.toggles.clone(), start, a.warmup));

    // Pace arrivals open-loop: every 10 ms credit rate/100 tokens on a monotonic schedule.
    // Backlog is capped at 3 s of tokens; credits beyond the cap are dropped (open-loop: a slow
    // proxy sheds offered load rather than stretching the schedule).
    let slot = Duration::from_millis(10);
    let per_slot = ((a.rate as f64 / 100.0).round() as i64).max(1);
    let cap = (a.rate as i64) * 3;
    for i in 0..(a.warmup + a.duration) * 100 {
        let cur = tokens.load(Ordering::Relaxed);
        if cur < cap {
            tokens.fetch_add(per_slot.min(cap - cur), Ordering::Relaxed);
        }
        tokio::time::sleep_until(start + slot * (i as u32 + 1)).await;
    }
    let grace = Instant::now() + Duration::from_secs(5);
    while tokens.load(Ordering::Relaxed) > 0 && Instant::now() < grace {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    done.store(true, Ordering::Relaxed);

    let mut timeline: HashMap<u64, HashMap<String, u64>> = HashMap::new();
    for h in handles {
        for (sec, insts) in h.await? {
            let bucket = timeline.entry(sec).or_default();
            for (k, v) in insts {
                *bucket.entry(k).or_insert(0) += v;
            }
        }
    }
    let events = ctl.await?;

    let mut secs: Vec<u64> = timeline
        .keys()
        .copied()
        .filter(|s| *s < a.duration)
        .collect();
    secs.sort_unstable();
    let mut csv = String::from("t,a,b,c,failed\n");
    for s in &secs {
        let row = &timeline[s];
        let g = |k: &str| row.get(k).copied().unwrap_or(0);
        let _ = writeln!(csv, "{s},{},{},{},{}", g("a"), g("b"), g("c"), g("failed"));
    }
    std::fs::write(&a.out, csv)?;

    let mut ev = String::from("t,label\n");
    for (tm, label) in &events {
        let _ = writeln!(ev, "{tm},{label}");
    }
    std::fs::write(&a.events_out, ev)?;

    let total: u64 = timeline.values().flat_map(HashMap::values).sum();
    let failed: u64 = timeline.values().filter_map(|m| m.get("failed")).sum();
    println!(
        "ejection: {total} responses over {} s, {failed} failed ({:.2}%); events={:?}",
        secs.len(),
        100.0 * failed as f64 / total.max(1) as f64,
        events.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>()
    );
    Ok(())
}

// ---------------------------------------------------------------------------- hold

struct HoldArgs {
    target: String,
    conns: u64,
    seconds: u64,
}

/// Open N keep-alive connections (one GET each to establish), hold them idle for S seconds, exit.
/// Used by the footprint phase to read the proxy's RSS under standing connections.
async fn run_hold(a: HoldArgs) -> Result<(), BoxError> {
    let t = parse_target(&a.target)?;
    let mut senders = Vec::with_capacity(a.conns as usize);
    for _ in 0..a.conns {
        if let Ok(mut sender) = connect(&t).await
            && get_once(&mut sender, &t).await.is_ok()
        {
            senders.push(sender);
        }
    }
    println!(
        "hold: {} connections open for {} s",
        senders.len(),
        a.seconds
    );
    tokio::time::sleep(Duration::from_secs(a.seconds)).await;
    drop(senders);
    Ok(())
}

// ---------------------------------------------------------------------------- CLI

fn usage() -> ! {
    eprintln!(
        "usage:\n  plecto-loadgen rr --target URL [--total N] [--workers W] [--out FILE]\n  \
         plecto-loadgen ejection --target URL --toggle a=URL b=URL c=URL \
         [--rate R] [--duration S] [--warmup S] [--workers W] [--out FILE] [--events-out FILE]\n  \
         plecto-loadgen hold --target URL [--conns N] [--seconds S]"
    );
    std::process::exit(2)
}

fn take(it: &mut std::slice::Iter<'_, String>, flag: &str) -> String {
    let Some(v) = it.next() else {
        eprintln!("missing value for {flag}");
        std::process::exit(2)
    };
    v.clone()
}

fn take_num(it: &mut std::slice::Iter<'_, String>, flag: &str) -> u64 {
    let v = take(it, flag);
    let Ok(n) = v.parse() else {
        eprintln!("bad number for {flag}: {v}");
        std::process::exit(2)
    };
    n
}

fn parse_rr(rest: &[String]) -> RrArgs {
    let mut a = RrArgs {
        target: "http://127.0.0.1:8080/".to_string(),
        total: 60_000,
        workers: 48,
        out: "rr.csv".to_string(),
    };
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--target" => a.target = take(&mut it, flag),
            "--total" => a.total = take_num(&mut it, flag),
            "--workers" => a.workers = take_num(&mut it, flag).max(1),
            "--out" => a.out = take(&mut it, flag),
            _ => usage(),
        }
    }
    a
}

fn parse_ejection(rest: &[String]) -> EjectionArgs {
    let mut a = EjectionArgs {
        target: "http://127.0.0.1:8080/".to_string(),
        rate: 4000,
        duration: 75,
        warmup: 5,
        workers: 64,
        toggles: HashMap::new(),
        out: "ejection_timeline.csv".to_string(),
        events_out: "ejection_events.csv".to_string(),
    };
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--target" => a.target = take(&mut it, flag),
            "--rate" => a.rate = take_num(&mut it, flag).max(1),
            "--duration" => a.duration = take_num(&mut it, flag).max(1),
            "--warmup" => a.warmup = take_num(&mut it, flag),
            "--workers" => a.workers = take_num(&mut it, flag).max(1),
            "--out" => a.out = take(&mut it, flag),
            "--events-out" => a.events_out = take(&mut it, flag),
            "--toggle" => {
                for _ in 0..3 {
                    let kv = take(&mut it, flag);
                    let Some((k, v)) = kv.split_once('=') else {
                        usage()
                    };
                    a.toggles.insert(k.to_string(), v.to_string());
                }
            }
            _ => usage(),
        }
    }
    if !["a", "b", "c"].iter().all(|k| a.toggles.contains_key(*k)) {
        eprintln!("--toggle must provide a=URL b=URL c=URL");
        std::process::exit(2)
    }
    a
}

fn parse_hold(rest: &[String]) -> HoldArgs {
    let mut a = HoldArgs {
        target: "http://127.0.0.1:8080/".to_string(),
        conns: 1000,
        seconds: 6,
    };
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--target" => a.target = take(&mut it, flag),
            "--conns" => a.conns = take_num(&mut it, flag).max(1),
            "--seconds" => a.seconds = take_num(&mut it, flag),
            _ => usage(),
        }
    }
    a
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = argv.split_first() else {
        usage()
    };
    let result = match cmd.as_str() {
        "rr" => run_rr(parse_rr(rest)).await,
        "ejection" => run_ejection(parse_ejection(rest)).await,
        "hold" => run_hold(parse_hold(rest)).await,
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("plecto-loadgen: {e}");
        std::process::exit(1);
    }
}
