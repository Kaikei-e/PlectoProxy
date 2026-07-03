//! Minimal RFC 6455 framing for the `/ws` mock upstream — just enough to accept a handshake and
//! echo text/binary frames. Not a general WS library: no fragmentation, no extensions. Plecto's
//! fast path tunnels bytes opaquely after the 101 (`crates/server/src/tunnel.rs`), so correctness
//! here only has to satisfy the bench client on the other end (`bench/loadgen`'s `ws` subcommand).

use std::convert::Infallible;
use std::net::SocketAddr;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// RFC 6455 §1.3: `Sec-WebSocket-Accept` = base64(SHA-1(client key + the magic GUID)).
pub(crate) fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

pub(crate) enum Frame {
    Text(Vec<u8>),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong,
    Close,
}

/// Read one MASKED frame (client -> server, RFC 6455 §5.2). `Ok(None)` on a clean EOF before any
/// byte of a new frame arrives. Fragmentation (FIN=0) is out of scope for this fixture.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Frame>> {
    let mut head = [0u8; 2];
    if let Err(e) = r.read_exact(&mut head).await {
        return if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Ok(None)
        } else {
            Err(e)
        };
    }
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    let mut len = u64::from(head[1] & 0x7f);
    if len == 126 {
        let mut ext = [0u8; 2];
        r.read_exact(&mut ext).await?;
        len = u64::from(u16::from_be_bytes(ext));
    } else if len == 127 {
        let mut ext = [0u8; 8];
        r.read_exact(&mut ext).await?;
        len = u64::from_be_bytes(ext);
    }
    if !fin {
        return Err(std::io::Error::other(
            "fragmented frames are not supported by this bench fixture",
        ));
    }
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok(Some(match opcode {
        0x1 => Frame::Text(payload),
        0x2 => Frame::Binary(payload),
        0x8 => Frame::Close,
        0x9 => Frame::Ping(payload),
        0xA => Frame::Pong,
        _ => {
            return Err(std::io::Error::other(format!(
                "unsupported opcode {opcode}"
            )));
        }
    }))
}

/// Write one UNMASKED frame (server -> client, RFC 6455 §5.1).
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    opcode: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut head = vec![0x80 | opcode];
    let len = payload.len();
    if len < 126 {
        head.push(len as u8);
    } else if len <= u16::MAX as usize {
        head.push(126);
        head.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        head.push(127);
        head.extend_from_slice(&(len as u64).to_be_bytes());
    }
    w.write_all(&head).await?;
    w.write_all(payload).await?;
    w.flush().await
}

/// Echo every text/binary frame back verbatim; reply to a ping with a pong; close on a close
/// frame or EOF. One task per established tunnel (spawned after the 101 by `handle`).
async fn echo_loop<S: AsyncRead + AsyncWrite + Unpin>(mut io: S) {
    loop {
        match read_frame(&mut io).await {
            Ok(Some(Frame::Text(payload))) => {
                if write_frame(&mut io, 0x1, &payload).await.is_err() {
                    return;
                }
            }
            Ok(Some(Frame::Binary(payload))) => {
                if write_frame(&mut io, 0x2, &payload).await.is_err() {
                    return;
                }
            }
            Ok(Some(Frame::Ping(payload))) => {
                if write_frame(&mut io, 0xA, &payload).await.is_err() {
                    return;
                }
            }
            Ok(Some(Frame::Pong)) => {}
            Ok(Some(Frame::Close)) | Ok(None) => {
                let _ = write_frame(&mut io, 0x8, &[]).await;
                return;
            }
            Err(e) => {
                eprintln!("ws upstream: frame error: {e}");
                return;
            }
        }
    }
}

/// The HTTP side of the handshake: `/healthz` for the active health check, else a websocket
/// Upgrade — compute the accept key, arm the upgrade future BEFORE returning the 101 (hyper's
/// upgrade contract), and hand the resulting `Upgraded` stream to `echo_loop`.
async fn handle(mut req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    if req.uri().path() == "/healthz" {
        return Ok(Response::builder()
            .status(200)
            .body(Full::new(Bytes::new()))
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()))));
    }
    let is_ws = req
        .headers()
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let key = req
        .headers()
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (Some(key), true) = (key, is_ws) else {
        return Ok(Response::builder()
            .status(400)
            .body(Full::new(Bytes::from_static(
                b"expected a websocket upgrade",
            )))
            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()))));
    };
    let accept = accept_key(&key);
    let on_upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => echo_loop(TokioIo::new(upgraded)).await,
            Err(e) => eprintln!("ws upstream: upgrade never completed: {e}"),
        }
    });
    Ok(Response::builder()
        .status(101)
        .header(hyper::header::UPGRADE, "websocket")
        .header(hyper::header::CONNECTION, "upgrade")
        .header("sec-websocket-accept", accept)
        .body(Full::new(Bytes::new()))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new()))))
}

/// A dedicated in-process upstream that speaks WebSocket: replies to the active health check on
/// `/healthz`, otherwise completes the RFC 6455 handshake and echoes every frame. This is the
/// upstream Plecto's `/ws` route (`[route.upgrade] protocols = ["websocket"]`) tunnels bytes to —
/// the proxy itself never parses WS framing (it splices opaque bytes after the 101).
pub(crate) async fn spawn_echo_upstream() -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            // Nagle + this bench's small echo frames is the exact delayed-ACK stall the body-hook
            // scenario already found once (performance/README.md's churn note) — disable it here
            // too, before the frame loop ever writes a small reply.
            let _ = stream.set_nodelay(true);
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(handle))
                    .with_upgrades()
                    .await;
            });
        }
    });
    Ok(addr)
}
