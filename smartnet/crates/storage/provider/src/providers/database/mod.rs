use crate::{
    either_writer::RawAuxDbBatch,
    providers::{state::latest::LatestStateProvider, AuxDbProvider, NodeTypesForProvider},
    traits::{BlockSource, ReceiptProvider},
    AuxDbProviderFactory, BlockHashReader, BlockNumReader, BlockReader, ChainSpecProvider,
    DatabaseProviderFactory, HashedPostStateProvider, HeaderProvider, HeaderSyncGapProvider,
    MetadataProvider, ProviderError, PruneCheckpointReader, StageCheckpointReader,
    StateProviderBox, TransactionVariant, TransactionsProvider,
};
use alloy_consensus::transaction::TransactionMeta;
use alloy_eips::BlockHashOrNumber;
use alloy_primitives::{Address, BlockHash, BlockNumber, TxHash, TxNumber, B256};
use core::fmt;
use parking_lot::RwLock;
use revm_database::BundleState;
use smartnet_chainspec::ChainInfo;
use smartnet_db::{init_db, DatabaseArguments, DatabaseEnv};
use smartnet_db_api::{database::Database, models::StoredBlockBodyIndices};
use smartnet_errors::{SmartnetError, SmartnetResult};
use smartnet_node_types::{
    BlockTy, HeaderTy, NodeTypesWithDB, NodeTypesWithDBAdapter, ReceiptTy, TxTy,
};
use smartnet_primitives_traits::{RecoveredBlock, SealedHeader};
use smartnet_prune_types::{
    PruneCheckpoint, PruneModes, PruneSegment, MINIMUM_UNWIND_SAFE_DISTANCE,
};
use smartnet_stages_types::{StageCheckpoint, StageId};
use smartnet_storage_api::{
    BlockBodyIndicesProvider, ChainStateBlockReader, ChainStateBlockWriter, DBProvider,
    NodePrimitivesProvider, StorageSettings, StorageSettingsCache, TryIntoHistoricalStateProvider,
};
use smartnet_storage_errors::provider::ProviderResult;
use smartnet_trie::HashedPostState;
use smartnet_trie_db::ChangesetCache;
use std::{
    ops::{RangeBounds, RangeInclusive},
    path::Path,
    sync::Arc,
};
use tracing::{info, instrument, trace};

mod provider;
pub use provider::{DatabaseProvider, DatabaseProviderRO, DatabaseProviderRW, SaveBlocksMode};

use super::ProviderNodeTypes;
use smartnet_trie::KeccakKeyHasher;

mod builder;
pub use builder::{ProviderFactoryBuilder, ReadOnlyConfig};

mod metrics;

mod chain;
pub use chain::*;

/// A common provider that fetches data from the database.
///
/// This provider implements most provider or provider factory traits.
pub struct ProviderFactory<N: NodeTypesWithDB> {
    /// Database instance
    db: N::DB,
    /// Chain spec
    chain_spec: Arc<N::ChainSpec>,
    /// Optional pruning configuration
    prune_modes: PruneModes,
    /// The node storage handler.
    storage: Arc<N::Storage>,
    /// Storage configuration settings for this node
    storage_settings: Arc<RwLock<StorageSettings>>,
    /// Optional `aux DB` provider.
    aux_db_provider: Option<AuxDbProvider>,
    /// Changeset cache for trie unwinding
    changeset_cache: ChangesetCache,
    /// Task runtime for spawning parallel I/O work.
    runtime: smartnet_tasks::Runtime,
    /// Minimum distance from tip required before pruning can occur.
    minimum_pruning_distance: u64,
}

impl<N: NodeTypesForProvider> ProviderFactory<NodeTypesWithDBAdapter<N, DatabaseEnv>> {
    /// Instantiates the builder for this type
    pub fn builder() -> ProviderFactoryBuilder<N> {
        ProviderFactoryBuilder::default()
    }
}

