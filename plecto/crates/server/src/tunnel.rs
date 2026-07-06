//! The post-101 upgrade tunnel (ADR 000048): once both sides of a protocol switch have handed
//! their connections over, bytes are copied bidirectionally and opaquely between them — the
//! general bidirectional-relay technique every upgrade-capable proxy shares. The only policy on
//! the tunnel is time: an activity-based idle timer (a byte in EITHER direction resets it — the
//! form nginx `proxy_read_timeout` / Envoy `stream_idle_timeout` / HAProxy `timeout tunnel` all
//! take) and the server's drain flag (ADR 000039), so an abandoned or indefinite tunnel can
//! neither hold its connection permit forever nor outlive a graceful shutdown.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;

/// Await both halves of the switch and splice them. Failing to obtain either upgraded connection
/// (the client vanished before the 101 landed, or the upstream reneged) just drops both ends —
/// there is no client to answer anymore. Returns the `(down, up)` byte totals the tunnel relayed
/// (ADR 000059) — `(0, 0)` when it never established.
pub(crate) async fn run(
    downstream: hyper::upgrade::OnUpgrade,
    upstream: hyper::upgrade::OnUpgrade,
    idle_timeout: Option<Duration>,
    drain: watch::Receiver<bool>,
) -> (u64, u64) {
    let (down, up) = match tokio::join!(downstream, upstream) {
        (Ok(d), Ok(u)) => (d, u),
        (d, u) => {
            tracing::debug!(
                downstream_ok = d.is_ok(),
                upstream_ok = u.is_ok(),
                "upgrade tunnel never established"
            );
            return (0, 0);
        }
    };
    splice(TokioIo::new(down), TokioIo::new(up), idle_timeout, drain).await
}

/// Copy bytes both ways until one side closes, the idle timer fires, or the server drains.
/// Generic over the two streams so the relay itself is unit-testable with in-memory pipes.
/// Returns how many bytes moved in each direction as `(down, up)` — `down` = written towards
/// `a` (the client side), `up` = written towards `b` (the upstream side).
pub(crate) async fn splice<A, B>(
    a: A,
    b: B,
    idle_timeout: Option<Duration>,
    mut drain: watch::Receiver<bool>,
) -> (u64, u64)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let start = Instant::now();
    let last_activity = Arc::new(AtomicU64::new(0));
    // The byte tallies live OUTSIDE the copy future (ADR 000059): the idle-timeout and drain
    // arms drop `copy_bidirectional` before it can return its totals, so the same wrapper that
    // stamps activity counts the bytes, and the counts survive the cancellation.
    let up_bytes = Arc::new(AtomicU64::new(0)); // read off the client side, upstream-bound
    let down_bytes = Arc::new(AtomicU64::new(0)); // read off the upstream side, client-bound
    let mut a = ActivityIo::new(a, start, last_activity.clone(), up_bytes.clone());
    let mut b = ActivityIo::new(b, start, last_activity.clone(), down_bytes.clone());
    let copy = tokio::io::copy_bidirectional(&mut a, &mut b);
    tokio::pin!(copy);
    tokio::select! {
        _ = &mut copy => {}
        _ = idle_elapsed(start, &last_activity, idle_timeout) => {
            tracing::debug!("upgrade tunnel closed by idle timeout");
        }
        _ = crate::listener::drained(&mut drain) => {
            tracing::debug!("upgrade tunnel closed by drain");
        }
    }
    (
        down_bytes.load(Ordering::Relaxed),
        up_bytes.load(Ordering::Relaxed),
    )
}

/// Resolve when no byte has moved in either direction for `idle_timeout`; pend forever when the
/// operator disabled the timer (`None`).
async fn idle_elapsed(start: Instant, last_activity: &AtomicU64, idle_timeout: Option<Duration>) {
    let Some(idle) = idle_timeout else {
        return std::future::pending().await;
    };
    loop {
        let last = Duration::from_millis(last_activity.load(Ordering::Relaxed));
        let since = start.elapsed().saturating_sub(last);
        if since >= idle {
            return;
        }
        tokio::time::sleep(idle - since).await;
    }
}

