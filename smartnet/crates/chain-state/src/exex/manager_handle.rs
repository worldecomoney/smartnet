//! No-op [`ExExManagerHandle`] when `SmartNet` runs without execution extensions (EX1).

use super::{ExExNotification, FinishedExExHeight};
use smartnet_primitives_traits::NodePrimitives;
use std::{
    marker::PhantomData,
    task::{Context, Poll},
};
use tokio::sync::watch;

/// Inert handle used when no `ExExs` are installed (always the case in `SmartNet` production).
#[derive(Debug)]
pub struct ExExManagerHandle<N: NodePrimitives> {
    finished_height: watch::Receiver<FinishedExExHeight>,
    _phantom: PhantomData<N>,
}

impl<N: NodePrimitives> ExExManagerHandle<N> {
    /// Handle with no extensions; always ready, never buffers notifications.
    pub fn empty() -> Self {
        let (_, finished_height) = watch::channel(FinishedExExHeight::NoExExs);
        Self { finished_height, _phantom: PhantomData }
    }

    /// Always `false` for [`Self::empty`].
    pub const fn has_exexs(&self) -> bool {
        false
    }

    /// Pruner uses this; [`FinishedExExHeight::NoExExs`] when no `ExExs` run.
    pub fn finished_height(&self) -> watch::Receiver<FinishedExExHeight> {
        self.finished_height.clone()
    }

    /// Always ready — no manager to backpressure.
    pub const fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<()> {
        Poll::Ready(())
    }

    /// No-op when no `ExExs` are installed.
    pub fn send(
        &self,
        _source: super::ExExNotificationSource,
        _notification: ExExNotification<N>,
    ) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

impl<N: NodePrimitives> Clone for ExExManagerHandle<N> {
    fn clone(&self) -> Self {
        Self { finished_height: self.finished_height.clone(), _phantom: PhantomData }
    }
}