impl<N: ProviderNodeTypes> ProviderFactory<N> {
    /// Create new database provider factory.
    pub fn new(
        db: N::DB,
        chain_spec: Arc<N::ChainSpec>,
        aux_db_provider: Option<AuxDbProvider>,
        runtime: smartnet_tasks::Runtime,
    ) -> ProviderResult<Self> {
        // Calibri Lite runtime: v1 layout, archival history indexing off by default.
        let legacy_settings = StorageSettings::calibri_lite_runtime();

        Ok(Self {
            db,
            chain_spec,
            prune_modes: PruneModes::default(),
            storage: Default::default(),
            storage_settings: Arc::new(RwLock::new(legacy_settings)),
            aux_db_provider,
            changeset_cache: ChangesetCache::new(),
            runtime,
            minimum_pruning_distance: MINIMUM_UNWIND_SAFE_DISTANCE,
        })
    }
}

impl<N: NodeTypesWithDB> ProviderFactory<N> {
    /// Sets the pruning configuration for an existing [`ProviderFactory`].
    pub fn with_prune_modes(mut self, prune_modes: PruneModes) -> Self {
        self.prune_modes = prune_modes;
        self
    }

    /// Sets the changeset cache for an existing [`ProviderFactory`].
    pub fn with_changeset_cache(mut self, changeset_cache: ChangesetCache) -> Self {
        self.changeset_cache = changeset_cache;
        self
    }

    /// Sets the minimum pruning distance for an existing [`ProviderFactory`].
    ///
    /// This controls the minimum distance from tip required before pruning can occur.
    /// The default is [`MINIMUM_UNWIND_SAFE_DISTANCE`].
    pub const fn with_minimum_pruning_distance(mut self, distance: u64) -> Self {
        self.minimum_pruning_distance = distance;
        self
    }

    /// Returns reference to the underlying database.
    pub const fn db_ref(&self) -> &N::DB {
        &self.db
    }

    #[cfg(any(test, feature = "test-utils"))]
    /// Consumes Self and returns DB
    pub fn into_db(self) -> N::DB {
        self.db
    }
}

impl<N: NodeTypesWithDB> StorageSettingsCache for ProviderFactory<N> {
    fn cached_storage_settings(&self) -> StorageSettings {
        *self.storage_settings.read()
    }

    fn set_storage_settings_cache(&self, settings: StorageSettings) {
        *self.storage_settings.write() = settings;
    }
}

impl<N: NodeTypesWithDB> AuxDbProviderFactory for ProviderFactory<N> {
    fn aux_db_provider(&self) -> AuxDbProvider {
        self.aux_db_provider
            .clone()
            .unwrap_or_else(|| AuxDbProvider::builder(std::path::PathBuf::new()).build().unwrap())
    }

    fn set_pending_aux_db_batch(&self, _batch: RawAuxDbBatch) {
        unimplemented!("ProviderFactory is a factory, not a provider - use DatabaseProvider::set_pending_aux_db_batch instead")
    }

    fn commit_pending_aux_db_batches(&self) -> ProviderResult<()> {
        unimplemented!("ProviderFactory is a factory, not a provider - use DatabaseProvider::commit_pending_aux_db_batches instead")
    }
}

impl<N: ProviderNodeTypes<DB = DatabaseEnv>> ProviderFactory<N> {
    /// Create new database provider by passing a path. [`ProviderFactory`] will own the database
    /// instance.
    pub fn new_with_database_path<P: AsRef<Path>>(
        path: P,
        chain_spec: Arc<N::ChainSpec>,
        args: DatabaseArguments,
        aux_db_provider: Option<AuxDbProvider>,
        runtime: smartnet_tasks::Runtime,
    ) -> SmartnetResult<Self> {
        Self::new(
            init_db(path, args).map_err(SmartnetError::msg)?,
            chain_spec,
            aux_db_provider,
            runtime,
        )
        .map_err(SmartnetError::Provider)
    }
}

