use crate::{
    either_writer::RawAuxDbBatch,
    providers::{AuxDbProvider, ConsistentProvider, ProviderNodeTypes},
    AccountReader, AuxDbProviderFactory, BlockHashReader, BlockIdReader, BlockNumReader,
    BlockReader, BlockReaderIdExt, BlockSource, CanonChainTracker, CanonStateNotifications,
    CanonStateSubscriptions, ChainSpecProvider, ChainStateBlockReader, ChangeSetReader,
    DatabaseProviderFactory, HashedPostStateProvider, HeaderProvider, ProviderError,
    ProviderFactory, PruneCheckpointReader, ReceiptProvider, ReceiptProviderIdExt,
    StageCheckpointReader, StateProviderBox, StateProviderFactory, StateReader,
    TransactionVariant, TransactionsProvider,
};
use alloy_consensus::transaction::TransactionMeta;
use alloy_eips::{BlockHashOrNumber, BlockId, BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{Address, BlockHash, BlockNumber, TxHash, TxNumber, B256};
use alloy_rpc_types_engine::ForkchoiceState;
use revm_database::BundleState;
use smartnet_chain_state::{
    BlockState, CanonicalInMemoryState, ForkChoiceNotifications, ForkChoiceSubscriptions,
    MemoryOverlayStateProvider, PersistedBlockNotifications, PersistedBlockSubscriptions,
};
use smartnet_chainspec::ChainInfo;
use smartnet_db_api::models::{AccountBeforeTx, BlockNumberAddress, StoredBlockBodyIndices};
use smartnet_execution_types::ExecutionOutcome;
use smartnet_node_types::{BlockTy, HeaderTy, NodeTypesWithDB, ReceiptTy, TxTy};
use smartnet_primitives_traits::{Account, RecoveredBlock, SealedHeader, StorageEntry};
use smartnet_prune_types::{PruneCheckpoint, PruneSegment};
use smartnet_stages_types::{StageCheckpoint, StageId};
use smartnet_storage_api::{
    BlockBodyIndicesProvider, NodePrimitivesProvider, StorageChangeSetReader,
};
use smartnet_storage_errors::provider::ProviderResult;
use smartnet_trie::{HashedPostState, KeccakKeyHasher};
use std::{
    ops::{RangeBounds, RangeInclusive},
    sync::Arc,
    time::Instant,
};
use tracing::trace;

/// The main type for interacting with the blockchain.
///
/// This type serves as the main entry point for interacting with the blockchain and provides data
/// from database storage and from the blockchain tree (pending state etc.) It is a simple wrapper
/// type that holds an instance of the database and the blockchain tree.
#[derive(Debug)]
pub struct BlockchainProvider<N: NodeTypesWithDB> {
    /// Provider factory used to access the database.
    pub(crate) database: ProviderFactory<N>,
    /// Tracks the chain info wrt forkchoice updates and in memory canonical
    /// state.
    pub(crate) canonical_in_memory_state: CanonicalInMemoryState<N::Primitives>,
}

impl<N: NodeTypesWithDB> Clone for BlockchainProvider<N> {
    fn clone(&self) -> Self {
        Self {
            database: self.database.clone(),
            canonical_in_memory_state: self.canonical_in_memory_state.clone(),
        }
    }
}

impl<N: ProviderNodeTypes> BlockchainProvider<N> {
    /// Create a new [`BlockchainProvider`] using only the storage, fetching the latest
    /// header from the database to initialize the provider.
    pub fn new(storage: ProviderFactory<N>) -> ProviderResult<Self> {
        let provider = storage.provider()?;
        let best = provider.chain_info()?;
        match provider.header_by_number(best.best_number)? {
            Some(header) => {
                drop(provider);
                Ok(Self::with_latest(storage, SealedHeader::new(header, best.best_hash))?)
            }
            None => Err(ProviderError::HeaderNotFound(best.best_number.into())),
        }
    }

    /// Create new provider instance that wraps the database and the blockchain tree, using the
    /// provided latest header to initialize the chain info tracker.
    ///
    /// This returns a `ProviderResult` since it tries the retrieve the last finalized header from
    /// `database`.
    pub fn with_latest(
        storage: ProviderFactory<N>,
        latest: SealedHeader<HeaderTy<N>>,
    ) -> ProviderResult<Self> {
        let provider = storage.provider()?;
        let finalized_header = provider
            .last_finalized_block_number()?
            .map(|num| provider.sealed_header(num))
            .transpose()?
            .flatten();
        let safe_header = provider
            .last_safe_block_number()?
            .or_else(|| {
                // for the purpose of this we can also use the finalized block if we don't have the
                // safe block
                provider.last_finalized_block_number().ok().flatten()
            })
            .map(|num| provider.sealed_header(num))
            .transpose()?
            .flatten();
        Ok(Self {
            database: storage,
            canonical_in_memory_state: CanonicalInMemoryState::with_head(
                latest,
                finalized_header,
                safe_header,
            ),
        })
    }

    /// Gets a clone of `canonical_in_memory_state`.
    pub fn canonical_in_memory_state(&self) -> CanonicalInMemoryState<N::Primitives> {
        self.canonical_in_memory_state.clone()
    }

    /// Returns a provider with a created `DbTx` inside, which allows fetching data from the
    /// database using different types of providers. Example: [`HeaderProvider`]
    /// [`BlockHashReader`]. This may fail if the inner read database transaction fails to open.
    #[track_caller]
    pub fn consistent_provider(&self) -> ProviderResult<ConsistentProvider<N>> {
        ConsistentProvider::new(self.database.clone(), self.canonical_in_memory_state())
    }

    /// This uses a given [`BlockState`] to initialize a state provider for that block.
    fn block_state_provider(
        &self,
        state: &BlockState<N::Primitives>,
    ) -> ProviderResult<MemoryOverlayStateProvider<N::Primitives>> {
        let anchor_hash = state.anchor().hash;
        let latest_historical = self.database.history_by_block_hash(anchor_hash)?;
        Ok(state.state_provider(latest_historical))
    }

    /// Return the last N blocks of state, recreating the [`ExecutionOutcome`].
    ///
    /// If the range is empty, or there are no blocks for the given range, then this returns `None`.
    pub fn get_state(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Option<ExecutionOutcome<ReceiptTy<N>>>> {
        self.consistent_provider()?.get_state(range)
    }
}

impl<N: NodeTypesWithDB> NodePrimitivesProvider for BlockchainProvider<N> {
    type Primitives = N::Primitives;
}

impl<N: ProviderNodeTypes> DatabaseProviderFactory for BlockchainProvider<N> {
    type DB = N::DB;
    type Provider = <ProviderFactory<N> as DatabaseProviderFactory>::Provider;
    type ProviderRW = <ProviderFactory<N> as DatabaseProviderFactory>::ProviderRW;

    fn database_provider_ro(&self) -> ProviderResult<Self::Provider> {
        self.database.database_provider_ro()
    }

    fn database_provider_rw(&self) -> ProviderResult<Self::ProviderRW> {
        self.database.database_provider_rw()
    }
}

impl<N: ProviderNodeTypes> AuxDbProviderFactory for BlockchainProvider<N> {
    fn aux_db_provider(&self) -> AuxDbProvider {
        self.database.aux_db_provider()
    }

    fn set_pending_aux_db_batch(&self, _batch: RawAuxDbBatch) {
        unimplemented!("BlockchainProvider wraps ProviderFactory - use DatabaseProvider::set_pending_aux_db_batch instead")
    }

    fn commit_pending_aux_db_batches(&self) -> ProviderResult<()> {
        unimplemented!("BlockchainProvider wraps ProviderFactory - use DatabaseProvider::commit_pending_aux_db_batches instead")
    }
}

impl<N: ProviderNodeTypes> HeaderProvider for BlockchainProvider<N> {
    type Header = HeaderTy<N>;

    fn header(&self, block_hash: BlockHash) -> ProviderResult<Option<Self::Header>> {
        self.consistent_provider()?.header(block_hash)
    }

    fn header_by_number(&self, num: BlockNumber) -> ProviderResult<Option<Self::Header>> {
        self.consistent_provider()?.header_by_number(num)
    }

    fn headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Self::Header>> {
        self.consistent_provider()?.headers_range(range)
    }

    fn sealed_header(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        self.consistent_provider()?.sealed_header(number)
    }

    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        self.consistent_provider()?.sealed_headers_range(range)
    }

    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        self.consistent_provider()?.sealed_headers_while(range, predicate)
    }
}

