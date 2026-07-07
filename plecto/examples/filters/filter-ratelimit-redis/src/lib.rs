//! filter-ratelimit-redis — the **global layer** of Plecto's local-floor × global two-tier
//! rate-limit model (ADR 000061). The native fast-path token bucket (ADR 000033) is the
//! per-replica flood floor; this filter is the reference implementation of the fleet-wide layer
//! Fork 6 ([[000053]]) always pointed at: shared state belongs in the extension plane, consulted
//! over a RESP-compatible store (Redis, Valkey, ...) — never native (declined in ADR 000053/000060).
//!
//! Algorithm: a general, textbook **fixed-window counter** — `INCRBY key cost` (atomic in Redis)
//! followed by an unconditional `EXPIRE key window_seconds NX` (Redis >= 7.0 / Valkey). Calling
//! `EXPIRE ... NX` on *every* request, not just the one that happens to create the key, closes the
//! orphaned-TTL race a naive "only the first request sets EXPIRE" implementation carries (a crash
//! between `INCRBY` and `EXPIRE` on that first request would otherwise leave a key with no TTL
//! forever) — self-healing without a Lua script, which keeps the guest portable across RESP stores
//! with uneven Lua support (ADR 000061 実装追補 #1). This is a from-scratch reimplementation of a
//! generic, widely-documented technique, not a port of any specific client library.
//!
//! Business config (Redis host/port, window, limit, cost source, `on_backend_error`, `route_tag`)
//! comes from the manifest `[filter.config]` section via the `host-config` capability (ADR
//! 000066) — read once in `init` and cached for the instance's lifetime. Because a missing/invalid
//! `on_backend_error` deliberately traps `init`, **this filter requires `isolation = "trusted"`**:
//! only a trusted filter's eager load-time instantiate (`plecto-host`'s `Host::load`) turns that
//! trap into a load failure instead of a per-request 503. Trusted isolation also lets one TCP
//! connection to the backend survive across requests on a pooled instance (host CONTEXT.md
//! "Persistent connection") instead of paying connect+PING on every request.
//!
//! RESP scope is deliberately minimal (v1): `PING` (to verify a freshly opened connection),
//! `INCRBY`, and `EXPIRE`. No `AUTH`, no TLS — this filter targets a trusted-network backend;
//! either can be added by extending [`send_pipeline`] / [`connect_and_ping`] without touching the
//! `plecto:filter` contract.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({
    path: "../../../wit",
    world: "filter",
});

use crate::plecto::filter::host_config;
use crate::plecto::filter::host_log;
use crate::plecto::filter::types::Header;

use std::cell::RefCell;

use wasi::io::streams::{InputStream, OutputStream};
use wasi::sockets::instance_network::instance_network;
use wasi::sockets::network::{
    ErrorCode, IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress,
    Network,
};
use wasi::sockets::tcp::TcpSocket;
use wasi::sockets::{ip_name_lookup, tcp_create_socket};

struct FilterRatelimitRedis;

/// The header carrying the caller's real address (ADR 000018 / 000057 — trustworthy from the
/// filter's viewpoint once the edge model reissues it; see ADR 000061 Decision 2).
const CLIENT_IDENTITY_HEADER: &str = "x-forwarded-for";

/// This filter's manifest business config (`[filter.config]`, ADR 000066), validated once in
/// `init` and cached for the pooled instance's lifetime — never re-read per request.
#[derive(Clone)]
struct Config {
    redis_host: String,
    redis_port: u16,
    window_seconds: u64,
    limit: u64,
    /// Fixed per-request cost when `cost_header` is absent, or the header is absent/unparseable.
    default_cost: u64,
    /// Optional header name carrying a per-request dynamic cost (ADR 000061 Decision 2).
    cost_header: Option<String>,
    /// Groups this deployment's keys apart from other routes sharing the same filter binary.
    route_tag: String,
    /// `true` = "deny" (fail closed: a backend consult failure short-circuits 503). `false` =
    /// "allow" (fail open: a backend failure continues, trusting the native floor, ADR 000033).
    deny_on_backend_error: bool,
}