impl<N: ProviderNodeTypes> ProviderFactory<N> {
    /// Returns a provider with a created `DbTx` inside, which allows fetching data from the
    /// database using different types of providers. Example: [`HeaderProvider`]
    /// [`BlockHashReader`]. This may fail if the inner read database transaction fails to open.
    ///
    /// This sets the [`PruneModes`] to [`None`], because they should only be relevant for writing
    /// data.
    #[track_caller]
    pub fn provider(&self) -> ProviderResult<DatabaseProviderRO<N::DB, N>> {
        let db_tx = self.db.tx()?;

        Ok(DatabaseProvider::new(
            db_tx,
            self.chain_spec.clone(),
            self.prune_modes.clone(),
            self.storage.clone(),
            self.storage_settings.clone(),
            self.aux_db_provider.clone(),
            self.changeset_cache.clone(),
            self.runtime.clone(),
            self.db.path(),
        )
        .with_minimum_pruning_distance(self.minimum_pruning_distance))
    }

    /// Returns a provider with a created `DbTxMut` inside, which allows fetching and updating
    /// data from the database using different types of providers. Example: [`HeaderProvider`]
    /// [`BlockHashReader`].  This may fail if the inner read/write database transaction fails to
    /// open.
    #[track_caller]
    pub fn provider_rw(&self) -> ProviderResult<DatabaseProviderRW<N::DB, N>> {
        Ok(DatabaseProviderRW(
            DatabaseProvider::new_rw(
                self.db.tx_mut()?,
                self.chain_spec.clone(),
                self.prune_modes.clone(),
                self.storage.clone(),
                self.storage_settings.clone(),
                self.aux_db_provider.clone(),
                self.changeset_cache.clone(),
                self.runtime.clone(),
                self.db.path(),
            )
            .with_minimum_pruning_distance(self.minimum_pruning_distance),
        ))
    }

    /// State provider for latest block
    #[track_caller]
    pub fn latest(&self) -> ProviderResult<StateProviderBox> {
        trace!(target: "providers::db", "Returning latest state provider");
        Ok(Box::new(LatestStateProvider::new(self.database_provider_ro()?)))
    }

    /// Storage provider for state at that given block
    pub fn history_by_block_number(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<StateProviderBox> {
        let state_provider = self.provider()?.try_into_history_at_block(block_number)?;
        trace!(target: "providers::db", ?block_number, "Returning historical state provider for block number");
        Ok(state_provider)
    }

    /// Storage provider for state at that given block hash
    pub fn history_by_block_hash(&self, block_hash: BlockHash) -> ProviderResult<StateProviderBox> {
        let provider = self.provider()?;

        let block_number = provider
            .block_number(block_hash)?
            .ok_or(ProviderError::BlockHashNotFound(block_hash))?;

        let state_provider = provider.try_into_history_at_block(block_number)?;
        trace!(target: "providers::db", ?block_number, %block_hash, "Returning historical state provider for block hash");
        Ok(state_provider)
    }

    /// Checks consistency and heals chain-state block numbers if needed.
    ///
    /// Both former unwind targets (aux DB, static files) are gone: this fork hard-forces
    /// v1 aux-DB semantics and static files are retired, so there is nothing left to
    /// compare against the database. The only remaining duty is healing finalized/safe
    /// block numbers that may be ahead of the highest header on legacy datadirs.
    #[instrument(err, skip(self))]
    pub fn check_consistency(&self) -> ProviderResult<()> {
        let provider_ro = self
            .database_provider_ro()?
            // Healing can run long-lived read transactions (e.g., iterating changesets
            // over millions of blocks). Disable the default timeout so a long-running read
            // does not get killed mid-heal, which would cause a crash loop on startup.
            .disable_long_read_transaction_safety();

        self.heal_chain_state_block_numbers(&provider_ro)?;
        Ok(())
    }

    /// If the stored finalized or safe block number is ahead of the highest
    /// header, resets it to the highest header.
    fn heal_chain_state_block_numbers(
        &self,
        provider_ro: &DatabaseProvider<<N::DB as Database>::TX, N>,
    ) -> ProviderResult<()> {
        let highest_header = self.last_block_number()?;

        let finalized = provider_ro.last_finalized_block_number()?;
        let safe = provider_ro.last_safe_block_number()?;

        if finalized.is_none_or(|f| f <= highest_header) && safe.is_none_or(|s| s <= highest_header)
        {
            return Ok(());
        }

        let provider_rw = self.provider_rw()?;

        if let Some(finalized) = finalized.filter(|&f| f > highest_header) {
            info!(
                target: "providers::db",
                finalized,
                highest_header,
                "Healing finalized block number",
            );
            provider_rw.save_finalized_block_number(highest_header)?;
        }

        if let Some(safe) = safe.filter(|&s| s > highest_header) {
            info!(
                target: "providers::db",
                safe,
                highest_header,
                "Healing safe block number",
            );
            provider_rw.save_safe_block_number(highest_header)?;
        }

        provider_rw.commit()?;

        Ok(())
    }
}

impl<N: NodeTypesWithDB> NodePrimitivesProvider for ProviderFactory<N> {
    type Primitives = N::Primitives;
}

impl<N: ProviderNodeTypes> DatabaseProviderFactory for ProviderFactory<N> {
    type DB = N::DB;
    type Provider = DatabaseProvider<<N::DB as Database>::TX, N>;
    type ProviderRW = DatabaseProvider<<N::DB as Database>::TXMut, N>;

