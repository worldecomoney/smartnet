//! Pre-authentication read budget — hard byte cap on a connection until its Layer-2
//! `HELLO` is verified.
//!
//! The mesh authenticates with a signed `HELLO_PING`/`HELLO_PONG` challenge, but the
//! `WebSocket` layer happily buffers an entire inbound message (up to
//! `Limits::max_message_bytes`, 32 MiB by default) *before* the handshake code ever
//! sees it. An unauthenticated stranger could therefore push the full frame cap on
//! every accepted socket. This module closes that gap: the raw stream is wrapped in
//! [`PreAuthGuard`], which refuses to read more than [`PREAUTH_BUDGET`] total bytes
//! until [`arm`] is called — exactly once, after a positive `HELLO` verification.
//!
//! The budget covers the HTTP `WebSocket` upgrade (≈1–2 KB) plus the fixed 184-byte
//! `HELLO_PING` frame, with margin. An exhausted budget surfaces as an I/O error, the
//! handshake/accept fails, and the connection is dropped — the same path as any other
//! rejected peer. Arm-on-success only: a failed handshake never lifts the cap.

use std::io;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Bytes a connection may read before its Layer-2 `HELLO` is verified: the `WebSocket`
/// upgrade request plus the fixed 184-byte `HELLO_PING` frame, with margin.
pub const PREAUTH_BUDGET: u64 = 16 * 1024;

/// Armed value — the connection authenticated; reads are unlimited from here on.
pub const ARMED: u64 = u64::MAX;

/// Shared remaining-read budget for one connection ([`ARMED`] = authenticated).
pub type Budget = Arc<AtomicU64>;

/// Fresh restricted budget for a new (unauthenticated) connection.
pub fn restricted() -> Budget {
    Arc::new(AtomicU64::new(PREAUTH_BUDGET))
}

/// Lift the cap — call exactly once, after a positive `HELLO` verification.
pub fn arm(budget: &Budget) {
    budget.store(ARMED, Ordering::Release);
}

/// `AsyncRead` wrapper that hard-stops a connection once its [`Budget`] is spent.
///
/// Reads are limited to the remaining budget each poll, so the cap is a true ceiling
/// on bytes pulled from the socket — not just on messages handed upward. Writes pass
/// through untouched (our own replies are never the `DoS` surface).
#[derive(Debug)]
pub struct PreAuthGuard<S> {
    inner: S,
    budget: Budget,
}

impl<S> PreAuthGuard<S> {
    /// Wraps `inner` with the shared `budget` (start with [`restricted`]).
    pub const fn new(inner: S, budget: Budget) -> Self {
        Self { inner, budget }
    }

    /// Authenticated: lift the budget this guard shares with the accept/dial path.
    pub fn arm(&self) {
        arm(&self.budget);
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PreAuthGuard<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let remaining = this.budget.load(Ordering::Acquire);
        if remaining == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "mesh pre-auth read budget exhausted",
            )));
        }
        let cap = usize::try_from(remaining).unwrap_or(usize::MAX).min(buf.remaining());
        let n = {
            let unfilled = buf.initialize_unfilled();
            let mut sub = ReadBuf::new(&mut unfilled[..cap]);
            match Pin::new(&mut this.inner).poll_read(cx, &mut sub) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Ready(Ok(())) => sub.filled().len(),
            }
        };
        if n > 0 && remaining != ARMED {
            this.budget.fetch_sub(n as u64, Ordering::AcqRel);
        }
        buf.advance(n);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PreAuthGuard<S> {
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

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll, Waker};

    fn noop_cx() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    #[test]
    fn budget_caps_reads_then_errors() {
        const DATA: &[u8] = b"0123456789abcdef"; // 16 bytes
        let budget = Arc::new(AtomicU64::new(4));
        let mut guard = PreAuthGuard::new(DATA, budget.clone());
        let mut cx = noop_cx();
        let mut buf = [0u8; 64];

        // First read succeeds up to the budget…
        let mut read_buf = ReadBuf::new(&mut buf);
        assert!(matches!(
            Pin::new(&mut guard).poll_read(&mut cx, &mut read_buf),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(read_buf.filled().len(), 4);
        assert_eq!(budget.load(Ordering::Acquire), 0);

        // …then the connection is cut.
        let mut read_buf = ReadBuf::new(&mut buf);
        match Pin::new(&mut guard).poll_read(&mut cx, &mut read_buf) {
            Poll::Ready(Err(err)) => assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted),
            other => panic!("expected budget-exhausted error, got {other:?}"),
        }
    }

    #[test]
    fn armed_budget_passes_through() {
        const DATA: &[u8] = b"0123456789abcdef";
        let mut guard = PreAuthGuard::new(DATA, restricted());
        guard.arm();
        let mut cx = noop_cx();
        let mut buf = [0u8; 64];
        let mut out = Vec::new();
        loop {
            let mut read_buf = ReadBuf::new(&mut buf);
            match Pin::new(&mut guard).poll_read(&mut cx, &mut read_buf) {
                Poll::Ready(Ok(())) => {
                    if read_buf.filled().is_empty() {
                        break;
                    }
                    out.extend_from_slice(read_buf.filled());
                }
                other => panic!("armed read failed: {other:?}"),
            }
        }
        assert_eq!(out, DATA);
    }

    #[test]
    fn restricted_budget_starts_at_preauth_cap() {
        assert_eq!(restricted().load(Ordering::Acquire), PREAUTH_BUDGET);
        assert_ne!(PREAUTH_BUDGET, ARMED);
    }
}
