use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumHash;
use alloy_primitives::BlockNumber;
use parking_lot::RwLock;
use smartnet_primitives_traits::{NodePrimitives, SealedHeader};
use smartnet_chainspec::ChainInfo;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::sync::watch;

/// Tracks the chain info: canonical head, safe block, finalized block.
#[derive(Debug, Clone)]
pub struct ChainInfoTracker<N: NodePrimitives> {
    inner: Arc<ChainInfoInner<N>>,
}

impl<N> ChainInfoTracker<N>
where
    N: NodePrimitives,
    N::BlockHeader: BlockHeader,
{
    /// Create a new chain info container for the given canonical head and finalized header if it
    /// exists.
    pub fn new(
        head: SealedHeader<N::BlockHeader>,
        finalized: Option<SealedHeader<N::BlockHeader>>,
        safe: Option<SealedHeader<N::BlockHeader>>,
    ) -> Self {
        let (finalized_block, _) = watch::channel(finalized);
        let (safe_block, _) = watch::channel(safe);
        let (persisted_block, _) = watch::channel(None);

        Self {
            inner: Arc::new(ChainInfoInner {
                last_forkchoice_update: RwLock::new(None),

                canonical_head_number: AtomicU64::new(head.number()),
                canonical_head: RwLock::new(head),
                safe_block,
                finalized_block,
                persisted_block,
            }),
        }
    }

    /// Returns the [`ChainInfo`] for the canonical head.
    pub fn chain_info(&self) -> ChainInfo {
        let inner = self.inner.canonical_head.read();
        ChainInfo { best_hash: inner.hash(), best_number: inner.number() }
    }

    /// Update the timestamp when we received a forkchoice update.
    pub fn on_forkchoice_update_received(&self) {
        self.inner.last_forkchoice_update.write().replace(Instant::now());
    }

    /// Returns the instant when we received the latest forkchoice update.
    pub fn last_forkchoice_update_received_at(&self) -> Option<Instant> {
        *self.inner.last_forkchoice_update.read()
    }

    /// Returns the canonical head of the chain.
    pub fn get_canonical_head(&self) -> SealedHeader<N::BlockHeader> {
        self.inner.canonical_head.read().clone()
    }

    /// Returns the safe header of the chain.
    pub fn get_safe_header(&self) -> Option<SealedHeader<N::BlockHeader>> {
        self.inner.safe_block.borrow().clone()
    }

    /// Returns the finalized header of the chain.
    pub fn get_finalized_header(&self) -> Option<SealedHeader<N::BlockHeader>> {
        self.inner.finalized_block.borrow().clone()
    }

    /// Returns the `BlockNumHash` of the canonical head.
    pub fn get_canonical_num_hash(&self) -> BlockNumHash {
        self.inner.canonical_head.read().num_hash()
    }

    /// Returns the block number of the canonical head.
    pub fn get_canonical_block_number(&self) -> BlockNumber {
        self.inner.canonical_head_number.load(Ordering::Relaxed)
    }

    /// Returns the `BlockNumHash` of the safe header.
    pub fn get_safe_num_hash(&self) -> Option<BlockNumHash> {
        self.inner.safe_block.borrow().as_ref().map(SealedHeader::num_hash)
    }

    /// Returns the `BlockNumHash` of the finalized header.
    pub fn get_finalized_num_hash(&self) -> Option<BlockNumHash> {
        self.inner.finalized_block.borrow().as_ref().map(SealedHeader::num_hash)
    }

    /// Returns the `BlockNumHash` of the persisted block.
    pub fn get_persisted_num_hash(&self) -> Option<BlockNumHash> {
        *self.inner.persisted_block.borrow()
    }

    /// Sets the canonical head of the chain.
    pub fn set_canonical_head(&self, header: SealedHeader<N::BlockHeader>) {
        let number = header.number();
        *self.inner.canonical_head.write() = header;

        // also update the atomic number.
        self.inner.canonical_head_number.store(number, Ordering::Relaxed);
    }

    /// Sets the safe header of the chain.
    pub fn set_safe(&self, header: SealedHeader<N::BlockHeader>) {
        self.inner.safe_block.send_if_modified(|current_header| {
            if current_header.as_ref().map(SealedHeader::hash) != Some(header.hash()) {
                let _ = current_header.replace(header);
                return true;
            }

            false
        });
    }

    /// Sets the finalized header of the chain.
    pub fn set_finalized(&self, header: SealedHeader<N::BlockHeader>) {
        self.inner.finalized_block.send_if_modified(|current_header| {
            if current_header.as_ref().map(SealedHeader::hash) != Some(header.hash()) {
                let _ = current_header.replace(header);
                return true;
            }

            false
        });
    }

    /// Sets the persisted block of the chain.
    pub fn set_persisted(&self, num_hash: BlockNumHash) {
        self.inner.persisted_block.send_if_modified(|current| {
            if current.map(|b| b.hash) != Some(num_hash.hash) {
                let _ = current.replace(num_hash);
                return true;
            }

            false
        });
    }

    /// Subscribe to the finalized block.
    pub fn subscribe_finalized_block(
        &self,
    ) -> watch::Receiver<Option<SealedHeader<N::BlockHeader>>> {
        self.inner.finalized_block.subscribe()
    }

    /// Subscribe to the safe block.
    pub fn subscribe_safe_block(&self) -> watch::Receiver<Option<SealedHeader<N::BlockHeader>>> {
        self.inner.safe_block.subscribe()
    }

    /// Subscribe to the persisted block.
    pub fn subscribe_persisted_block(&self) -> watch::Receiver<Option<BlockNumHash>> {
        self.inner.persisted_block.subscribe()
    }
}

/// Container type for all chain info fields
#[derive(Debug)]
struct ChainInfoInner<N: NodePrimitives = smartnet_ethereum_primitives::EthPrimitives> {
    /// Timestamp when we received the last fork choice update.
    ///
    /// This is mainly used to track if we're connected to a beacon node.
    last_forkchoice_update: RwLock<Option<Instant>>,

    /// Tracks the number of the `canonical_head`.
    canonical_head_number: AtomicU64,
    /// The canonical head of the chain.
    canonical_head: RwLock<SealedHeader<N::BlockHeader>>,
    /// The block that the beacon node considers safe.
    safe_block: watch::Sender<Option<SealedHeader<N::BlockHeader>>>,
    /// The block that the beacon node considers finalized.
    finalized_block: watch::Sender<Option<SealedHeader<N::BlockHeader>>>,
    /// The last block that was persisted to disk.
    persisted_block: watch::Sender<Option<BlockNumHash>>,
}