/// Read a required `[filter.config]` key, or trap. A missing/invalid business config is an
/// operator mistake, not untrusted request input — deliberately failing `init` (rather than
/// silently defaulting) is how this filter gets ADR 000061's "undeclared fails load, not
/// request" guarantee, via `isolation = "trusted"`'s eager load-time instantiate (ADR 000066
/// Decision 4). This never runs on the per-request hot path.
fn required_config(key: &str) -> String {
    match host_config::get(key) {
        Some(v) if !v.is_empty() => v,
        _ => panic!(
            "filter-ratelimit-redis: [filter.config] must declare a non-empty '{key}' \
             (ADR 000061 / 000066); requires isolation = \"trusted\" so this fails at load"
        ),
    }
}

fn required_parsed<T: std::str::FromStr>(key: &str) -> T {
    required_config(key)
        .parse()
        .unwrap_or_else(|_| panic!("filter-ratelimit-redis: [filter.config].{key} is not valid"))
}

impl Config {
    fn from_host_config() -> Self {
        let deny_on_backend_error = match required_config("on_backend_error").as_str() {
            "deny" => true,
            "allow" => false,
            other => panic!(
                "filter-ratelimit-redis: on_backend_error must be \"deny\" or \"allow\", got {other:?}"
            ),
        };
        Config {
            redis_host: required_config("redis_host"),
            redis_port: required_parsed("redis_port"),
            window_seconds: required_parsed("window_seconds"),
            limit: required_parsed("limit"),
            default_cost: host_config::get("cost")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
            cost_header: host_config::get("cost_header"),
            route_tag: required_config("route_tag"),
            deny_on_backend_error,
        }
    }
}

thread_local! {
    static CONFIG: RefCell<Option<Config>> = const { RefCell::new(None) };
    static CONN: RefCell<Option<RespConn>> = const { RefCell::new(None) };
}

fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

fn short_circuit(status: u16, reason: &str, retry_after_secs: Option<u64>) -> RequestDecision {
    let mut headers = vec![Header {
        name: "content-type".to_string(),
        value: "application/json".to_string(),
    }];
    if let Some(secs) = retry_after_secs {
        headers.push(Header {
            name: "retry-after".to_string(),
            value: secs.to_string(),
        });
    }
    RequestDecision::ShortCircuit(HttpResponse {
        status,
        headers,
        body: format!("{{\"error\":\"{reason}\"}}").into_bytes(),
    })
}

// --- RESP (minimal): encode requests as arrays of bulk strings, decode only the three reply
// --- types INCRBY / EXPIRE / PING ever return (integer, simple string, error) — see module docs.

enum RespReply {
    Simple(String),
    Error(String),
    Integer(i64),
}

fn encode_command(parts: &[&str]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(format!("*{}\r\n", parts.len()).as_bytes());
    for p in parts {
        buf.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        buf.extend_from_slice(p.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

struct RespReader<'a> {
    input: &'a InputStream,
    buf: Vec<u8>,
}

impl<'a> RespReader<'a> {
    fn new(input: &'a InputStream) -> Self {
        Self {
            input,
            buf: Vec::new(),
        }
    }

    /// Read one CRLF-terminated line, blocking for more bytes as needed.
    fn read_line(&mut self) -> Result<Vec<u8>, String> {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line = self.buf[..pos].to_vec();
                self.buf.drain(..pos + 2);
                return Ok(line);
            }
            let chunk = self
                .input
                .blocking_read(4096)
                .map_err(|e| format!("read: {e:?}"))?;
            if chunk.is_empty() {
                return Err("backend closed the connection".to_string());
            }
            self.buf.extend_from_slice(&chunk);
        }
    }

    fn read_reply(&mut self) -> Result<RespReply, String> {
        let line = self.read_line()?;
        match line.first() {
            Some(b'+') => Ok(RespReply::Simple(
                String::from_utf8_lossy(&line[1..]).into_owned(),
            )),
            Some(b'-') => Ok(RespReply::Error(
                String::from_utf8_lossy(&line[1..]).into_owned(),
            )),
            Some(b':') => std::str::from_utf8(&line[1..])
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .map(RespReply::Integer)
                .ok_or_else(|| "malformed integer reply".to_string()),
            _ => Err(format!(
                "unexpected reply: {:?}",
                String::from_utf8_lossy(&line)
            )),
        }
    }
}

