//! Types for tracking the canonical chain state in memory.

use crate::{
    CanonStateNotification, CanonStateNotificationSender, CanonStateNotifications,
    ChainInfoTracker, ComputedTrieData, DeferredTrieData, MemoryOverlayStateProvider,
};
use alloy_consensus::{transaction::TransactionMeta, BlockHeader};
use alloy_eips::{BlockHashOrNumber, BlockNumHash};
use alloy_primitives::{map::B256Map, Address, BlockNumber, TxHash, B256};
use parking_lot::RwLock;
use smartnet_ethereum_primitives::EthPrimitives;
use smartnet_execution_types::{BlockExecutionOutput, BlockExecutionResult, Chain, ExecutionOutcome};
use smartnet_primitives_traits::{
    BlockBody as _, IndexedTx, NodePrimitives, RecoveredBlock, SealedBlock, SealedHeader,
    SignedTransaction, TokenMeta,
};
use smartnet_trie::{
    updates::TrieUpdatesSorted, HashedPostStateSorted, LazyTrieData, SortedTrieData,
    TrieInputSorted,
};
use smartnet_chainspec::ChainInfo;
use smartnet_metrics::{metrics::Gauge, Metrics};
use smartnet_storage_api::StateProviderBox;
use std::{collections::BTreeMap, sync::Arc, time::Instant};
use tokio::sync::{broadcast, watch};

/// Size of the broadcast channel used to notify canonical state events.
const CANON_STATE_NOTIFICATION_CHANNEL_SIZE: usize = 256;

/// Metrics for the in-memory state.
#[derive(Metrics)]
#[metrics(scope = "blockchain_tree.in_mem_state")]
pub(crate) struct InMemoryStateMetrics {
    /// The block number of the earliest block in the in-memory state.
    pub(crate) earliest_block: Gauge,
    /// The block number of the latest block in the in-memory state.
    pub(crate) latest_block: Gauge,
    /// The number of blocks in the in-memory state.
    pub(crate) num_blocks: Gauge,
}

/// Container type for in memory state data of the canonical chain.
///
/// This tracks blocks and their state that haven't been persisted to disk yet but are part of the
/// canonical chain that can be traced back to a canonical block on disk.
///
/// # Locking behavior on state updates
///
/// All update calls must acquire all locks at once before modifying state to ensure the internal
/// state remains consistent. This prevents readers from observing partially updated state where
/// the numbers and blocks maps are out of sync.
/// Update functions ensure that the numbers write lock is always acquired first, because lookup by
/// numbers first read the numbers map and then the blocks map.
/// By acquiring the numbers lock first, we ensure that read-only lookups don't deadlock updates.
/// This holds, because only lookup by number functions need to acquire the numbers lock first to
/// get the block hash.
#[derive(Debug, Default)]
pub(crate) struct InMemoryState<N: NodePrimitives = EthPrimitives> {
    /// All canonical blocks that are not on disk yet.
    blocks: RwLock<B256Map<Arc<BlockState<N>>>>,
    /// Mapping of block numbers to block hashes.
    numbers: RwLock<BTreeMap<u64, B256>>,
    /// The pending block that has not yet been made canonical.
    pending: watch::Sender<Option<BlockState<N>>>,
    /// Metrics for the in-memory state.
    metrics: InMemoryStateMetrics,
}

impl<N: NodePrimitives> InMemoryState<N> {
    pub(crate) fn new(
        blocks: B256Map<Arc<BlockState<N>>>,
        numbers: BTreeMap<u64, B256>,
        pending: Option<BlockState<N>>,
    ) -> Self {
        let (pending, _) = watch::channel(pending);
        let this = Self {
            blocks: RwLock::new(blocks),
            numbers: RwLock::new(numbers),
            pending,
            metrics: Default::default(),
        };
        this.update_metrics();
        this
    }

    /// Update the metrics for the in-memory state.
    ///
    /// # Locking behavior
    ///
    /// This tries to acquire a read lock. Drop any write locks before calling this.
    pub(crate) fn update_metrics(&self) {
        let (count, earliest, latest) = {
            let numbers = self.numbers.read();
            let count = numbers.len();
            let earliest = numbers.first_key_value().map(|(number, _)| *number);
            let latest = numbers.last_key_value().map(|(number, _)| *number);
            (count, earliest, latest)
        };
        if let Some(earliest_block_number) = earliest {
            self.metrics.earliest_block.set(earliest_block_number as f64);
        }
        if let Some(latest_block_number) = latest {
            self.metrics.latest_block.set(latest_block_number as f64);
        }
        self.metrics.num_blocks.set(count as f64);
    }

    /// Returns the state for a given block hash.
    pub(crate) fn state_by_hash(&self, hash: B256) -> Option<Arc<BlockState<N>>> {
        self.blocks.read().get(&hash).cloned()
    }

