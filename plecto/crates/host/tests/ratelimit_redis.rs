//! End-to-end behaviour of the global-layer reference filter `filter-ratelimit-redis` (ADR
//! 000061) through the real wasm guest, over the outbound TCP capability (ADR 000060). Compiled
//! only with the `outbound-tcp` feature (OFF by default).
//!
//! `MockResp` below is a tiny in-process RESP server stand-in — just enough of the protocol
//! (arrays of bulk strings in, simple/integer/error replies out) to answer `PING` / `INCRBY` /
//! `EXPIRE ... NX`, exactly the three commands this filter ever sends. It is a test fixture, not a
//! Redis reimplementation.
#![cfg(feature = "outbound-tcp")]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use plecto_host::test_support::{TestSigner, bound_sbom, filter_ratelimit_redis_component};
use plecto_host::{
    Header, Host, HttpRequest, LoadOptions, LoadedFilter, RequestDecision, RequestTrace,
    SignedArtifact, TcpAllowEntry,
};

/// A non-loopback local address to bind the mock backend on — the outbound-TCP reserved floor
/// makes loopback unusable for the success path (mirrors `tcp_gate.rs`'s helper).
fn non_loopback_local_ip() -> Option<IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("192.0.2.1:9").ok()?; // TEST-NET-1: never actually reached
    let ip = probe.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// Read one RESP command (an array of bulk strings) off `stream`, buffering leftover bytes in
/// `buf` across calls. `None` on EOF/malformed input.
fn read_command(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<Vec<String>> {
    fn read_line(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<String> {
        loop {
            if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8(buf[..pos].to_vec()).ok()?;
                buf.drain(..pos + 2);
                return Some(line);
            }
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }
    fn read_exact_buffered(stream: &mut TcpStream, buf: &mut Vec<u8>, n: usize) -> Option<Vec<u8>> {
        while buf.len() < n {
            let mut chunk = [0u8; 4096];
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..read]);
        }
        Some(buf.drain(..n).collect())
    }

    let header = read_line(stream, buf)?;
    let count: usize = header.strip_prefix('*')?.parse().ok()?;
    let mut parts = Vec::with_capacity(count);
    for _ in 0..count {
        let len_line = read_line(stream, buf)?;
        let len: usize = len_line.strip_prefix('$')?.parse().ok()?;
        let data = read_exact_buffered(stream, buf, len)?;
        read_exact_buffered(stream, buf, 2)?; // trailing CRLF
        parts.push(String::from_utf8(data).ok()?);
    }
    Some(parts)
}

/// Minimal RESP server: in-memory `INCRBY` counters + first-write-wins `EXPIRE ... NX` flags.
/// Tracks how many distinct TCP connections it accepted, so a test can assert the filter reuses
/// one persistent connection across requests instead of dialing fresh each time.
struct MockResp {
    addr: SocketAddr,
    connections_accepted: Arc<AtomicUsize>,
}

fn spawn_mock_resp(bind: IpAddr) -> MockResp {
    let listener = TcpListener::bind((bind, 0)).expect("bind mock resp backend");
    let addr = listener.local_addr().unwrap();
    let connections_accepted = Arc::new(AtomicUsize::new(0));
    let counters: Arc<Mutex<HashMap<String, i64>>> = Arc::new(Mutex::new(HashMap::new()));
    let ttls: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    let accepted = connections_accepted.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            accepted.fetch_add(1, Ordering::SeqCst);
            let counters = counters.clone();
            let ttls = ttls.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                loop {
                    let Some(cmd) = read_command(&mut stream, &mut buf) else {
                        return;
                    };
                    let reply = handle_command(&cmd, &counters, &ttls);
                    if stream.write_all(&reply).is_err() {
                        return;
                    }
                }
            });
        }
    });

    MockResp {
        addr,
        connections_accepted,
    }
}