impl<N: ProviderNodeTypes> BlockHashReader for BlockchainProvider<N> {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.consistent_provider()?.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.consistent_provider()?.canonical_hashes_range(start, end)
    }
}

impl<N: ProviderNodeTypes> BlockNumReader for BlockchainProvider<N> {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        Ok(self.canonical_in_memory_state.chain_info())
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(self.canonical_in_memory_state.get_canonical_block_number())
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        self.database.last_block_number()
    }

    fn earliest_block_number(&self) -> ProviderResult<BlockNumber> {
        self.database.earliest_block_number()
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        self.consistent_provider()?.block_number(hash)
    }
}

impl<N: ProviderNodeTypes> BlockIdReader for BlockchainProvider<N> {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        Ok(self.canonical_in_memory_state.pending_block_num_hash())
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        Ok(self.canonical_in_memory_state.get_safe_num_hash())
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        Ok(self.canonical_in_memory_state.get_finalized_num_hash())
    }
}

impl<N: ProviderNodeTypes> BlockReader for BlockchainProvider<N> {
    type Block = BlockTy<N>;

    fn find_block_by_hash(
        &self,
        hash: B256,
        source: BlockSource,
    ) -> ProviderResult<Option<Self::Block>> {
        self.consistent_provider()?.find_block_by_hash(hash, source)
    }

    fn block(&self, id: BlockHashOrNumber) -> ProviderResult<Option<Self::Block>> {
        self.consistent_provider()?.block(id)
    }

    fn pending_block(&self) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        Ok(self.canonical_in_memory_state.pending_recovered_block())
    }

    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<(RecoveredBlock<Self::Block>, Vec<Self::Receipt>)>> {
        Ok(self.canonical_in_memory_state.pending_block_and_receipts())
    }

    /// Returns the block with senders with matching number or hash from database.
    ///
    /// **NOTE: If [`TransactionVariant::NoHash`] is provided then the transactions have invalid
    /// hashes, since they would need to be calculated on the spot, and we want fast querying.**
    ///
    /// Returns `None` if block is not found.
    fn recovered_block(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.consistent_provider()?.recovered_block(id, transaction_kind)
    }

    fn sealed_block_with_senders(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.consistent_provider()?.sealed_block_with_senders(id, transaction_kind)
    }

    fn block_range(&self, range: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<Self::Block>> {
        self.consistent_provider()?.block_range(range)
    }

    fn block_by_transaction_id(&self, id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        self.consistent_provider()?.block_by_transaction_id(id)
    }
}

impl<N: ProviderNodeTypes> TransactionsProvider for BlockchainProvider<N> {
    type Transaction = TxTy<N>;

    fn transaction_id(&self, tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        self.consistent_provider()?.transaction_id(tx_hash)
    }

    fn transaction_by_id(&self, id: TxNumber) -> ProviderResult<Option<Self::Transaction>> {
        self.consistent_provider()?.transaction_by_id(id)
    }

    fn transaction_by_id_unhashed(
        &self,
        id: TxNumber,
    ) -> ProviderResult<Option<Self::Transaction>> {
        self.consistent_provider()?.transaction_by_id_unhashed(id)
    }

    fn transaction_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Transaction>> {
        self.consistent_provider()?.transaction_by_hash(hash)
    }

    fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> ProviderResult<Option<(Self::Transaction, TransactionMeta)>> {
        self.consistent_provider()?.transaction_by_hash_with_meta(tx_hash)
    }

    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Transaction>>> {
        self.consistent_provider()?.transactions_by_block(id)
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Transaction>>> {
        self.consistent_provider()?.transactions_by_block_range(range)
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Transaction>> {
        self.consistent_provider()?.transactions_by_tx_range(range)
    }

    fn senders_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        self.consistent_provider()?.senders_by_tx_range(range)
    }

    fn transaction_sender(&self, id: TxNumber) -> ProviderResult<Option<Address>> {
        self.consistent_provider()?.transaction_sender(id)
    }
}

impl<N: ProviderNodeTypes> ReceiptProvider for BlockchainProvider<N> {
    type Receipt = ReceiptTy<N>;

    fn receipt(&self, id: TxNumber) -> ProviderResult<Option<Self::Receipt>> {
        self.consistent_provider()?.receipt(id)
    }

    fn receipt_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Receipt>> {
        self.consistent_provider()?.receipt_by_hash(hash)
    }

    fn receipts_by_block(
        &self,
        block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        self.consistent_provider()?.receipts_by_block(block)
    }

    fn receipts_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Receipt>> {
        self.consistent_provider()?.receipts_by_tx_range(range)
    }

    fn receipts_by_block_range(
        &self,
        block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Receipt>>> {
        self.consistent_provider()?.receipts_by_block_range(block_range)
    }
}

impl<N: ProviderNodeTypes> ReceiptProviderIdExt for BlockchainProvider<N> {
    fn receipts_by_block_id(&self, block: BlockId) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        self.consistent_provider()?.receipts_by_block_id(block)
    }
}

impl<N: ProviderNodeTypes> BlockBodyIndicesProvider for BlockchainProvider<N> {
    fn block_body_indices(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        self.consistent_provider()?.block_body_indices(number)
    }