    /// Returns the state for a given block number.
    pub(crate) fn state_by_number(&self, number: u64) -> Option<Arc<BlockState<N>>> {
        let hash = self.hash_by_number(number)?;
        self.state_by_hash(hash)
    }

    /// Returns the hash for a specific block number
    pub(crate) fn hash_by_number(&self, number: u64) -> Option<B256> {
        self.numbers.read().get(&number).copied()
    }

    /// Returns the current chain head state.
    pub(crate) fn head_state(&self) -> Option<Arc<BlockState<N>>> {
        let hash = *self.numbers.read().last_key_value()?.1;
        self.state_by_hash(hash)
    }

    /// Returns the pending state corresponding to the current head plus one,
    /// from the payload received in newPayload that does not have a FCU yet.
    pub(crate) fn pending_state(&self) -> Option<BlockState<N>> {
        self.pending.borrow().clone()
    }
}

/// Inner type to provide in memory state. It includes a chain tracker to be
/// advanced internally by the tree.
#[derive(Debug)]
pub(crate) struct CanonicalInMemoryStateInner<N: NodePrimitives> {
    /// Tracks certain chain information, such as the canonical head, safe head, and finalized
    /// head.
    pub(crate) chain_info_tracker: ChainInfoTracker<N>,
    /// Tracks blocks at the tip of the chain that have not been persisted to disk yet.
    pub(crate) in_memory_state: InMemoryState<N>,
    /// A broadcast stream that emits events when the canonical chain is updated.
    pub(crate) canon_state_notification_sender: CanonStateNotificationSender<N>,
}

impl<N: NodePrimitives> CanonicalInMemoryStateInner<N> {
    /// Clears all entries in the in memory state.
    fn clear(&self) {
        {
            // acquire locks, starting with the numbers lock
            let mut numbers = self.in_memory_state.numbers.write();
            let mut blocks = self.in_memory_state.blocks.write();
            numbers.clear();
            blocks.clear();
            self.in_memory_state.pending.send_modify(|p| {
                p.take();
            });
        }
        self.in_memory_state.update_metrics();
    }
}

type PendingBlockAndReceipts<N> =
    (RecoveredBlock<<N as NodePrimitives>::Block>, Vec<smartnet_primitives_traits::ReceiptTy<N>>);

/// This type is responsible for providing the blocks, receipts, and state for
/// all canonical blocks not on disk yet and keeps track of the block range that
/// is in memory.
#[derive(Debug, Clone)]
pub struct CanonicalInMemoryState<N: NodePrimitives = EthPrimitives> {
    pub(crate) inner: Arc<CanonicalInMemoryStateInner<N>>,
}

impl<N: NodePrimitives> CanonicalInMemoryState<N> {
    /// Create a new in-memory state with the given blocks, numbers, pending state, and optional
    /// finalized header.
    pub fn new(
        blocks: B256Map<Arc<BlockState<N>>>,
        numbers: BTreeMap<u64, B256>,
        pending: Option<BlockState<N>>,
        finalized: Option<SealedHeader<N::BlockHeader>>,
        safe: Option<SealedHeader<N::BlockHeader>>,
    ) -> Self {
        let in_memory_state = InMemoryState::new(blocks, numbers, pending);
        let header = in_memory_state.head_state().map_or_else(SealedHeader::default, |state| {
            state.block_ref().recovered_block().clone_sealed_header()
        });
        let chain_info_tracker = ChainInfoTracker::new(header, finalized, safe);
        let (canon_state_notification_sender, _) =
            broadcast::channel(CANON_STATE_NOTIFICATION_CHANNEL_SIZE);

        Self {
            inner: Arc::new(CanonicalInMemoryStateInner {
                chain_info_tracker,
                in_memory_state,
                canon_state_notification_sender,
            }),
        }
    }

    /// Create an empty state.
    pub fn empty() -> Self {
        Self::new(B256Map::default(), BTreeMap::new(), None, None, None)
    }

    /// Create a new in memory state with the given local head and finalized header
    /// if it exists.
    pub fn with_head(
        head: SealedHeader<N::BlockHeader>,
        finalized: Option<SealedHeader<N::BlockHeader>>,
        safe: Option<SealedHeader<N::BlockHeader>>,
    ) -> Self {
        let chain_info_tracker = ChainInfoTracker::new(head, finalized, safe);
        let in_memory_state = InMemoryState::default();
        let (canon_state_notification_sender, _) =
            broadcast::channel(CANON_STATE_NOTIFICATION_CHANNEL_SIZE);
        let inner = CanonicalInMemoryStateInner {
            chain_info_tracker,
            in_memory_state,
            canon_state_notification_sender,
        };

        Self { inner: Arc::new(inner) }
    }

    /// Returns the block hash corresponding to the given number.
    pub fn hash_by_number(&self, number: u64) -> Option<B256> {
        self.inner.in_memory_state.hash_by_number(number)
    }