fn handle_command(
    cmd: &[String],
    counters: &Mutex<HashMap<String, i64>>,
    ttls: &Mutex<HashSet<String>>,
) -> Vec<u8> {
    let Some(name) = cmd.first() else {
        return b"-ERR empty command\r\n".to_vec();
    };
    match name.to_ascii_uppercase().as_str() {
        "PING" => b"+PONG\r\n".to_vec(),
        "INCRBY" => {
            let (Some(key), Some(delta)) =
                (cmd.get(1), cmd.get(2).and_then(|d| d.parse::<i64>().ok()))
            else {
                return b"-ERR wrong number of arguments\r\n".to_vec();
            };
            let mut c = counters.lock().unwrap();
            let v = c.entry(key.clone()).or_insert(0);
            *v += delta;
            format!(":{v}\r\n").into_bytes()
        }
        "EXPIRE" => {
            let Some(key) = cmd.get(1) else {
                return b"-ERR wrong number of arguments\r\n".to_vec();
            };
            let mut t = ttls.lock().unwrap();
            let just_set = t.insert(key.clone());
            format!(":{}\r\n", i32::from(just_set)).into_bytes()
        }
        other => format!("-ERR unknown command '{other}'\r\n").into_bytes(),
    }
}

fn signed_load(opts: LoadOptions) -> anyhow::Result<(Host, LoadedFilter)> {
    let bytes = filter_ratelimit_redis_component();
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(&bytes).unwrap();
    let sbom = bound_sbom(&bytes);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let artifact = SignedArtifact {
        component_bytes: &bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };
    let filter = host.load("filter-ratelimit-redis", &artifact, opts)?;
    Ok((host, filter))
}

fn config(
    backend: SocketAddr,
    window_seconds: u64,
    limit: u64,
    on_backend_error: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("on_backend_error".to_string(), on_backend_error.to_string()),
        ("redis_host".to_string(), backend.ip().to_string()),
        ("redis_port".to_string(), backend.port().to_string()),
        ("window_seconds".to_string(), window_seconds.to_string()),
        ("limit".to_string(), limit.to_string()),
        ("route_tag".to_string(), "test-route".to_string()),
    ])
}

fn opts_for(backend: SocketAddr, cfg: BTreeMap<String, String>) -> LoadOptions {
    // The mock backend binds a non-loopback LOCAL address, which is private space (RFC1918/ULA) —
    // the SSRF guard blocks all private space by default, so the test must opt it in by CIDR, the
    // same way `tcp_gate.rs`'s success-path test does.
    let cidr = match backend.ip() {
        IpAddr::V4(_) => format!("{}/32", backend.ip()),
        IpAddr::V6(_) => format!("{}/128", backend.ip()),
    };
    LoadOptions::trusted()
        .with_outbound_tcp(
            vec![TcpAllowEntry {
                host: backend.ip().to_string(),
                port: backend.port(),
            }],
            vec![cidr],
            Some(4),
            Some(5_000),
        )
        .with_config(cfg)
}

fn request(client_ip: &str) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: "/api".to_string(),
        authority: "gateway.test".to_string(),
        scheme: "https".to_string(),
        headers: vec![Header {
            name: "x-forwarded-for".to_string(),
            value: client_ip.as_bytes().to_vec(),
        }],
    }
}

#[test]
fn allows_under_limit_and_denies_over_limit() {
    let Some(ip) = non_loopback_local_ip() else {
        eprintln!("skip: no non-loopback local address on this machine");
        return;
    };
    let backend = spawn_mock_resp(ip);
    let opts = opts_for(backend.addr, config(backend.addr, 60, 2, "deny"));
    let (_host, filter) = signed_load(opts).expect("load filter-ratelimit-redis");

    let req = request("203.0.113.9");
    for attempt in 1..=2 {
        let (decision, _logs) = filter
            .on_request(&req, &RequestTrace::root())
            .expect("run on_request");
        assert!(
            matches!(decision, RequestDecision::Continue),
            "attempt {attempt} is within the limit of 2, got {decision:?}"
        );
    }

    let (decision, _logs) = filter
        .on_request(&req, &RequestTrace::root())
        .expect("run on_request");
    match decision {
        RequestDecision::ShortCircuit(resp) => {
            assert_eq!(
                resp.status, 429,
                "the 3rd request over a limit of 2 is denied"
            );
            assert!(
                resp.headers
                    .iter()
                    .any(|h| h.name.eq_ignore_ascii_case("retry-after")),
                "a 429 carries a Retry-After hint"
            );
        }
        other => panic!("expected 429 over-limit short-circuit, got {other:?}"),
    }
}

