//! Event streams related functionality.

use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio_stream::Stream;
use tracing::warn;

/// Thin wrapper around tokio's `BroadcastStream` to allow skipping broadcast errors.
#[derive(Debug)]
pub struct EventStream<T> {
    inner: tokio_stream::wrappers::BroadcastStream<T>,
}

impl<T> EventStream<T>
where
    T: Clone + Send + 'static,
{
    /// Creates a new `EventStream`.
    pub fn new(receiver: tokio::sync::broadcast::Receiver<T>) -> Self {
        let inner = tokio_stream::wrappers::BroadcastStream::new(receiver);
        Self { inner }
    }
}

impl<T> Stream for EventStream<T>
where
    T: Clone + Send + 'static,
{
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(item))) => return Poll::Ready(Some(item)),
                Poll::Ready(Some(Err(e))) => {
                    warn!("BroadcastStream lagged: {e:?}");
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