    /// Returns the header corresponding to the given hash.
    pub fn header_by_hash(&self, hash: B256) -> Option<SealedHeader<N::BlockHeader>> {
        self.state_by_hash(hash)
            .map(|block| block.block_ref().recovered_block().clone_sealed_header())
    }

    /// Clears all entries in the in memory state.
    pub fn clear_state(&self) {
        self.inner.clear()
    }

    /// Updates the pending block with the given block.
    ///
    /// Note: This assumes that the parent block of the pending block is canonical.
    pub fn set_pending_block(&self, pending: ExecutedBlock<N>) {
        // fetch the state of the pending block's parent block
        let parent = self.state_by_hash(pending.recovered_block().parent_hash());
        let pending = BlockState::with_parent(pending, parent);
        self.inner.in_memory_state.pending.send_modify(|p| {
            p.replace(pending);
        });
        self.inner.in_memory_state.update_metrics();
    }

    /// Append new blocks to the in memory state.
    ///
    /// This removes all reorged blocks and appends the new blocks to the tracked chain and connects
    /// them to their parent blocks.
    fn update_blocks<I, R>(&self, new_blocks: I, reorged: R)
    where
        I: IntoIterator<Item = ExecutedBlock<N>>,
        R: IntoIterator<Item = ExecutedBlock<N>>,
    {
        {
            // acquire locks, starting with the numbers lock
            let mut numbers = self.inner.in_memory_state.numbers.write();
            let mut blocks = self.inner.in_memory_state.blocks.write();

            // we first remove the blocks from the reorged chain
            for block in reorged {
                let hash = block.recovered_block().hash();
                let number = block.recovered_block().number();
                blocks.remove(&hash);
                numbers.remove(&number);
            }

            // insert the new blocks
            for block in new_blocks {
                let parent = blocks.get(&block.recovered_block().parent_hash()).cloned();
                let block_state = BlockState::with_parent(block, parent);
                let hash = block_state.hash();
                let number = block_state.number();

                // append new blocks
                blocks.insert(hash, Arc::new(block_state));
                numbers.insert(number, hash);
            }

            // remove the pending state
            self.inner.in_memory_state.pending.send_modify(|p| {
                p.take();
            });
        }
        self.inner.in_memory_state.update_metrics();
    }

    /// Update the in memory state with the given chain update.
    pub fn update_chain(&self, new_chain: NewCanonicalChain<N>) {
        match new_chain {
            NewCanonicalChain::Commit { new } => {
                self.update_blocks(new, vec![]);
            }
            NewCanonicalChain::Reorg { new, old } => {
                self.update_blocks(new, old);
            }
        }
    }

    /// Removes blocks from the in memory state that are persisted to the given height.
    ///
    /// This will update the links between blocks and remove all blocks that are [..
    /// `persisted_height`].
    pub fn remove_persisted_blocks(&self, persisted_num_hash: BlockNumHash) {
        self.set_persisted(persisted_num_hash);
        // if the persisted hash is not in the canonical in memory state, do nothing, because it
        // means canonical blocks were not actually persisted.
        //
        // This can happen if the persistence task takes a long time, while a reorg is happening.
        {
            if self.inner.in_memory_state.blocks.read().get(&persisted_num_hash.hash).is_none() {
                // do nothing
                return;
            }
        }

        {
            // acquire locks, starting with the numbers lock
            let mut numbers = self.inner.in_memory_state.numbers.write();
            let mut blocks = self.inner.in_memory_state.blocks.write();

            let BlockNumHash { number: persisted_height, hash: _ } = persisted_num_hash;

            // clear all numbers
            numbers.clear();

            // drain all blocks and only keep the ones that are not persisted (below the persisted
            // height)
            let mut old_blocks = blocks
                .drain()
                .filter(|(_, b)| b.block_ref().recovered_block().number() > persisted_height)
                .map(|(_, b)| b.block.clone())
                .collect::<Vec<_>>();

            // sort the blocks by number so we can insert them back in natural order (low -> high)
            old_blocks.sort_unstable_by_key(|block| block.recovered_block().number());

            // re-insert the blocks in natural order and connect them to their parent blocks
            for block in old_blocks {
                let parent = blocks.get(&block.recovered_block().parent_hash()).cloned();
                let block_state = BlockState::with_parent(block, parent);
                let hash = block_state.hash();
                let number = block_state.number();

                // append new blocks
                blocks.insert(hash, Arc::new(block_state));
                numbers.insert(number, hash);
            }

            // also shift the pending state if it exists
            self.inner.in_memory_state.pending.send_modify(|p| {
                if let Some(p) = p.as_mut() {
                    p.parent = blocks.get(&p.block_ref().recovered_block().parent_hash()).cloned();
                }
            });
        }
        self.inner.in_memory_state.update_metrics();
    }

    /// Returns in memory state corresponding the given hash.
    pub fn state_by_hash(&self, hash: B256) -> Option<Arc<BlockState<N>>> {
        self.inner.in_memory_state.state_by_hash(hash)
    }