    fn block_body_indices_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        self.consistent_provider()?.block_body_indices_range(range)
    }
}

impl<N: ProviderNodeTypes> StageCheckpointReader for BlockchainProvider<N> {
    fn get_stage_checkpoint(&self, id: StageId) -> ProviderResult<Option<StageCheckpoint>> {
        self.consistent_provider()?.get_stage_checkpoint(id)
    }

    fn get_stage_checkpoint_progress(&self, id: StageId) -> ProviderResult<Option<Vec<u8>>> {
        self.consistent_provider()?.get_stage_checkpoint_progress(id)
    }

    fn get_all_checkpoints(&self) -> ProviderResult<Vec<(String, StageCheckpoint)>> {
        self.consistent_provider()?.get_all_checkpoints()
    }
}

impl<N: ProviderNodeTypes> PruneCheckpointReader for BlockchainProvider<N> {
    fn get_prune_checkpoint(
        &self,
        segment: PruneSegment,
    ) -> ProviderResult<Option<PruneCheckpoint>> {
        self.consistent_provider()?.get_prune_checkpoint(segment)
    }

    fn get_prune_checkpoints(&self) -> ProviderResult<Vec<(PruneSegment, PruneCheckpoint)>> {
        self.consistent_provider()?.get_prune_checkpoints()
    }
}

impl<N: NodeTypesWithDB> ChainSpecProvider for BlockchainProvider<N> {
    type ChainSpec = N::ChainSpec;

    fn chain_spec(&self) -> Arc<N::ChainSpec> {
        self.database.chain_spec()
    }
}

impl<N: ProviderNodeTypes> StateProviderFactory for BlockchainProvider<N> {
    /// Storage provider for latest block
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        trace!(target: "providers::blockchain", "Getting latest block state provider");
        // use latest state provider if the head state exists
        if let Some(state) = self.canonical_in_memory_state.head_state() {
            trace!(target: "providers::blockchain", "Using head state for latest state provider");
            Ok(self.block_state_provider(&state)?.boxed())
        } else {
            trace!(target: "providers::blockchain", "Using database state for latest state provider");
            self.database.latest()
        }
    }

    /// Returns a [`StateProviderBox`] indexed by the given block number or tag.
    fn state_by_block_number_or_tag(
        &self,
        number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        match number_or_tag {
            BlockNumberOrTag::Latest => self.latest(),
            BlockNumberOrTag::Finalized => {
                // we can only get the finalized state by hash, not by num
                let hash =
                    self.finalized_block_hash()?.ok_or(ProviderError::FinalizedBlockNotFound)?;
                self.state_by_block_hash(hash)
            }
            BlockNumberOrTag::Safe => {
                // we can only get the safe state by hash, not by num
                let hash = self.safe_block_hash()?.ok_or(ProviderError::SafeBlockNotFound)?;
                self.state_by_block_hash(hash)
            }
            BlockNumberOrTag::Earliest => {
                self.history_by_block_number(self.earliest_block_number()?)
            }
            BlockNumberOrTag::Pending => self.pending(),
            BlockNumberOrTag::Number(num) => {
                let hash = self
                    .block_hash(num)?
                    .ok_or_else(|| ProviderError::HeaderNotFound(num.into()))?;
                self.state_by_block_hash(hash)
            }
        }
    }

    fn history_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<StateProviderBox> {
        trace!(target: "providers::blockchain", ?block_number, "Getting history by block number");
        let provider = self.consistent_provider()?;
        let hash = provider
            .block_hash(block_number)?
            .ok_or_else(|| ProviderError::HeaderNotFound(block_number.into()))?;
        provider.into_state_provider_at_block_hash(hash)
    }

    fn history_by_block_hash(&self, block_hash: BlockHash) -> ProviderResult<StateProviderBox> {
        trace!(target: "providers::blockchain", ?block_hash, "Getting history by block hash");
        self.consistent_provider()?.into_state_provider_at_block_hash(block_hash)
    }

    fn state_by_block_hash(&self, hash: BlockHash) -> ProviderResult<StateProviderBox> {
        trace!(target: "providers::blockchain", ?hash, "Getting state by block hash");
        match self.history_by_block_hash(hash) {
            // This could be tracked by a historical block
            Ok(state) => Ok(state),
            // The state-window guard's refusal (block below the changeset floor) is a
            // definitive answer: the node cannot serve that state at all, so it must not be
            // swallowed by the pending fallback below and resurfaced as a generic
            // "not found" (fjall4a.md §8.2 relies on this error reaching the RPC layer).
            Err(err @ ProviderError::StateAtBlockPruned(_)) => Err(err),
            // .. or this could be the pending state
            Err(_) => {
                if let Ok(Some(pending)) = self.pending_state_by_hash(hash) {
                    Ok(pending)
                } else {
                    // if we couldn't find it anywhere, then we should return an error
                    Err(ProviderError::StateForHashNotFound(hash))
                }
            }
        }
    }

    /// Returns the state provider for pending state.
    ///
    /// If there's no pending block available then the latest state provider is returned:
    /// [`Self::latest`]
    fn pending(&self) -> ProviderResult<StateProviderBox> {
        trace!(target: "providers::blockchain", "Getting provider for pending state");

        if let Some(pending) = self.canonical_in_memory_state.pending_state() {
            // we have a pending block
            return Ok(Box::new(self.block_state_provider(&pending)?));
        }

        // fallback to latest state if the pending block is not available
        self.latest()
    }

    fn pending_state_by_hash(&self, block_hash: B256) -> ProviderResult<Option<StateProviderBox>> {
        if let Some(pending) = self.canonical_in_memory_state.pending_state() &&
            pending.hash() == block_hash
        {
            return Ok(Some(Box::new(self.block_state_provider(&pending)?)));
        }
        Ok(None)
    }

    fn maybe_pending(&self) -> ProviderResult<Option<StateProviderBox>> {
        if let Some(pending) = self.canonical_in_memory_state.pending_state() {
            return Ok(Some(Box::new(self.block_state_provider(&pending)?)));
        }

        Ok(None)
    }
}

impl<N: NodeTypesWithDB> HashedPostStateProvider for BlockchainProvider<N> {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<N: ProviderNodeTypes> CanonChainTracker for BlockchainProvider<N> {
    type Header = HeaderTy<N>;

    fn on_forkchoice_update_received(&self, _update: &ForkchoiceState) {
        // update timestamp
        self.canonical_in_memory_state.on_forkchoice_update_received();
    }

    fn last_received_update_timestamp(&self) -> Option<Instant> {
        self.canonical_in_memory_state.last_received_update_timestamp()
    }

    fn set_canonical_head(&self, header: SealedHeader<Self::Header>) {
        self.canonical_in_memory_state.set_canonical_head(header);
    }

