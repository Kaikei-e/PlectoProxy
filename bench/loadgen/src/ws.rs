//! A minimal RFC 6455 WebSocket client: just enough to open-loop drive Plecto's `/ws` Upgrade
//! tunnel (ADR 000048) — handshake, masked frame writes, unmasked frame reads. Not a general WS
//! library (no fragmentation, no extensions); it mirrors the mock upstream's codec in
//! `bench/harnesses/bench-server/ws.rs`, but is a separate, standalone crate (`bench/loadgen` is
//! its own Cargo workspace), so the two are deliberately not shared.

use base64::Engine as _;
use rand::RngCore;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{BoxError, Target};

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

fn random_ws_key() -> String {
    let mut nonce = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce);
    base64::engine::general_purpose::STANDARD.encode(nonce)
}

async fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>, BoxError> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err("connection closed before the response head completed".into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
    }
}

/// Open a TCP connection and complete the RFC 6455 handshake against `t`. Verifies the 101 status
/// and the `Sec-WebSocket-Accept` value; returns the raw stream positioned right after the head,
/// ready for framed I/O.
pub(crate) async fn connect(t: &Target) -> Result<TcpStream, BoxError> {
    let mut stream = TcpStream::connect(&t.addr).await?;
    stream.set_nodelay(true)?;
    let key = random_ws_key();
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         host: {host}\r\n\
         connection: upgrade\r\n\
         upgrade: websocket\r\n\
         sec-websocket-version: 13\r\n\
         sec-websocket-key: {key}\r\n\r\n",
        path = t.path,
        host = t.authority,
    );
    stream.write_all(request.as_bytes()).await?;
    let head = read_head(&mut stream).await?;
    let head = String::from_utf8_lossy(&head).to_lowercase();
    if !head.starts_with("http/1.1 101") {
        return Err(format!("handshake rejected: {head:?}").into());
    }
    let expected = format!("sec-websocket-accept: {}", accept_key(&key).to_lowercase());
    if !head.contains(&expected) {
        return Err(format!("unexpected sec-websocket-accept in: {head:?}").into());
    }
    Ok(stream)
}

/// Write one MASKED frame (client -> server, RFC 6455 §5.2 requires client frames to be masked).
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    opcode: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut head = vec![0x80 | opcode];
    let len = payload.len();
    if len < 126 {
        head.push(0x80 | len as u8);
    } else if len <= u16::MAX as usize {
        head.push(0x80 | 126);
        head.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        head.push(0x80 | 127);
        head.extend_from_slice(&(len as u64).to_be_bytes());
    }
    let mut mask = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut mask);
    head.extend_from_slice(&mask);
    w.write_all(&head).await?;
    let masked: Vec<u8> = payload
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ mask[i % 4])
        .collect();
    w.write_all(&masked).await?;
    w.flush().await
}

/// Read one frame from the server. Server frames are UNMASKED by spec, but an unmasked read is
/// tolerated defensively (masked or not) since this is a benchmark client, not a conformance
/// checker. Returns `None` on a clean EOF before any byte of a new frame arrives.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut head = [0u8; 2];
    if let Err(e) = r.read_exact(&mut head).await {
        return if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Ok(None)
        } else {
            Err(e)
        };
    }
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
    if opcode == 0x8 {
        return Ok(None); // a close frame ends this connection's useful life
    }
    Ok(Some(payload))
}