/// An `AsyncRead + AsyncWrite` wrapper that stamps a shared millisecond clock on every byte
/// moved — so the idle watchdog observes activity without instrumenting the copy loop itself —
/// and tallies the bytes it reads into `read_bytes` (counting reads, not writes, so a byte is
/// never counted on both sides of the relay).
struct ActivityIo<T> {
    inner: T,
    start: Instant,
    last_activity: Arc<AtomicU64>,
    read_bytes: Arc<AtomicU64>,
}

impl<T> ActivityIo<T> {
    fn new(
        inner: T,
        start: Instant,
        last_activity: Arc<AtomicU64>,
        read_bytes: Arc<AtomicU64>,
    ) -> Self {
        Self {
            inner,
            start,
            last_activity,
            read_bytes,
        }
    }

    fn mark(&self) {
        let elapsed = self.start.elapsed().as_millis() as u64;
        self.last_activity.store(elapsed, Ordering::Relaxed);
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ActivityIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        let read = buf.filled().len() - before;
        if matches!(poll, Poll::Ready(Ok(()))) && read > 0 {
            self.mark();
            self.read_bytes.fetch_add(read as u64, Ordering::Relaxed);
        }
        poll
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ActivityIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if matches!(poll, Poll::Ready(Ok(n)) if n > 0) {
            self.mark();
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn splice_relays_bytes_both_ways() {
        let (client, client_far) = tokio::io::duplex(64);
        let (server, server_far) = tokio::io::duplex(64);
        let (_tx, drain) = watch::channel(false);
        let tunnel = tokio::spawn(splice(client_far, server_far, None, drain));

        let (mut client, mut server) = (client, server);
        client.write_all(b"c2s").await.unwrap();
        let mut buf = [0u8; 3];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"c2s");
        server.write_all(b"s2c").await.unwrap();
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"s2c");

        // Half-close passes through (copy_bidirectional): the relay ends once BOTH sides closed.
        drop(client);
        drop(server);
        let (down, up) = tokio::time::timeout(Duration::from_secs(1), tunnel)
            .await
            .expect("the relay ends when both sides close")
            .unwrap();
        assert_eq!(
            (down, up),
            (3, 3),
            "the relay reports per-direction byte totals (ADR 000059)"
        );
    }

    #[tokio::test]
    async fn splice_closes_an_idle_tunnel() {
        let (_client, client_far) = tokio::io::duplex(64);
        let (_server, server_far) = tokio::io::duplex(64);
        let (_tx, drain) = watch::channel(false);
        tokio::time::timeout(
            Duration::from_secs(2),
            splice(
                client_far,
                server_far,
                Some(Duration::from_millis(50)),
                drain,
            ),
        )
        .await
        .expect("an idle tunnel must be closed by the idle timer");
    }

    #[tokio::test]
    async fn splice_closes_on_drain() {
        let (_client, client_far) = tokio::io::duplex(64);
        let (_server, server_far) = tokio::io::duplex(64);
        let (tx, drain) = watch::channel(false);
        let tunnel = tokio::spawn(splice(client_far, server_far, None, drain));
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), tunnel)
            .await
            .expect("a drain flip must close the tunnel")
            .unwrap();
    }

    #[tokio::test]
    async fn byte_totals_survive_a_drain_cancellation() {
        // The drain arm drops the copy future before it returns its totals; the counts must
        // come from the wrapper, not the copy's return value (ADR 000059).
        let (mut client, client_far) = tokio::io::duplex(64);
        let (mut server, server_far) = tokio::io::duplex(64);
        let (tx, drain) = watch::channel(false);
        let tunnel = tokio::spawn(splice(client_far, server_far, None, drain));

        client.write_all(b"c2s").await.unwrap();
        let mut buf = [0u8; 3];
        server.read_exact(&mut buf).await.unwrap();
        server.write_all(b"s2c-x").await.unwrap();
        let mut buf5 = [0u8; 5];
        client.read_exact(&mut buf5).await.unwrap();

        tx.send(true).unwrap();
        let (down, up) = tokio::time::timeout(Duration::from_secs(1), tunnel)
            .await
            .expect("a drain flip must close the tunnel")
            .unwrap();
        assert_eq!((down, up), (5, 3), "totals survive the cancelled copy");
    }
}