// --- outbound TCP plumbing (ADR 000060), the same resolve → connect shape as filter-tcp-gate ---

struct RespConn {
    // Kept alive alongside the streams it owns — never read/written directly again.
    _socket: TcpSocket,
    input: InputStream,
    output: OutputStream,
}

fn resolve(network: &Network, host: &str) -> Result<IpAddress, String> {
    if let Ok(v4) = host.parse::<core::net::Ipv4Addr>() {
        let [a, b, c, d] = v4.octets();
        return Ok(IpAddress::Ipv4((a, b, c, d)));
    }
    if let Ok(v6) = host.parse::<core::net::Ipv6Addr>() {
        let [a, b, c, d, e, f, g, h] = v6.segments();
        return Ok(IpAddress::Ipv6((a, b, c, d, e, f, g, h)));
    }
    let stream =
        ip_name_lookup::resolve_addresses(network, host).map_err(|e| format!("resolve: {e:?}"))?;
    let pollable = stream.subscribe();
    loop {
        match stream.resolve_next_address() {
            Ok(Some(addr)) => return Ok(addr),
            Ok(None) => return Err("resolve: no addresses".to_string()),
            Err(ErrorCode::WouldBlock) => pollable.block(),
            Err(e) => return Err(format!("resolve: {e:?}")),
        }
    }
}

fn socket_address(addr: IpAddress, port: u16) -> IpSocketAddress {
    match addr {
        IpAddress::Ipv4(address) => IpSocketAddress::Ipv4(Ipv4SocketAddress { port, address }),
        IpAddress::Ipv6(address) => IpSocketAddress::Ipv6(Ipv6SocketAddress {
            port,
            address,
            flow_info: 0,
            scope_id: 0,
        }),
    }
}

fn connect(network: &Network, addr: IpSocketAddress) -> Result<RespConn, String> {
    let family = match addr {
        IpSocketAddress::Ipv4(_) => IpAddressFamily::Ipv4,
        IpSocketAddress::Ipv6(_) => IpAddressFamily::Ipv6,
    };
    let socket =
        tcp_create_socket::create_tcp_socket(family).map_err(|e| format!("socket: {e:?}"))?;
    socket
        .start_connect(network, addr)
        .map_err(|e| format!("connect: {e:?}"))?;
    let pollable = socket.subscribe();
    loop {
        match socket.finish_connect() {
            Ok((input, output)) => {
                return Ok(RespConn {
                    _socket: socket,
                    input,
                    output,
                });
            }
            Err(ErrorCode::WouldBlock) => pollable.block(),
            Err(e) => return Err(format!("connect: {e:?}")),
        }
    }
}

/// Open a fresh connection and verify it with `PING` before trusting it (ADR 000061 実装追補 #4:
/// RESP scope is INCRBY / EXPIRE / PING only).
fn connect_and_ping(cfg: &Config) -> Result<RespConn, String> {
    let network = instance_network();
    let addr = socket_address(resolve(&network, &cfg.redis_host)?, cfg.redis_port);
    let conn = connect(&network, addr)?;
    conn.output
        .blocking_write_and_flush(&encode_command(&["PING"]))
        .map_err(|e| format!("write: {e:?}"))?;
    match RespReader::new(&conn.input).read_reply()? {
        RespReply::Simple(s) if s == "PONG" => Ok(conn),
        RespReply::Error(e) => Err(format!("PING error: {e}")),
        _ => Err("unexpected PING reply".to_string()),
    }
}

/// One fixed-window consult: pipeline `INCRBY key cost` + `EXPIRE key window_seconds NX` in a
/// single write (halving round trips vs. sending them separately), then read both replies in
/// order. `EXPIRE ... NX` runs unconditionally, not only when `INCRBY` just created the key — see
/// the module docs for why this closes the orphaned-TTL race a "first request only" check leaves.
fn send_pipeline(
    conn: &RespConn,
    key: &str,
    cost: u64,
    window_seconds: u64,
) -> Result<i64, String> {
    let cost = cost.to_string();
    let window = window_seconds.to_string();
    let mut buf = encode_command(&["INCRBY", key, &cost]);
    buf.extend_from_slice(&encode_command(&["EXPIRE", key, &window, "NX"]));
    conn.output
        .blocking_write_and_flush(&buf)
        .map_err(|e| format!("write: {e:?}"))?;

    let mut reader = RespReader::new(&conn.input);
    let count = match reader.read_reply()? {
        RespReply::Integer(n) => n,
        RespReply::Error(e) => return Err(format!("INCRBY error: {e}")),
        _ => return Err("unexpected INCRBY reply".to_string()),
    };
    match reader.read_reply()? {
        RespReply::Integer(_) => {}
        RespReply::Error(e) => return Err(format!("EXPIRE error: {e}")),
        _ => return Err("unexpected EXPIRE reply".to_string()),
    }
    Ok(count)
}

