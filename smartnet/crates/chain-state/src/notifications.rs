//! Canonical chain state notification trait and types.

use alloy_eips::{eip2718::Encodable2718, BlockNumHash};
use derive_more::{Deref, DerefMut};
use smartnet_execution_types::{BlockReceipts, Chain};
use smartnet_primitives_traits::{NodePrimitives, RecoveredBlock, SealedHeader};
use smartnet_storage_api::NodePrimitivesProvider;
use std::{
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
};
use tokio::sync::{broadcast, watch};
use tokio_stream::{
    wrappers::{BroadcastStream, WatchStream},
    Stream,
};
use tracing::debug;

/// Type alias for a receiver that receives [`CanonStateNotification`]
pub type CanonStateNotifications<N = smartnet_ethereum_primitives::EthPrimitives> =
    broadcast::Receiver<CanonStateNotification<N>>;

/// Type alias for a sender that sends [`CanonStateNotification`]
pub type CanonStateNotificationSender<N = smartnet_ethereum_primitives::EthPrimitives> =
    broadcast::Sender<CanonStateNotification<N>>;

/// A type that allows to register chain related event subscriptions.
pub trait CanonStateSubscriptions: NodePrimitivesProvider + Send + Sync {
    /// Get notified when a new canonical chain was imported.
    ///
    /// A canonical chain be one or more blocks, a reorg or a revert.
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications<Self::Primitives>;

    /// Convenience method to get a stream of [`CanonStateNotification`].
    fn canonical_state_stream(&self) -> CanonStateNotificationStream<Self::Primitives> {
        CanonStateNotificationStream {
            st: BroadcastStream::new(self.subscribe_to_canonical_state()),
        }
    }
}

impl<T: CanonStateSubscriptions> CanonStateSubscriptions for &T {
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications<Self::Primitives> {
        (*self).subscribe_to_canonical_state()
    }

    fn canonical_state_stream(&self) -> CanonStateNotificationStream<Self::Primitives> {
        (*self).canonical_state_stream()
    }
}

/// A Stream of [`CanonStateNotification`].
#[derive(Debug)]
#[pin_project::pin_project]
pub struct CanonStateNotificationStream<N: NodePrimitives = smartnet_ethereum_primitives::EthPrimitives>
{
    #[pin]
    st: BroadcastStream<CanonStateNotification<N>>,
}

impl<N: NodePrimitives> Stream for CanonStateNotificationStream<N> {
    type Item = CanonStateNotification<N>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            return match ready!(self.as_mut().project().st.poll_next(cx)) {
                Some(Ok(notification)) => Poll::Ready(Some(notification)),
                Some(Err(err)) => {
                    debug!(%err, "canonical state notification stream lagging behind");
                    continue;
                }
                None => Poll::Ready(None),
            };
        }
    }
}

/// A notification that is sent when a new block is imported, or an old block is reverted.
///
/// The notification contains at least one [`Chain`] with the imported segment. If some blocks were
/// reverted (e.g. during a reorg), the old chain is also returned.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(bound = ""))]
pub enum CanonStateNotification<N: NodePrimitives = smartnet_ethereum_primitives::EthPrimitives> {
    /// The canonical chain was extended.
    Commit {
        /// The newly added chain segment.
        new: Arc<Chain<N>>,
    },
    /// A chain segment was reverted or reorged.
    ///
    /// - In the case of a reorg, the reverted blocks are present in `old`, and the new blocks are
    ///   present in `new`.
    /// - In the case of a revert, the reverted blocks are present in `old`, and `new` is an empty
    ///   chain segment.
    Reorg {
        /// The chain segment that was reverted.
        old: Arc<Chain<N>>,
        /// The chain segment that was added on top of the canonical chain, minus the reverted
        /// blocks.
        ///
        /// In the case of a revert, not a reorg, this chain segment is empty.
        new: Arc<Chain<N>>,
    },
}

impl<N: NodePrimitives> CanonStateNotification<N> {
    /// Get the chain segment that was reverted, if any.
    pub fn reverted(&self) -> Option<Arc<Chain<N>>> {
        match self {
            Self::Commit { .. } => None,
            Self::Reorg { old, .. } => Some(old.clone()),
        }
    }

    /// Get the newly imported chain segment, if any.
    pub fn committed(&self) -> Arc<Chain<N>> {
        match self {
            Self::Commit { new } | Self::Reorg { new, .. } => new.clone(),
        }
    }