    fn database_provider_ro(&self) -> ProviderResult<Self::Provider> {
        self.provider()
    }

    fn database_provider_rw(&self) -> ProviderResult<Self::ProviderRW> {
        self.provider_rw().map(|provider| provider.0)
    }
}

impl<N: ProviderNodeTypes> HeaderSyncGapProvider for ProviderFactory<N> {
    type Header = HeaderTy<N>;
    fn local_tip_header(
        &self,
        highest_uninterrupted_block: BlockNumber,
    ) -> ProviderResult<SealedHeader<Self::Header>> {
        self.provider()?.local_tip_header(highest_uninterrupted_block)
    }
}

impl<N: ProviderNodeTypes> HeaderProvider for ProviderFactory<N> {
    type Header = HeaderTy<N>;

    fn header(&self, block_hash: BlockHash) -> ProviderResult<Option<Self::Header>> {
        self.provider()?.header(block_hash)
    }

    fn header_by_number(&self, num: BlockNumber) -> ProviderResult<Option<Self::Header>> {
        self.provider()?.header_by_number(num)
    }

    fn headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Self::Header>> {
        self.provider()?.headers_range(range)
    }

    fn sealed_header(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        self.provider()?.sealed_header(number)
    }

    fn sealed_headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        self.provider()?.sealed_headers_range(range)
    }

    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        self.provider()?.sealed_headers_while(range, predicate)
    }
}

impl<N: ProviderNodeTypes> BlockHashReader for ProviderFactory<N> {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.provider()?.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.provider()?.canonical_hashes_range(start, end)
    }
}

impl<N: ProviderNodeTypes> BlockNumReader for ProviderFactory<N> {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        self.provider()?.chain_info()
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        self.provider()?.best_block_number()
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        self.provider()?.last_block_number()
    }

    fn earliest_block_number(&self) -> ProviderResult<BlockNumber> {
        // earliest block number tracks the first block that has __not__ been expired, in
        // other words, the first/earliest available block.
        self.provider()?.earliest_block_number()
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        self.provider()?.block_number(hash)
    }
}

impl<N: ProviderNodeTypes> BlockReader for ProviderFactory<N> {
    type Block = BlockTy<N>;

    fn find_block_by_hash(
        &self,
        hash: B256,
        source: BlockSource,
    ) -> ProviderResult<Option<Self::Block>> {
        self.provider()?.find_block_by_hash(hash, source)
    }

    fn block(&self, id: BlockHashOrNumber) -> ProviderResult<Option<Self::Block>> {
        self.provider()?.block(id)
    }

