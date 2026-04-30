//! Two-phase bidirectional stream forwarding with bounded post-half-close idle.
//!
//! Phase 1: both directions copy concurrently with no timeout.
//! Phase 2: once one direction has reached EOF, its FIN is forwarded to the
//! peer's write side via `shutdown()`; the surviving direction continues but
//! must observe at least one byte of progress per `idle` interval, otherwise
//! it is torn down. This bounds CLOSE-WAIT/FIN-WAIT-2 stalls when a peer
//! never closes its own write side, while preserving the half-close-then-
//! respond protocol semantics that `tokio::io::copy_bidirectional` provides.

use std::time::Duration;
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;

/// Buffer size per direction. Matches Tokio's internal `io::copy` chunk size.
const FORWARD_BUF_SIZE: usize = 8 * 1024;

/// Forward bytes in both directions between `a` and `b` until either side
/// closes (or errors). After the first side closes, the surviving direction
/// is bounded by `idle`: every successful read resets the deadline; if the
/// deadline fires the forwarder returns `Err(TimedOut)`. `None` disables the
/// post-half-close timeout entirely (matching legacy `copy_bidirectional`
/// behavior).
pub(crate) async fn forward_bidirectional_with_idle_timeout<A, B>(
    a: A,
    b: B,
    idle: Option<Duration>,
) -> io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (ar, aw) = io::split(a);
    let (br, bw) = io::split(b);

    // Each direction owns a oneshot it watches. When the other direction
    // finishes Ok(()), the parent sends an idle Duration through the channel
    // and the surviving pump arms its own per-read timeout.
    let (atob_arm_tx, atob_arm_rx) = oneshot::channel::<Duration>();
    let (btoa_arm_tx, btoa_arm_rx) = oneshot::channel::<Duration>();

    let atob = pump(ar, bw, atob_arm_rx, idle);
    let btoa = pump(br, aw, btoa_arm_rx, idle);
    tokio::pin!(atob);
    tokio::pin!(btoa);

    tokio::select! {
        r = &mut atob => match r {
            Ok(()) => {
                if let Some(t) = idle {
                    let _ = btoa_arm_tx.send(t);
                }
                btoa.await
            }
            // Error in one direction tears down the other immediately, the
            // same way `copy_bidirectional` returns on first error.
            Err(e) => Err(e),
        },
        r = &mut btoa => match r {
            Ok(()) => {
                if let Some(t) = idle {
                    let _ = atob_arm_tx.send(t);
                }
                atob.await
            }
            Err(e) => Err(e),
        },
    }
}

/// Marker error used as the inner error of the `io::Error` we raise on a
/// post-half-close idle timeout. Kept module-private; downstream code that
/// needs to recognize the leak-guard reaper goes through
/// `is_post_half_close_idle_timeout`, not the type itself.
#[derive(Debug)]
struct PostHalfCloseIdleTimeout;

impl std::fmt::Display for PostHalfCloseIdleTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("post half-close idle timeout")
    }
}

impl std::error::Error for PostHalfCloseIdleTimeout {}

fn post_half_close_idle_timeout_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, PostHalfCloseIdleTimeout)
}

/// True iff `e` is the leak-guard reaper we raised, not a generic
/// transport-level `TimedOut` (TLS handshake, keepalive, etc.).
pub(crate) fn is_post_half_close_idle_timeout(e: &io::Error) -> bool {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<PostHalfCloseIdleTimeout>())
        .is_some()
}

/// Read from `reader` and write to `writer` until reader EOFs or errors.
/// `arm_idle` is the phase-1 → phase-2 trigger sent by the parent when the
/// sibling pump finishes; `configured_idle` is the immutable upper bound the
/// EOF `shutdown()` is allowed to take (so a transport whose `poll_shutdown`
/// flushes protocol bytes — e.g. WebSocket's Close frame — cannot stall the
/// pump indefinitely on an unresponsive peer).
async fn pump<R, W>(
    mut reader: R,
    mut writer: W,
    mut arm_idle: oneshot::Receiver<Duration>,
    configured_idle: Option<Duration>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; FORWARD_BUF_SIZE];
    let mut idle: Option<Duration> = None;

    loop {
        let n = await_with_arm_or_timeout(reader.read(&mut buf), &mut arm_idle, &mut idle).await?;
        if n == 0 {
            return shutdown_with_optional_timeout(writer, configured_idle).await;
        }
        // Both `write_all` and `flush` must also race the arm/idle deadline:
        // a peer that half-closes and then stops draining its read side
        // would otherwise stall the surviving direction inside `write_all`
        // (TCP backpressure) before the deadline can be applied.
        //
        // WebSocket writers buffer frames in `Sink::start_send` and only
        // push them on `poll_flush`, so the explicit flush is also needed
        // for transport correctness on `websocket{,_tls}` (see #460).
        await_with_arm_or_timeout(writer.write_all(&buf[..n]), &mut arm_idle, &mut idle).await?;
        await_with_arm_or_timeout(writer.flush(), &mut arm_idle, &mut idle).await?;
    }
}