    /// Returns in memory state corresponding the block number.
    pub fn state_by_number(&self, number: u64) -> Option<Arc<BlockState<N>>> {
        self.inner.in_memory_state.state_by_number(number)
    }

    /// Returns the in memory head state.
    pub fn head_state(&self) -> Option<Arc<BlockState<N>>> {
        self.inner.in_memory_state.head_state()
    }

    /// Returns the in memory pending state.
    pub fn pending_state(&self) -> Option<BlockState<N>> {
        self.inner.in_memory_state.pending_state()
    }

    /// Returns the in memory pending `BlockNumHash`.
    pub fn pending_block_num_hash(&self) -> Option<BlockNumHash> {
        self.inner
            .in_memory_state
            .pending_state()
            .map(|state| BlockNumHash { number: state.number(), hash: state.hash() })
    }

    /// Returns the current `ChainInfo`.
    pub fn chain_info(&self) -> ChainInfo {
        self.inner.chain_info_tracker.chain_info()
    }

    /// Returns the latest canonical block number.
    pub fn get_canonical_block_number(&self) -> u64 {
        self.inner.chain_info_tracker.get_canonical_block_number()
    }

    /// Returns the `BlockNumHash` of the safe head.
    pub fn get_safe_num_hash(&self) -> Option<BlockNumHash> {
        self.inner.chain_info_tracker.get_safe_num_hash()
    }

    /// Returns the `BlockNumHash` of the finalized head.
    pub fn get_finalized_num_hash(&self) -> Option<BlockNumHash> {
        self.inner.chain_info_tracker.get_finalized_num_hash()
    }

    /// Hook for new fork choice update.
    pub fn on_forkchoice_update_received(&self) {
        self.inner.chain_info_tracker.on_forkchoice_update_received();
    }

    /// Returns the timestamp of the last received update.
    pub fn last_received_update_timestamp(&self) -> Option<Instant> {
        self.inner.chain_info_tracker.last_forkchoice_update_received_at()
    }

    /// Canonical head setter.
    pub fn set_canonical_head(&self, header: SealedHeader<N::BlockHeader>) {
        self.inner.chain_info_tracker.set_canonical_head(header);
    }

    /// Safe head setter.
    pub fn set_safe(&self, header: SealedHeader<N::BlockHeader>) {
        self.inner.chain_info_tracker.set_safe(header);
    }

    /// Finalized head setter.
    pub fn set_finalized(&self, header: SealedHeader<N::BlockHeader>) {
        self.inner.chain_info_tracker.set_finalized(header);
    }

    /// Persisted block setter.
    pub fn set_persisted(&self, num_hash: BlockNumHash) {
        self.inner.chain_info_tracker.set_persisted(num_hash);
    }

    /// Canonical head getter.
    pub fn get_canonical_head(&self) -> SealedHeader<N::BlockHeader> {
        self.inner.chain_info_tracker.get_canonical_head()
    }

    /// Finalized header getter.
    pub fn get_finalized_header(&self) -> Option<SealedHeader<N::BlockHeader>> {
        self.inner.chain_info_tracker.get_finalized_header()
    }

    /// Safe header getter.
    pub fn get_safe_header(&self) -> Option<SealedHeader<N::BlockHeader>> {
        self.inner.chain_info_tracker.get_safe_header()
    }

    /// Persisted block `BlockNumHash` getter.
    pub fn get_persisted_num_hash(&self) -> Option<BlockNumHash> {
        self.inner.chain_info_tracker.get_persisted_num_hash()
    }

    /// Returns the `SealedHeader` corresponding to the pending state.
    pub fn pending_sealed_header(&self) -> Option<SealedHeader<N::BlockHeader>> {
        self.pending_state().map(|h| h.block_ref().recovered_block().clone_sealed_header())
    }

    /// Returns the `Header` corresponding to the pending state.
    pub fn pending_header(&self) -> Option<N::BlockHeader> {
        self.pending_sealed_header().map(|sealed_header| sealed_header.unseal())
    }

    /// Returns the `SealedBlock` corresponding to the pending state.
    pub fn pending_block(&self) -> Option<SealedBlock<N::Block>> {
        self.pending_state()
            .map(|block_state| block_state.block_ref().recovered_block().sealed_block().clone())
    }

    /// Returns the `RecoveredBlock` corresponding to the pending state.
    pub fn pending_recovered_block(&self) -> Option<RecoveredBlock<N::Block>>
    where
        N::SignedTx: SignedTransaction,
    {
        self.pending_state().map(|block_state| block_state.block_ref().recovered_block().clone())
    }

    /// Returns a tuple with the `SealedBlock` corresponding to the pending
    /// state and a vector of its `Receipt`s.
    pub fn pending_block_and_receipts(&self) -> Option<PendingBlockAndReceipts<N>> {
        self.pending_state().map(|block_state| {
            (
                block_state.block_ref().recovered_block().clone(),
                block_state.executed_block_receipts(),
            )
        })
    }