    fn pending_block(&self) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.provider()?.pending_block()
    }

    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<(RecoveredBlock<Self::Block>, Vec<Self::Receipt>)>> {
        self.provider()?.pending_block_and_receipts()
    }

    fn recovered_block(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.provider()?.recovered_block(id, transaction_kind)
    }

    fn sealed_block_with_senders(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.provider()?.sealed_block_with_senders(id, transaction_kind)
    }

    fn block_range(&self, range: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<Self::Block>> {
        self.provider()?.block_range(range)
    }

    fn block_by_transaction_id(&self, id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        self.provider()?.block_by_transaction_id(id)
    }
}

impl<N: ProviderNodeTypes> TransactionsProvider for ProviderFactory<N> {
    type Transaction = TxTy<N>;

    fn transaction_id(&self, tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        self.provider()?.transaction_id(tx_hash)
    }

    fn transaction_by_id(&self, id: TxNumber) -> ProviderResult<Option<Self::Transaction>> {
        self.provider()?.transaction_by_id(id)
    }

    fn transaction_by_id_unhashed(
        &self,
        id: TxNumber,
    ) -> ProviderResult<Option<Self::Transaction>> {
        self.provider()?.transaction_by_id_unhashed(id)
    }

    fn transaction_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Transaction>> {
        self.provider()?.transaction_by_hash(hash)
    }

    fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> ProviderResult<Option<(Self::Transaction, TransactionMeta)>> {
        self.provider()?.transaction_by_hash_with_meta(tx_hash)
    }

    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Transaction>>> {
        self.provider()?.transactions_by_block(id)
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Transaction>>> {
        self.provider()?.transactions_by_block_range(range)
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Transaction>> {
        self.provider()?.transactions_by_tx_range(range)
    }

    fn senders_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        self.provider()?.senders_by_tx_range(range)
    }

    fn transaction_sender(&self, id: TxNumber) -> ProviderResult<Option<Address>> {
        self.provider()?.transaction_sender(id)
    }
}

impl<N: ProviderNodeTypes> ReceiptProvider for ProviderFactory<N> {
    type Receipt = ReceiptTy<N>;

    fn receipt(&self, id: TxNumber) -> ProviderResult<Option<Self::Receipt>> {
        self.provider()?.receipt(id)
    }

    fn receipt_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Receipt>> {
        self.provider()?.receipt_by_hash(hash)
    }

    fn receipts_by_block(
        &self,
        block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        self.provider()?.receipts_by_block(block)
    }

    fn receipts_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Receipt>> {
        self.provider()?.receipts_by_tx_range(range)
    }

    fn receipts_by_block_range(
        &self,
        block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Receipt>>> {
        self.provider()?.receipts_by_block_range(block_range)
    }
}

impl<N: ProviderNodeTypes> BlockBodyIndicesProvider for ProviderFactory<N> {
    fn block_body_indices(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        self.provider()?.block_body_indices(number)
    }

    fn block_body_indices_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        self.provider()?.block_body_indices_range(range)
    }
}

impl<N: ProviderNodeTypes> StageCheckpointReader for ProviderFactory<N> {
    fn get_stage_checkpoint(&self, id: StageId) -> ProviderResult<Option<StageCheckpoint>> {
        self.provider()?.get_stage_checkpoint(id)
    }

    fn get_stage_checkpoint_progress(&self, id: StageId) -> ProviderResult<Option<Vec<u8>>> {
        self.provider()?.get_stage_checkpoint_progress(id)
    }
    fn get_all_checkpoints(&self) -> ProviderResult<Vec<(String, StageCheckpoint)>> {
        self.provider()?.get_all_checkpoints()
    }
}

impl<N: NodeTypesWithDB> ChainSpecProvider for ProviderFactory<N> {
    type ChainSpec = N::ChainSpec;

