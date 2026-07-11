//! plecto-loadgen — load generators for run-perf.sh's rr / ejection phases.
//!
//! Replaces the Python drivers: their GIL-bound worker threads saturated below the proxy's
//! ceiling, so the generator melted first and its own queueing bled into the measured timeline.
//!
//!   plecto-loadgen rr --target http://127.0.0.1:28080/ --total 120000 --workers 48 --out rr.csv
//!   plecto-loadgen ejection --target http://127.0.0.1:28080/ --rate 4000 --duration 75 \
//!       --workers 64 --toggle a=URL b=URL c=URL --out timeline.csv --events-out events.csv
//!   plecto-loadgen swap --target http://127.0.0.1:28080/ --rate 4000 --duration 60 \
//!       --exec-at 15='cp swapped.toml plecto.toml && kill -HUP 12345' --out swap.csv
//!   plecto-loadgen ws --mode echo --target ws://127.0.0.1:28085/ws --conns 50 --size 1024 \
//!       --duration 30 --out ws_echo.csv
//!   plecto-loadgen tls --mode resumed --target https://127.0.0.1:28443/ --ca cert.pem \
//!       --duration 20 --workers 48 --out tls_resumed.csv
//!   plecto-loadgen openloop --target http://127.0.0.1:28080/ --rate 60000 --duration 90 \
//!       --warmup 5 --workers 64 --out openloop.json
//!
//! `rr` fires N keep-alive GETs and tallies the `X-Instance` header (round-robin split to
//! single-request precision). `ejection` holds a fixed open-loop arrival rate while a controller
//! drives the fault timeline (default: 15 s eject b / 30 rejoin b / 45 eject all / 60 restore all
//! / 75 end; `--toggle-at SEC=keys[:label]` replaces it — the gate tier runs a compressed 40 s one)
//! and buckets per-second per-instance served counts plus the 503/error rate; `--warmup` seconds
//! of unrecorded load precede t=0 so the timeline starts at steady state. `hold` opens N idle
//! keep-alive connections for the footprint phase's RSS read. `swap` is `ejection`'s open-loop
//! timeline generalized: instead of HTTP-hitting `/toggle` endpoints, it runs arbitrary shell
//! commands at scheduled offsets (a manifest rewrite + `kill -HUP`, for ADR 000044's endpoint-swap
//! scenario) and buckets served counts by whatever `X-Instance` labels appear — the label set
//! itself is allowed to change mid-run. `ws` drives Plecto's Upgrade-tunneled `/ws` route (ADR
//! 000048): `handshake` paces open-loop connection attempts, `hold` opens N idle tunnels for an
//! RSS read, `echo` sustains a per-connection request/response loop measuring throughput + latency.
//! `tls` measures the handshake-per-request rungs of the TLS decomposition (ADR 000052) with
//! EXPLICIT resumption control — `--mode full` disables client resumption (every connection pays
//! the certificate + key-exchange handshake), `--mode resumed` shares one session cache so
//! post-warmup connections resume via the server's stateless tickets; oha can't split these two
//! (it shares one ClientConfig, so its cold connections silently resume once the server tickets).
//! `openloop` is the **authoritative** coordinated-omission-safe tail driver (wrk2 schedule
//! latency): constant arrival rate with latency measured from the intended send time. Prefer it
//! over k6 `constant-arrival-rate` when the co-resident generator must sustain tens of kRPS —
//! k6's VU model often becomes the ceiling first (see `bench/methodology.md`).

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
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::Resumption;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, HandshakeKind, RootCertStore};

mod openloop;
mod ws;

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone)]
pub(crate) struct Target {
    pub(crate) addr: String,
    pub(crate) authority: String,
    pub(crate) path: String,
}

/// Accepts both `http://` (the HTTP generators) and `ws://` (the `ws` subcommand) — the handshake
/// itself is a plain HTTP/1.1 request either way, so only the scheme label differs.
fn parse_target(url: &str) -> Result<Target, BoxError> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("ws://"))
        .ok_or_else(|| format!("target must be an http:// or ws:// URL: {url}"))?;
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
    plan: Vec<ToggleEvent>,
    out: String,
    events_out: String,
}