    /// Subscribe to new blocks events.
    pub fn subscribe_canon_state(&self) -> CanonStateNotifications<N> {
        self.inner.canon_state_notification_sender.subscribe()
    }

    /// Subscribe to new safe block events.
    pub fn subscribe_safe_block(&self) -> watch::Receiver<Option<SealedHeader<N::BlockHeader>>> {
        self.inner.chain_info_tracker.subscribe_safe_block()
    }

    /// Subscribe to new finalized block events.
    pub fn subscribe_finalized_block(
        &self,
    ) -> watch::Receiver<Option<SealedHeader<N::BlockHeader>>> {
        self.inner.chain_info_tracker.subscribe_finalized_block()
    }

    /// Subscribe to new persisted block events.
    pub fn subscribe_persisted_block(&self) -> watch::Receiver<Option<BlockNumHash>> {
        self.inner.chain_info_tracker.subscribe_persisted_block()
    }

    /// Attempts to send a new [`CanonStateNotification`] to all active Receiver handles.
    pub fn notify_canon_state(&self, event: CanonStateNotification<N>) {
        self.inner.canon_state_notification_sender.send(event).ok();
    }

    /// Return state provider with reference to in-memory blocks that overlay database state.
    ///
    /// This merges the state of all blocks that are part of the chain that the requested block is
    /// the head of. This includes all blocks that connect back to the canonical block on disk.
    pub fn state_provider(
        &self,
        hash: B256,
        historical: StateProviderBox,
    ) -> MemoryOverlayStateProvider<N> {
        let in_memory = if let Some(state) = self.state_by_hash(hash) {
            state.chain().map(|block_state| block_state.block()).collect()
        } else {
            Vec::new()
        };

        MemoryOverlayStateProvider::new(historical, in_memory)
    }

    /// Returns an iterator over all __canonical blocks__ in the in-memory state, from newest to
    /// oldest (highest to lowest).
    ///
    /// This iterator contains a snapshot of the in-memory state at the time of the call.
    pub fn canonical_chain(&self) -> impl Iterator<Item = Arc<BlockState<N>>> {
        self.inner.in_memory_state.head_state().into_iter().flat_map(|head| head.iter())
    }

    /// Returns [`SignedTransaction`] type for the given `TxHash` if found.
    pub fn transaction_by_hash(&self, hash: TxHash) -> Option<N::SignedTx> {
        for block_state in self.canonical_chain() {
            if let Some(tx) =
                block_state.block_ref().recovered_block().body().transaction_by_hash(&hash)
            {
                return Some(tx.clone());
            }
        }
        None
    }

    /// Returns a tuple with [`SignedTransaction`] type and [`TransactionMeta`] for the
    /// given [`TxHash`] if found.
    pub fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> Option<(N::SignedTx, TransactionMeta)> {
        for block_state in self.canonical_chain() {
            if let Some(indexed) = block_state.find_indexed(tx_hash) {
                return Some((indexed.tx().clone(), indexed.meta()));
            }
        }
        None
    }
}

/// State after applying the given block, this block is part of the canonical chain that partially
/// stored in memory and can be traced back to a canonical block on disk.
#[derive(Debug, Clone)]
pub struct BlockState<N: NodePrimitives = EthPrimitives> {
    /// The executed block that determines the state after this block has been executed.
    block: ExecutedBlock<N>,
    /// The block's parent block if it exists.
    parent: Option<Arc<Self>>,
}

impl<N: NodePrimitives> PartialEq for BlockState<N> {
    fn eq(&self, other: &Self) -> bool {
        self.block == other.block && self.parent == other.parent
    }
}

impl<N: NodePrimitives> BlockState<N> {
    /// [`BlockState`] constructor.
    pub const fn new(block: ExecutedBlock<N>) -> Self {
        Self { block, parent: None }
    }

    /// [`BlockState`] constructor with parent.
    pub const fn with_parent(block: ExecutedBlock<N>, parent: Option<Arc<Self>>) -> Self {
        Self { block, parent }
    }

    /// Returns the hash and block of the on disk block this state can be traced back to.
    pub fn anchor(&self) -> BlockNumHash {
        let mut current = self;
        while let Some(parent) = &current.parent {
            current = parent;
        }
        current.block.recovered_block().parent_num_hash()
    }

    /// Returns the executed block that determines the state.
    pub fn block(&self) -> ExecutedBlock<N> {
        self.block.clone()
    }

    /// Returns a reference to the executed block that determines the state.
    pub const fn block_ref(&self) -> &ExecutedBlock<N> {
        &self.block
    }

    /// Returns the hash of executed block that determines the state.
    pub fn hash(&self) -> B256 {
        self.block.recovered_block().hash()
    }