    fn set_safe(&self, header: SealedHeader<Self::Header>) {
        self.canonical_in_memory_state.set_safe(header);
    }

    fn set_finalized(&self, header: SealedHeader<Self::Header>) {
        self.canonical_in_memory_state.set_finalized(header);
    }
}

impl<N: ProviderNodeTypes> BlockReaderIdExt for BlockchainProvider<N>
where
    Self: ReceiptProviderIdExt,
{
    fn block_by_id(&self, id: BlockId) -> ProviderResult<Option<Self::Block>> {
        self.consistent_provider()?.block_by_id(id)
    }

    fn header_by_number_or_tag(
        &self,
        id: BlockNumberOrTag,
    ) -> ProviderResult<Option<Self::Header>> {
        self.consistent_provider()?.header_by_number_or_tag(id)
    }

    fn sealed_header_by_number_or_tag(
        &self,
        id: BlockNumberOrTag,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        self.consistent_provider()?.sealed_header_by_number_or_tag(id)
    }

    fn sealed_header_by_id(
        &self,
        id: BlockId,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        self.consistent_provider()?.sealed_header_by_id(id)
    }

    fn header_by_id(&self, id: BlockId) -> ProviderResult<Option<Self::Header>> {
        self.consistent_provider()?.header_by_id(id)
    }
}

impl<N: ProviderNodeTypes> CanonStateSubscriptions for BlockchainProvider<N> {
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications<Self::Primitives> {
        self.canonical_in_memory_state.subscribe_canon_state()
    }
}

impl<N: ProviderNodeTypes> ForkChoiceSubscriptions for BlockchainProvider<N> {
    type Header = HeaderTy<N>;

    fn subscribe_safe_block(&self) -> ForkChoiceNotifications<Self::Header> {
        let receiver = self.canonical_in_memory_state.subscribe_safe_block();
        ForkChoiceNotifications(receiver)
    }

    fn subscribe_finalized_block(&self) -> ForkChoiceNotifications<Self::Header> {
        let receiver = self.canonical_in_memory_state.subscribe_finalized_block();
        ForkChoiceNotifications(receiver)
    }
}

impl<N: ProviderNodeTypes> PersistedBlockSubscriptions for BlockchainProvider<N> {
    fn subscribe_persisted_block(&self) -> PersistedBlockNotifications {
        let receiver = self.canonical_in_memory_state.subscribe_persisted_block();
        PersistedBlockNotifications(receiver)
    }
}

impl<N: ProviderNodeTypes> StorageChangeSetReader for BlockchainProvider<N> {
    fn storage_changeset(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<Vec<(BlockNumberAddress, StorageEntry)>> {
        self.consistent_provider()?.storage_changeset(block_number)
    }

    fn get_storage_before_block(
        &self,
        block_number: BlockNumber,
        address: Address,
        storage_key: B256,
    ) -> ProviderResult<Option<StorageEntry>> {
        self.consistent_provider()?.get_storage_before_block(block_number, address, storage_key)
    }

    fn storage_changesets_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<(BlockNumberAddress, StorageEntry)>> {
        self.consistent_provider()?.storage_changesets_range(range)
    }
}

impl<N: ProviderNodeTypes> ChangeSetReader for BlockchainProvider<N> {
    fn account_block_changeset(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<Vec<AccountBeforeTx>> {
        self.consistent_provider()?.account_block_changeset(block_number)
    }

    fn get_account_before_block(
        &self,
        block_number: BlockNumber,
        address: Address,
    ) -> ProviderResult<Option<AccountBeforeTx>> {
        self.consistent_provider()?.get_account_before_block(block_number, address)
    }

    fn account_changesets_range(
        &self,
        range: impl core::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<(BlockNumber, AccountBeforeTx)>> {
        self.consistent_provider()?.account_changesets_range(range)
    }
}

impl<N: ProviderNodeTypes> AccountReader for BlockchainProvider<N> {
    /// Get basic account information.
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        self.consistent_provider()?.basic_account(address)
    }
}

impl<N: ProviderNodeTypes> StateReader for BlockchainProvider<N> {
    type Receipt = ReceiptTy<N>;

    /// Re-constructs the [`ExecutionOutcome`] from in-memory and database state, if necessary.
    ///
    /// If data for the block does not exist, this will return [`None`].
    ///
    /// NOTE: This cannot be called safely in a loop outside of the blockchain tree thread. This is
    /// because the [`CanonicalInMemoryState`] could change during a reorg, causing results to be
    /// inconsistent. Currently this can safely be called within the blockchain tree thread,
    /// because the tree thread is responsible for modifying the [`CanonicalInMemoryState`] in the
    /// first place.
    fn get_state(
        &self,
        block: BlockNumber,
    ) -> ProviderResult<Option<ExecutionOutcome<Self::Receipt>>> {
        StateReader::get_state(&self.consistent_provider()?, block)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        providers::BlockchainProvider,
        test_utils::{
            create_test_provider_factory, create_test_provider_factory_with_chain_spec,
            MockNodeTypesWithDB,
        },
        CanonChainTracker, ProviderFactory, SaveBlocksMode,
    };
    use alloy_eips::{BlockHashOrNumber, BlockNumHash};
    use alloy_primitives::{BlockNumber, TxNumber, B256};
    use rand::Rng;
    use revm_database::{BundleState, OriginalValuesKnown};
    use smartnet_chain_state::{
        test_utils::TestBlockBuilder, CanonStateNotification, CanonStateSubscriptions,
        CanonicalInMemoryState, ExecutedBlock, NewCanonicalChain,
    };
    use smartnet_chainspec::{ChainSpec, MAINNET};
    use smartnet_db_api::models::StoredBlockBodyIndices;
    use smartnet_errors::ProviderError;
    use smartnet_ethereum_primitives::{Block, Receipt};
    use smartnet_execution_types::{
        BlockExecutionOutput, BlockExecutionResult, Chain, ExecutionOutcome,
    };
    use smartnet_primitives_traits::{RecoveredBlock, SealedBlock, SignerRecoverable};
    use smartnet_storage_api::{
        BlockBodyIndicesProvider, BlockHashReader, BlockIdReader, BlockNumReader, BlockReader,
        DBProvider, DatabaseProviderFactory, HeaderProvider, ReceiptProvider, StateWriteConfig,
        StateWriter, TransactionVariant, TransactionsProvider,
    };
    use smartnet_testing_utils::generators::{
        self, random_block, random_block_range, random_receipt, BlockParams, BlockRangeParams,
    };
    use std::{
        collections::BTreeMap,
        ops::{Bound, Range, RangeBounds},
        sync::Arc,
    };

    const TEST_BLOCKS_COUNT: usize = 5;

    const TEST_TRANSACTIONS_COUNT: u8 = 4;