/// Consult the global limiter for one request, reusing the cached connection when present and
/// retrying exactly once on a fresh connection if it turns out to be dead (ADR 000060 実装追補 #4
/// persistent connection reuse across requests on a pooled/trusted instance).
fn consult(cfg: &Config, key: &str, cost: u64) -> Result<i64, String> {
    let mut last_err = String::new();
    for attempt in 0..2 {
        let existing = CONN.with(|c| c.borrow_mut().take());
        let conn = match existing {
            Some(conn) => conn,
            None => match connect_and_ping(cfg) {
                Ok(conn) => conn,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            },
        };
        match send_pipeline(&conn, key, cost, cfg.window_seconds) {
            Ok(count) => {
                CONN.with(|c| *c.borrow_mut() = Some(conn));
                return Ok(count);
            }
            Err(e) => {
                // The connection is presumed broken — drop it (do not cache) and, on the first
                // attempt, retry once against a freshly dialed connection.
                last_err = e;
                if attempt == 0 {
                    continue;
                }
            }
        }
    }
    Err(last_err)
}

fn dynamic_cost(req: &HttpRequest, cfg: &Config) -> u64 {
    cfg.cost_header
        .as_deref()
        .and_then(|name| header(req, name))
        .and_then(|v| v.parse().ok())
        .unwrap_or(cfg.default_cost)
}

fn handle_request(cfg: &Config, req: &HttpRequest) -> RequestDecision {
    let client_identity = header(req, CLIENT_IDENTITY_HEADER).unwrap_or("unknown");
    let key = format!("rl:{}:{client_identity}", cfg.route_tag);
    let cost = dynamic_cost(req, cfg);

    match consult(cfg, &key, cost) {
        Ok(count) if count as u64 <= cfg.limit => RequestDecision::Continue,
        Ok(_) => short_circuit(429, "rate limit exceeded", Some(cfg.window_seconds)),
        Err(reason) => {
            host_log::log(
                host_log::Level::Warn,
                &format!("filter-ratelimit-redis: backend consult failed: {reason}"),
            );
            if cfg.deny_on_backend_error {
                short_circuit(503, "rate limiter backend unavailable", None)
            } else {
                // fail open: the native local floor (ADR 000033) still shields this route.
                RequestDecision::Continue
            }
        }
    }
}

impl Guest for FilterRatelimitRedis {
    fn init() {
        let cfg = Config::from_host_config();
        host_log::log(
            host_log::Level::Info,
            &format!(
                "filter-ratelimit-redis: init route_tag={} redis={}:{} window={}s limit={} on_backend_error={}",
                cfg.route_tag,
                cfg.redis_host,
                cfg.redis_port,
                cfg.window_seconds,
                cfg.limit,
                if cfg.deny_on_backend_error {
                    "deny"
                } else {
                    "allow"
                },
            ),
        );
        CONFIG.with(|c| *c.borrow_mut() = Some(cfg));
    }

    fn on_request(req: HttpRequest) -> RequestDecision {
        CONFIG.with(|c| {
            let cfg_ref = c.borrow();
            match cfg_ref.as_ref() {
                Some(cfg) => handle_request(cfg, &req),
                // Structurally unreachable — `init` always runs (and would have trapped) before
                // any `on_request` on this instance. The hot path never panics on it regardless
                // (Tenet: no data-plane panic even on a "can't happen" branch).
                None => short_circuit(503, "not initialized", None),
            }
        })
    }

    fn on_response(_resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Continue
    }
}

export!(FilterRatelimitRedis);