    /// Returns the block number of executed block that determines the state.
    pub fn number(&self) -> u64 {
        self.block.recovered_block().number()
    }

    /// Returns the state root after applying the executed block that determines
    /// the state.
    pub fn state_root(&self) -> B256 {
        self.block.recovered_block().state_root()
    }

    /// Returns the `Receipts` of executed block that determines the state.
    pub fn receipts(&self) -> &Vec<N::Receipt> {
        &self.block.execution_outcome().receipts
    }

    /// Returns a vector of `Receipt` of executed block that determines the state.
    /// We assume that the `Receipts` in the executed block `ExecutionOutcome`
    /// has only one element corresponding to the executed block associated to
    /// the state.
    ///
    /// This clones the vector of receipts. To avoid it, use [`Self::executed_block_receipts_ref`].
    pub fn executed_block_receipts(&self) -> Vec<N::Receipt> {
        self.receipts().clone()
    }

    /// Returns a slice of `Receipt` of executed block that determines the state.
    /// We assume that the `Receipts` in the executed block `ExecutionOutcome`
    /// has only one element corresponding to the executed block associated to
    /// the state.
    pub fn executed_block_receipts_ref(&self) -> &[N::Receipt] {
        self.receipts()
    }

    /// Returns an iterator over __parent__ `BlockStates`.
    ///
    /// The block state order is newest to oldest (highest to lowest):
    /// `[5,4,3,2,1]`
    ///
    /// Note: This does not include self.
    pub fn parent_state_chain(&self) -> impl Iterator<Item = &Self> + '_ {
        std::iter::successors(self.parent.as_deref(), |state| state.parent.as_deref())
    }

    /// Returns a vector of `BlockStates` representing the entire in memory chain.
    /// The block state order in the output vector is newest to oldest (highest to lowest),
    /// including self as the first element.
    pub fn chain(&self) -> impl Iterator<Item = &Self> {
        std::iter::successors(Some(self), |state| state.parent.as_deref())
    }

    /// Appends the parent chain of this [`BlockState`] to the given vector.
    ///
    /// Parents are appended in order from newest to oldest (highest to lowest).
    /// This does not include self, only the parent states.
    ///
    /// This is a convenience method equivalent to `chain.extend(self.parent_state_chain())`.
    pub fn append_parent_chain<'a>(&'a self, chain: &mut Vec<&'a Self>) {
        chain.extend(self.parent_state_chain());
    }

    /// Returns an iterator over the atomically captured chain of in memory blocks.
    ///
    /// This yields the blocks from newest to oldest (highest to lowest).
    pub fn iter(self: Arc<Self>) -> impl Iterator<Item = Arc<Self>> {
        std::iter::successors(Some(self), |state| state.parent.clone())
    }

    /// Return state provider with reference to in-memory blocks that overlay database state.
    ///
    /// This merges the state of all blocks that are part of the chain that the this block is
    /// the head of. This includes all blocks that connect back to the canonical block on disk.
    pub fn state_provider(&self, historical: StateProviderBox) -> MemoryOverlayStateProvider<N> {
        let in_memory = self.chain().map(|block_state| block_state.block()).collect();

        MemoryOverlayStateProvider::new(historical, in_memory)
    }

    /// Tries to find a block by [`BlockHashOrNumber`] in the chain ending at this block.
    pub fn block_on_chain(&self, hash_or_num: BlockHashOrNumber) -> Option<&Self> {
        self.chain().find(|block| match hash_or_num {
            BlockHashOrNumber::Hash(hash) => block.hash() == hash,
            BlockHashOrNumber::Number(number) => block.number() == number,
        })
    }

    /// Tries to find a transaction by [`TxHash`] in the chain ending at this block.
    pub fn transaction_on_chain(&self, hash: TxHash) -> Option<N::SignedTx> {
        self.chain().find_map(|block_state| {
            block_state.block_ref().recovered_block().body().transaction_by_hash(&hash).cloned()
        })
    }

    /// Tries to find a transaction with meta by [`TxHash`] in the chain ending at this block.
    pub fn transaction_meta_on_chain(
        &self,
        tx_hash: TxHash,
    ) -> Option<(N::SignedTx, TransactionMeta)> {
        self.chain().find_map(|block_state| {
            block_state.find_indexed(tx_hash).map(|indexed| (indexed.tx().clone(), indexed.meta()))
        })
    }

    /// Finds a transaction by hash and returns it with its index and block context.
    pub fn find_indexed(&self, tx_hash: TxHash) -> Option<IndexedTx<'_, N::Block>> {
        self.block_ref().recovered_block().find_indexed(tx_hash)
    }
}