#[test]
fn distinct_clients_get_independent_counters() {
    let Some(ip) = non_loopback_local_ip() else {
        eprintln!("skip: no non-loopback local address on this machine");
        return;
    };
    let backend = spawn_mock_resp(ip);
    let opts = opts_for(backend.addr, config(backend.addr, 60, 1, "deny"));
    let (_host, filter) = signed_load(opts).expect("load filter-ratelimit-redis");

    // Drain client A's single-request budget.
    let a = request("198.51.100.1");
    assert!(matches!(
        filter.on_request(&a, &RequestTrace::root()).unwrap().0,
        RequestDecision::Continue
    ));
    assert!(matches!(
        filter.on_request(&a, &RequestTrace::root()).unwrap().0,
        RequestDecision::ShortCircuit(_)
    ));

    // Client B is keyed independently and still has its full budget.
    let b = request("198.51.100.2");
    assert!(matches!(
        filter.on_request(&b, &RequestTrace::root()).unwrap().0,
        RequestDecision::Continue
    ));
}

#[test]
fn persistent_connection_is_reused_across_requests() {
    let Some(ip) = non_loopback_local_ip() else {
        eprintln!("skip: no non-loopback local address on this machine");
        return;
    };
    let backend = spawn_mock_resp(ip);
    let opts = opts_for(backend.addr, config(backend.addr, 60, 1_000_000, "deny"));
    let (_host, filter) = signed_load(opts).expect("load filter-ratelimit-redis");

    let req = request("203.0.113.50");
    for _ in 0..5 {
        let (decision, _logs) = filter.on_request(&req, &RequestTrace::root()).unwrap();
        assert!(matches!(decision, RequestDecision::Continue));
    }

    assert_eq!(
        backend.connections_accepted.load(Ordering::SeqCst),
        1,
        "a pooled/trusted instance must reuse one persistent connection, not reconnect per request"
    );
}

#[test]
fn backend_unreachable_denies_when_on_backend_error_is_deny() {
    // Bind and immediately drop the listener: the address is valid but nothing answers connect.
    let ip = non_loopback_local_ip().unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let listener = TcpListener::bind((ip, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let opts = opts_for(addr, config(addr, 60, 100, "deny"));
    let (_host, filter) = signed_load(opts).expect("load filter-ratelimit-redis");

    let (decision, _logs) = filter
        .on_request(&request("203.0.113.77"), &RequestTrace::root())
        .expect("run on_request");
    match decision {
        RequestDecision::ShortCircuit(resp) => assert_eq!(resp.status, 503),
        other => panic!("expected fail-closed 503, got {other:?}"),
    }
}

#[test]
fn backend_unreachable_allows_when_on_backend_error_is_allow() {
    let ip = non_loopback_local_ip().unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let listener = TcpListener::bind((ip, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let opts = opts_for(addr, config(addr, 60, 100, "allow"));
    let (_host, filter) = signed_load(opts).expect("load filter-ratelimit-redis");

    let (decision, _logs) = filter
        .on_request(&request("203.0.113.78"), &RequestTrace::root())
        .expect("run on_request");
    assert!(
        matches!(decision, RequestDecision::Continue),
        "on_backend_error = allow must fail open, got {decision:?}"
    );
}

#[test]
fn missing_on_backend_error_fails_the_load_not_the_request() {
    // `on_backend_error` absent from `[filter.config]` — init() must trap, and because this
    // filter requires `isolation = "trusted"`, that trap surfaces as a load failure (ADR 000066
    // Decision 4), never as a per-request 503.
    let Some(ip) = non_loopback_local_ip() else {
        eprintln!("skip: no non-loopback local address on this machine");
        return;
    };
    let backend = spawn_mock_resp(ip);
    let mut cfg = config(backend.addr, 60, 100, "deny");
    cfg.remove("on_backend_error");
    let opts = opts_for(backend.addr, cfg);

    assert!(
        signed_load(opts).is_err(),
        "a manifest missing the required on_backend_error key must fail to load"
    );
}

#[test]
fn invalid_on_backend_error_value_fails_the_load() {
    let Some(ip) = non_loopback_local_ip() else {
        eprintln!("skip: no non-loopback local address on this machine");
        return;
    };
    let backend = spawn_mock_resp(ip);
    let opts = opts_for(backend.addr, config(backend.addr, 60, 100, "maybe"));

    assert!(
        signed_load(opts).is_err(),
        "on_backend_error must be exactly \"deny\" or \"allow\""
    );
}
