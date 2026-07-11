//! filter-tcp-gate — a `plecto:filter` that consults a raw TCP backend over the lent outbound TCP
//! capability (ADR 000060) and decides:
//!   - backend answers the `PING\n` probe with a line starting `OK` → `continue`,
//!   - any other answer → short-circuit 403,
//!   - ANY outbound error (name-lookup deny / SSRF block / connect deny / connect failure) →
//!     short-circuit 503. A failed or blocked consult is NEVER treated as "allow" (fail-closed).
//!
//! The target is taken from the `x-tcp-target` request header (`host:port`) so a test can point it
//! at different destinations; in production it would be fixed in the filter. Built for
//! wasm32-wasip2 — unlike the header-only filters it imports `wasi:sockets` (via the `wasi` crate).
//! The host still gates every resolve and connect by the operator allowlist + SSRF guard + IP pin;
//! this guest cannot widen that.
#![allow(clippy::all)]

wit_bindgen::generate!({
    path: "../../../wit",
    world: "filter",
});

use crate::plecto::filter::host_log;
use crate::plecto::filter::types::Header;

use wasi::io::streams::{InputStream, OutputStream};
use wasi::sockets::instance_network::instance_network;
use wasi::sockets::network::{
    ErrorCode, IpAddress, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress,
    Network,
};
use wasi::sockets::tcp::TcpSocket;
use wasi::sockets::{ip_name_lookup, tcp_create_socket};

struct FilterTcpGate;

const TARGET_HEADER: &str = "x-tcp-target";

fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(&h.value).ok())
}

fn short_circuit(status: u16, reason: &str) -> RequestDecision {
    RequestDecision::ShortCircuit(HttpResponse {
        status,
        headers: vec![Header {
            name: "content-type".to_string(),
            value: b"text/plain".to_vec(),
        }],
        body: format!("tcp-gate: {reason}").into_bytes(),
    })
}

/// Split `host:port` (the port is the digits after the LAST `:`, so a bracketed IPv6 literal
/// `[::1]:80` still parses).
fn parse_target(target: &str) -> Option<(&str, u16)> {
    let (host, port) = target.rsplit_once(':')?;
    let host = host.strip_prefix('[').unwrap_or(host);
    let host = host.strip_suffix(']').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some((host, port.parse().ok()?))
}

/// The target as an address: an IP literal directly, otherwise the FIRST address the host's
/// (vetted) `ip-name-lookup` returns.
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

/// Connect to a vetted address. The socket resource is returned alongside its streams: it owns the
/// connection and must outlive them.
fn connect(
    network: &Network,
    addr: IpSocketAddress,
) -> Result<(TcpSocket, InputStream, OutputStream), String> {
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
            Ok((input, output)) => return Ok((socket, input, output)),
            Err(ErrorCode::WouldBlock) => pollable.block(),
            Err(e) => return Err(format!("connect: {e:?}")),
        }
    }
}

/// One probe round-trip: connect, send `PING\n`, read the backend's first bytes.
fn probe(target: &str) -> Result<Vec<u8>, String> {
    let (host, port) = parse_target(target).ok_or_else(|| "bad target".to_string())?;
    let network = instance_network();
    let addr = socket_address(resolve(&network, host)?, port);
    let (_socket, input, output) = connect(&network, addr)?;
    output
        .blocking_write_and_flush(b"PING\n")
        .map_err(|e| format!("write: {e:?}"))?;
    input.blocking_read(64).map_err(|e| format!("read: {e:?}"))
}

impl Guest for FilterTcpGate {
    fn init() {
        host_log::log(host_log::Level::Info, "filter-tcp-gate: init");
    }

    fn on_request(req: HttpRequest) -> RequestDecision {
        let Some(target) = header(&req, TARGET_HEADER) else {
            return short_circuit(503, "no target");
        };
        match probe(target) {
            Ok(reply) if reply.starts_with(b"OK") => RequestDecision::Continue,
            Ok(reply) => short_circuit(
                403,
                &format!("backend said {:?}", String::from_utf8_lossy(&reply)),
            ),
            Err(reason) => short_circuit(503, &reason),
        }
    }

    fn on_response(_req: HttpRequest, _resp: HttpResponse) -> ResponseDecision {
        ResponseDecision::Continue
    }
}

export!(FilterTcpGate);