/// Represents an executed block stored in-memory.
#[derive(Clone, Debug)]
pub struct ExecutedBlock<N: NodePrimitives = EthPrimitives> {
    /// Recovered Block
    pub recovered_block: Arc<RecoveredBlock<N::Block>>,
    /// Block's execution outcome.
    pub execution_output: Arc<BlockExecutionOutput<N::Receipt>>,
    /// Deferred trie data produced by execution.
    ///
    /// This allows deferring the computation of the trie data which can be expensive.
    /// The data can be populated asynchronously after the block was validated.
    pub trie_data: DeferredTrieData,
    /// Token metadata captured while executing this block (`name` / `symbol` /
    /// `decimals` of token contracts it introduced), keyed by deployed address —
    /// written beside the block's runtime bytecodes at commit (`{address}.meta` next
    /// to `{address}`).
    pub token_meta: Vec<(Address, TokenMeta)>,
}

impl<N: NodePrimitives> Default for ExecutedBlock<N> {
    fn default() -> Self {
        Self {
            recovered_block: Default::default(),
            execution_output: Arc::new(BlockExecutionOutput {
                result: BlockExecutionResult {
                    receipts: Default::default(),
                    requests: Default::default(),
                    gas_used: 0,
                    blob_gas_used: 0,
                },
                state: Default::default(),
            }),
            trie_data: DeferredTrieData::ready(ComputedTrieData::default()),
            token_meta: Vec::new(),
        }
    }
}

impl<N: NodePrimitives> PartialEq for ExecutedBlock<N> {
    fn eq(&self, other: &Self) -> bool {
        // Trie data is computed asynchronously and doesn't define block identity.
        self.recovered_block == other.recovered_block &&
            self.execution_output == other.execution_output
    }
}

impl<N: NodePrimitives> ExecutedBlock<N> {
    /// Create a new [`ExecutedBlock`] with already-computed trie data.
    ///
    /// Use this constructor when trie data is available immediately (e.g., sequencers,
    /// payload builders). This is the safe default path.
    pub fn new(
        recovered_block: Arc<RecoveredBlock<N::Block>>,
        execution_output: Arc<BlockExecutionOutput<N::Receipt>>,
        trie_data: ComputedTrieData,
    ) -> Self {
        Self {
            recovered_block,
            execution_output,
            trie_data: DeferredTrieData::ready(trie_data),
            token_meta: Vec::new(),
        }
    }

    /// Create a new [`ExecutedBlock`] with deferred trie data.
    ///
    /// This is useful if the trie data is populated somewhere else, e.g. asynchronously
    /// after the block was validated.
    ///
    /// The [`DeferredTrieData`] handle allows expensive trie operations (sorting hashed state,
    /// sorting trie updates, and building the accumulated trie input overlay) to be performed
    /// outside the critical validation path. This can improve latency for time-sensitive
    /// operations like block validation.
    ///
    /// If the data hasn't been populated when [`Self::trie_data()`] is called, computation
    /// occurs synchronously from stored inputs, so there is no blocking or deadlock risk.
    ///
    /// Use [`Self::new()`] instead when trie data is already computed and available immediately.
    pub const fn with_deferred_trie_data(
        recovered_block: Arc<RecoveredBlock<N::Block>>,
        execution_output: Arc<BlockExecutionOutput<N::Receipt>>,
        trie_data: DeferredTrieData,
    ) -> Self {
        Self { recovered_block, execution_output, trie_data, token_meta: Vec::new() }
    }

    /// Attaches the token metadata captured while executing this block (keyed by code
    /// hash); the persistence writes each one as `{hash}.meta` beside the bytecode.
    pub fn with_token_meta(mut self, token_meta: Vec<(Address, TokenMeta)>) -> Self {
        self.token_meta = token_meta;
        self
    }

    /// The token metadata captured while executing this block, keyed by deployed address.
    pub fn token_meta(&self) -> &[(Address, TokenMeta)] {
        &self.token_meta
    }

    /// Returns a reference to an inner [`SealedBlock`]
    #[inline]
    pub fn sealed_block(&self) -> &SealedBlock<N::Block> {
        self.recovered_block.sealed_block()
    }

    /// Returns a reference to [`RecoveredBlock`]
    #[inline]
    pub fn recovered_block(&self) -> &RecoveredBlock<N::Block> {
        &self.recovered_block
    }

    /// Returns a reference to the block's execution outcome
    #[inline]
    pub fn execution_outcome(&self) -> &BlockExecutionOutput<N::Receipt> {
        &self.execution_output
    }

    /// Returns the trie data, computing it synchronously if not already cached.
    ///
    /// Uses `OnceLock::get_or_init` internally:
    /// - If already computed: returns cached result immediately
    /// - If not computed: first caller computes, others wait for that result
    #[inline]
    #[tracing::instrument(level = "debug", target = "engine::tree", name = "trie_data", skip_all)]
    pub fn trie_data(&self) -> ComputedTrieData {
        self.trie_data.wait_cloned()
    }