/// Bound the EOF-side `shutdown` by `idle`. When `idle` is `None` the
/// behavior matches legacy `copy_bidirectional` (unbounded). On expiry the
/// caller sees the same `PostHalfCloseIdleTimeout` sentinel as a phase-2
/// stall, so log filtering treats it identically.
async fn shutdown_with_optional_timeout<W>(mut writer: W, idle: Option<Duration>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match idle {
        None => writer.shutdown().await,
        Some(t) => match tokio::time::timeout(t, writer.shutdown()).await {
            Ok(r) => r,
            Err(_) => Err(post_half_close_idle_timeout_error()),
        },
    }
}

/// Drive `fut` while remaining responsive to the phase-1 → phase-2 arm
/// signal and, once armed, enforcing the per-step idle deadline. This means
/// the deadline starts to apply at the granularity of every I/O step (read,
/// write, flush) — not just reads — so a malicious peer cannot hide a stall
/// inside a long `write_all`.
async fn await_with_arm_or_timeout<F, T>(
    fut: F,
    arm: &mut oneshot::Receiver<Duration>,
    idle: &mut Option<Duration>,
) -> io::Result<T>
where
    F: std::future::Future<Output = io::Result<T>>,
{
    tokio::pin!(fut);
    loop {
        match *idle {
            None => {
                tokio::select! {
                    biased; // prefer making progress on `fut` over arming idle
                    r = &mut fut => return r,
                    armed = &mut *arm => {
                        if let Ok(t) = armed {
                            *idle = Some(t);
                        }
                        // Loop again: now in `Some` branch.
                    }
                }
            }
            Some(t) => {
                tokio::select! {
                    biased;
                    r = &mut fut => return r,
                    _ = tokio::time::sleep(t) => {
                        return Err(post_half_close_idle_timeout_error());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, ReadBuf};
    use tokio::sync::Notify;
    use tokio::time::timeout;

    /// An `AsyncRead + AsyncWrite` wrapper that surfaces a `Notify` the moment
    /// `poll_write` returns `Pending` for the first time. Tests use it to
    /// deterministically know when the forwarder's pump is parked inside
    /// `write_all` waiting on backpressure, without relying on a sleep race.
    struct WriteStallProbe<S> {
        inner: S,
        first_pending: Arc<AtomicBool>,
        notify: Arc<Notify>,
    }

    impl<S> WriteStallProbe<S> {
        fn new(inner: S) -> (Self, Arc<Notify>) {
            let notify = Arc::new(Notify::new());
            let probe = WriteStallProbe {
                inner,
                first_pending: Arc::new(AtomicBool::new(false)),
                notify: notify.clone(),
            };
            (probe, notify)
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for WriteStallProbe<S> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let me = self.get_mut();
            let r = Pin::new(&mut me.inner).poll_write(cx, buf);
            if matches!(r, Poll::Pending)
                && !me.first_pending.swap(true, Ordering::SeqCst)
            {
                me.notify.notify_waiters();
            }
            r
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    impl<S: AsyncRead + Unpin> AsyncRead for WriteStallProbe<S> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
        }
    }

    /// An `AsyncRead + AsyncWrite` wrapper whose `poll_shutdown` never
    /// completes. Models a transport whose shutdown handshake (e.g. WebSocket
    /// `poll_close` flushing a Close frame on a non-draining peer) cannot
    /// finish, so the `shutdown_with_optional_timeout` bound must reap it.
    struct PendingShutdown<S> {
        inner: S,
    }

    impl<S> PendingShutdown<S> {
        fn new(inner: S) -> Self {
            PendingShutdown { inner }
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for PendingShutdown<S> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl<S: AsyncRead + Unpin> AsyncRead for PendingShutdown<S> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
        }
    }

    /// Phase-1 + clean phase-2: side A writes, half-closes, side B writes a
    /// response after EOF, side A reads the response. Must succeed.
    #[tokio::test]
    async fn half_close_then_response_is_forwarded() {
        let (a_outer, a_inner) = duplex(64);
        let (b_outer, b_inner) = duplex(64);

        let fwd = tokio::spawn(forward_bidirectional_with_idle_timeout(
            a_inner,
            b_inner,
            Some(Duration::from_secs(5)),
        ));

        let (mut a_r, mut a_w) = tokio::io::split(a_outer);
        let (mut b_r, mut b_w) = tokio::io::split(b_outer);

        // Visitor (a) sends request and half-closes.
        a_w.write_all(b"req").await.unwrap();
        a_w.shutdown().await.unwrap();

        // Upstream (b) reads request + EOF, then writes response.
        let mut got = [0u8; 3];
        b_r.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"req");
        let mut tail = [0u8; 1];
        let n = b_r.read(&mut tail).await.unwrap();
        assert_eq!(n, 0, "upstream must see FIN");
        b_w.write_all(b"resp").await.unwrap();
        b_w.shutdown().await.unwrap();

        // Visitor reads response.
        let mut resp = [0u8; 4];
        a_r.read_exact(&mut resp).await.unwrap();
        assert_eq!(&resp, b"resp");
        let n = a_r.read(&mut [0u8; 1]).await.unwrap();
        assert_eq!(n, 0);

        timeout(Duration::from_secs(2), fwd)
            .await
            .expect("forwarder hung")
            .unwrap()
            .unwrap();
    }

    /// Phase-2 must bound write-side stalls too: A half-closes, B writes
    /// faster than A reads (small duplex buffer). Once A stops draining,
    /// the forwarder's `write_all` on A would block forever if the deadline
    /// only covered reads. The per-step timeout must catch this.
    #[tokio::test]
    async fn stuck_writer_after_half_close_times_out() {
        let (a_outer, a_inner) = duplex(8);
        let (b_outer, b_inner) = duplex(8);

        let fwd = tokio::spawn(forward_bidirectional_with_idle_timeout(
            a_inner,
            b_inner,
            Some(Duration::from_millis(200)),
        ));

        let (_a_r, mut a_w) = tokio::io::split(a_outer);
        let (_b_r, mut b_w) = tokio::io::split(b_outer);

        a_w.shutdown().await.unwrap();

        // The producer write itself can backpressure; spawn it so the test
        // doesn't deadlock if the forwarder regression makes it hang.
        let producer = tokio::spawn(async move {
            let _ = b_w.write_all(&[0u8; 4096]).await;
        });

        let res = timeout(Duration::from_secs(2), fwd)
            .await
            .expect("forwarder did not return — write-stall not bounded")
            .unwrap();
        let err = res.expect_err("expected TimedOut on stuck writer");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        producer.abort();
    }

    /// Round-2 regression, deterministic: when the phase-1 pump is already
    /// parked inside `write_all` (because B has been streaming bytes), the
    /// arm signal must still be processed even though the pump isn't
    /// currently at the read select. Without `await_with_arm_or_timeout` the
    /// pump would stay in phase 1 forever and the deadline would never apply.
    ///
    /// The `WriteStallProbe` wrapper notifies the test the *moment* pump's
    /// `poll_write` returns Pending for the first time, replacing the prior
    /// `sleep(50ms)` sync that could miss the in-progress write on slow CI.
    #[tokio::test]
    async fn arm_signal_during_phase1_write_still_bounds_stall() {
        let (a_outer, a_inner) = duplex(8);
        let (b_outer, b_inner) = duplex(8);

        // Wrap A's inner end so we can observe when the forwarder's pump
        // hits backpressure on the A-write side.
        let (a_probe, write_pending) = WriteStallProbe::new(a_inner);

        let fwd = tokio::spawn(forward_bidirectional_with_idle_timeout(
            a_probe,
            b_inner,
            Some(Duration::from_millis(200)),
        ));

        let (_a_r, mut a_w) = tokio::io::split(a_outer);
        let (_b_r, mut b_w) = tokio::io::split(b_outer);

        // B starts streaming first. Forwarder's pump_b_to_a reads + writes
        // to A. A's _a_r doesn't drain, so write_all stalls in phase 1.
        let producer = tokio::spawn(async move {
            let _ = b_w.write_all(&[0u8; 4096]).await;
        });

        // Wait *deterministically* until the forwarder is wedged inside
        // `write_all` on A. Cap the wait so a regression that prevents the
        // pump from ever reaching write_all fails the test cleanly.
        timeout(Duration::from_secs(2), write_pending.notified())
            .await
            .expect("forwarder never entered write_all on A");

        // Now A half-closes — pump_a_to_b returns Ok, parent arms idle on
        // pump_b_to_a. pump_b_to_a is currently inside write_all; the new
        // helper must propagate the arm signal mid-write.
        a_w.shutdown().await.unwrap();

        let res = timeout(Duration::from_secs(2), fwd)
            .await
            .expect("forwarder did not return — arm signal lost during write_all")
            .unwrap();
        let err = res.expect_err("expected TimedOut after mid-write arm");
        assert!(
            is_post_half_close_idle_timeout(&err),
            "expected the leak-guard sentinel, got {err}"
        );
        producer.abort();
    }

    /// Round-3 regression: a transport whose `poll_shutdown` flushes protocol
    /// bytes (TLS close_notify, WebSocket Close frame, …) can stall on an
    /// unresponsive peer. Phase-1 EOF used to call `writer.shutdown()`
    /// without any deadline, so the pump would hang in there before the
    /// sibling could be armed. `shutdown_with_optional_timeout` must bound it
    /// by `configured_idle`.
    #[tokio::test]
    async fn eof_shutdown_is_bounded_by_configured_idle() {
        let (a_outer, a_inner) = duplex(64);
        let (_b_outer, b_inner) = duplex(64);

        // B's inner side has a `poll_shutdown` that never completes. Pump
        // a_to_b reads from a_inner, and on EOF calls `b_inner.shutdown()`
        // — that's the call that must time out.
        let b_inner = PendingShutdown::new(b_inner);

        let fwd = tokio::spawn(forward_bidirectional_with_idle_timeout(
            a_inner,
            b_inner,
            Some(Duration::from_millis(150)),
        ));

        let (_a_r, mut a_w) = tokio::io::split(a_outer);
        // Trigger EOF on A → pump_a_to_b reads 0 → calls b_inner.shutdown()
        // which is the stuck path.
        a_w.shutdown().await.unwrap();

        let res = timeout(Duration::from_secs(2), fwd)
            .await
            .expect("forwarder did not return — EOF shutdown was unbounded")
            .unwrap();
        let err = res.expect_err("expected TimedOut on stuck shutdown");
        assert!(
            is_post_half_close_idle_timeout(&err),
            "expected the leak-guard sentinel, got {err}"
        );
    }

    /// Phase-2 idle: side A half-closes, side B never writes, never closes.
    /// The forwarder must return TimedOut once `idle` elapses.
    #[tokio::test]
    async fn stuck_peer_after_half_close_times_out() {
        let (a_outer, a_inner) = duplex(64);
        let (_b_outer, b_inner) = duplex(64);

        let fwd = tokio::spawn(forward_bidirectional_with_idle_timeout(
            a_inner,
            b_inner,
            Some(Duration::from_millis(150)),
        ));

        let (_a_r, mut a_w) = tokio::io::split(a_outer);
        a_w.write_all(b"hi").await.unwrap();
        a_w.shutdown().await.unwrap();

        let res = timeout(Duration::from_secs(2), fwd)
            .await
            .expect("forwarder did not return")
            .unwrap();
        let err = res.expect_err("expected TimedOut");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    /// `idle = None` must restore the legacy `copy_bidirectional` behavior:
    /// after one side half-closes the surviving direction may sleep
    /// indefinitely, so the forwarder does NOT terminate on its own.
    #[tokio::test]
    async fn idle_none_disables_phase2_timeout() {
        let (a_outer, a_inner) = duplex(64);
        let (_b_outer, b_inner) = duplex(64);

        let fwd = tokio::spawn(forward_bidirectional_with_idle_timeout(
            a_inner, b_inner, None,
        ));

        let (_a_r, mut a_w) = tokio::io::split(a_outer);
        a_w.shutdown().await.unwrap();

        // Without an idle timeout, the forwarder should still be running.
        let still_running = timeout(Duration::from_millis(300), fwd).await;
        assert!(
            still_running.is_err(),
            "forwarder must not return when idle=None and the peer is silent"
        );
    }

    /// Bidirectional steady traffic must keep flowing as long as both sides
    /// make progress within `idle`, even if the interval is short.
    #[tokio::test]
    async fn steady_traffic_survives_short_idle() {
        let (a_outer, a_inner) = duplex(64);
        let (b_outer, b_inner) = duplex(64);

        let fwd = tokio::spawn(forward_bidirectional_with_idle_timeout(
            a_inner,
            b_inner,
            Some(Duration::from_millis(200)),
        ));

        let (mut a_r, mut a_w) = tokio::io::split(a_outer);
        let (mut b_r, mut b_w) = tokio::io::split(b_outer);

        // 5 round-trips spaced under the 200ms deadline.
        for i in 0..5u8 {
            a_w.write_all(&[i]).await.unwrap();
            let mut got = [0u8; 1];
            b_r.read_exact(&mut got).await.unwrap();
            assert_eq!(got[0], i);
            b_w.write_all(&[i ^ 0xff]).await.unwrap();
            a_r.read_exact(&mut got).await.unwrap();
            assert_eq!(got[0], i ^ 0xff);
            tokio::time::sleep(Duration::from_millis(80)).await;
        }

        // Close both sides cleanly.
        a_w.shutdown().await.unwrap();
        b_w.shutdown().await.unwrap();

        timeout(Duration::from_secs(2), fwd)
            .await
            .expect("forwarder hung")
            .unwrap()
            .unwrap();
    }
}
