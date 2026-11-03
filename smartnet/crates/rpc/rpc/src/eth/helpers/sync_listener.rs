//! A utility Future to asynchronously wait until a node has finished syncing.

use futures::Stream;
use pin_project::pin_project;
use smartnet_network_api::NetworkInfo;
use std::{
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll},
};

/// This future resolves once the node is no longer syncing: [`NetworkInfo::is_syncing`].
#[must_use = "futures do nothing unless polled"]
#[pin_project]
#[derive(Debug)]
pub struct SyncListener<N, St> {
    #[pin]
    tick: St,
    network_info: N,
}

impl<N, St> SyncListener<N, St> {
    /// Create a new [`SyncListener`] using the given tick stream.
    pub const fn new(network_info: N, tick: St) -> Self {
        Self { tick, network_info }
    }
}

impl<N, St> Future for SyncListener<N, St>
where
    N: NetworkInfo,
    St: Stream + Unpin,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        if !this.network_info.is_syncing() {
            return Poll::Ready(());
        }

        loop {
            let tick_event = ready!(this.tick.as_mut().poll_next(cx));

            match tick_event {
                Some(_) => {
                    if !this.network_info.is_syncing() {
                        return Poll::Ready(());
                    }
                }
                None => return Poll::Ready(()),
            }
        }
    }
}