/// One controller action: at `at` seconds (post-warmup) hit the `/toggle` of every key in `keys`,
/// then record `label` in the events CSV.
struct ToggleEvent {
    at: u64,
    keys: Vec<String>,
    label: String,
}

/// The classic 75 s fault timeline, kept as the default so existing invocations are unchanged.
/// `--toggle-at` replaces it wholesale (the gate tier runs a compressed 40 s variant).
fn default_ejection_plan() -> Vec<ToggleEvent> {
    let ev = |at: u64, keys: &[&str], label: &str| ToggleEvent {
        at,
        keys: keys.iter().map(|k| k.to_string()).collect(),
        label: label.to_string(),
    };
    vec![
        ev(15, &["b"], "eject b"),
        ev(30, &["b"], "rejoin b"),
        ev(45, &["a", "b", "c"], "eject all"),
        ev(60, &["a", "b", "c"], "restore all"),
    ]
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
    plan: Vec<ToggleEvent>,
    start: Instant,
    warmup: u64,
) -> Vec<(u64, String)> {
    let mut events = Vec::new();
    for ev in plan {
        tokio::time::sleep_until(start + Duration::from_secs(warmup + ev.at)).await;
        for k in &ev.keys {
            if let Some(url) = toggles.get(k) {
                toggle(url).await;
            }
        }
        events.push((ev.at, ev.label));
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
    let ctl = tokio::spawn(controller(a.toggles.clone(), a.plan, start, a.warmup));

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

// ---------------------------------------------------------------------------- swap

struct SwapArgs {
    target: String,
    rate: u64,
    duration: u64,
    warmup: u64,
    workers: u64,
    exec_at: Vec<(u64, String)>,
    out: String,
    events_out: String,
}

/// Run `cmd` (via `sh -c`) at `warmup + delay` seconds after `start`, for each `(delay, cmd)` in
/// `exec_at` — the ADR 000044 analogue of `ejection`'s HTTP `/toggle` controller, except the
/// action is an operator-side one (a manifest rewrite + `kill -HUP`) rather than a request.
async fn exec_controller(
    mut exec_at: Vec<(u64, String)>,
    start: Instant,
    warmup: u64,
) -> Vec<(u64, String)> {
    exec_at.sort_by_key(|(sec, _)| *sec);
    let mut events = Vec::new();
    for (delay, cmd) in exec_at {
        tokio::time::sleep_until(start + Duration::from_secs(warmup + delay)).await;
        let label = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .status()
            .await
        {
            Ok(s) if s.success() => format!("exec: {cmd}"),
            Ok(s) => format!("exec (exit {:?}): {cmd}", s.code()),
            Err(e) => format!("exec failed ({e}): {cmd}"),
        };
        events.push((delay, label.replace(',', ";")));
    }
    events
}

/// `ejection`'s open-loop timeline generalized: the same fixed-rate arrival pacing and
/// per-second/per-`X-Instance`-label bucketing (`ejection_worker` is reused verbatim — it has no
/// ejection-specific logic, only the controller differs), but the label SET is not assumed to be
/// `{a,b,c}` — a swap can retire one label and introduce another mid-run, so the CSV's columns are
/// derived from whatever labels actually appear.
async fn run_swap(a: SwapArgs) -> Result<(), BoxError> {
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
    let ctl = tokio::spawn(exec_controller(a.exec_at.clone(), start, a.warmup));

    // Pace arrivals open-loop, identical to `ejection` (see its comment for the rationale).
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

    let mut labels: Vec<String> = timeline
        .values()
        .flat_map(|m| m.keys().cloned())
        .filter(|k| k != "failed")
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    labels.sort();
    labels.push("failed".to_string());

    let mut secs: Vec<u64> = timeline
        .keys()
        .copied()
        .filter(|s| *s < a.duration)
        .collect();
    secs.sort_unstable();
    let mut csv = format!("t,{}\n", labels.join(","));
    for s in &secs {
        let row = &timeline[s];
        let vals: Vec<String> = labels
            .iter()
            .map(|l| row.get(l).copied().unwrap_or(0).to_string())
            .collect();
        let _ = writeln!(csv, "{s},{}", vals.join(","));
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
        "swap: {total} responses over {} s, {failed} failed ({:.2}%); events={:?}",
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

// ---------------------------------------------------------------------------- tls

struct TlsArgs {
    target: String,
    mode: String,
    ca: String,
    duration: u64,
    warmup: u64,
    workers: u64,
    out: String,
}

struct TlsTarget {
    addr: String,
    authority: String,
    path: String,
    sni: ServerName<'static>,
}

/// `https://` only — the other subcommands' plain-TCP `parse_target` stays https-free on purpose.
/// (Host parsing is `rsplit_once(':')`, so bare IPv6 authorities are out of scope — bench targets
/// are loopback IPv4.)
fn parse_tls_target(url: &str) -> Result<TlsTarget, BoxError> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("tls target must be an https:// URL: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, addr) = match authority.rsplit_once(':') {
        Some((host, _port)) => (host, authority.to_string()),
        None => (authority, format!("{authority}:443")),
    };
    let sni = ServerName::try_from(host.to_string())
        .map_err(|e| format!("bad SNI host in target {url}: {e}"))?;
    Ok(TlsTarget {
        addr,
        authority: authority.to_string(),
        path: path.to_string(),
        sni,
    })
}

/// One shared ClientConfig trusting exactly `--ca` (the bench's self-signed cert). `full` disables
/// client resumption — rustls clients resume by default, and with ADR 000052's stateless tickets
/// on the server a shared default config would silently turn cold connections into resumed ones
/// (which is exactly what the `resumed` mode wants, and what oha cannot opt out of).
fn tls_client_config(ca_path: &str, mode: &str) -> Result<Arc<ClientConfig>, BoxError> {
    let certs = CertificateDer::pem_file_iter(ca_path)
        .map_err(|e| format!("read {ca_path}: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("bad CA PEM in {ca_path}: {e}"))?;
    let mut roots = RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(certs);
    if added == 0 {
        return Err(format!("no usable root certificate in {ca_path}").into());
    }
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    if mode == "full" {
        config.resumption = Resumption::disabled();
    }
    Ok(Arc::new(config))
}

/// One request on a FRESH connection: TCP connect + TLS handshake + GET, then close. Returns which
/// handshake happened. The response read matters beyond realism: TLS 1.3 NewSessionTickets are
/// post-handshake messages, so reading is what pulls them into the shared session cache for the
/// next connection to offer.
async fn tls_get_once(t: &TlsTarget, connector: &TlsConnector) -> Result<HandshakeKind, BoxError> {
    let tcp = TcpStream::connect(&t.addr).await?;
    tcp.set_nodelay(true)?;
    let tls = connector.connect(t.sni.clone(), tcp).await?;
    let kind = tls
        .get_ref()
        .1
        .handshake_kind()
        .ok_or("handshake kind unavailable after connect")?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = Request::get(t.path.as_str())
        .header(hyper::header::HOST, t.authority.as_str())
        .body(Empty::<Bytes>::new())?;
    let mut resp = sender.send_request(req).await?;
    while let Some(frame) = resp.frame().await {
        frame?;
    }
    Ok(kind)
}

/// Closed-loop per worker (oha's `-c N` shape): connect/handshake/GET back-to-back until
/// `warmup + duration` elapses. Latencies + handshake-kind counts recorded only past warmup —
/// in `resumed` mode the warmup is also what seeds the session cache with tickets.
async fn tls_worker(
    t: Arc<TlsTarget>,
    config: Arc<ClientConfig>,
    start: Instant,
    warmup: u64,
    duration: u64,
) -> (Vec<f64>, u64, u64, u64) {
    let connector = TlsConnector::from(config);
    let mut latencies = Vec::new();
    let (mut full, mut resumed, mut errors) = (0u64, 0u64, 0u64);
    let end = start + Duration::from_secs(warmup + duration);
    let warm_until = start + Duration::from_secs(warmup);
    while Instant::now() < end {
        let t0 = Instant::now();
        let res = tls_get_once(&t, &connector).await;
        if t0 < warm_until {
            continue;
        }
        match res {
            Ok(kind) => {
                latencies.push(t0.elapsed().as_secs_f64() * 1000.0);
                match kind {
                    HandshakeKind::Full | HandshakeKind::FullWithHelloRetryRequest => full += 1,
                    HandshakeKind::Resumed => resumed += 1,
                }
            }
            Err(_) => errors += 1,
        }
    }
    (latencies, resumed, full, errors)
}

async fn run_tls(a: TlsArgs) -> Result<(), BoxError> {
    let t = Arc::new(parse_tls_target(&a.target)?);
    let config = tls_client_config(&a.ca, &a.mode)?;
    let start = Instant::now();
    let handles: Vec<_> = (0..a.workers)
        .map(|_| {
            tokio::spawn(tls_worker(
                t.clone(),
                config.clone(),
                start,
                a.warmup,
                a.duration,
            ))
        })
        .collect();

    let mut all: Vec<f64> = Vec::new();
    let (mut resumed, mut full, mut errors) = (0u64, 0u64, 0u64);
    for h in handles {
        let (lat, r, f, e) = h.await?;
        all.extend(lat);
        resumed += r;
        full += f;
        errors += e;
    }
    all.sort_by(f64::total_cmp);
    let n = all.len();
    let pct = |p: f64| -> f64 {
        if n == 0 {
            return 0.0;
        }
        let idx = ((p / 100.0) * (n as f64 - 1.0)).round() as usize;
        all[idx.min(n - 1)]
    };
    let (p50, p90, p99) = (pct(50.0), pct(90.0), pct(99.0));
    let rps = n as f64 / a.duration.max(1) as f64;
    // Both kind counts are always emitted, whatever the mode asked for: a `resumed` run that in
    // fact took full handshakes (cold cache, key mismatch across a restart) must be visible in
    // the CSV, not silently mislabeled.
    let resumed_pct = 100.0 * resumed as f64 / (resumed + full).max(1) as f64;
    let csv = format!(
        "mode,workers,duration_s,requests,rps,full,resumed,resumed_pct,errors,p50_ms,p90_ms,p99_ms\n\
         {},{},{},{n},{rps:.1},{full},{resumed},{resumed_pct:.1},{errors},{p50:.3},{p90:.3},{p99:.3}\n",
        a.mode, a.workers, a.duration
    );
    std::fs::write(&a.out, csv)?;
    println!(
        "tls {}: {n} requests over {} s ({} workers) -> {rps:.0} req/s, \
         full={full} resumed={resumed} ({resumed_pct:.1}% resumed), errors={errors}, \
         p50={p50:.3}ms p99={p99:.3}ms",
        a.mode, a.duration, a.workers
    );
    Ok(())
}

// ---------------------------------------------------------------------------- ws

struct WsArgs {
    mode: String,
    target: String,
    rate: u64,
    duration: u64,
    warmup: u64,
    workers: u64,
    conns: u64,
    seconds: u64,
    size: u64,
    out: String,
}

/// One open-loop handshake attempt per available token: connect + complete the RFC 6455 upgrade,
/// then close cleanly. Buckets successes/failures per second (after `warmup`), the same steady-
/// state convention as `ejection_worker`.
async fn ws_handshake_worker(
    t: Target,
    tokens: Arc<AtomicI64>,
    done: Arc<AtomicBool>,
    start: Instant,
    warmup: u64,
) -> HashMap<u64, (u64, u64)> {
    let mut local: HashMap<u64, (u64, u64)> = HashMap::new();
    while !done.load(Ordering::Relaxed) {
        if tokens.fetch_sub(1, Ordering::Relaxed) <= 0 {
            tokens.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_micros(500)).await;
            continue;
        }
        let res = tokio::time::timeout(Duration::from_secs(5), ws::connect(&t)).await;
        let Some(sec) = start.elapsed().as_secs().checked_sub(warmup) else {
            continue;
        };
        let entry = local.entry(sec).or_insert((0, 0));
        match res {
            Ok(Ok(mut stream)) => {
                entry.0 += 1;
                let _ = ws::write_frame(&mut stream, 0x8, &[]).await; // close cleanly
            }
            _ => entry.1 += 1,
        }
    }
    local
}

async fn run_ws_handshake(a: &WsArgs) -> Result<(), BoxError> {
    let t = parse_target(&a.target)?;
    let tokens = Arc::new(AtomicI64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let start = Instant::now();

    let handles: Vec<_> = (0..a.workers)
        .map(|_| {
            tokio::spawn(ws_handshake_worker(
                t.clone(),
                tokens.clone(),
                done.clone(),
                start,
                a.warmup,
            ))
        })
        .collect();

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

    let mut timeline: HashMap<u64, (u64, u64)> = HashMap::new();
    for h in handles {
        for (sec, (ok, fail)) in h.await? {
            let entry = timeline.entry(sec).or_insert((0, 0));
            entry.0 += ok;
            entry.1 += fail;
        }
    }
    let mut secs: Vec<u64> = timeline
        .keys()
        .copied()
        .filter(|s| *s < a.duration)
        .collect();
    secs.sort_unstable();
    let mut csv = String::from("t,success,failed\n");
    for s in &secs {
        let (ok, fail) = timeline[s];
        let _ = writeln!(csv, "{s},{ok},{fail}");
    }
    std::fs::write(&a.out, csv)?;

    let total_ok: u64 = timeline.values().map(|(ok, _)| ok).sum();
    let total_fail: u64 = timeline.values().map(|(_, fail)| fail).sum();
    println!(
        "ws handshake: {total_ok} ok, {total_fail} failed over {} s ({:.1}/s achieved)",
        secs.len(),
        total_ok as f64 / secs.len().max(1) as f64
    );
    Ok(())
}

/// Open N established WS tunnels (post-101), hold them idle for `seconds`, exit — the `hold`
/// subcommand's shape, but through the Upgrade handshake, for a tunnel-footprint RSS read.
async fn run_ws_hold(a: &WsArgs) -> Result<(), BoxError> {
    let t = parse_target(&a.target)?;
    let mut streams = Vec::with_capacity(a.conns as usize);
    for _ in 0..a.conns {
        if let Ok(stream) = ws::connect(&t).await {
            streams.push(stream);
        }
    }
    println!(
        "ws hold: {} tunnels open for {} s",
        streams.len(),
        a.seconds
    );
    tokio::time::sleep(Duration::from_secs(a.seconds)).await;
    drop(streams);
    Ok(())
}

/// One connection's sustained request/response loop: send a `size`-byte binary frame, wait for
/// the echo, repeat back-to-back (closed-loop per connection — the same concurrency model oha's
/// `-c N` uses) until `warmup + duration` has elapsed since the shared `start`. Round-trip
/// latencies are recorded only once warmup has passed.
async fn ws_echo_worker(
    t: Target,
    size: usize,
    start: Instant,
    warmup: u64,
    duration: u64,
) -> Vec<f64> {
    let mut latencies = Vec::new();
    let Ok(mut stream) = ws::connect(&t).await else {
        return latencies;
    };
    let payload = vec![b'x'; size];
    let end = start + Duration::from_secs(warmup + duration);
    let warm_until = start + Duration::from_secs(warmup);
    while Instant::now() < end {
        let t0 = Instant::now();
        if ws::write_frame(&mut stream, 0x2, &payload).await.is_err() {
            break;
        }
        match ws::read_frame(&mut stream).await {
            Ok(Some(_)) => {
                if t0 >= warm_until {
                    latencies.push(t0.elapsed().as_secs_f64() * 1000.0);
                }
            }
            _ => break,
        }
    }
    latencies
}

async fn run_ws_echo(a: &WsArgs) -> Result<(), BoxError> {
    let t = parse_target(&a.target)?;
    let start = Instant::now();
    let handles: Vec<_> = (0..a.conns)
        .map(|_| {
            tokio::spawn(ws_echo_worker(
                t.clone(),
                a.size as usize,
                start,
                a.warmup,
                a.duration,
            ))
        })
        .collect();
    let mut all: Vec<f64> = Vec::new();
    for h in handles {
        all.extend(h.await?);
    }
    all.sort_by(f64::total_cmp);
    let n = all.len();
    let pct = |p: f64| -> f64 {
        if n == 0 {
            return 0.0;
        }
        let idx = ((p / 100.0) * (n as f64 - 1.0)).round() as usize;
        all[idx.min(n - 1)]
    };
    let dur = a.duration.max(1) as f64;
    let msgs_per_sec = n as f64 / dur;
    let mb_per_sec = (n as f64 * a.size as f64) / dur / 1_000_000.0;
    let (p50, p90, p99) = (pct(50.0), pct(90.0), pct(99.0));
    let csv = format!(
        "conns,size_bytes,duration_s,messages,messages_per_sec,mb_per_sec,p50_ms,p90_ms,p99_ms\n\
         {},{},{},{n},{msgs_per_sec:.1},{mb_per_sec:.2},{p50:.3},{p90:.3},{p99:.3}\n",
        a.conns, a.size, a.duration
    );
    std::fs::write(&a.out, csv)?;
    println!(
        "ws echo: {n} messages over {} s ({} conns, {}B) -> {msgs_per_sec:.0} msg/s, p50={p50:.3}ms p99={p99:.3}ms",
        a.duration, a.conns, a.size
    );
    Ok(())
}

async fn run_ws(a: WsArgs) -> Result<(), BoxError> {
    match a.mode.as_str() {
        "handshake" => run_ws_handshake(&a).await,
        "hold" => run_ws_hold(&a).await,
        "echo" => run_ws_echo(&a).await,
        other => Err(format!("unknown ws --mode: {other} (want handshake|hold|echo)").into()),
    }
}

// ---------------------------------------------------------------------------- CLI

fn usage() -> ! {
    eprintln!(
        "usage:\n  plecto-loadgen rr --target URL [--total N] [--workers W] [--out FILE]\n  \
         plecto-loadgen openloop --target URL --rate R [--duration S] [--warmup S] \
         [--workers W] [--backlog-secs S] [--out FILE] [--hist-out FILE]\n  \
         plecto-loadgen ejection --target URL --toggle a=URL b=URL c=URL \
         [--toggle-at SEC=key[,key...][:label] ...] \
         [--rate R] [--duration S] [--warmup S] [--workers W] [--out FILE] [--events-out FILE]\n  \
         plecto-loadgen swap --target URL --exec-at SEC=CMD [--exec-at SEC=CMD ...] \
         [--rate R] [--duration S] [--warmup S] [--workers W] [--out FILE] [--events-out FILE]\n  \
         plecto-loadgen hold --target URL [--conns N] [--seconds S]\n  \
         plecto-loadgen ws --mode handshake|hold|echo --target ws://HOST:PORT/PATH \
         [--rate R] [--duration S] [--warmup S] [--workers W] [--conns N] [--seconds S] \
         [--size BYTES] [--out FILE]\n  \
         plecto-loadgen tls --mode full|resumed --target https://HOST:PORT/PATH --ca PEM \
         [--duration S] [--warmup S] [--workers W] [--out FILE]"
    );
    std::process::exit(2)
}

fn parse_openloop(rest: &[String]) -> openloop::OpenloopArgs {
    let mut a = openloop::OpenloopArgs {
        target: "http://127.0.0.1:28080/".to_string(),
        rate: 0,
        duration: 90,
        warmup: 5,
        workers: 64,
        backlog_secs: 3,
        out: "openloop.json".to_string(),
        hist_out: None,
    };
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--target" => a.target = take(&mut it, flag),
            "--rate" => a.rate = take_num(&mut it, flag),
            "--duration" => a.duration = take_num(&mut it, flag).max(1),
            "--warmup" => a.warmup = take_num(&mut it, flag),
            "--workers" => a.workers = take_num(&mut it, flag).max(1),
            "--backlog-secs" => a.backlog_secs = take_num(&mut it, flag).max(1),
            "--out" => a.out = take(&mut it, flag),
            "--hist-out" => a.hist_out = Some(take(&mut it, flag)),
            _ => usage(),
        }
    }
    if a.rate == 0 {
        eprintln!("openloop requires --rate R (target arrivals per second)");
        std::process::exit(2)
    }
    a
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
        plan: Vec::new(),
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
            // "SEC=key[,key...][:label]" — giving any --toggle-at replaces the default plan
            // wholesale, so a caller owns the whole timeline or none of it.
            "--toggle-at" => {
                let spec = take(&mut it, flag);
                let Some((at, tail)) = spec.split_once('=') else { usage() };
                let Ok(at) = at.parse::<u64>() else { usage() };
                let (keys, label) = match tail.split_once(':') {
                    Some((k, l)) => (k, l.to_string()),
                    None => (tail, format!("toggle {tail}")),
                };
                a.plan.push(ToggleEvent {
                    at,
                    keys: keys.split(',').map(str::to_string).collect(),
                    label,
                });
            }
            _ => usage(),
        }
    }
    if a.plan.is_empty() {
        a.plan = default_ejection_plan();
    }
    if !["a", "b", "c"].iter().all(|k| a.toggles.contains_key(*k)) {
        eprintln!("--toggle must provide a=URL b=URL c=URL");
        std::process::exit(2)
    }
    a
}

fn parse_swap(rest: &[String]) -> SwapArgs {
    let mut a = SwapArgs {
        target: "http://127.0.0.1:8080/".to_string(),
        rate: 4000,
        duration: 60,
        warmup: 5,
        workers: 64,
        exec_at: Vec::new(),
        out: "swap.csv".to_string(),
        events_out: "swap_events.csv".to_string(),
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
            "--exec-at" => {
                let kv = take(&mut it, flag);
                let Some((sec, cmd)) = kv.split_once('=') else {
                    usage()
                };
                let Ok(sec) = sec.parse::<u64>() else { usage() };
                a.exec_at.push((sec, cmd.to_string()));
            }
            _ => usage(),
        }
    }
    if a.exec_at.is_empty() {
        eprintln!("--exec-at SEC=CMD is required (at least one)");
        std::process::exit(2)
    }
    a
}