    /// Returns a clone of the deferred trie data handle.
    ///
    /// A handle is a lightweight reference that can be passed to descendants without
    /// forcing trie data to be computed immediately. The actual work runs when
    /// `wait_cloned()` is called by a consumer (e.g. when merging overlays).
    #[inline]
    pub fn trie_data_handle(&self) -> DeferredTrieData {
        self.trie_data.clone()
    }

    /// Returns the hashed state result of the execution outcome.
    ///
    /// May compute trie data synchronously if the deferred task hasn't completed.
    #[inline]
    pub fn hashed_state(&self) -> Arc<HashedPostStateSorted> {
        self.trie_data().hashed_state
    }

    /// Returns the trie updates resulting from the execution outcome.
    ///
    /// May compute trie data synchronously if the deferred task hasn't completed.
    #[inline]
    pub fn trie_updates(&self) -> Arc<TrieUpdatesSorted> {
        self.trie_data().trie_updates
    }

    /// Returns the trie input anchored to the persisted ancestor.
    ///
    /// May compute trie data synchronously if the deferred task hasn't completed.
    #[inline]
    pub fn trie_input(&self) -> Option<Arc<TrieInputSorted>> {
        self.trie_data().trie_input().cloned()
    }

    /// Returns the anchor hash of the trie input, if present.
    #[inline]
    pub fn anchor_hash(&self) -> Option<B256> {
        self.trie_data().anchor_hash()
    }

    /// Returns a [`BlockNumber`] of the block.
    #[inline]
    pub fn block_number(&self) -> BlockNumber {
        self.recovered_block.header().number()
    }
}

/// Non-empty chain of blocks.
#[derive(Debug)]
pub enum NewCanonicalChain<N: NodePrimitives = EthPrimitives> {
    /// A simple append to the current canonical head
    Commit {
        /// all blocks that lead back to the canonical head
        new: Vec<ExecutedBlock<N>>,
    },
    /// A reorged chain consists of two chains that trace back to a shared ancestor block at which
    /// point they diverge.
    Reorg {
        /// All blocks of the _new_ chain
        new: Vec<ExecutedBlock<N>>,
        /// All blocks of the _old_ chain
        old: Vec<ExecutedBlock<N>>,
    },
}

impl<N: NodePrimitives<SignedTx: SignedTransaction>> NewCanonicalChain<N> {
    /// Returns the length of the new chain.
    pub const fn new_block_count(&self) -> usize {
        match self {
            Self::Commit { new } | Self::Reorg { new, .. } => new.len(),
        }
    }

    /// Returns the length of the reorged chain.
    pub const fn reorged_block_count(&self) -> usize {
        match self {
            Self::Commit { .. } => 0,
            Self::Reorg { old, .. } => old.len(),
        }
    }

    /// Converts the new chain into a notification that will be emitted to listeners
    pub fn to_chain_notification(&self) -> CanonStateNotification<N> {
        match self {
            Self::Commit { new } => {
                CanonStateNotification::Commit { new: Arc::new(Self::blocks_to_chain(new)) }
            }
            Self::Reorg { new, old } => CanonStateNotification::Reorg {
                new: Arc::new(Self::blocks_to_chain(new)),
                old: Arc::new(Self::blocks_to_chain(old)),
            },
        }
    }

    /// Converts a slice of executed blocks into a [`Chain`].
    fn blocks_to_chain(blocks: &[ExecutedBlock<N>]) -> Chain<N> {
        match blocks {
            [] => Chain::default(),
            [first, rest @ ..] => {
                let trie_data_handle = first.trie_data_handle();
                let mut chain = Chain::from_block(
                    first.recovered_block().clone(),
                    ExecutionOutcome::from((
                        first.execution_outcome().clone(),
                        first.block_number(),
                    )),
                    LazyTrieData::deferred(move || {
                        let trie_data = trie_data_handle.wait_cloned();
                        SortedTrieData {
                            hashed_state: trie_data.hashed_state,
                            trie_updates: trie_data.trie_updates,
                        }
                    }),
                );
                for exec in rest {
                    let trie_data_handle = exec.trie_data_handle();
                    chain.append_block(
                        exec.recovered_block().clone(),
                        ExecutionOutcome::from((
                            exec.execution_outcome().clone(),
                            exec.block_number(),
                        )),
                        LazyTrieData::deferred(move || {
                            let trie_data = trie_data_handle.wait_cloned();
                            SortedTrieData {
                                hashed_state: trie_data.hashed_state,
                                trie_updates: trie_data.trie_updates,
                            }
                        }),
                    );
                }
                chain
            }
        }
    }

    /// Returns the new tip of the chain.
    ///
    /// Returns the new tip for [`Self::Reorg`] and [`Self::Commit`] variants which commit at least
    /// 1 new block.
    pub fn tip(&self) -> &RecoveredBlock<N::Block> {
        match self {
            Self::Commit { new } | Self::Reorg { new, .. } => {
                new.last().expect("non empty blocks").recovered_block()
            }
        }
    }
}