    fn random_blocks(
        rng: &mut impl Rng,
        database_blocks: usize,
        in_memory_blocks: usize,
        requests_count: Option<Range<u8>>,
        withdrawals_count: Option<Range<u8>>,
        tx_count: impl RangeBounds<u8>,
    ) -> (Vec<SealedBlock<Block>>, Vec<SealedBlock<Block>>) {
        let block_range = (database_blocks + in_memory_blocks - 1) as u64;

        let tx_start = match tx_count.start_bound() {
            Bound::Included(&n) | Bound::Excluded(&n) => n,
            Bound::Unbounded => u8::MIN,
        };
        let tx_end = match tx_count.end_bound() {
            Bound::Included(&n) | Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => u8::MAX,
        };

        let blocks = random_block_range(
            rng,
            0..=block_range,
            BlockRangeParams {
                parent: Some(B256::ZERO),
                tx_count: tx_start..tx_end,
                requests_count,
                withdrawals_count,
            },
        );
        let (database_blocks, in_memory_blocks) = blocks.split_at(database_blocks);
        (database_blocks.to_vec(), in_memory_blocks.to_vec())
    }

    #[expect(clippy::type_complexity)]
    fn provider_with_chain_spec_and_random_blocks(
        rng: &mut impl Rng,
        chain_spec: Arc<ChainSpec>,
        database_blocks: usize,
        in_memory_blocks: usize,
        block_range_params: BlockRangeParams,
    ) -> eyre::Result<(
        BlockchainProvider<MockNodeTypesWithDB>,
        Vec<SealedBlock<Block>>,
        Vec<SealedBlock<Block>>,
        Vec<Vec<Receipt>>,
    )> {
        let (database_blocks, in_memory_blocks) = random_blocks(
            rng,
            database_blocks,
            in_memory_blocks,
            block_range_params.requests_count,
            block_range_params.withdrawals_count,
            block_range_params.tx_count,
        );

        let receipts: Vec<Vec<_>> = database_blocks
            .iter()
            .chain(in_memory_blocks.iter())
            .map(|block| block.body().transactions.iter())
            .map(|tx| tx.map(|tx| random_receipt(rng, tx, Some(2), None)).collect())
            .collect();

        let factory = create_test_provider_factory_with_chain_spec(chain_spec);
        let provider_rw = factory.database_provider_rw()?;

        // Insert blocks into the database
        for block in &database_blocks {
            provider_rw.insert_block(
                &block.clone().try_recover().expect("failed to seal block with senders"),
            )?;
        }

        // Insert receipts into the database
        if let Some(first_block) = database_blocks.first() {
            provider_rw.write_state(
                &ExecutionOutcome {
                    first_block: first_block.number,
                    receipts: receipts.iter().take(database_blocks.len()).cloned().collect(),
                    ..Default::default()
                },
                OriginalValuesKnown::No,
                StateWriteConfig::default(),
            )?;
        }

        provider_rw.commit()?;

        let provider = BlockchainProvider::new(factory)?;

        // Insert the rest of the blocks and receipts into the in-memory state
        let chain = NewCanonicalChain::Commit {
            new: in_memory_blocks
                .iter()
                .map(|block| {
                    let senders = block.senders().expect("failed to recover senders");
                    let block_receipts = receipts.get(block.number as usize).unwrap().clone();
                    let execution_outcome = BlockExecutionOutput {
                        result: BlockExecutionResult {
                            receipts: block_receipts,
                            requests: Default::default(),
                            gas_used: 0,
                            blob_gas_used: 0,
                        },
                        state: BundleState::default(),
                    };

                    ExecutedBlock {
                        recovered_block: Arc::new(RecoveredBlock::new_sealed(
                            block.clone(),
                            senders,
                        )),
                        execution_output: execution_outcome.into(),
                        ..Default::default()
                    }
                })
                .collect(),
        };
        provider.canonical_in_memory_state.update_chain(chain);

        // Get canonical, safe, and finalized blocks
        let blocks = database_blocks.iter().chain(in_memory_blocks.iter()).collect::<Vec<_>>();
        let block_count = blocks.len();
        let canonical_block = blocks.get(block_count - 1).unwrap();
        let safe_block = blocks.get(block_count - 2).unwrap();
        let finalized_block = blocks.get(block_count - 3).unwrap();

        // Set the canonical head, safe, and finalized blocks
        provider.set_canonical_head(canonical_block.clone_sealed_header());
        provider.set_safe(safe_block.clone_sealed_header());
        provider.set_finalized(finalized_block.clone_sealed_header());

        Ok((provider, database_blocks.clone(), in_memory_blocks.clone(), receipts))
    }

    #[expect(clippy::type_complexity)]
    fn provider_with_random_blocks(
        rng: &mut impl Rng,
        database_blocks: usize,
        in_memory_blocks: usize,
        block_range_params: BlockRangeParams,
    ) -> eyre::Result<(
        BlockchainProvider<MockNodeTypesWithDB>,
        Vec<SealedBlock<Block>>,
        Vec<SealedBlock<Block>>,
        Vec<Vec<Receipt>>,
    )> {
        provider_with_chain_spec_and_random_blocks(
            rng,
            MAINNET.clone(),
            database_blocks,
            in_memory_blocks,
            block_range_params,
        )
    }

    /// This will persist the last block in-memory and delete it from
    /// `canonical_in_memory_state` right after a database read transaction is created.
    ///
    /// This simulates a RPC method having a different view than when its database transaction was
    /// created.
    fn persist_block_after_db_tx_creation(
        provider: BlockchainProvider<MockNodeTypesWithDB>,
        block_number: BlockNumber,
    ) {
        let hook_provider = provider.clone();
        provider.database.db_ref().set_post_transaction_hook(Box::new(move || {
            if let Some(state) = hook_provider.canonical_in_memory_state.head_state() &&
                state.anchor().number + 1 == block_number
            {
                let mut lowest_memory_block =
                    state.parent_state_chain().last().expect("qed").block();
                let num_hash = lowest_memory_block.recovered_block().num_hash();

                let execution_output = (*lowest_memory_block.execution_output).clone();
                lowest_memory_block.execution_output = Arc::new(execution_output);

                // Push to disk
                let provider_rw = hook_provider.database_provider_rw().unwrap();
                provider_rw.save_blocks(vec![lowest_memory_block], SaveBlocksMode::Full).unwrap();
                provider_rw.commit().unwrap();

                // Remove from memory
                hook_provider.canonical_in_memory_state.remove_persisted_blocks(num_hash);
            }
        }));
    }