    /// Gets the new tip of the chain.
    ///
    /// Returns the new tip for [`Self::Reorg`] and [`Self::Commit`] variants which commit at least
    /// 1 new block.
    ///
    /// # Panics
    ///
    /// If chain doesn't have any blocks.
    pub fn tip(&self) -> &RecoveredBlock<N::Block> {
        match self {
            Self::Commit { new } | Self::Reorg { new, .. } => new.tip(),
        }
    }

    /// Gets the new tip of the chain.
    ///
    /// If the chain has no blocks, it returns `None`. Otherwise, it returns the new tip for
    /// [`Self::Reorg`] and [`Self::Commit`] variants.
    pub fn tip_checked(&self) -> Option<&RecoveredBlock<N::Block>> {
        match self {
            Self::Commit { new } | Self::Reorg { new, .. } => {
                if new.is_empty() {
                    None
                } else {
                    Some(new.tip())
                }
            }
        }
    }

    /// Get receipts in the reverted and newly imported chain segments with their corresponding
    /// block numbers and transaction hashes.
    ///
    /// The boolean in the tuple (2nd element) denotes whether the receipt was from the reverted
    /// chain segment.
    pub fn block_receipts(&self) -> Vec<(BlockReceipts<N::Receipt>, bool)>
    where
        N::SignedTx: Encodable2718,
    {
        let mut receipts = Vec::new();

        // get old receipts
        if let Some(old) = self.reverted() {
            receipts
                .extend(old.receipts_with_attachment().into_iter().map(|receipt| (receipt, true)));
        }
        // get new receipts
        receipts.extend(
            self.committed().receipts_with_attachment().into_iter().map(|receipt| (receipt, false)),
        );
        receipts
    }
}

/// Wrapper around a broadcast receiver that receives fork choice notifications.
#[derive(Debug, Deref, DerefMut)]
pub struct ForkChoiceNotifications<T = alloy_consensus::Header>(
    pub watch::Receiver<Option<SealedHeader<T>>>,
);

/// A trait that allows to register to fork choice related events
/// and get notified when a new fork choice is available.
pub trait ForkChoiceSubscriptions: Send + Sync {
    /// Block Header type.
    type Header: Clone + Send + Sync + 'static;

    /// Get notified when a new safe block of the chain is selected.
    fn subscribe_safe_block(&self) -> ForkChoiceNotifications<Self::Header>;

    /// Get notified when a new finalized block of the chain is selected.
    fn subscribe_finalized_block(&self) -> ForkChoiceNotifications<Self::Header>;

    /// Convenience method to get a stream of the new safe blocks of the chain.
    fn safe_block_stream(&self) -> ForkChoiceStream<SealedHeader<Self::Header>> {
        ForkChoiceStream::new(self.subscribe_safe_block().0)
    }

    /// Convenience method to get a stream of the new finalized blocks of the chain.
    fn finalized_block_stream(&self) -> ForkChoiceStream<SealedHeader<Self::Header>> {
        ForkChoiceStream::new(self.subscribe_finalized_block().0)
    }
}

/// A stream that yields values from a `watch::Receiver<Option<T>>`, filtering out `None` values.
#[derive(Debug)]
#[pin_project::pin_project]
pub struct WatchValueStream<T> {
    #[pin]
    st: WatchStream<Option<T>>,
}

impl<T: Clone + Sync + Send + 'static> WatchValueStream<T> {
    /// Creates a new [`WatchValueStream`]
    pub fn new(rx: watch::Receiver<Option<T>>) -> Self {
        Self { st: WatchStream::from_changes(rx) }
    }
}

impl<T: Clone + Sync + Send + 'static> Stream for WatchValueStream<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match ready!(self.as_mut().project().st.poll_next(cx)) {
                Some(Some(notification)) => return Poll::Ready(Some(notification)),
                Some(None) => {}
                None => return Poll::Ready(None),
            }
        }
    }
}

/// Alias for [`WatchValueStream`] for fork choice watch channels.
pub type ForkChoiceStream<T> = WatchValueStream<T>;

/// Wrapper around a watch receiver that receives persisted block notifications.
#[derive(Debug, Deref, DerefMut)]
pub struct PersistedBlockNotifications(pub watch::Receiver<Option<BlockNumHash>>);

/// A trait that allows subscribing to persisted block events.
pub trait PersistedBlockSubscriptions: Send + Sync {
    /// Get notified when a new block is persisted to disk.
    fn subscribe_persisted_block(&self) -> PersistedBlockNotifications;

    /// Convenience method to get a stream of the persisted blocks.
    fn persisted_block_stream(&self) -> WatchValueStream<BlockNumHash> {
        WatchValueStream::new(self.subscribe_persisted_block().0)
    }
}