    fn chain_spec(&self) -> Arc<N::ChainSpec> {
        self.chain_spec.clone()
    }
}

impl<N: ProviderNodeTypes> PruneCheckpointReader for ProviderFactory<N> {
    fn get_prune_checkpoint(
        &self,
        segment: PruneSegment,
    ) -> ProviderResult<Option<PruneCheckpoint>> {
        self.provider()?.get_prune_checkpoint(segment)
    }

    fn get_prune_checkpoints(&self) -> ProviderResult<Vec<(PruneSegment, PruneCheckpoint)>> {
        self.provider()?.get_prune_checkpoints()
    }
}

impl<N: ProviderNodeTypes> HashedPostStateProvider for ProviderFactory<N> {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<N: ProviderNodeTypes> MetadataProvider for ProviderFactory<N> {
    fn get_metadata(&self, key: &str) -> ProviderResult<Option<Vec<u8>>> {
        self.provider()?.get_metadata(key)
    }
}

impl<N> fmt::Debug for ProviderFactory<N>
where
    N: NodeTypesWithDB<DB: fmt::Debug, ChainSpec: fmt::Debug, Storage: fmt::Debug>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            db,
            chain_spec,
            prune_modes,
            storage,
            storage_settings,
            aux_db_provider,
            changeset_cache,
            runtime,
            minimum_pruning_distance,
        } = self;
        f.debug_struct("ProviderFactory")
            .field("db", &db)
            .field("chain_spec", &chain_spec)
            .field("prune_modes", &prune_modes)
            .field("storage", &storage)
            .field("storage_settings", &*storage_settings.read())
            .field("aux_db_provider", &aux_db_provider)
            .field("changeset_cache", &changeset_cache)
            .field("runtime", &runtime)
            .field("minimum_pruning_distance", &minimum_pruning_distance)
            .finish()
    }
}

impl<N: NodeTypesWithDB> Clone for ProviderFactory<N> {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            chain_spec: self.chain_spec.clone(),
            prune_modes: self.prune_modes.clone(),
            storage: self.storage.clone(),
            storage_settings: self.storage_settings.clone(),
            aux_db_provider: self.aux_db_provider.clone(),
            changeset_cache: self.changeset_cache.clone(),
            runtime: self.runtime.clone(),
            minimum_pruning_distance: self.minimum_pruning_distance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{create_test_provider_factory, MockNodeTypesWithDB};
    use alloy_primitives::B256;
    use smartnet_chainspec::ChainSpecBuilder;
    use smartnet_db::{test_utils::ERROR_TEMPDIR, DatabaseArguments};
    use std::sync::Arc;

    #[test]
    fn common_history_provider() {
        let factory = create_test_provider_factory();
        let _ = factory.latest();
    }

    #[test]
    fn default_chain_info() {
        let factory = create_test_provider_factory();
        let provider = factory.provider().unwrap();

        let chain_info = provider.chain_info().expect("should be ok");
        assert_eq!(chain_info.best_number, 0);
        assert_eq!(chain_info.best_hash, B256::ZERO);
    }

    #[test]
    fn provider_flow() {
        let factory = create_test_provider_factory();
        let provider = factory.provider().unwrap();
        provider.block_hash(0).unwrap();
        let provider_rw = factory.provider_rw().unwrap();
        provider_rw.block_hash(0).unwrap();
        provider.block_hash(0).unwrap();
    }

    #[test]
    fn provider_factory_with_database_path() {
        let chain_spec = ChainSpecBuilder::mainnet().build();
        let _db_tempdir = tempfile::TempDir::new().expect(ERROR_TEMPDIR);
        let factory = ProviderFactory::<MockNodeTypesWithDB<DatabaseEnv>>::new_with_database_path(
            _db_tempdir.path(),
            Arc::new(chain_spec),
            DatabaseArguments::new(Default::default()),
            None,
            smartnet_tasks::Runtime::test(),
        )
        .unwrap();
        let provider = factory.provider().unwrap();
        provider.block_hash(0).unwrap();
        let provider_rw = factory.provider_rw().unwrap();
        provider_rw.block_hash(0).unwrap();
        provider.block_hash(0).unwrap();
    }
}