    #[test]
    fn test_block_reader_pending_block() -> eyre::Result<()> {
        let mut rng = generators::rng();
        let (provider, _, _, _) = provider_with_random_blocks(
            &mut rng,
            TEST_BLOCKS_COUNT,
            TEST_BLOCKS_COUNT,
            BlockRangeParams::default(),
        )?;

        // Generate a random block
        let mut rng = generators::rng();
        let block = random_block(
            &mut rng,
            0,
            BlockParams { parent: Some(B256::ZERO), ..Default::default() },
        );

        // Set the block as pending
        provider.canonical_in_memory_state.set_pending_block(ExecutedBlock {
            recovered_block: Arc::new(RecoveredBlock::new_sealed(
                block.clone(),
                block.senders().unwrap(),
            )),
            ..Default::default()
        });

        // Assertions related to the pending block

        assert_eq!(
            provider.pending_block()?,
            Some(RecoveredBlock::new_sealed(block.clone(), block.senders().unwrap()))
        );

        assert_eq!(
            provider.pending_block_and_receipts()?,
            Some((RecoveredBlock::new_sealed(block.clone(), block.senders().unwrap()), vec![]))
        );

        Ok(())
    }

    #[test]
    fn test_block_body_indices() -> eyre::Result<()> {
        // Create a new provider
        let mut rng = generators::rng();
        let (provider, database_blocks, in_memory_blocks, _) = provider_with_random_blocks(
            &mut rng,
            TEST_BLOCKS_COUNT,
            TEST_BLOCKS_COUNT,
            BlockRangeParams {
                tx_count: TEST_TRANSACTIONS_COUNT..TEST_TRANSACTIONS_COUNT,
                ..Default::default()
            },
        )?;

        let first_in_mem_block = in_memory_blocks.first().unwrap();

        // Insert the first block into the in-memory state
        let in_memory_block_senders =
            first_in_mem_block.senders().expect("failed to recover senders");
        let chain = NewCanonicalChain::Commit {
            new: vec![ExecutedBlock {
                recovered_block: Arc::new(RecoveredBlock::new_sealed(
                    first_in_mem_block.clone(),
                    in_memory_block_senders,
                )),
                ..Default::default()
            }],
        };
        provider.canonical_in_memory_state.update_chain(chain);

        let first_db_block = database_blocks.first().unwrap().clone();
        let first_in_mem_block = in_memory_blocks.first().unwrap().clone();

        // First database block body indices should be found
        assert_eq!(
            provider.block_body_indices(first_db_block.number)?.unwrap(),
            StoredBlockBodyIndices { first_tx_num: 0, tx_count: 4 }
        );

        // First in-memory block body indices should be found with the first tx after the database
        // blocks
        assert_eq!(
            provider.block_body_indices(first_in_mem_block.number)?.unwrap(),
            StoredBlockBodyIndices { first_tx_num: 20, tx_count: 4 }
        );

        // A random block number should return None as the block is not found
        let mut rng = rand::rng();
        let random_block_number: u64 = rng.random();
        assert_eq!(provider.block_body_indices(random_block_number)?, None);

        Ok(())
    }