fn parse_ws(rest: &[String]) -> WsArgs {
    let mut a = WsArgs {
        mode: String::new(),
        target: "ws://127.0.0.1:8080/ws".to_string(),
        rate: 500,
        duration: 30,
        warmup: 5,
        workers: 32,
        conns: 50,
        seconds: 6,
        size: 1024,
        out: "ws.csv".to_string(),
    };
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--mode" => a.mode = take(&mut it, flag),
            "--target" => a.target = take(&mut it, flag),
            "--rate" => a.rate = take_num(&mut it, flag).max(1),
            "--duration" => a.duration = take_num(&mut it, flag).max(1),
            "--warmup" => a.warmup = take_num(&mut it, flag),
            "--workers" => a.workers = take_num(&mut it, flag).max(1),
            "--conns" => a.conns = take_num(&mut it, flag).max(1),
            "--seconds" => a.seconds = take_num(&mut it, flag),
            "--size" => a.size = take_num(&mut it, flag).max(1),
            "--out" => a.out = take(&mut it, flag),
            _ => usage(),
        }
    }
    if !["handshake", "hold", "echo"].contains(&a.mode.as_str()) {
        eprintln!("--mode is required: handshake|hold|echo");
        std::process::exit(2)
    }
    a
}

fn parse_tls(rest: &[String]) -> TlsArgs {
    let mut a = TlsArgs {
        target: "https://127.0.0.1:28443/".to_string(),
        mode: String::new(),
        ca: String::new(),
        duration: 20,
        warmup: 3,
        workers: 48,
        out: "tls_handshake.csv".to_string(),
    };
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--mode" => a.mode = take(&mut it, flag),
            "--target" => a.target = take(&mut it, flag),
            "--ca" => a.ca = take(&mut it, flag),
            "--duration" => a.duration = take_num(&mut it, flag).max(1),
            "--warmup" => a.warmup = take_num(&mut it, flag),
            "--workers" => a.workers = take_num(&mut it, flag).max(1),
            "--out" => a.out = take(&mut it, flag),
            _ => usage(),
        }
    }
    if !["full", "resumed"].contains(&a.mode.as_str()) {
        eprintln!("--mode is required: full|resumed");
        std::process::exit(2)
    }
    if a.ca.is_empty() {
        eprintln!("--ca PEM is required (the bench cert to trust)");
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
        "openloop" => openloop::run_openloop(parse_openloop(rest)).await,
        "ejection" => run_ejection(parse_ejection(rest)).await,
        "swap" => run_swap(parse_swap(rest)).await,
        "hold" => run_hold(parse_hold(rest)).await,
        "ws" => run_ws(parse_ws(rest)).await,
        "tls" => run_tls(parse_tls(rest)).await,
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("plecto-loadgen: {e}");
        std::process::exit(1);
    }
}