    #[test]
    fn test_block_hash_reader() -> eyre::Result<()> {
        let mut rng = generators::rng();
        let (provider, database_blocks, in_memory_blocks, _) = provider_with_random_blocks(
            &mut rng,
            TEST_BLOCKS_COUNT,
            TEST_BLOCKS_COUNT,
            BlockRangeParams::default(),
        )?;

        let database_block = database_blocks.first().unwrap().clone();
        let in_memory_block = in_memory_blocks.last().unwrap().clone();

        assert_eq!(provider.block_hash(database_block.number)?, Some(database_block.hash()));
        assert_eq!(provider.block_hash(in_memory_block.number)?, Some(in_memory_block.hash()));

        assert_eq!(
            provider.canonical_hashes_range(0, 10)?,
            [database_blocks, in_memory_blocks]
                .concat()
                .iter()
                .map(|block| block.hash())
                .collect::<Vec<_>>()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_canon_state_subscriptions() -> eyre::Result<()> {
        let factory = create_test_provider_factory();

        // Generate a random block to initialize the blockchain provider.
        let mut test_block_builder = TestBlockBuilder::eth();
        let block_1 = test_block_builder.generate_random_block(0, B256::ZERO).try_recover()?;
        let block_hash_1 = block_1.hash();

        // Insert and commit the block.
        let provider_rw = factory.provider_rw()?;
        provider_rw.insert_block(&block_1)?;
        provider_rw.commit()?;

        let provider = BlockchainProvider::new(factory)?;

        // Subscribe twice for canonical state updates.
        let in_memory_state = provider.canonical_in_memory_state();
        let mut rx_1 = provider.subscribe_to_canonical_state();
        let mut rx_2 = provider.subscribe_to_canonical_state();

        // Send and receive commit notifications.
        let block_2 = test_block_builder.generate_random_block(1, block_hash_1).try_recover()?;
        let chain = Chain::new(vec![block_2], ExecutionOutcome::default(), BTreeMap::new());
        let commit = CanonStateNotification::Commit { new: Arc::new(chain.clone()) };
        in_memory_state.notify_canon_state(commit.clone());
        let (notification_1, notification_2) = tokio::join!(rx_1.recv(), rx_2.recv());
        assert_eq!(notification_1, Ok(commit.clone()));
        assert_eq!(notification_2, Ok(commit.clone()));

        // Send and receive re-org notifications.
        let block_3 = test_block_builder.generate_random_block(1, block_hash_1).try_recover()?;
        let block_4 = test_block_builder.generate_random_block(2, block_3.hash()).try_recover()?;
        let new_chain =
            Chain::new(vec![block_3, block_4], ExecutionOutcome::default(), BTreeMap::new());
        let re_org =
            CanonStateNotification::Reorg { old: Arc::new(chain), new: Arc::new(new_chain) };
        in_memory_state.notify_canon_state(re_org.clone());
        let (notification_1, notification_2) = tokio::join!(rx_1.recv(), rx_2.recv());
        assert_eq!(notification_1, Ok(re_org.clone()));
        assert_eq!(notification_2, Ok(re_org.clone()));

        Ok(())
    }

    #[test]
    fn test_block_num_reader() -> eyre::Result<()> {
        let mut rng = generators::rng();
        let (provider, database_blocks, in_memory_blocks, _) = provider_with_random_blocks(
            &mut rng,
            TEST_BLOCKS_COUNT,
            TEST_BLOCKS_COUNT,
            BlockRangeParams::default(),
        )?;

        assert_eq!(provider.best_block_number()?, in_memory_blocks.last().unwrap().number);
        assert_eq!(provider.last_block_number()?, database_blocks.last().unwrap().number);

        let database_block = database_blocks.first().unwrap().clone();
        let in_memory_block = in_memory_blocks.first().unwrap().clone();
        assert_eq!(provider.block_number(database_block.hash())?, Some(database_block.number));
        assert_eq!(provider.block_number(in_memory_block.hash())?, Some(in_memory_block.number));

        Ok(())
    }

    #[test]
    fn test_block_id_reader() -> eyre::Result<()> {
        // Create a new provider
        let mut rng = generators::rng();
        let (provider, _, in_memory_blocks, _) = provider_with_random_blocks(
            &mut rng,
            TEST_BLOCKS_COUNT,
            TEST_BLOCKS_COUNT,
            BlockRangeParams::default(),
        )?;

        // Set the pending block in memory
        let pending_block = in_memory_blocks.last().unwrap();
        provider.canonical_in_memory_state.set_pending_block(ExecutedBlock {
            recovered_block: Arc::new(RecoveredBlock::new_sealed(
                pending_block.clone(),
                Default::default(),
            )),
            ..Default::default()
        });

        // Set the safe block in memory
        let safe_block = in_memory_blocks[in_memory_blocks.len() - 2].clone();
        provider.canonical_in_memory_state.set_safe(safe_block.clone_sealed_header());

        // Set the finalized block in memory
        let finalized_block = in_memory_blocks[in_memory_blocks.len() - 3].clone();
        provider.canonical_in_memory_state.set_finalized(finalized_block.clone_sealed_header());

        // Verify the pending block number and hash
        assert_eq!(
            provider.pending_block_num_hash()?,
            Some(BlockNumHash { number: pending_block.number, hash: pending_block.hash() })
        );

        // Verify the safe block number and hash
        assert_eq!(
            provider.safe_block_num_hash()?,
            Some(BlockNumHash { number: safe_block.number, hash: safe_block.hash() })
        );

        // Verify the finalized block number and hash
        assert_eq!(
            provider.finalized_block_num_hash()?,
            Some(BlockNumHash { number: finalized_block.number, hash: finalized_block.hash() })
        );

        Ok(())
    }

    /// Helper macro to call a provider method based on argument count and check its result
    macro_rules! call_method {
        ($provider:expr, $method:ident, ($($args:expr),*), $expected_item:expr) => {{
            let result = $provider.$method($($args),*)?;
            assert_eq!(
                result,
                $expected_item,
                "{}: item does not match the expected item for arguments {:?}",
                stringify!($method),
                ($($args),*)
            );
        }};

        // Handle valid or invalid arguments for one argument
        (ONE, $provider:expr, $method:ident, $item_extractor:expr, $txnum:expr, $txhash:expr, $block:expr, $receipts:expr) => {{
            let (arg, expected_item) = $item_extractor($block, $txnum($block), $txhash($block), $receipts);
            call_method!($provider, $method, (arg), expected_item);
        }};

        // Handle valid or invalid arguments for two arguments
        (TWO, $provider:expr, $method:ident, $item_extractor:expr, $txnum:expr, $txhash:expr, $block:expr, $receipts:expr) => {{
            let ((arg1, arg2), expected_item) = $item_extractor($block, $txnum($block), $txhash($block), $receipts);
            call_method!($provider, $method, (arg1, arg2), expected_item);
        }};
    }

    /// Macro to test non-range methods.
    ///
    /// ( `NUMBER_ARGUMENTS`, METHOD, FN -> ((`METHOD_ARGUMENT(s)`,...), `EXPECTED_RESULT`),
    /// `INVALID_ARGUMENTS`)
    macro_rules! test_non_range {
    ([$(($arg_count:ident, $method:ident, $item_extractor:expr, $invalid_args:expr)),* $(,)?]) => {{

        // Get the number methods being tested.
        // Since each method tested will move a block from memory to storage, this ensures we have enough.
        let extra_blocks = [$(stringify!($arg_count)),*].len();

        let mut rng = generators::rng();
        let (provider, mut database_blocks, in_memory_blocks, receipts) = provider_with_random_blocks(
            &mut rng,
            TEST_BLOCKS_COUNT,
            TEST_BLOCKS_COUNT + extra_blocks,
            BlockRangeParams {
                tx_count: TEST_TRANSACTIONS_COUNT..TEST_TRANSACTIONS_COUNT,
                ..Default::default()
            },
        )?;

        let mut in_memory_blocks: std::collections::VecDeque<_> = in_memory_blocks.into();

        $(
            let tx_hash = |block: &SealedBlock<Block>| *block.body().transactions[0].tx_hash();
            let tx_num = |block: &SealedBlock<Block>| {
                database_blocks
                    .iter()
                    .chain(in_memory_blocks.iter())
                    .take_while(|b| b.number < block.number)
                    .map(|b| b.transaction_count())
                    .sum::<usize>() as u64
            };

            // Ensure that the first generated in-memory block exists
            {
                // This block will be persisted to disk and removed from memory AFTER the first database query. This ensures that we query the in-memory state before the database avoiding any race condition.
                persist_block_after_db_tx_creation(provider.clone(), in_memory_blocks[0].number);

                call_method!($arg_count, provider, $method, $item_extractor, tx_num, tx_hash, &in_memory_blocks[0], &receipts);

                // Move the block as well in our own structures
                database_blocks.push(in_memory_blocks.pop_front().unwrap());
            }

            // database_blocks is changed above
            let tx_num = |block: &SealedBlock<Block>| {
                database_blocks
                    .iter()
                    .chain(in_memory_blocks.iter())
                    .take_while(|b| b.number < block.number)
                    .map(|b| b.transaction_count())
                    .sum::<usize>() as u64
            };

            // Invalid/Non-existent argument should return `None`
            {
                call_method!($arg_count, provider, $method, |_,_,_,_|  ($invalid_args, None), tx_num, tx_hash, &in_memory_blocks[0], &receipts);
            }

            // Check that the item is only in memory and not in database
            {
                let last_mem_block = &in_memory_blocks[in_memory_blocks.len() - 1];

                let (args, expected_item) = $item_extractor(last_mem_block, tx_num(last_mem_block), tx_hash(last_mem_block), &receipts);
                call_method!($arg_count, provider, $method, |_,_,_,_| (args.clone(), expected_item), tx_num, tx_hash, last_mem_block, &receipts);

                // Ensure the item is not in storage
                call_method!($arg_count, provider.database, $method, |_,_,_,_|  (args, None), tx_num, tx_hash, last_mem_block, &receipts);
            }
        )*
    }};
}

    #[test]
    fn test_non_range_methods() -> eyre::Result<()> {
        let test_tx_index = 0;

        test_non_range!([
            (
                ONE,
                header,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    block.hash(),
                    Some(block.header().clone())
                ),
                B256::random()
            ),
            (
                ONE,
                header_by_number,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    block.number,
                    Some(block.header().clone())
                ),
                u64::MAX
            ),
            (
                ONE,
                sealed_header,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    block.number,
                    Some(block.clone_sealed_header())
                ),
                u64::MAX
            ),
            (
                ONE,
                block_hash,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    block.number,
                    Some(block.hash())
                ),
                u64::MAX
            ),
            (
                ONE,
                block_number,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    block.hash(),
                    Some(block.number)
                ),
                B256::random()
            ),
            (
                ONE,
                block,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    BlockHashOrNumber::Hash(block.hash()),
                    Some(block.clone().into_block())
                ),
                BlockHashOrNumber::Hash(B256::random())
            ),
            (
                ONE,
                block,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    BlockHashOrNumber::Number(block.number),
                    Some(block.clone().into_block())
                ),
                BlockHashOrNumber::Number(u64::MAX)
            ),
            (
                ONE,
                block_body_indices,
                |block: &SealedBlock<Block>, tx_num: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    block.number,
                    Some(StoredBlockBodyIndices {
                        first_tx_num: tx_num,
                        tx_count: block.transaction_count() as u64
                    })
                ),
                u64::MAX
            ),
            (
                TWO,
                recovered_block,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    (BlockHashOrNumber::Number(block.number), TransactionVariant::WithHash),
                    block.clone().try_recover().ok()
                ),
                (BlockHashOrNumber::Number(u64::MAX), TransactionVariant::WithHash)
            ),
            (
                TWO,
                recovered_block,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    (BlockHashOrNumber::Hash(block.hash()), TransactionVariant::WithHash),
                    block.clone().try_recover().ok()
                ),
                (BlockHashOrNumber::Hash(B256::random()), TransactionVariant::WithHash)
            ),
            (
                TWO,
                sealed_block_with_senders,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    (BlockHashOrNumber::Number(block.number), TransactionVariant::WithHash),
                    block.clone().try_recover().ok()
                ),
                (BlockHashOrNumber::Number(u64::MAX), TransactionVariant::WithHash)
            ),
            (
                TWO,
                sealed_block_with_senders,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    (BlockHashOrNumber::Hash(block.hash()), TransactionVariant::WithHash),
                    block.clone().try_recover().ok()
                ),
                (BlockHashOrNumber::Hash(B256::random()), TransactionVariant::WithHash)
            ),
            (
                ONE,
                transaction_id,
                |_: &SealedBlock<Block>, tx_num: TxNumber, tx_hash: B256, _: &Vec<Vec<Receipt>>| (
                    tx_hash,
                    Some(tx_num)
                ),
                B256::random()
            ),
            (
                ONE,
                transaction_by_id,
                |block: &SealedBlock<Block>, tx_num: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    tx_num,
                    Some(block.body().transactions[test_tx_index].clone())
                ),
                u64::MAX
            ),
            (
                ONE,
                transaction_by_id_unhashed,
                |block: &SealedBlock<Block>, tx_num: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    tx_num,
                    Some(block.body().transactions[test_tx_index].clone())
                ),
                u64::MAX
            ),
            (
                ONE,
                transaction_by_hash,
                |block: &SealedBlock<Block>, _: TxNumber, tx_hash: B256, _: &Vec<Vec<Receipt>>| (
                    tx_hash,
                    Some(block.body().transactions[test_tx_index].clone())
                ),
                B256::random()
            ),
            (
                ONE,
                block_by_transaction_id,
                |block: &SealedBlock<Block>, tx_num: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    tx_num,
                    Some(block.number)
                ),
                u64::MAX
            ),
            (
                ONE,
                transactions_by_block,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    BlockHashOrNumber::Number(block.number),
                    Some(block.body().transactions.clone())
                ),
                BlockHashOrNumber::Number(u64::MAX)
            ),
            (
                ONE,
                transactions_by_block,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    BlockHashOrNumber::Hash(block.hash()),
                    Some(block.body().transactions.clone())
                ),
                BlockHashOrNumber::Number(u64::MAX)
            ),
            (
                ONE,
                transaction_sender,
                |block: &SealedBlock<Block>, tx_num: TxNumber, _: B256, _: &Vec<Vec<Receipt>>| (
                    tx_num,
                    block.body().transactions[test_tx_index].recover_signer().ok()
                ),
                u64::MAX
            ),
            (
                ONE,
                receipt,
                |block: &SealedBlock<Block>,
                 tx_num: TxNumber,
                 _: B256,
                 receipts: &Vec<Vec<Receipt>>| (
                    tx_num,
                    Some(receipts[block.number as usize][test_tx_index].clone())
                ),
                u64::MAX
            ),
            (
                ONE,
                receipt_by_hash,
                |block: &SealedBlock<Block>,
                 _: TxNumber,
                 tx_hash: B256,
                 receipts: &Vec<Vec<Receipt>>| (
                    tx_hash,
                    Some(receipts[block.number as usize][test_tx_index].clone())
                ),
                B256::random()
            ),
            (
                ONE,
                receipts_by_block,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, receipts: &Vec<Vec<Receipt>>| (
                    BlockHashOrNumber::Number(block.number),
                    Some(receipts[block.number as usize].clone())
                ),
                BlockHashOrNumber::Number(u64::MAX)
            ),
            (
                ONE,
                receipts_by_block,
                |block: &SealedBlock<Block>, _: TxNumber, _: B256, receipts: &Vec<Vec<Receipt>>| (
                    BlockHashOrNumber::Hash(block.hash()),
                    Some(receipts[block.number as usize].clone())
                ),
                BlockHashOrNumber::Hash(B256::random())
            ),
            // TODO: withdrawals, requests, ommers
        ]);

        Ok(())
    }

    #[test]
    fn test_race() -> eyre::Result<()> {
        let mut rng = generators::rng();
        let (provider, _, in_memory_blocks, _) = provider_with_random_blocks(
            &mut rng,
            TEST_BLOCKS_COUNT - 1,
            TEST_BLOCKS_COUNT + 1,
            BlockRangeParams {
                tx_count: TEST_TRANSACTIONS_COUNT..TEST_TRANSACTIONS_COUNT,
                ..Default::default()
            },
        )?;

        // Correct implementation queries in-memory first. (The historical database-first
        // implementation relied on snapshot-isolated read transactions, which the current
        // backend does not provide, so its stale-view scenario is not reproducible here.)
        let correct_transaction_hash_fn =
            |hash: B256,
             canonical_in_memory_state: CanonicalInMemoryState,
             _factory: ProviderFactory<MockNodeTypesWithDB>| {
                if let Some(tx) = canonical_in_memory_state.transaction_by_hash(hash) {
                    return Ok::<_, ProviderError>(Some(tx));
                }
                panic!("should not be in database");
                // _factory.transaction_by_hash(hash)
            };

        // CORRECT BEHAVIOUR
        {
            // This will persist block 1 AFTER a database is created. Moving it from memory to
            // storage.
            persist_block_after_db_tx_creation(provider.clone(), in_memory_blocks[1].number);
            let to_be_persisted_tx = in_memory_blocks[1].body().transactions[0].clone();

            assert_eq!(
                correct_transaction_hash_fn(
                    *to_be_persisted_tx.tx_hash(),
                    provider.canonical_in_memory_state(),
                    provider.database
                )
                .unwrap(),
                Some(to_be_persisted_tx)
            );
        }

        Ok(())
    }
}
