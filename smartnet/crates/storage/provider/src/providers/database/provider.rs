use crate::{
    changesets_utils::StorageRevertsIter,
    providers::{
        aux_db::{AuxDbProvider, PendingAuxDbBatches},
        database::{chain::ChainStorage, metrics},
        NodeTypesForProvider,
    },
    to_range,
    traits::{
        AccountExtReader, BlockSource, ChangeSetReader, ReceiptProvider, StageCheckpointWriter,
    },
    AccountReader, AuxDbBatchArg, AuxDbProviderFactory, BlockBodyWriter, BlockExecutionWriter,
    BlockHashReader, BlockNumReader, BlockReader, BlockWriter, BundleStateInit,
    ChainStateBlockReader, ChainStateBlockWriter, DBProvider, EitherReader, EitherWriter,
    HashingWriter, HeaderProvider, HeaderSyncGapProvider, HistoricalStateProvider,
    HistoricalStateProviderRef, HistoryWriter, LatestStateProvider, LatestStateProviderRef,
    OriginalValuesKnown, ProviderError, PruneCheckpointReader, PruneCheckpointWriter,
    RawAuxDbBatch, RevertsInit, StageCheckpointReader, StateProviderBox, StateWriter, StatsReader,
    StorageReader, StorageTrieWriter, TransactionVariant, TransactionsProvider,
    TransactionsProviderExt, TrieWriter,
};
use alloy_consensus::{
    transaction::{TransactionMeta, TxHashRef},
    BlockHeader,
};
use alloy_eips::{eip4895::Withdrawals, BlockHashOrNumber};
use alloy_primitives::{
    keccak256,
    map::{hash_map, B256Map, HashMap},
    Address, BlockHash, BlockNumber, Log, TxHash, TxNumber, B256,
};
use itertools::Itertools;
use parking_lot::RwLock;
use rayon::slice::ParallelSliceMut;
use revm_database::states::{
    PlainStateReverts, PlainStorageChangeset, PlainStorageRevert, StateChangeset,
};
use smartnet_chain_state::ExecutedBlock;
use smartnet_chainspec::{ChainInfo, ChainSpecProvider, EthChainSpec, EthereumHardforks};
use smartnet_db_api::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    database::Database,
    models::{
        sharded_key, storage_sharded_key::StorageShardedKey, AccountBeforeTx, BlockNumberAddress,
        BlockNumberAddressRange, ShardedKey, StorageBeforeTx, StorageSettings,
        StoredBlockBodyIndices, StoredTransactionLocation,
    },
    table::Table,
    tables,
    transaction::{DbTx, DbTxMut},
    BlockNumberList, PlainAccountState, PlainStorageState,
};
#[cfg(any(test, feature = "test-utils"))]
use smartnet_execution_types::ExecutionOutcome;
use smartnet_file_tier::{
    build_withdrawals, read_block, read_blocks_range_at, BlockBundle, BtRecord, BuildBody,
    ReceiptBuild, TxRecord,
};
use smartnet_node_types::{BlockTy, BodyTy, HeaderTy, NodeTypes, ReceiptTy, TxTy};
use smartnet_primitives_traits::{
    Account, Block as _, BlockBody as _, Bytecode, FastInstant as Instant, RecoveredBlock,
    SealedHeader, SignedTransaction, StorageEntry,
};
use smartnet_prune_types::{
    PruneCheckpoint, PruneModes, PruneSegment, MINIMUM_UNWIND_SAFE_DISTANCE,
};
use smartnet_stages_types::{StageCheckpoint, StageId};
use smartnet_storage_api::{
    metadata::keys::CHANGESET_WINDOW_FLOOR, BlockBodyIndicesProvider,
    MetadataProvider, MetadataWriter, NodePrimitivesProvider, StateProvider, StateWriteConfig,
    StorageChangeSetReader, StoragePath, StorageSettingsCache, TryIntoHistoricalStateProvider,
    WriteStateInput,
};
use smartnet_storage_errors::provider::ProviderResult;
use smartnet_trie::{
    updates::{StorageTrieUpdatesSorted, TrieUpdatesSorted},
    HashedPostStateSorted,
};
use smartnet_trie_db::{ChangesetCache, DatabaseStorageTrieCursor, TrieTableAdapter};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    ops::{Deref, DerefMut, Range, RangeBounds, RangeInclusive},
    path::{Path, PathBuf},
    sync::Arc,
};
#[cfg(any(test, feature = "test-utils"))]
use tracing::debug;
use tracing::{instrument, trace};

/// A [`DatabaseProvider`] that holds a read-only database transaction.
pub type DatabaseProviderRO<DB, N> = DatabaseProvider<<DB as Database>::TX, N>;

/// A [`DatabaseProvider`] that holds a read-write database transaction.
///
/// Ideally this would be an alias type. However, there's some weird compiler error (<https://github.com/rust-lang/rust/issues/102211>), that forces us to wrap this in a struct instead.
/// Once that issue is solved, we can probably revert back to being an alias type.
#[derive(Debug)]
pub struct DatabaseProviderRW<DB: Database, N: NodeTypes>(
    pub DatabaseProvider<<DB as Database>::TXMut, N>,
);

impl<DB: Database, N: NodeTypes> Deref for DatabaseProviderRW<DB, N> {
    type Target = DatabaseProvider<<DB as Database>::TXMut, N>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<DB: Database, N: NodeTypes> DerefMut for DatabaseProviderRW<DB, N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<DB: Database, N: NodeTypes> AsRef<DatabaseProvider<<DB as Database>::TXMut, N>>
    for DatabaseProviderRW<DB, N>
{
    fn as_ref(&self) -> &DatabaseProvider<<DB as Database>::TXMut, N> {
        &self.0
    }
}

impl<DB: Database, N: NodeTypes + 'static> DatabaseProviderRW<DB, N> {
    /// Commit database transaction.
    pub fn commit(self) -> ProviderResult<()> {
        self.0.commit()
    }

    /// Consume `DbTx` or `DbTxMut`.
    pub fn into_tx(self) -> <DB as Database>::TXMut {
        self.0.into_tx()
    }

    /// Override the minimum pruning distance for testing purposes.
    #[cfg(any(test, feature = "test-utils"))]
    pub const fn with_minimum_pruning_distance(mut self, distance: u64) -> Self {
        self.0.minimum_pruning_distance = distance;
        self
    }
}

impl<DB: Database, N: NodeTypes> From<DatabaseProviderRW<DB, N>>
    for DatabaseProvider<<DB as Database>::TXMut, N>
{
    fn from(provider: DatabaseProviderRW<DB, N>) -> Self {
        provider.0
    }
}

/// Mode for [`DatabaseProvider::save_blocks`] (test-only payload writer).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveBlocksMode {
    /// Full mode: write block structure + receipts + state + trie.
    /// Used by engine/production code.
    Full,
    /// Blocks only: write block structure (headers, txs, senders, indices).
    /// Receipts/state/trie are skipped - they may come later via separate calls.
    /// Used by `insert_block`.
    BlocksOnly,
}

#[allow(dead_code)]
impl SaveBlocksMode {
    /// Returns `true` if this is [`SaveBlocksMode::Full`].
    pub const fn with_state(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// A provider struct that fetches data from the database.
/// Wrapper around [`DbTx`] and [`DbTxMut`]. Example: [`HeaderProvider`] [`BlockHashReader`]
pub struct DatabaseProvider<TX, N: NodeTypes> {
    /// Database transaction.
    tx: TX,
    /// Chain spec
    chain_spec: Arc<N::ChainSpec>,
    /// Pruning configuration
    prune_modes: PruneModes,
    /// Node storage handler.
    storage: Arc<N::Storage>,
    /// Storage configuration settings for this node
    storage_settings: Arc<RwLock<StorageSettings>>,
    /// Optional `aux DB` provider.
    aux_db_provider: Option<AuxDbProvider>,
    /// Changeset cache for trie unwinding
    changeset_cache: ChangesetCache,
    /// Task runtime for spawning parallel I/O work.
    runtime: smartnet_tasks::Runtime,
    /// Path to the database directory.
    db_path: PathBuf,
    /// The file-tier base **scoped to this database's datadir**: the process-global base
    /// only when it points at this database's datadir; `None` when it is unset or points
    /// at a *different* datadir (parallel tests each own their temp datadir — a foreign
    /// base must never be honored). Precomputed at construction so hot reads are a field
    /// access, not a global lookup.
    file_tier_base: Option<&'static Path>,
    /// Pending `aux DB` batches to be committed at provider commit time.
    pending_aux_db_batches: PendingAuxDbBatches,
    /// Minimum distance from tip required for pruning
    minimum_pruning_distance: u64,
    /// Database provider metrics
    metrics: metrics::DatabaseProviderMetrics,
}

impl<TX: Debug, N: NodeTypes> Debug for DatabaseProvider<TX, N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("DatabaseProvider");
        s.field("tx", &self.tx)
            .field("chain_spec", &self.chain_spec)
            .field("prune_modes", &self.prune_modes)
            .field("storage", &self.storage)
            .field("storage_settings", &self.storage_settings)
            .field("aux_db_provider", &self.aux_db_provider)
            .field("changeset_cache", &self.changeset_cache)
            .field("runtime", &self.runtime)
            .field("pending_aux_db_batches", &"<pending batches>")
            .field("minimum_pruning_distance", &self.minimum_pruning_distance)
            .finish()
    }
}

/// The process-global file-tier base, scoped to `db_path`'s datadir. Returns `None` when the
/// global base is unset **or belongs to a different datadir** (a single process may only own
/// one file-tier datadir; parallel tests each have their own, and a foreign base must not be
/// honored).
fn scoped_file_tier_base(db_path: &Path) -> Option<&'static Path> {
    let base = smartnet_file_tier::base()?;
    // Honor the global only when this database lives *under* it (`base` is the chain-level
    // ancestor: `<DIR>/<CHAIN_ID>` or deeper; db is `<...>/[ENGINE/]db`). A foreign datadir's
    // base (parallel tests each own a sibling temp dir) is never a path-prefix of this
    // database — component-wise `starts_with` cannot false-positive across random siblings.
    db_path.starts_with(base).then_some(base)
}

impl<TX, N: NodeTypes> DatabaseProvider<TX, N> {
    /// Returns reference to prune modes.
    pub const fn prune_modes_ref(&self) -> &PruneModes {
        &self.prune_modes
    }

    /// Sets the minimum pruning distance.
    pub const fn with_minimum_pruning_distance(mut self, distance: u64) -> Self {
        self.minimum_pruning_distance = distance;
        self
    }
}

impl<TX: DbTx + 'static, N: NodeTypes> DatabaseProvider<TX, N> {
    /// State provider for latest state
    pub fn latest<'a>(&'a self) -> Box<dyn StateProvider + 'a> {
        trace!(target: "providers::db", "Returning latest state provider");
        Box::new(LatestStateProviderRef::new(self))
    }

    /// Storage provider for state at that given block hash
    pub fn history_by_block_hash<'a>(
        &'a self,
        block_hash: BlockHash,
    ) -> ProviderResult<Box<dyn StateProvider + 'a>> {
        let mut block_number =
            self.block_number(block_hash)?.ok_or(ProviderError::BlockHashNotFound(block_hash))?;
        if block_number == self.best_block_number().unwrap_or_default() &&
            block_number == self.last_block_number().unwrap_or_default()
        {
            return Ok(Box::new(LatestStateProviderRef::new(self)));
        }

        // +1 as the changeset that we want is the one that was applied after this block.
        block_number += 1;

        let account_history_prune_checkpoint =
            self.get_prune_checkpoint(PruneSegment::AccountHistory)?;
        let storage_history_prune_checkpoint =
            self.get_prune_checkpoint(PruneSegment::StorageHistory)?;

        let mut state_provider = HistoricalStateProviderRef::new(self, block_number);

        // If we pruned account or storage history, we can't return state on every historical block.
        // Instead, we should cap it at the latest prune checkpoint for corresponding prune segment.
        if let Some(prune_checkpoint_block_number) =
            account_history_prune_checkpoint.and_then(|checkpoint| checkpoint.block_number)
        {
            state_provider = state_provider.with_lowest_available_account_history_block_number(
                prune_checkpoint_block_number + 1,
            );
        }
        if let Some(prune_checkpoint_block_number) =
            storage_history_prune_checkpoint.and_then(|checkpoint| checkpoint.block_number)
        {
            state_provider = state_provider.with_lowest_available_storage_history_block_number(
                prune_checkpoint_block_number + 1,
            );
        }

        Ok(Box::new(state_provider))
    }

    #[cfg(feature = "test-utils")]
    /// Sets the prune modes for provider.
    pub fn set_prune_modes(&mut self, prune_modes: PruneModes) {
        self.prune_modes = prune_modes;
    }
}

impl<TX, N: NodeTypes> NodePrimitivesProvider for DatabaseProvider<TX, N> {
    type Primitives = N::Primitives;
}

impl<TX, N: NodeTypes> AuxDbProviderFactory for DatabaseProvider<TX, N> {
    /// Returns the `aux DB` provider.
    fn aux_db_provider(&self) -> AuxDbProvider {
        // Without an `aux DB`, we may still have callers that expect a AuxDbProvider,
        // e.g. shutdown or tooling paths. When no real provider is wired, return a
        // stub instance so those paths become no-ops instead of panicking.
        self.aux_db_provider
            .clone()
            .unwrap_or_else(|| AuxDbProvider::builder(std::path::PathBuf::new()).build().unwrap())
    }

    fn set_pending_aux_db_batch(&self, batch: RawAuxDbBatch) {
        self.pending_aux_db_batches.lock().push(batch);
    }

    fn commit_pending_aux_db_batches(&self) -> ProviderResult<()> {
        let Some(aux_db_provider) = &self.aux_db_provider else {
            return Ok(());
        };
        let batches = std::mem::take(&mut *self.pending_aux_db_batches.lock());
        for batch in batches {
            aux_db_provider.commit_batch(batch)?;
        }
        Ok(())
    }
}

impl<TX: Debug + Send, N: NodeTypes<ChainSpec: EthChainSpec + 'static>> ChainSpecProvider
    for DatabaseProvider<TX, N>
{
    type ChainSpec = N::ChainSpec;

    fn chain_spec(&self) -> Arc<Self::ChainSpec> {
        self.chain_spec.clone()
    }
}

impl<TX: DbTxMut, N: NodeTypes> DatabaseProvider<TX, N> {
    /// Creates a provider with an inner read-write transaction.
    #[expect(clippy::too_many_arguments)]
    fn new_rw_inner(
        tx: TX,
        chain_spec: Arc<N::ChainSpec>,
        prune_modes: PruneModes,
        storage: Arc<N::Storage>,
        storage_settings: Arc<RwLock<StorageSettings>>,
        aux_db_provider: Option<AuxDbProvider>,
        changeset_cache: ChangesetCache,
        runtime: smartnet_tasks::Runtime,
        db_path: PathBuf,
    ) -> Self {
        Self {
            tx,
            chain_spec,
            prune_modes,
            storage,
            storage_settings,
            aux_db_provider,
            changeset_cache,
            runtime,
            file_tier_base: scoped_file_tier_base(&db_path),
            db_path,
            pending_aux_db_batches: Default::default(),
            minimum_pruning_distance: MINIMUM_UNWIND_SAFE_DISTANCE,
            metrics: metrics::DatabaseProviderMetrics::default(),
        }
    }

    /// Creates a provider with an inner read-write transaction.
    #[expect(clippy::too_many_arguments)]
    pub fn new_rw(
        tx: TX,
        chain_spec: Arc<N::ChainSpec>,
        prune_modes: PruneModes,
        storage: Arc<N::Storage>,
        storage_settings: Arc<RwLock<StorageSettings>>,
        aux_db_provider: Option<AuxDbProvider>,
        changeset_cache: ChangesetCache,
        runtime: smartnet_tasks::Runtime,
        db_path: PathBuf,
    ) -> Self {
        Self::new_rw_inner(
            tx,
            chain_spec,
            prune_modes,
            storage,
            storage_settings,
            aux_db_provider,
            changeset_cache,
            runtime,
            db_path,
        )
    }
}

impl<TX, N: NodeTypes> AsRef<Self> for DatabaseProvider<TX, N> {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl<TX: DbTx + DbTxMut + 'static, N: NodeTypesForProvider> DatabaseProvider<TX, N> {
    /// Executes a closure with a `aux DB` batch, automatically registering it for commit.
    ///
    /// This helper encapsulates all the cfg-gated `aux DB` batch handling.
    pub fn with_aux_db_batch<F, R>(&self, f: F) -> ProviderResult<R>
    where
        F: FnOnce(AuxDbBatchArg<'_>) -> ProviderResult<(R, Option<RawAuxDbBatch>)>,
    {
        let aux_db = self.aux_db_provider();
        let aux_db_batch = aux_db.batch();

        let (result, raw_batch) = f(aux_db_batch)?;

        if let Some(batch) = raw_batch {
            self.set_pending_aux_db_batch(batch);
        }
        let _ = raw_batch; // silence unused warning when aux_db feature is disabled

        Ok(result)
    }

    /// Test-only fixture writer: persists the index and state portion of executed blocks.
    ///
    /// The immutable payload (headers, transactions, receipts, logs) is never written to the
    /// database. Production appends it to the file tier and calls
    /// [`Self::save_block_state_and_index`].
    #[cfg(any(test, feature = "test-utils"))]
    #[instrument(level = "debug", target = "providers::db", skip_all, fields(block_count = blocks.len()))]
    pub fn save_blocks(
        &self,
        blocks: Vec<ExecutedBlock<N::Primitives>>,
        _save_mode: SaveBlocksMode,
    ) -> ProviderResult<()> {
        self.save_block_state_and_index(blocks)
    }

    /// The next transaction number to assign: one past the highest number recorded in
    /// [`tables::TxLocations`].
    ///
    /// `TxLocations` is the only numbering record now that the payload lives in the file
    /// tier. The file-tier write path stamps each block's first tx number from this counter
    /// into the block's `b` record (4.1).
    pub fn next_tx_number(&self) -> ProviderResult<TxNumber> {
        let from_locations = self.tx.cursor_read::<tables::TxLocations>()?.last()?.map(|(n, _)| n);
        Ok(from_locations.map(|n| n + 1).unwrap_or_default())
    }

    /// Writes only the *relevant part* of a batch of executed blocks to the primary database:
    /// the block index (`CanonicalChain`, `HeaderNumbers`, `TxLocations`, `TransactionHashNumbers`)
    /// and the resulting state (changesets, hashed state, trie, history).
    ///
    /// The immutable block payload (header/txs/receipts/logs) is **never** stored here — the
    /// engine appends it to the gz file tier and every read is served from there via this index.
    /// This keeps Fjall free of whole-block duplication; there is no write-then-handoff latency
    /// because the payload never enters the database.
    ///
    /// Transaction numbers are assigned from [`Self::next_tx_number`], so this path and the
    /// payload writer can never disagree on what the next number is.
    ///
    /// The `TransactionHashNumbers` rows obey the operator's `transaction_lookup` prune mode
    /// exactly as `save_blocks` and `insert_block` do: under `full` that index is wiped by the
    /// pruning stage, so writing it here would leave rows the mode says must not exist.
    /// `TxLocations` and `HeaderNumbers` locate blocks and transactions and are never pruned.
    #[instrument(level = "debug", target = "providers::db", skip_all, fields(block_count = blocks.len()))]
    pub fn save_block_state_and_index(
        &self,
        blocks: Vec<ExecutedBlock<N::Primitives>>,
    ) -> ProviderResult<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        let first_number = blocks.first().unwrap().recovered_block().number();
        let last_block_number = blocks.last().unwrap().recovered_block().number();

        let write_tx_hash_numbers =
            self.prune_modes.transaction_lookup.is_none_or(|m| !m.is_full());
        let mut next_tx_num = self.next_tx_number()?;

        for block in &blocks {
            let recovered_block = block.recovered_block();
            let number = recovered_block.number();
            let hash = recovered_block.hash();
            let tx_count = recovered_block.body().transaction_count() as u64;
            let first_tx_num = next_tx_num;

            // Index: locate the block and each of its transactions in the file tier.
            self.tx.put::<tables::CanonicalChain>(number, hash)?;
            self.tx.put::<tables::HeaderNumbers>(hash, number)?;
            for (idx, tx) in recovered_block.body().transactions().iter().enumerate() {
                let tx_num = first_tx_num + idx as u64;
                self.tx.put::<tables::TxLocations>(
                    tx_num,
                    StoredTransactionLocation::new(number, idx as u64),
                )?;
                if write_tx_hash_numbers {
                    self.tx.put::<tables::TransactionHashNumbers>(*tx.tx_hash(), tx_num)?;
                }
            }
            next_tx_num += tx_count;

            // State: changesets only (receipts/payload live in the file tier).
            self.write_state(
                WriteStateInput::Single { outcome: block.execution_outcome(), block: number },
                OriginalValuesKnown::No,
                StateWriteConfig {
                    write_receipts: false,
                    write_account_changesets: true,
                    write_storage_changesets: true,
                },
            )?;
        }

        let merged_hashed_state = HashedPostStateSorted::merge_batch(
            blocks.iter().rev().map(|b| b.trie_data().hashed_state),
        );
        if !merged_hashed_state.is_empty() {
            self.write_hashed_state(&merged_hashed_state)?;
        }

        let merged_trie =
            TrieUpdatesSorted::merge_batch(blocks.iter().rev().map(|b| b.trie_updates()));
        if !merged_trie.is_empty() {
            // Isolated timing: the trie-node write is the audit's key unknown
            // (TRIE-RES §7) — measured separately from the buffer/commit segments.
            let trie_write_start = Instant::now();
            self.write_trie_updates_sorted(&merged_trie)?;
            self.metrics.record_write_trie_updates(trie_write_start.elapsed());
        }

        self.update_history_indices(first_number..=last_block_number)?;
        self.update_pipeline_stages(last_block_number, false)?;
        Ok(())
    }

    /// Unwinds trie state starting at and including the given block.
    ///
    /// This includes calculating the resulted state root and comparing it with the parent block
    /// state root.
    pub fn unwind_trie_state_from(&self, from: BlockNumber) -> ProviderResult<()> {
        let changed_accounts = self.account_changesets_range(from..)?;

        // Unwind account hashes.
        self.unwind_account_hashing(changed_accounts.iter())?;

        // Unwind account history indices.
        self.unwind_account_history_indices(changed_accounts.iter())?;

        let changed_storages = self.storage_changesets_range(from..)?;

        // Unwind storage hashes.
        self.unwind_storage_hashing(changed_storages.iter().copied())?;

        // Unwind storage history indices.
        self.unwind_storage_history_indices(changed_storages.iter().copied())?;

        // Unwind accounts/storages trie tables using the revert.
        // Get the database tip block number
        let db_tip_block = self
            .get_stage_checkpoint(smartnet_stages_types::StageId::Finish)?
            .as_ref()
            .map(|chk| chk.block_number)
            .ok_or_else(|| ProviderError::InsufficientChangesets {
                requested: from,
                available: 0..=0,
            })?;

        let trie_revert = self.changeset_cache.get_or_compute_range(self, from..=db_tip_block)?;
        self.write_trie_updates_sorted(&trie_revert)?;

        Ok(())
    }

    /// Writes bytecodes to their 6C home: content-addressed files when the file tier is
    /// enabled, the `Bytecodes` table otherwise (tier-disabled test mode only).
    ///
    /// With the file tier enabled this is a *move* (spec §7: Fjall does not hold runtime
    /// bytecode): each code is written once to `bytecodes/{aa}/{bb}/{hash}` — raw bytes,
    /// no transcoding — ahead of the caller's transaction commit, so a crash leaves at
    /// most an orphan file, never a committed account whose code exists nowhere. There is
    /// **no fallback and no dupe**: when the tier is enabled the table is never written,
    /// and the readers never consult it (operator rule: bytecode is FS-only).
    fn write_bytecodes(
        &self,
        bytecodes: impl IntoIterator<Item = (B256, Bytecode)>,
    ) -> ProviderResult<()> {
        if let Some(base) = self.file_tier_base {
            for (hash, bytecode) in bytecodes {
                smartnet_file_tier::write_bytecode(base, &hash, bytecode.0.original_byte_slice())
                    .map_err(ProviderError::other)?;
            }
            return Ok(());
        }
        let mut bytecodes_cursor = self.tx_ref().cursor_write::<tables::Bytecodes>()?;
        for (hash, bytecode) in bytecodes {
            bytecodes_cursor.upsert(hash, &bytecode)?;
        }
        Ok(())
    }
}

impl<TX: DbTx + 'static, N: NodeTypes> TryIntoHistoricalStateProvider for DatabaseProvider<TX, N> {
    fn try_into_history_at_block(
        self,
        mut block_number: BlockNumber,
    ) -> ProviderResult<StateProviderBox> {
        let best_block = self.best_block_number().unwrap_or_default();

        // Reject requests for blocks beyond the best block
        if block_number > best_block {
            return Err(ProviderError::BlockNotExecuted {
                requested: block_number,
                executed: best_block,
            });
        }

        // If requesting state at the best block, use the latest state provider
        if block_number == best_block {
            return Ok(Box::new(LatestStateProvider::new(self)));
        }

        // Stage 5 changeset-window guard (D5): refuse a historical query below the changeset
        // floor. The limiter deletes rows for blocks below this floor, so any older state cannot
        // be reconstructed. Without this, the empty history index (default profile) would fall
        // through to plain state and silently return the *current* value for an old block.
        // The floor is written by the limiter in the same transaction that trims the tables, so
        // it never disagrees with what is actually retained.
        if let Some(floor_bytes) = self.get_metadata(CHANGESET_WINDOW_FLOOR)? {
            let mut floor_arr = [0u8; 8];
            floor_arr.copy_from_slice(&floor_bytes);
            let floor = u64::from_be_bytes(floor_arr);
            if block_number < floor {
                return Err(ProviderError::StateAtBlockPruned(block_number));
            }
        }

        // +1 as the changeset that we want is the one that was applied after this block.
        block_number += 1;

        let account_history_prune_checkpoint =
            self.get_prune_checkpoint(PruneSegment::AccountHistory)?;
        let storage_history_prune_checkpoint =
            self.get_prune_checkpoint(PruneSegment::StorageHistory)?;

        let mut state_provider = HistoricalStateProvider::new(self, block_number);

        // If we pruned account or storage history, we can't return state on every historical block.
        // Instead, we should cap it at the latest prune checkpoint for corresponding prune segment.
        if let Some(prune_checkpoint_block_number) =
            account_history_prune_checkpoint.and_then(|checkpoint| checkpoint.block_number)
        {
            state_provider = state_provider.with_lowest_available_account_history_block_number(
                prune_checkpoint_block_number + 1,
            );
        }
        if let Some(prune_checkpoint_block_number) =
            storage_history_prune_checkpoint.and_then(|checkpoint| checkpoint.block_number)
        {
            state_provider = state_provider.with_lowest_available_storage_history_block_number(
                prune_checkpoint_block_number + 1,
            );
        }

        Ok(Box::new(state_provider))
    }
}

/// For a given key, unwind all history shards that contain block numbers at or above the given
/// block number.
///
/// S - Sharded key subtype.
/// T - Table to walk over.
/// C - Cursor implementation.
///
/// This function walks the entries from the given start key and deletes all shards that belong to
/// the key and contain block numbers at or above the given block number. Shards entirely below
/// the block number are preserved.
///
/// The boundary shard (the shard that spans across the block number) is removed from the database.
/// Any indices that are below the block number are filtered out and returned for reinsertion.
/// The boundary shard is returned for reinsertion (if it's not empty).
fn unwind_history_shards<S, T, C>(
    cursor: &mut C,
    start_key: T::Key,
    block_number: BlockNumber,
    mut shard_belongs_to_key: impl FnMut(&T::Key) -> bool,
) -> ProviderResult<Vec<u64>>
where
    T: Table<Value = BlockNumberList>,
    T::Key: AsRef<ShardedKey<S>>,
    C: DbCursorRO<T> + DbCursorRW<T>,
{
    // Start from the given key and iterate through shards
    let mut item = cursor.seek_exact(start_key)?;
    while let Some((sharded_key, list)) = item {
        // If the shard does not belong to the key, break.
        if !shard_belongs_to_key(&sharded_key) {
            break;
        }

        // Always delete the current shard from the database first
        // We'll decide later what (if anything) to reinsert
        cursor.delete_current()?;

        // Get the first (lowest) block number in this shard
        // All block numbers in a shard are sorted in ascending order
        let first = list.iter().next().expect("List can't be empty");

        // Case 1: Entire shard is at or above the unwinding point
        // Keep it deleted (don't return anything for reinsertion)
        if first >= block_number {
            item = cursor.prev()?;
            continue;
        }
        // Case 2: This is a boundary shard (spans across the unwinding point)
        // The shard contains some blocks below and some at/above the unwinding point
        else if block_number <= sharded_key.as_ref().highest_block_number {
            // Return only the block numbers that are below the unwinding point
            // These will be reinserted to preserve the historical data
            return Ok(list.iter().take_while(|i| *i < block_number).collect::<Vec<_>>());
        }
        // Case 3: Entire shard is below the unwinding point
        // Return all block numbers for reinsertion (preserve entire shard)
        return Ok(list.iter().collect::<Vec<_>>());
    }

    // No shards found or all processed
    Ok(Vec::new())
}

impl<TX: DbTx + 'static, N: NodeTypesForProvider> DatabaseProvider<TX, N> {
    /// Creates a provider with an inner read-only transaction.
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        tx: TX,
        chain_spec: Arc<N::ChainSpec>,
        prune_modes: PruneModes,
        storage: Arc<N::Storage>,
        storage_settings: Arc<RwLock<StorageSettings>>,
        aux_db_provider: Option<AuxDbProvider>,
        changeset_cache: ChangesetCache,
        runtime: smartnet_tasks::Runtime,
        db_path: PathBuf,
    ) -> Self {
        Self {
            tx,
            chain_spec,
            prune_modes,
            storage,
            storage_settings,
            aux_db_provider,
            changeset_cache,
            runtime,
            file_tier_base: scoped_file_tier_base(&db_path),
            db_path,
            pending_aux_db_batches: Default::default(),
            minimum_pruning_distance: MINIMUM_UNWIND_SAFE_DISTANCE,
            metrics: metrics::DatabaseProviderMetrics::default(),
        }
    }

    /// Consume `DbTx` or `DbTxMut`.
    pub fn into_tx(self) -> TX {
        self.tx
    }

    /// Pass `DbTx` or `DbTxMut` mutable reference.
    pub const fn tx_mut(&mut self) -> &mut TX {
        &mut self.tx
    }

    /// Pass `DbTx` or `DbTxMut` immutable reference.
    pub const fn tx_ref(&self) -> &TX {
        &self.tx
    }

    /// Returns a reference to the chain specification.
    pub fn chain_spec(&self) -> &N::ChainSpec {
        &self.chain_spec
    }
}

impl<TX: DbTx + 'static, N: NodeTypesForProvider> DatabaseProvider<TX, N> {
    fn recovered_block<H, HF, B, BF>(
        &self,
        id: BlockHashOrNumber,
        _transaction_kind: TransactionVariant,
        header_by_number: HF,
        construct_block: BF,
    ) -> ProviderResult<Option<B>>
    where
        H: AsRef<HeaderTy<N>>,
        HF: FnOnce(BlockNumber) -> ProviderResult<Option<H>>,
        BF: FnOnce(H, BodyTy<N>, Vec<Address>) -> ProviderResult<Option<B>>,
    {
        let Some(block_number) = self.convert_hash_or_number(id)? else { return Ok(None) };

        // File-tier enabled: payload comes only from the files — no chain-spec/Fjall fallback.
        // File-tier disabled (tests/legacy): genesis is chain-spec data with an empty body.
        let Some(bundle) = self.file_tier_bundle(block_number)? else {
            if self.file_tier_base.is_none() && block_number == 0 {
                let Some(header) = header_by_number(0)? else { return Ok(None) };
                let body = BodyTy::<N>::build_from_records(Vec::new(), None);
                return construct_block(header, body, Vec::new());
            }
            return Ok(None);
        };
        let Some(header) = header_by_number(block_number)? else { return Ok(None) };
        let senders = bundle
            .txs
            .iter()
            .map(|tx| tx.from.parse::<Address>().map_err(|_| ProviderError::SenderRecoveryError))
            .collect::<Result<Vec<_>, _>>()?;
        let withdrawals = self.file_tier_withdrawals(&bundle.bt)?;
        let body = BodyTy::<N>::build_from_records(bundle.txs, withdrawals);
        construct_block(header, body, senders)
    }

    /// Populate a [`BundleStateInit`] and [`RevertsInit`] using cursors over the
    /// [`PlainAccountState`] and [`PlainStorageState`] tables, based on the given storage and
    /// account changesets.
    fn populate_bundle_state<A, S>(
        &self,
        account_changeset: Vec<(u64, AccountBeforeTx)>,
        storage_changeset: Vec<(BlockNumberAddress, StorageEntry)>,
        plain_accounts_cursor: &mut A,
        plain_storage_cursor: &mut S,
    ) -> ProviderResult<(BundleStateInit, RevertsInit)>
    where
        A: DbCursorRO<PlainAccountState>,
        S: DbDupCursorRO<PlainStorageState>,
    {
        // iterate previous value and get plain state value to create changeset
        // Double option around Account represent if Account state is know (first option) and
        // account is removed (Second Option)
        let mut state: BundleStateInit = HashMap::default();

        // This is not working for blocks that are not at tip. as plain state is not the last
        // state of end range. We should rename the functions or add support to access
        // History state. Accessing history state can be tricky but we are not gaining
        // anything.

        let mut reverts: RevertsInit = HashMap::default();

        // add account changeset changes
        for (block_number, account_before) in account_changeset.into_iter().rev() {
            let AccountBeforeTx { info: old_info, address } = account_before;
            match state.entry(address) {
                hash_map::Entry::Vacant(entry) => {
                    let new_info = plain_accounts_cursor.seek_exact(address)?.map(|kv| kv.1);
                    entry.insert((old_info, new_info, HashMap::default()));
                }
                hash_map::Entry::Occupied(mut entry) => {
                    // overwrite old account state.
                    entry.get_mut().0 = old_info;
                }
            }
            // insert old info into reverts.
            reverts.entry(block_number).or_default().entry(address).or_default().0 = Some(old_info);
        }

        // add storage changeset changes
        for (block_and_address, old_storage) in storage_changeset.into_iter().rev() {
            let BlockNumberAddress((block_number, address)) = block_and_address;
            // get account state or insert from plain state.
            let account_state = match state.entry(address) {
                hash_map::Entry::Vacant(entry) => {
                    let present_info = plain_accounts_cursor.seek_exact(address)?.map(|kv| kv.1);
                    entry.insert((present_info, present_info, HashMap::default()))
                }
                hash_map::Entry::Occupied(entry) => entry.into_mut(),
            };

            // match storage.
            match account_state.2.entry(old_storage.key) {
                hash_map::Entry::Vacant(entry) => {
                    let new_storage = plain_storage_cursor
                        .seek_by_key_subkey(address, old_storage.key)?
                        .filter(|storage| storage.key == old_storage.key)
                        .unwrap_or_default();
                    entry.insert((old_storage.value, new_storage.value));
                }
                hash_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().0 = old_storage.value;
                }
            };

            reverts
                .entry(block_number)
                .or_default()
                .entry(address)
                .or_default()
                .1
                .push(old_storage);
        }

        Ok((state, reverts))
    }
}

impl<TX: DbTxMut + DbTx + 'static, N: NodeTypes> DatabaseProvider<TX, N> {
    /// Insert history index to the database.
    ///
    /// For each updated partial key, this function retrieves the last shard from the database
    /// (if any), appends the new indices to it, chunks the resulting list if needed, and upserts
    /// the shards back into the database.
    ///
    /// This function is used by history indexing stages.
    fn append_history_index<P, T>(
        &self,
        index_updates: impl IntoIterator<Item = (P, impl IntoIterator<Item = u64>)>,
        mut sharded_key_factory: impl FnMut(P, BlockNumber) -> T::Key,
    ) -> ProviderResult<()>
    where
        P: Copy,
        T: Table<Value = BlockNumberList>,
    {
        // This function cannot be used with DUPSORT tables because `upsert` on DUPSORT tables
        // will append duplicate entries instead of updating existing ones, causing data corruption.
        assert!(!T::DUPSORT, "append_history_index cannot be used with DUPSORT tables");

        let mut cursor = self.tx.cursor_write::<T>()?;

        for (partial_key, indices) in index_updates {
            let last_key = sharded_key_factory(partial_key, u64::MAX);
            let mut last_shard = cursor
                .seek_exact(last_key.clone())?
                .map(|(_, list)| list)
                .unwrap_or_else(BlockNumberList::empty);

            last_shard.append(indices).map_err(ProviderError::other)?;

            // fast path: all indices fit in one shard
            if last_shard.len() <= sharded_key::NUM_OF_INDICES_IN_SHARD as u64 {
                cursor.upsert(last_key, &last_shard)?;
                continue;
            }

            // slow path: rechunk into multiple shards
            let chunks = last_shard.iter().chunks(sharded_key::NUM_OF_INDICES_IN_SHARD);
            let mut chunks_peekable = chunks.into_iter().peekable();

            while let Some(chunk) = chunks_peekable.next() {
                let shard = BlockNumberList::new_pre_sorted(chunk);
                let highest_block_number = if chunks_peekable.peek().is_some() {
                    shard.iter().next_back().expect("`chunks` does not return empty list")
                } else {
                    // Insert last list with `u64::MAX`.
                    u64::MAX
                };

                cursor.upsert(sharded_key_factory(partial_key, highest_block_number), &shard)?;
            }
        }

        Ok(())
    }
}

impl<TX: DbTx, N: NodeTypes> AccountReader for DatabaseProvider<TX, N> {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        Ok(self.tx.get_by_encoded_key::<tables::PlainAccountState>(address)?)
    }
}

impl<TX: DbTx + 'static, N: NodeTypes> AccountExtReader for DatabaseProvider<TX, N> {
    fn changed_accounts_with_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<BTreeSet<Address>> {
        let mut reader = EitherReader::new_account_changesets(self)?;

        reader.changed_accounts_with_range(range)
    }

    fn basic_accounts(
        &self,
        iter: impl IntoIterator<Item = Address>,
    ) -> ProviderResult<Vec<(Address, Option<Account>)>> {
        let mut plain_accounts = self.tx.cursor_read::<tables::PlainAccountState>()?;
        Ok(iter
            .into_iter()
            .map(|address| plain_accounts.seek_exact(address).map(|a| (address, a.map(|(_, v)| v))))
            .collect::<Result<Vec<_>, _>>()?)
    }

    fn changed_accounts_and_blocks_with_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<BTreeMap<Address, Vec<u64>>> {
        let mut changeset_cursor = self.tx.cursor_read::<tables::AccountChangeSets>()?;

        let account_transitions = changeset_cursor.walk_range(range)?.try_fold(
            BTreeMap::new(),
            |mut accounts: BTreeMap<Address, Vec<u64>>, entry| -> ProviderResult<_> {
                let (index, account) = entry?;
                accounts.entry(account.address).or_default().push(index);
                Ok(accounts)
            },
        )?;

        Ok(account_transitions)
    }
}

impl<TX: DbTx, N: NodeTypes> StorageChangeSetReader for DatabaseProvider<TX, N> {
    fn storage_changeset(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<Vec<(BlockNumberAddress, StorageEntry)>> {
        let range = block_number..=block_number;
        let storage_range = BlockNumberAddress::range(range);
        self.tx
            .cursor_dup_read::<tables::StorageChangeSets>()?
            .walk_range(storage_range)?
            .map(|r| {
                let (bna, entry) = r?;
                Ok((bna, entry))
            })
            .collect()
    }

    fn get_storage_before_block(
        &self,
        block_number: BlockNumber,
        address: Address,
        storage_key: B256,
    ) -> ProviderResult<Option<StorageEntry>> {
        Ok(self
            .tx
            .cursor_dup_read::<tables::StorageChangeSets>()?
            .seek_by_key_subkey(BlockNumberAddress((block_number, address)), storage_key)?
            .filter(|entry| entry.key == storage_key))
    }

    fn storage_changesets_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<(BlockNumberAddress, StorageEntry)>> {
        self.tx
            .cursor_dup_read::<tables::StorageChangeSets>()?
            .walk_range(BlockNumberAddressRange::from(range))?
            .map(|r| {
                let (bna, entry) = r?;
                Ok((bna, entry))
            })
            .collect()
    }
}

impl<TX: DbTx, N: NodeTypes> ChangeSetReader for DatabaseProvider<TX, N> {
    fn account_block_changeset(
        &self,
        block_number: BlockNumber,
    ) -> ProviderResult<Vec<AccountBeforeTx>> {
        let range = block_number..=block_number;
        self.tx
            .cursor_read::<tables::AccountChangeSets>()?
            .walk_range(range)?
            .map(|result| -> ProviderResult<_> {
                let (_, account_before) = result?;
                Ok(account_before)
            })
            .collect()
    }

    fn get_account_before_block(
        &self,
        block_number: BlockNumber,
        address: Address,
    ) -> ProviderResult<Option<AccountBeforeTx>> {
        self.tx
            .cursor_dup_read::<tables::AccountChangeSets>()?
            .seek_by_key_subkey(block_number, address)?
            .filter(|acc| acc.address == address)
            .map(Ok)
            .transpose()
    }

    fn account_changesets_range(
        &self,
        range: impl core::ops::RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<(BlockNumber, AccountBeforeTx)>> {
        self.tx
            .cursor_read::<tables::AccountChangeSets>()?
            .walk_range(to_range(range))?
            .map(|r| r.map_err(Into::into))
            .collect()
    }
}

impl<TX: DbTx + 'static, N: NodeTypesForProvider> HeaderSyncGapProvider
    for DatabaseProvider<TX, N>
{
    type Header = HeaderTy<N>;

    fn local_tip_header(
        &self,
        highest_uninterrupted_block: BlockNumber,
    ) -> ProviderResult<SealedHeader<Self::Header>> {
        // Since Stage 3 (3A.3) the primary database is the only header authority: there is no
        // static height to compare against or unwind.
        let local_head = self
            .sealed_header(highest_uninterrupted_block)?
            .ok_or_else(|| ProviderError::HeaderNotFound(highest_uninterrupted_block.into()))?;

        Ok(local_head)
    }
}

/// File-tier read path (6B, `fjall4a.md` §6): when the file tier is enabled every block's
/// payload is served from the gz records (sealed `.gz` or the active `.jsonl`) via the
/// `CanonicalChain`/`TxLocations` index — the payload is never stored in the database.
impl<TX: DbTx + 'static, N: NodeTypesForProvider> DatabaseProvider<TX, N> {
    /// Whether `number` is served from the file tier. When a file-tier base is set, every
    /// block's payload — genesis included — lives only in the files (never in the database),
    /// so those heights branch to the files. Without a base the file tier is disabled
    /// (legacy/tests only) and reads fall through to Fjall rows.
    fn in_file_tier(&self, number: BlockNumber) -> ProviderResult<bool> {
        // Bounded by the canonical head: a height above it has no file record yet, so claiming
        // the file tier for it would turn a legitimate "not found" into a missing-record error.
        Ok(number <= self.last_block_number()? && self.file_tier_base.is_some())
    }

    /// The block's file-tier bundle, the sole source of payload once the file tier is enabled.
    ///
    /// `Ok(None)` means the file tier is disabled **or** the records do not belong to this chain
    /// (see [`Self::verified_file_tier_bundle`]); the caller must not invent a payload from
    /// Fjall/chain-spec when the file tier is on.
    fn file_tier_bundle(&self, number: BlockNumber) -> ProviderResult<Option<BlockBundle>> {
        if !self.in_file_tier(number)? {
            return Ok(None);
        }
        self.verified_file_tier_bundle(number)
    }

    /// File-tier bundle for a height already known to be inside the canonical chain (a range
    /// walk over `CanonicalChain`), skipping the head bound whose per-call cursor seek would be
    /// paid once per height.
    fn file_tier_bundle_in_chain(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<BlockBundle>> {
        if self.file_tier_base.is_none() {
            return Ok(None);
        }
        self.verified_file_tier_bundle(number)
    }

    /// Every file-tier bundle in `start..=end`, read with **one forward pass per file**
    /// (`read_blocks_range_at`) instead of one full scan per height. Each bundle is verified
    /// against the canonical hash (same filter as [`Self::verified_file_tier_bundle`]);
    /// non-canonical or hash-mismatched records are dropped. Empty when the file tier is off.
    fn file_tier_bundles_in_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<HashMap<BlockNumber, BlockBundle>> {
        let mut out = HashMap::default();
        if self.file_tier_base.is_none() || start > end {
            return Ok(out);
        }
        let base = self.file_tier_base.expect("checked above");
        let bundles = read_blocks_range_at(base, start, end).map_err(ProviderError::other)?;
        for bundle in bundles {
            let number = bundle.bt.b;
            let Some(canonical) = self.tx.get::<tables::CanonicalChain>(number)? else {
                continue;
            };
            match bundle.bt.h.parse::<B256>() {
                Ok(record_hash) if record_hash == canonical => {
                    out.insert(number, bundle);
                }
                Ok(record_hash) => {
                    tracing::error!(
                        target: "providers::db",
                        number,
                        record_hash = %record_hash,
                        canonical_hash = %canonical,
                        "file-tier record hash does not match the canonical index"
                    );
                }
                Err(_) => {
                    tracing::error!(
                        target: "providers::db",
                        number,
                        record_hash = %bundle.bt.h,
                        "file-tier record carries a malformed block hash"
                    );
                }
            }
        }
        Ok(out)
    }

    /// Reads `number`'s records and serves them **only if they belong to this chain**: the `b`
    /// record's block hash must equal the canonical hash at that height. A missing record or a
    /// hash mismatch yields `Ok(None)` (fall through to Fjall).
    ///
    /// The file-tier base is a process global, so this verification keeps a stale or foreign base
    /// — and an up-to-date record set from another chain — from ever serving the wrong payload.
    /// A genuine mismatch surfaces as "not found" (there is no Fjall `Blocks` row to fall back to).
    fn verified_file_tier_bundle(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<BlockBundle>> {
        let Some(bundle) = read_block(number).map_err(ProviderError::other)? else {
            return Ok(None);
        };
        let Some(canonical) = self.tx.get::<tables::CanonicalChain>(number)? else {
            return Ok(None);
        };
        match bundle.bt.h.parse::<B256>() {
            Ok(record_hash) if record_hash == canonical => Ok(Some(bundle)),
            Ok(record_hash) => {
                // A record whose hash disagrees with the canonical index is corruption, or the
                // file-tier base points at a different chain. Never silent: the caller falls
                // through to Fjall, which is authoritative wherever a `Blocks` row still exists.
                tracing::error!(
                    target: "providers::db",
                    number,
                    record_hash = %record_hash,
                    canonical_hash = %canonical,
                    "file-tier record hash does not match the canonical index"
                );
                Ok(None)
            }
            Err(_) => {
                tracing::error!(
                    target: "providers::db",
                    number,
                    record_hash = %bundle.bt.h,
                    "file-tier record carries a malformed block hash"
                );
                Ok(None)
            }
        }
    }

    /// Body indices derived from the index rows when the file tier is disabled (tests and
    /// legacy databases). `TxLocations` is ordered by tx number and block numbers are
    /// non-decreasing along it (blocks write their transactions in order; unwinds only
    /// ever delete a suffix), so the block's first transaction is found by binary search
    /// instead of walking the whole table; an empty block takes the next block's first
    /// number (or the next free number for the trailing block).
    fn db_body_indices(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        if self.tx.get::<tables::CanonicalChain>(number)?.is_none() {
            return Ok(None);
        }
        let mut cursor = self.tx.cursor_read::<tables::TxLocations>()?;
        let Some((last_tx, _)) = cursor.last()? else {
            return Ok(Some(StoredBlockBodyIndices { first_tx_num: 0, tx_count: 0 }));
        };

        // Binary search for the first tx whose `block_number >= number`. Each probe seeks
        // to the first key at or after `mid`, so missing keys (fixtures that delete
        // individual rows) are skipped rather than assumed present.
        let (mut lo, mut hi) = (0u64, last_tx);
        let mut lower: Option<(u64, BlockNumber)> = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            // `mid <= last_tx` and `last_tx` exists, so a row at or after `mid` always does.
            let Some((tx_num, loc)) = cursor.walk(Some(mid))?.next().transpose()? else {
                break;
            };
            if loc.block_number >= number {
                if lower.is_none_or(|(best, _)| tx_num < best) {
                    lower = Some((tx_num, loc.block_number));
                }
                if mid == 0 {
                    break;
                }
                hi = mid - 1;
            } else if let Some(next) = tx_num.checked_add(1) {
                // Every key up to `tx_num` has a smaller block number; continue after it.
                lo = next;
            } else {
                break;
            }
        }

        let Some((first_tx, first_bn)) = lower else {
            // Every transaction belongs to an earlier block: trailing empty block(s).
            return Ok(Some(StoredBlockBodyIndices { first_tx_num: last_tx + 1, tx_count: 0 }));
        };
        if first_bn > number {
            // Empty block: it starts where the next block's transactions do.
            return Ok(Some(StoredBlockBodyIndices { first_tx_num: first_tx, tx_count: 0 }));
        }
        // Count the block's transactions — a short walk bounded by the block's own size.
        let mut tx_count = 0u64;
        for entry in cursor.walk(Some(first_tx))? {
            let (_, loc) = entry?;
            if loc.block_number != number {
                break;
            }
            tx_count += 1;
        }
        Ok(Some(StoredBlockBodyIndices { first_tx_num: first_tx, tx_count }))
    }

    /// Body indices for a file-tier block (4.1): the `b` record carries the first tx number
    /// (`ft`) and the count (`tc`), so the file tier supplies the same `StoredBlockBodyIndices`
    /// a `Blocks` row would. Records written before `ft` existed fall back to resolving the
    /// first transaction's number through `TransactionHashNumbers`; an empty old record has no
    /// transaction to resolve and reports `0`.
    fn file_tier_body_indices(
        &self,
        bundle: &BlockBundle,
    ) -> ProviderResult<StoredBlockBodyIndices> {
        let tx_count = bundle.bt.tc;
        let first_tx_num = match bundle.bt.ft {
            Some(ft) => ft,
            None if tx_count == 0 => 0,
            None => {
                let first_hash = bundle
                    .txs
                    .first()
                    .map(|tx| tx.h.as_str())
                    .unwrap_or_default()
                    .parse::<B256>()
                    .map_err(|_| {
                        ProviderError::other(std::io::Error::other(format!(
                            "block {}: malformed first tx hash in the `b`/`txs` records",
                            bundle.bt.b
                        )))
                    })?;
                self.tx.get::<tables::TransactionHashNumbers>(first_hash)?.ok_or_else(|| {
                    ProviderError::other(std::io::Error::other(format!(
                        "block {}: no `TxLocations` entry for the block's first transaction",
                        bundle.bt.b
                    )))
                })?
            }
        };
        Ok(StoredBlockBodyIndices { first_tx_num, tx_count })
    }

    /// The body withdrawals for an aged block, mirroring the Fjall read path's rule
    /// (`storage-api` `read_block_bodies`): a withdrawal list — defaulting an absent
    /// record to empty — only at/after Shanghai at the block's timestamp, `None`
    /// before. The record carries the body as-is, so without this gate a body written
    /// under a different fork view than the chain spec would not round-trip.
    fn file_tier_withdrawals(&self, bt: &BtRecord) -> ProviderResult<Option<Withdrawals>> {
        if !self.chain_spec().is_shanghai_active_at_timestamp(bt.ts) {
            return Ok(None);
        }
        Ok(Some(
            build_withdrawals(bt.wd.as_ref()).map_err(ProviderError::other)?.unwrap_or_default(),
        ))
    }

    /// The transaction located at `tx_num` from its block's `txs`, once its Fjall row
    /// is gone. `Ok(None)` when the location row is absent (pruned/unwound) or the block
    /// is not actually below the floor.
    fn file_tier_transaction(&self, tx_num: TxNumber) -> ProviderResult<Option<TxTy<N>>> {
        let Some(loc) = self.tx.get::<tables::TxLocations>(tx_num)? else { return Ok(None) };
        let Some(bundle) = self.file_tier_bundle(loc.block_number)? else { return Ok(None) };
        let tx = bundle.txs.get(loc.index as usize).ok_or_else(|| {
            ProviderError::other(std::io::Error::other(format!(
                "tx {tx_num} location ({}, {}) is out of range in block {} records",
                loc.block_number,
                loc.index,
                bundle.txs.len()
            )))
        })?;
        Ok(Some(TxTy::<N>::from(tx)))
    }

    /// The file-tier transactions located in `range` via `TxLocations`, each with its number.
    ///
    /// Consecutive numbers usually share a block, so the last block's bundle is read once and
    /// reused across its transactions. Heights with no `TxLocations` entry, or whose records are
    /// absent / not canonical (file tier disabled), are skipped — the same `Ok(None)` fallthrough
    /// [`Self::file_tier_transaction`] uses.
    fn file_tier_txs_in_range(
        &self,
        range: std::ops::Range<TxNumber>,
    ) -> ProviderResult<Vec<(TxNumber, TxRecord)>> {
        let mut out = Vec::new();
        let mut locs = self.tx.cursor_read::<tables::TxLocations>()?;
        let mut last: Option<(BlockNumber, BlockBundle)> = None;
        for entry in locs.walk_range(range)? {
            let (tx_num, loc) = entry?;
            if last.as_ref().map(|(n, _)| *n) != Some(loc.block_number) {
                last = self.file_tier_bundle(loc.block_number)?.map(|b| (loc.block_number, b));
            }
            let Some((_, bundle)) = &last else { continue };
            let Some(tx) = bundle.txs.get(loc.index as usize) else {
                return Err(ProviderError::other(std::io::Error::other(format!(
                    "tx {tx_num} location ({}, {}) is out of range in block {} records",
                    loc.block_number,
                    loc.index,
                    bundle.txs.len()
                ))));
            };
            out.push((tx_num, tx.clone()));
        }
        Ok(out)
    }

    /// The block's receipts rebuilt from the `txs` records (receipt scalar folded in) plus
    /// logs joined from `logs`. `cumulative_gas_used` is derived from the running sum of
    /// each tx's `gu`, in block order.
    fn file_tier_receipts(&self, bundle: &BlockBundle) -> ProviderResult<Vec<ReceiptTy<N>>> {
        let mut cgu = 0u64;
        let mut out = Vec::with_capacity(bundle.txs.len());
        for (index, tx) in bundle.txs.iter().enumerate() {
            cgu += tx.gu;
            let logs =
                bundle.receipt_logs(index as u64).into_iter().map(Log::from).collect::<Vec<_>>();
            out.push(ReceiptTy::<N>::from(ReceiptBuild {
                ty: tx.ty.clone(),
                st: tx.st,
                cgu,
                logs,
            }));
        }
        Ok(out)
    }
}

impl<TX: DbTx + 'static, N: NodeTypesForProvider> HeaderProvider for DatabaseProvider<TX, N> {
    type Header = HeaderTy<N>;

    fn header(&self, block_hash: BlockHash) -> ProviderResult<Option<Self::Header>> {
        // `HeaderNumbers` is the hash → height lookup (spec §6.2); the payload itself is
        // read from the file tier.
        let Some(number) = self.tx.get::<tables::HeaderNumbers>(block_hash)? else {
            return Ok(None);
        };
        if let Some(bundle) = self.file_tier_bundle(number)? {
            return Ok(Some(HeaderTy::<N>::from(&bundle.bt)));
        }
        // File-tier disabled only: genesis header is chain-spec data.
        if self.file_tier_base.is_none() && number == 0 {
            return Ok(Some(self.chain_spec.genesis_header().clone()));
        }
        Ok(None)
    }

    fn header_by_number(&self, num: BlockNumber) -> ProviderResult<Option<Self::Header>> {
        if let Some(bundle) = self.file_tier_bundle(num)? {
            return Ok(Some(HeaderTy::<N>::from(&bundle.bt)));
        }
        // File-tier disabled only: genesis header is chain-spec data.
        if self.file_tier_base.is_none() && num == 0 {
            return Ok(Some(self.chain_spec.genesis_header().clone()));
        }
        Ok(None)
    }

    fn headers_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Self::Header>> {
        let range = to_range(range);
        let mut headers = Vec::new();
        // One pass over the files for the whole range; canonical walk picks order/membership.
        let bundles = if self.file_tier_base.is_some() && !range.is_empty() {
            self.file_tier_bundles_in_range(range.start, range.end.saturating_sub(1))?
        } else {
            HashMap::default()
        };
        for entry in self.tx.cursor_read::<tables::CanonicalChain>()?.walk_range(range)? {
            let (number, _hash) = entry?;
            if let Some(bundle) = bundles.get(&number) {
                headers.push(HeaderTy::<N>::from(&bundle.bt));
            } else if self.file_tier_base.is_none() && number == 0 {
                headers.push(self.chain_spec.genesis_header().clone());
            }
        }
        Ok(headers)
    }

    fn sealed_header(
        &self,
        number: BlockNumber,
    ) -> ProviderResult<Option<SealedHeader<Self::Header>>> {
        if let Some(bundle) = self.file_tier_bundle(number)? {
            let Some(hash) = self.tx.get::<tables::CanonicalChain>(number)? else {
                return Ok(None);
            };
            return Ok(Some(SealedHeader::new(HeaderTy::<N>::from(&bundle.bt), hash)));
        }
        // File-tier disabled only: genesis header is chain-spec data.
        if self.file_tier_base.is_none() && number == 0 {
            let Some(hash) = self.tx.get::<tables::CanonicalChain>(number)? else {
                return Ok(None);
            };
            return Ok(Some(SealedHeader::new(self.chain_spec.genesis_header().clone(), hash)));
        }
        Ok(None)
    }

    fn sealed_headers_while(
        &self,
        range: impl RangeBounds<BlockNumber>,
        mut predicate: impl FnMut(&SealedHeader<Self::Header>) -> bool,
    ) -> ProviderResult<Vec<SealedHeader<Self::Header>>> {
        let mut result = Vec::new();
        let mut cursor = self.tx.cursor_read::<tables::CanonicalChain>()?;
        for entry in cursor.walk_range(to_range(range))? {
            let (number, hash) = entry?;
            // A missing file-tier record stops the walk (no Fjall/chain-spec fallback when
            // the file tier is on); file-tier disabled still serves genesis from chain-spec.
            let sealed = if let Some(bundle) = self.file_tier_bundle_in_chain(number)? {
                SealedHeader::new(HeaderTy::<N>::from(&bundle.bt), hash)
            } else if self.file_tier_base.is_none() && number == 0 {
                SealedHeader::new(self.chain_spec.genesis_header().clone(), hash)
            } else {
                break;
            };
            if !predicate(&sealed) {
                break;
            }
            result.push(sealed);
        }
        Ok(result)
    }
}

impl<TX: DbTx + 'static, N: NodeTypes> BlockHashReader for DatabaseProvider<TX, N> {
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        Ok(self.tx.get::<tables::CanonicalChain>(number)?)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.cursor_read_collect::<tables::CanonicalChain>(start..end)
    }
}

impl<TX: DbTx + 'static, N: NodeTypes> BlockNumReader for DatabaseProvider<TX, N> {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        let best_number = self.best_block_number()?;
        let best_hash = self.block_hash(best_number)?.unwrap_or_default();
        Ok(ChainInfo { best_hash, best_number })
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        // The best block number is tracked via the finished stage which gets updated in the same tx
        // when new blocks committed
        Ok(self
            .get_stage_checkpoint(StageId::Finish)?
            .map(|checkpoint| checkpoint.block_number)
            .unwrap_or_default())
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(self
            .tx
            .cursor_read::<tables::CanonicalChain>()?
            .last()?
            .map(|(number, _)| number)
            .unwrap_or_default())
    }

    fn earliest_block_number(&self) -> ProviderResult<BlockNumber> {
        // The earliest block with body indices is the first block still fully available in the
        // primary database. Nothing is expired in the database itself.
        let mut cursor = self.tx.cursor_read::<tables::CanonicalChain>()?;
        for entry in cursor.walk(None)? {
            let (number, _hash) = entry?;
            if self.file_tier_base.is_some() &&
                read_block(number).map_err(ProviderError::other)?.is_some()
            {
                return Ok(number);
            }
        }
        Ok(0)
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        // `HeaderNumbers` carries the hash → height mapping (spec §6.2); the payload is
        // read from the file tier.
        Ok(self.tx.get::<tables::HeaderNumbers>(hash)?)
    }
}

impl<TX: DbTx + 'static, N: NodeTypesForProvider> BlockReader for DatabaseProvider<TX, N> {
    type Block = BlockTy<N>;

    fn find_block_by_hash(
        &self,
        hash: B256,
        source: BlockSource,
    ) -> ProviderResult<Option<Self::Block>> {
        if source.is_canonical() {
            self.block(hash.into())
        } else {
            Ok(None)
        }
    }

    /// Returns the block with matching number from database.
    ///
    /// Strict store split (operator rule): the file tier owns the block's entire payload —
    /// no DB fallback and no dupes. The DB owns only the indices (`CanonicalChain`,
    /// `HeaderNumbers`, `TxLocations`). When the file tier is enabled a bundle miss is
    /// either a pruned-below-the-floor block (classified as [`ProviderError::BlockExpired`])
    /// or a block that never existed. The tier-disabled test mode serves genesis from the
    /// chain spec and nothing else.
    ///
    /// Returns an error if the requested block is below the earliest available history.
    fn block(&self, id: BlockHashOrNumber) -> ProviderResult<Option<Self::Block>> {
        if let Some(number) = self.convert_hash_or_number(id)? {
            if let Some(bundle) = self.file_tier_bundle(number)? {
                let BlockBundle { bt, txs, .. } = bundle;
                let withdrawals = self.file_tier_withdrawals(&bt)?;
                let header = HeaderTy::<N>::from(&bt);
                let body = BodyTy::<N>::build_from_records(txs, withdrawals);
                return Ok(Some(Self::Block::new(header, body)));
            }

            if self.file_tier_base.is_some() {
                // Tier enabled and no record: pruned below the floor, or never existed.
                let earliest_available = self.earliest_block_number()?;
                if number < earliest_available {
                    return Err(ProviderError::BlockExpired { requested: number, earliest_available });
                }
                return Ok(None);
            }

            // File-tier disabled only (tests): genesis is chain-spec data; no other
            // payload store exists in that mode.
            if number == 0 {
                let header = self.chain_spec.genesis_header().clone();
                let body = BodyTy::<N>::build_from_records(Vec::new(), None);
                return Ok(Some(Self::Block::new(header, body)));
            }
        }

        Ok(None)
    }

    fn pending_block(&self) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        Ok(None)
    }

    fn pending_block_and_receipts(
        &self,
    ) -> ProviderResult<Option<(RecoveredBlock<Self::Block>, Vec<Self::Receipt>)>> {
        Ok(None)
    }

    /// Returns the block with senders with matching number or hash from database.
    ///
    /// **NOTE: The transactions have invalid hashes, since they would need to be calculated on the
    /// spot, and we want fast querying.**
    ///
    /// If the header for this block is not found, this returns `None`.
    /// If the header is found, but the transactions either do not exist, or are not indexed, this
    /// will return None.
    fn recovered_block(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.recovered_block(
            id,
            transaction_kind,
            |block_number| self.header_by_number(block_number),
            |header, body, senders| {
                Self::Block::new(header, body)
                    // Note: we're using unchecked here because we know the block contains valid txs
                    // wrt to its height and can ignore the s value check so pre
                    // EIP-2 txs are allowed
                    .try_into_recovered_unchecked(senders)
                    .map(Some)
                    .map_err(|_| ProviderError::SenderRecoveryError)
            },
        )
    }

    fn sealed_block_with_senders(
        &self,
        id: BlockHashOrNumber,
        transaction_kind: TransactionVariant,
    ) -> ProviderResult<Option<RecoveredBlock<Self::Block>>> {
        self.recovered_block(
            id,
            transaction_kind,
            |block_number| self.sealed_header(block_number),
            |header, body, senders| {
                Self::Block::new_sealed(header, body)
                    // Note: we're using unchecked here because we know the block contains valid txs
                    // wrt to its height and can ignore the s value check so pre
                    // EIP-2 txs are allowed
                    .try_with_senders_unchecked(senders)
                    .map(Some)
                    .map_err(|_| ProviderError::SenderRecoveryError)
            },
        )
    }

    fn block_range(&self, range: RangeInclusive<BlockNumber>) -> ProviderResult<Vec<Self::Block>> {
        if range.is_empty() {
            return Ok(Vec::new());
        }
        // One pass over the files for the whole range; walk canonical for order/membership.
        let start = *range.start();
        let end = *range.end();
        let mut bundles = if self.file_tier_base.is_some() {
            self.file_tier_bundles_in_range(start, end)?
        } else {
            HashMap::default()
        };
        let mut blocks =
            Vec::with_capacity(end.saturating_sub(start) as usize + 1);
        for entry in
            self.tx.cursor_read::<tables::CanonicalChain>()?.walk_range(start..end + 1)?
        {
            let (number, _hash) = entry?;
            if let Some(bundle) = bundles.remove(&number) {
                let header = HeaderTy::<N>::from(&bundle.bt);
                let withdrawals = self.file_tier_withdrawals(&bundle.bt)?;
                let body = BodyTy::<N>::build_from_records(bundle.txs, withdrawals);
                blocks.push(Self::Block::new(header, body));
            } else if self.file_tier_base.is_none() && number == 0 {
                // File-tier disabled only: genesis has an empty body (chain-spec data).
                let header = self.chain_spec.genesis_header().clone();
                let body = BodyTy::<N>::build_from_records(Vec::new(), None);
                blocks.push(Self::Block::new(header, body));
            }
        }
        Ok(blocks)
    }

    fn block_by_transaction_id(&self, id: TxNumber) -> ProviderResult<Option<BlockNumber>> {
        // `TxLocations` is the tx-number index; the payload is read from the file tier.
        Ok(self.tx.get::<tables::TxLocations>(id)?.map(|loc| loc.block_number))
    }
}

impl<TX: DbTx + 'static, N: NodeTypesForProvider> TransactionsProviderExt
    for DatabaseProvider<TX, N>
{
    /// Recovers transaction hashes by walking through `Transactions` table and
    /// calculating them in a parallel manner. Returned unsorted.
    fn transaction_hashes_by_range(
        &self,
        tx_range: Range<TxNumber>,
    ) -> ProviderResult<Vec<(TxHash, TxNumber)>> {
        let mut result = Vec::with_capacity((tx_range.end - tx_range.start) as usize);
        for tx_id in tx_range {
            if let Some(tx) = self.file_tier_transaction(tx_id)? {
                result.push((tx.recalculate_hash(), tx_id));
            }
        }
        Ok(result)
    }
}

// Calculates the hash of the given transaction
impl<TX: DbTx + 'static, N: NodeTypesForProvider> TransactionsProvider for DatabaseProvider<TX, N> {
    type Transaction = TxTy<N>;

    fn transaction_id(&self, tx_hash: TxHash) -> ProviderResult<Option<TxNumber>> {
        self.with_aux_db_snapshot(|aux_db_ref| {
            let mut reader = EitherReader::new_transaction_hash_numbers(self, aux_db_ref)?;
            reader.get_transaction_hash_number(tx_hash)
        })
    }

    fn transaction_by_id(&self, id: TxNumber) -> ProviderResult<Option<Self::Transaction>> {
        self.file_tier_transaction(id)
    }

    fn transaction_by_id_unhashed(
        &self,
        id: TxNumber,
    ) -> ProviderResult<Option<Self::Transaction>> {
        self.file_tier_transaction(id)
    }

    fn transaction_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Transaction>> {
        if let Some(id) = self.transaction_id(hash)? {
            Ok(self.transaction_by_id_unhashed(id)?)
        } else {
            Ok(None)
        }
    }

    fn transaction_by_hash_with_meta(
        &self,
        tx_hash: TxHash,
    ) -> ProviderResult<Option<(Self::Transaction, TransactionMeta)>> {
        if let Some(transaction_id) = self.transaction_id(tx_hash)? &&
            let Some(transaction) = self.transaction_by_id_unhashed(transaction_id)? &&
            let Some(block_number) = self.block_by_transaction_id(transaction_id)? &&
            let Some(sealed_header) = self.sealed_header(block_number)?
        {
            let (header, block_hash) = sealed_header.split();
            // `TxLocations` is authoritative: it carries the in-block index.
            let Some(loc) = self.tx.get::<tables::TxLocations>(transaction_id)? else {
                return Ok(None);
            };
            let index = loc.index;

            let meta = TransactionMeta {
                tx_hash,
                index,
                block_hash,
                block_number,
                base_fee: header.base_fee_per_gas(),
                excess_blob_gas: header.excess_blob_gas(),
                timestamp: header.timestamp(),
            };

            return Ok(Some((transaction, meta)));
        }

        Ok(None)
    }

    fn transactions_by_block(
        &self,
        id: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Transaction>>> {
        if let Some(block_number) = self.convert_hash_or_number(id)? {
            // 6B: below the floor the body indices are gone; the bundle carries the
            // block's transactions (spec §6.3).
            if let Some(bundle) = self.file_tier_bundle(block_number)? {
                return Ok(Some(bundle.txs.iter().map(TxTy::<N>::from).collect()));
            }
            // File-tier disabled only: genesis has no transactions.
            if self.file_tier_base.is_none() && block_number == 0 {
                return Ok(Some(Vec::new()));
            }
        }
        Ok(None)
    }

    fn transactions_by_block_range(
        &self,
        range: impl RangeBounds<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Transaction>>> {
        let range = to_range(range);
        let mut out = Vec::new();
        // One pass over the files for the whole range; walk canonical for order/membership.
        let bundles = if self.file_tier_base.is_some() && !range.is_empty() {
            self.file_tier_bundles_in_range(range.start, range.end.saturating_sub(1))?
        } else {
            HashMap::default()
        };
        for entry in self.tx.cursor_read::<tables::CanonicalChain>()?.walk_range(range)? {
            let (number, _hash) = entry?;
            if let Some(bundle) = bundles.get(&number) {
                out.push(bundle.txs.iter().map(TxTy::<N>::from).collect());
            }
        }
        Ok(out)
    }

    fn transactions_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Transaction>> {
        let range = to_range(range);
        Ok(self
            .file_tier_txs_in_range(range)?
            .into_iter()
            .map(|(_, tx)| TxTy::<N>::from(&tx))
            .collect())
    }

    fn senders_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Address>> {
        let range = to_range(range);
        let mut senders = Vec::new();
        for (_, tx) in self.file_tier_txs_in_range(range)? {
            let sender =
                tx.from.parse::<Address>().map_err(|_| ProviderError::SenderRecoveryError)?;
            senders.push(sender);
        }
        Ok(senders)
    }

    fn transaction_sender(&self, id: TxNumber) -> ProviderResult<Option<Address>> {
        // The sender is the `txs` record's `0` field, reached through `TxLocations`.
        let Some(loc) = self.tx.get::<tables::TxLocations>(id)? else { return Ok(None) };
        let Some(bundle) = self.file_tier_bundle(loc.block_number)? else { return Ok(None) };
        let tx = bundle.txs.get(loc.index as usize).ok_or_else(|| {
            ProviderError::other(std::io::Error::other(format!(
                "tx {id} location ({}, {}) is out of range in block {} records",
                loc.block_number,
                loc.index,
                bundle.txs.len()
            )))
        })?;
        tx.from.parse::<Address>().map(Some).map_err(|_| ProviderError::SenderRecoveryError)
    }
}

impl<TX: DbTx + 'static, N: NodeTypesForProvider> ReceiptProvider for DatabaseProvider<TX, N> {
    type Receipt = ReceiptTy<N>;

    fn receipt(&self, id: TxNumber) -> ProviderResult<Option<Self::Receipt>> {
        // Rebuild from the tx records + `logs` joined through `TxLocations`.
        let Some(loc) = self.tx.get::<tables::TxLocations>(id)? else { return Ok(None) };
        let Some(bundle) = self.file_tier_bundle(loc.block_number)? else { return Ok(None) };
        let receipts = self.file_tier_receipts(&bundle)?;
        receipts.into_iter().nth(loc.index as usize).map(Some).ok_or_else(|| {
            ProviderError::other(std::io::Error::other(format!(
                "tx {id} location ({}, {}) is out of range in block {} receipts",
                loc.block_number,
                loc.index,
                bundle.txs.len()
            )))
        })
    }

    fn receipt_by_hash(&self, hash: TxHash) -> ProviderResult<Option<Self::Receipt>> {
        if let Some(id) = self.transaction_id(hash)? {
            self.receipt(id)
        } else {
            Ok(None)
        }
    }

    fn receipts_by_block(
        &self,
        block: BlockHashOrNumber,
    ) -> ProviderResult<Option<Vec<Self::Receipt>>> {
        if let Some(number) = self.convert_hash_or_number(block)? {
            // 6B: below the floor the body indices are gone; rebuild the block's receipts
            // from the tx records, joining each one's logs from `logs`.
            if let Some(bundle) = self.file_tier_bundle(number)? {
                return Ok(Some(self.file_tier_receipts(&bundle)?));
            }
            // File-tier disabled only: genesis has no receipts.
            if self.file_tier_base.is_none() && number == 0 {
                return Ok(Some(Vec::new()));
            }
        }
        Ok(None)
    }

    fn receipts_by_tx_range(
        &self,
        range: impl RangeBounds<TxNumber>,
    ) -> ProviderResult<Vec<Self::Receipt>> {
        let range = to_range(range);
        let mut out = Vec::new();
        // Consecutive tx numbers usually share a block: build that block's receipts once and
        // pick by index — the same reuse pattern as `file_tier_txs_in_range` (no N+1 reload).
        let mut locs = self.tx.cursor_read::<tables::TxLocations>()?;
        let mut last_block: Option<BlockNumber> = None;
        let mut last_receipts: Option<Vec<Self::Receipt>> = None;
        for entry in locs.walk_range(range)? {
            let (tx_num, loc) = entry?;
            if last_block != Some(loc.block_number) {
                last_block = Some(loc.block_number);
                last_receipts = match self.file_tier_bundle(loc.block_number)? {
                    Some(bundle) => Some(self.file_tier_receipts(&bundle)?),
                    None => None,
                };
            }
            // Bundle absent for this block (file tier off / not found): skip, as before.
            let Some(receipts) = &last_receipts else { continue };
            let Some(receipt) = receipts.get(loc.index as usize) else {
                return Err(ProviderError::other(std::io::Error::other(format!(
                    "tx {tx_num} location ({}, {}) is out of range in block {} receipts",
                    loc.block_number,
                    loc.index,
                    receipts.len()
                ))));
            };
            out.push(receipt.clone());
        }
        Ok(out)
    }

    fn receipts_by_block_range(
        &self,
        block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<Vec<Self::Receipt>>> {
        if block_range.is_empty() {
            return Ok(Vec::new());
        }

        // One pass over the files for the whole range; lookup per height after.
        let bundles = self.file_tier_bundles_in_range(*block_range.start(), *block_range.end())?;
        let mut result =
            Vec::with_capacity((block_range.end() - block_range.start() + 1) as usize);
        for block_num in block_range {
            if let Some(bundle) = bundles.get(&block_num) {
                result.push(self.file_tier_receipts(bundle)?);
            } else {
                result.push(Vec::new());
            }
        }
        Ok(result)
    }
}

impl<TX: DbTx + 'static, N: NodeTypesForProvider> BlockBodyIndicesProvider
    for DatabaseProvider<TX, N>
{
    fn block_body_indices(&self, num: u64) -> ProviderResult<Option<StoredBlockBodyIndices>> {
        // The body indices come from the `b` record (first tx number + count), or from
        // `TxLocations` when the file tier is disabled.
        if let Some(bundle) = self.file_tier_bundle(num)? {
            return Ok(Some(self.file_tier_body_indices(&bundle)?));
        }
        self.db_body_indices(num)
    }

    fn block_body_indices_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<Vec<StoredBlockBodyIndices>> {
        let mut indices = Vec::new();
        for entry in self.tx.cursor_read::<tables::CanonicalChain>()?.walk_range(range)? {
            let (number, _hash) = entry?;
            if let Some(bundle) = self.file_tier_bundle_in_chain(number)? {
                indices.push(self.file_tier_body_indices(&bundle)?);
            } else if let Some(body_indices) = self.db_body_indices(number)? {
                indices.push(body_indices);
            }
        }
        Ok(indices)
    }
}

impl<TX: DbTx, N: NodeTypes> StageCheckpointReader for DatabaseProvider<TX, N> {
    fn get_stage_checkpoint(&self, id: StageId) -> ProviderResult<Option<StageCheckpoint>> {
        Ok(if let Some(encoded) = id.get_pre_encoded() {
            self.tx.get_by_encoded_key::<tables::StageCheckpoints>(encoded)?
        } else {
            self.tx.get::<tables::StageCheckpoints>(id.to_string())?
        })
    }

    /// Get stage checkpoint progress.
    fn get_stage_checkpoint_progress(&self, id: StageId) -> ProviderResult<Option<Vec<u8>>> {
        Ok(self.tx.get::<tables::StageCheckpointProgresses>(id.to_string())?)
    }

    fn get_all_checkpoints(&self) -> ProviderResult<Vec<(String, StageCheckpoint)>> {
        self.tx
            .cursor_read::<tables::StageCheckpoints>()?
            .walk(None)?
            .collect::<Result<Vec<(String, StageCheckpoint)>, _>>()
            .map_err(ProviderError::Database)
    }
}

impl<TX: DbTxMut, N: NodeTypes> StageCheckpointWriter for DatabaseProvider<TX, N> {
    /// Save stage checkpoint.
    fn save_stage_checkpoint(
        &self,
        id: StageId,
        checkpoint: StageCheckpoint,
    ) -> ProviderResult<()> {
        Ok(self.tx.put::<tables::StageCheckpoints>(id.to_string(), checkpoint)?)
    }

    /// Save stage checkpoint progress.
    fn save_stage_checkpoint_progress(
        &self,
        id: StageId,
        checkpoint: Vec<u8>,
    ) -> ProviderResult<()> {
        Ok(self.tx.put::<tables::StageCheckpointProgresses>(id.to_string(), checkpoint)?)
    }

    #[instrument(level = "debug", target = "providers::db", skip_all)]
    fn update_pipeline_stages(
        &self,
        block_number: BlockNumber,
        drop_stage_checkpoint: bool,
    ) -> ProviderResult<()> {
        // iterate over all existing stages in the table and update its progress.
        let mut cursor = self.tx.cursor_write::<tables::StageCheckpoints>()?;
        for stage_id in StageId::ALL {
            let (_, checkpoint) = cursor.seek_exact(stage_id.to_string())?.unwrap_or_default();
            let updated = StageCheckpoint {
                block_number,
                ..if drop_stage_checkpoint { Default::default() } else { checkpoint }
            };
            // The seek above already read the row, so the comparison is free: a
            // re-persist of the same block (or an unwind to the block already
            // recorded) keeps its unchanged rows out of the commit's write batch.
            if updated != checkpoint {
                cursor.upsert(stage_id.to_string(), &updated)?;
            }
        }

        Ok(())
    }
}

impl<TX: DbTx + 'static, N: NodeTypes> StorageReader for DatabaseProvider<TX, N> {
    fn plain_state_storages(
        &self,
        addresses_with_keys: impl IntoIterator<Item = (Address, impl IntoIterator<Item = B256>)>,
    ) -> ProviderResult<Vec<(Address, Vec<StorageEntry>)>> {
        let mut plain_storage = self.tx.cursor_dup_read::<tables::PlainStorageState>()?;

        addresses_with_keys
            .into_iter()
            .map(|(address, storage)| {
                storage
                    .into_iter()
                    .map(|key| -> ProviderResult<_> {
                        Ok(plain_storage
                            .seek_by_key_subkey(address, key)?
                            .filter(|v| v.key == key)
                            .unwrap_or_else(|| StorageEntry { key, value: Default::default() }))
                    })
                    .collect::<ProviderResult<Vec<_>>>()
                    .map(|storage| (address, storage))
            })
            .collect::<ProviderResult<Vec<(_, _)>>>()
    }

    fn changed_storages_with_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<BTreeMap<Address, BTreeSet<B256>>> {
        self.tx
            .cursor_read::<tables::StorageChangeSets>()?
            .walk_range(BlockNumberAddress::range(range))?
            // fold all storages and save its old state so we can remove it from HashedStorage
            // it is needed as it is dup table.
            .try_fold(BTreeMap::new(), |mut accounts: BTreeMap<Address, BTreeSet<B256>>, entry| {
                let (BlockNumberAddress((_, address)), storage_entry) = entry?;
                accounts.entry(address).or_default().insert(storage_entry.key);
                Ok(accounts)
            })
    }

    fn changed_storages_and_blocks_with_range(
        &self,
        range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<BTreeMap<(Address, B256), Vec<u64>>> {
        let mut changeset_cursor = self.tx.cursor_read::<tables::StorageChangeSets>()?;

        let storage_changeset_lists =
            changeset_cursor.walk_range(BlockNumberAddress::range(range))?.try_fold(
                BTreeMap::new(),
                |mut storages: BTreeMap<(Address, B256), Vec<u64>>, entry| -> ProviderResult<_> {
                    let (index, storage) = entry?;
                    storages
                        .entry((index.address(), storage.key))
                        .or_default()
                        .push(index.block_number());
                    Ok(storages)
                },
            )?;

        Ok(storage_changeset_lists)
    }
}

impl<TX: DbTxMut + DbTx + 'static, N: NodeTypesForProvider> StateWriter
    for DatabaseProvider<TX, N>
{
    type Receipt = ReceiptTy<N>;

    #[instrument(level = "debug", target = "providers::db", skip_all)]
    fn write_state<'a>(
        &self,
        execution_outcome: impl Into<WriteStateInput<'a, Self::Receipt>>,
        is_value_known: OriginalValuesKnown,
        config: StateWriteConfig,
    ) -> ProviderResult<()> {
        let execution_outcome = execution_outcome.into();

        let first_block = execution_outcome.first_block();
        let (plain_state, reverts) =
            execution_outcome.state().to_plain_state_and_reverts(is_value_known);

        self.write_state_reverts(reverts, first_block, config)?;
        self.write_state_changes(plain_state)?;

        Ok(())
    }

    fn write_state_reverts(
        &self,
        reverts: PlainStateReverts,
        first_block: BlockNumber,
        config: StateWriteConfig,
    ) -> ProviderResult<()> {
        // Write storage changes
        if config.write_storage_changesets {
            tracing::trace!("Writing storage changes");
            let mut storages_cursor =
                self.tx_ref().cursor_dup_write::<tables::PlainStorageState>()?;
            for (block_index, mut storage_changes) in reverts.storage.into_iter().enumerate() {
                let block_number = first_block + block_index as BlockNumber;

                tracing::trace!(block_number, "Writing block change");
                // sort changes by address.
                storage_changes.par_sort_unstable_by_key(|a| a.address);
                let total_changes =
                    storage_changes.iter().map(|change| change.storage_revert.len()).sum();
                let mut changeset = Vec::with_capacity(total_changes);
                for PlainStorageRevert { address, wiped, storage_revert } in storage_changes {
                    let mut storage = storage_revert
                        .into_iter()
                        .map(|(k, v)| (B256::from(k.to_be_bytes()), v))
                        .collect::<Vec<_>>();
                    // sort storage slots by key.
                    storage.par_sort_unstable_by_key(|a| a.0);

                    // If we are writing the primary storage wipe transition, the pre-existing
                    // storage state has to be taken from the database and written to storage
                    // history. See [StorageWipe::Primary] for more details.
                    //
                    // TODO(mediocregopher): This could be rewritten in a way which doesn't
                    // require collecting wiped entries into a Vec like this, see
                    // `write_storage_trie_changesets`.
                    let mut wiped_storage = Vec::new();
                    if wiped {
                        tracing::trace!(?address, "Wiping storage");
                        if let Some((_, entry)) = storages_cursor.seek_exact(address)? {
                            wiped_storage.push((entry.key, entry.value));
                            while let Some(entry) = storages_cursor.next_dup_val()? {
                                wiped_storage.push((entry.key, entry.value))
                            }
                        }
                    }

                    tracing::trace!(?address, ?storage, "Writing storage reverts");
                    for (key, value) in StorageRevertsIter::new(storage, wiped_storage) {
                        changeset.push(StorageBeforeTx { address, key, value });
                    }
                }

                let mut storage_changesets_writer =
                    EitherWriter::new_storage_changesets(self, block_number)?;
                storage_changesets_writer.put_storage_changeset(block_number, changeset)?;
            }
        }

        if !config.write_account_changesets {
            return Ok(());
        }

        // Write account changes
        tracing::trace!(?first_block, "Writing account changes");
        for (block_index, account_block_reverts) in reverts.accounts.into_iter().enumerate() {
            let block_number = first_block + block_index as BlockNumber;
            let changeset = account_block_reverts
                .into_iter()
                .map(|(address, info)| AccountBeforeTx { address, info: info.map(Into::into) })
                .collect::<Vec<_>>();
            let mut account_changesets_writer =
                EitherWriter::new_account_changesets(self, block_number)?;

            account_changesets_writer.put_account_changeset(block_number, changeset)?;
        }

        Ok(())
    }

    fn write_state_changes(&self, mut changes: StateChangeset) -> ProviderResult<()> {
        // sort all entries so they can be written to database in more performant way.
        // and take smaller memory footprint.
        changes.accounts.par_sort_by_key(|a| a.0);
        changes.storage.par_sort_by_key(|a| a.address);
        changes.contracts.par_sort_by_key(|a| a.0);

        // Write new account state
        tracing::trace!(len = changes.accounts.len(), "Writing new account state");
        let mut accounts_cursor = self.tx_ref().cursor_write::<tables::PlainAccountState>()?;
        // write account to database.
        for (address, account) in changes.accounts {
            if let Some(account) = account {
                tracing::trace!(?address, "Updating plain state account");
                accounts_cursor.upsert(address, &account.into())?;
            } else if accounts_cursor.seek_exact(address)?.is_some() {
                tracing::trace!(?address, "Deleting plain state account");
                accounts_cursor.delete_current()?;
            }
        }

        // Write new storage state and wipe storage if needed.
        tracing::trace!(len = changes.storage.len(), "Writing new storage state");
        let mut storages_cursor = self.tx_ref().cursor_dup_write::<tables::PlainStorageState>()?;
        for PlainStorageChangeset { address, wipe_storage, storage } in changes.storage {
            // Wiping of storage.
            if wipe_storage && storages_cursor.seek_exact(address)?.is_some() {
                storages_cursor.delete_current_duplicates()?;
            }
            // cast storages to B256.
            let mut storage = storage
                .into_iter()
                .map(|(k, value)| StorageEntry { key: k.into(), value })
                .collect::<Vec<_>>();
            // sort storage slots by key.
            storage.par_sort_unstable_by_key(|a| a.key);

            for entry in storage {
                tracing::trace!(?address, ?entry.key, "Updating plain state storage");
                if let Some(db_entry) = storages_cursor.seek_by_key_subkey(address, entry.key)? &&
                    db_entry.key == entry.key
                {
                    storages_cursor.delete_current()?;
                }

                if !entry.value.is_zero() {
                    storages_cursor.upsert(address, &entry)?;
                }
            }
        }

        // Write bytecode
        tracing::trace!(len = changes.contracts.len(), "Writing bytecodes");
        self.write_bytecodes(
            changes.contracts.into_iter().map(|(hash, bytecode)| (hash, Bytecode(bytecode))),
        )?;

        Ok(())
    }

    #[instrument(level = "debug", target = "providers::db", skip_all)]
    fn write_hashed_state(&self, hashed_state: &HashedPostStateSorted) -> ProviderResult<()> {
        // Write hashed account updates.
        let mut hashed_accounts_cursor = self.tx_ref().cursor_write::<tables::HashedAccounts>()?;
        for (hashed_address, account) in hashed_state.accounts() {
            if let Some(account) = account {
                hashed_accounts_cursor.upsert(*hashed_address, account)?;
            } else if hashed_accounts_cursor.seek_exact(*hashed_address)?.is_some() {
                hashed_accounts_cursor.delete_current()?;
            }
        }

        // Write hashed storage changes.
        let sorted_storages = hashed_state.account_storages().iter().sorted_by_key(|(key, _)| *key);
        let mut hashed_storage_cursor =
            self.tx_ref().cursor_dup_write::<tables::HashedStorages>()?;
        for (hashed_address, storage) in sorted_storages {
            if storage.is_wiped() && hashed_storage_cursor.seek_exact(*hashed_address)?.is_some() {
                hashed_storage_cursor.delete_current_duplicates()?;
            }

            for (hashed_slot, value) in storage.storage_slots_ref() {
                let entry = StorageEntry { key: *hashed_slot, value: *value };

                if let Some(db_entry) =
                    hashed_storage_cursor.seek_by_key_subkey(*hashed_address, entry.key)? &&
                    db_entry.key == entry.key
                {
                    hashed_storage_cursor.delete_current()?;
                }

                if !entry.value.is_zero() {
                    hashed_storage_cursor.upsert(*hashed_address, &entry)?;
                }
            }
        }

        Ok(())
    }

    /// Remove the last N blocks of state.
    ///
    /// The latest state will be unwound
    ///
    /// 1. Read the body indices of the canonical blocks (the `Blocks` rows) to get all the
    ///    transaction ids.
    /// 2. Iterate over the [`StorageChangeSets`][tables::StorageChangeSets] table and the
    ///    [`AccountChangeSets`][tables::AccountChangeSets] tables in reverse order to reconstruct
    ///    the changesets.
    ///    - In order to have both the old and new values in the changesets, we also access the
    ///      plain state tables.
    /// 3. While iterating over the changeset tables, if we encounter a new account or storage slot,
    ///    we:
    ///     1. Take the old value from the changeset
    ///     2. Take the new value from the plain state
    ///     3. Save the old value to the local state
    /// 4. While iterating over the changeset tables, if we encounter an account/storage slot we
    ///    have seen before we:
    ///     1. Take the old value from the changeset
    ///     2. Take the new value from the local state
    ///     3. Set the local state to the value in the changeset
    fn remove_state_above(&self, block: BlockNumber) -> ProviderResult<()> {
        let range = block + 1..=self.last_block_number()?;

        if range.is_empty() {
            return Ok(());
        }

        let storage_range = BlockNumberAddress::range(range.clone());
        // Changeset rows live in the primary database only since Stage 3 (3A.3); take them
        // out of the tables.
        let storage_changeset =
            self.take::<tables::StorageChangeSets>(storage_range)?.into_iter().collect();
        let account_changeset = self.take::<tables::AccountChangeSets>(range)?;

        // This is not working for blocks that are not at tip. as plain state is not the last
        // state of end range. We should rename the functions or add support to access
        // History state. Accessing history state can be tricky but we are not gaining
        // anything.
        let mut plain_accounts_cursor = self.tx.cursor_write::<tables::PlainAccountState>()?;
        let mut plain_storage_cursor = self.tx.cursor_dup_write::<tables::PlainStorageState>()?;

        let (state, _) = self.populate_bundle_state(
            account_changeset,
            storage_changeset,
            &mut plain_accounts_cursor,
            &mut plain_storage_cursor,
        )?;

        for (address, (old_account, new_account, storage)) in &state {
            if old_account != new_account {
                let existing_entry = plain_accounts_cursor.seek_exact(*address)?;
                if let Some(account) = old_account {
                    plain_accounts_cursor.upsert(*address, account)?;
                } else if existing_entry.is_some() {
                    plain_accounts_cursor.delete_current()?;
                }
            }

            for (storage_key, (old_storage_value, _new_storage_value)) in storage {
                let storage_entry = StorageEntry { key: *storage_key, value: *old_storage_value };
                if plain_storage_cursor
                    .seek_by_key_subkey(*address, *storage_key)?
                    .is_some_and(|s| s.key == *storage_key)
                {
                    plain_storage_cursor.delete_current()?
                }

                if !old_storage_value.is_zero() {
                    plain_storage_cursor.upsert(*address, &storage_entry)?;
                }
            }
        }

        Ok(())
    }

    /// Take the last N blocks of state, recreating the [`ExecutionOutcome`].
    ///
    /// The latest state will be unwound and returned back with all the blocks
    ///
    /// 1. Read the body indices of the canonical blocks (the `Blocks` rows) to get all the
    ///    transaction ids.
    /// 2. Iterate over the [`StorageChangeSets`][tables::StorageChangeSets] table and the
    ///    [`AccountChangeSets`][tables::AccountChangeSets] tables in reverse order to reconstruct
    ///    the changesets.
    ///    - In order to have both the old and new values in the changesets, we also access the
    ///      plain state tables.
    /// 3. While iterating over the changeset tables, if we encounter a new account or storage slot,
    ///    we:
    ///     1. Take the old value from the changeset
    ///     2. Take the new value from the plain state
    ///     3. Save the old value to the local state
    /// 4. While iterating over the changeset tables, if we encounter an account/storage slot we
    ///    have seen before we:
    ///     1. Take the old value from the changeset
    ///     2. Take the new value from the local state
    ///     3. Set the local state to the value in the changeset
    fn remove_changesets_below(&self, floor: BlockNumber) -> ProviderResult<usize> {
        if floor == 0 {
            return Ok(0);
        }

        // `AccountChangeSets` is keyed by `BlockNumber` (big-endian), so `..floor` is a low-end
        // prefix range covering every block strictly below `floor`.
        let accounts = self.remove::<tables::AccountChangeSets>(..floor)?;

        // `StorageChangeSets` is keyed by `BlockNumberAddress`, which encodes the big-endian block
        // number first and the address second. Ordering therefore matches encoding, so the
        // equivalent `..floor` prefix range is correct here too.
        let storage =
            self.remove::<tables::StorageChangeSets>(BlockNumberAddressRange::from(..floor))?;

        Ok(accounts + storage)
    }
}

impl<TX: DbTxMut + DbTx + 'static, N: NodeTypes> DatabaseProvider<TX, N> {
    fn write_account_trie_updates<A: TrieTableAdapter>(
        tx: &TX,
        trie_updates: &TrieUpdatesSorted,
        num_entries: &mut usize,
    ) -> ProviderResult<()>
    where
        TX: DbTxMut,
    {
        let mut account_trie_cursor = tx.cursor_write::<A::AccountTrieTable>()?;
        // Process sorted account nodes
        for (key, updated_node) in trie_updates.account_nodes_ref() {
            let nibbles = A::AccountKey::from(*key);
            match updated_node {
                Some(node) => {
                    if !key.is_empty() {
                        *num_entries += 1;
                        account_trie_cursor.upsert(nibbles, node)?;
                    }
                }
                None => {
                    *num_entries += 1;
                    if account_trie_cursor.seek_exact(nibbles)?.is_some() {
                        account_trie_cursor.delete_current()?;
                    }
                }
            }
        }
        Ok(())
    }

    fn write_storage_tries<A: TrieTableAdapter>(
        tx: &TX,
        storage_tries: Vec<(&B256, &StorageTrieUpdatesSorted)>,
        num_entries: &mut usize,
    ) -> ProviderResult<()>
    where
        TX: DbTxMut,
    {
        let mut cursor = tx.cursor_dup_write::<A::StorageTrieTable>()?;
        for (hashed_address, storage_trie_updates) in storage_tries {
            let mut db_storage_trie_cursor: DatabaseStorageTrieCursor<_, A> =
                DatabaseStorageTrieCursor::new(cursor, *hashed_address);
            *num_entries +=
                db_storage_trie_cursor.write_storage_trie_updates_sorted(storage_trie_updates)?;
            cursor = db_storage_trie_cursor.cursor;
        }
        Ok(())
    }
}

impl<TX: DbTxMut + DbTx + 'static, N: NodeTypes> TrieWriter for DatabaseProvider<TX, N> {
    /// Writes trie updates to the database with already sorted updates.
    ///
    /// Returns the number of entries modified.
    #[instrument(level = "debug", target = "providers::db", skip_all)]
    fn write_trie_updates_sorted(&self, trie_updates: &TrieUpdatesSorted) -> ProviderResult<usize> {
        if trie_updates.is_empty() {
            return Ok(0);
        }

        // Track the number of inserted entries.
        let mut num_entries = 0;

        smartnet_trie_db::with_adapter!(self, |A| {
            Self::write_account_trie_updates::<A>(self.tx_ref(), trie_updates, &mut num_entries)?;
        });

        num_entries +=
            self.write_storage_trie_updates_sorted(trie_updates.storage_tries_ref().iter())?;

        Ok(num_entries)
    }
}

impl<TX: DbTxMut + DbTx + 'static, N: NodeTypes> StorageTrieWriter for DatabaseProvider<TX, N> {
    /// Writes storage trie updates from the given storage trie map with already sorted updates.
    ///
    /// Expects the storage trie updates to already be sorted by the hashed address key.
    ///
    /// Returns the number of entries modified.
    fn write_storage_trie_updates_sorted<'a>(
        &self,
        storage_tries: impl Iterator<Item = (&'a B256, &'a StorageTrieUpdatesSorted)>,
    ) -> ProviderResult<usize> {
        let mut num_entries = 0;
        let mut storage_tries = storage_tries.collect::<Vec<_>>();
        storage_tries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        smartnet_trie_db::with_adapter!(self, |A| {
            Self::write_storage_tries::<A>(self.tx_ref(), storage_tries, &mut num_entries)?;
        });
        Ok(num_entries)
    }
}

impl<TX: DbTxMut + DbTx + 'static, N: NodeTypes> HashingWriter for DatabaseProvider<TX, N> {
    fn unwind_account_hashing<'a>(
        &self,
        changesets: impl Iterator<Item = &'a (BlockNumber, AccountBeforeTx)>,
    ) -> ProviderResult<BTreeMap<B256, Option<Account>>> {
        // Aggregate all block changesets and make a list of accounts that have been changed.
        // Note that collecting and then reversing the order is necessary to ensure that the
        // changes are applied in the correct order.
        let hashed_accounts = changesets
            .into_iter()
            .map(|(_, e)| (keccak256(e.address), e.info))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<BTreeMap<_, _>>();

        // Apply values to HashedState, and remove the account if it's None.
        let mut hashed_accounts_cursor = self.tx.cursor_write::<tables::HashedAccounts>()?;
        for (hashed_address, account) in &hashed_accounts {
            if let Some(account) = account {
                hashed_accounts_cursor.upsert(*hashed_address, account)?;
            } else if hashed_accounts_cursor.seek_exact(*hashed_address)?.is_some() {
                hashed_accounts_cursor.delete_current()?;
            }
        }

        Ok(hashed_accounts)
    }

    fn insert_account_for_hashing(
        &self,
        changesets: impl IntoIterator<Item = (Address, Option<Account>)>,
    ) -> ProviderResult<BTreeMap<B256, Option<Account>>> {
        let mut hashed_accounts_cursor = self.tx.cursor_write::<tables::HashedAccounts>()?;
        let hashed_accounts =
            changesets.into_iter().map(|(ad, ac)| (keccak256(ad), ac)).collect::<BTreeMap<_, _>>();
        for (hashed_address, account) in &hashed_accounts {
            if let Some(account) = account {
                hashed_accounts_cursor.upsert(*hashed_address, account)?;
            } else if hashed_accounts_cursor.seek_exact(*hashed_address)?.is_some() {
                hashed_accounts_cursor.delete_current()?;
            }
        }
        Ok(hashed_accounts)
    }

    fn unwind_storage_hashing(
        &self,
        changesets: impl Iterator<Item = (BlockNumberAddress, StorageEntry)>,
    ) -> ProviderResult<B256Map<BTreeSet<B256>>> {
        // Aggregate all block changesets and make list of accounts that have been changed.
        let mut hashed_storages = changesets
            .into_iter()
            .map(|(BlockNumberAddress((_, address)), storage_entry)| {
                let hashed_key = keccak256(storage_entry.key);
                (keccak256(address), hashed_key, storage_entry.value)
            })
            .collect::<Vec<_>>();
        hashed_storages.sort_by_key(|(ha, hk, _)| (*ha, *hk));

        // Apply values to HashedState, and remove the account if it's None.
        let mut hashed_storage_keys: B256Map<BTreeSet<B256>> =
            B256Map::with_capacity_and_hasher(hashed_storages.len(), Default::default());
        let mut hashed_storage = self.tx.cursor_dup_write::<tables::HashedStorages>()?;
        for (hashed_address, key, value) in hashed_storages.into_iter().rev() {
            hashed_storage_keys.entry(hashed_address).or_default().insert(key);

            if hashed_storage
                .seek_by_key_subkey(hashed_address, key)?
                .is_some_and(|entry| entry.key == key)
            {
                hashed_storage.delete_current()?;
            }

            if !value.is_zero() {
                hashed_storage.upsert(hashed_address, &StorageEntry { key, value })?;
            }
        }
        Ok(hashed_storage_keys)
    }

    fn insert_storage_for_hashing(
        &self,
        storages: impl IntoIterator<Item = (Address, impl IntoIterator<Item = StorageEntry>)>,
    ) -> ProviderResult<B256Map<BTreeSet<B256>>> {
        // hash values
        let hashed_storages =
            storages.into_iter().fold(BTreeMap::new(), |mut map, (address, storage)| {
                let storage = storage.into_iter().fold(BTreeMap::new(), |mut map, entry| {
                    map.insert(keccak256(entry.key), entry.value);
                    map
                });
                map.insert(keccak256(address), storage);
                map
            });

        let hashed_storage_keys = hashed_storages
            .iter()
            .map(|(hashed_address, entries)| (*hashed_address, entries.keys().copied().collect()))
            .collect();

        let mut hashed_storage_cursor = self.tx.cursor_dup_write::<tables::HashedStorages>()?;
        // Hash the address and key and apply them to HashedStorage (if Storage is None
        // just remove it);
        hashed_storages.into_iter().try_for_each(|(hashed_address, storage)| {
            storage.into_iter().try_for_each(|(key, value)| -> ProviderResult<()> {
                if hashed_storage_cursor
                    .seek_by_key_subkey(hashed_address, key)?
                    .is_some_and(|entry| entry.key == key)
                {
                    hashed_storage_cursor.delete_current()?;
                }

                if !value.is_zero() {
                    hashed_storage_cursor.upsert(hashed_address, &StorageEntry { key, value })?;
                }
                Ok(())
            })
        })?;

        Ok(hashed_storage_keys)
    }
}

impl<TX: DbTxMut + DbTx + 'static, N: NodeTypes> HistoryWriter for DatabaseProvider<TX, N> {
    fn unwind_account_history_indices<'a>(
        &self,
        changesets: impl Iterator<Item = &'a (BlockNumber, AccountBeforeTx)>,
    ) -> ProviderResult<usize> {
        let mut last_indices = changesets
            .into_iter()
            .map(|(index, account)| (account.address, *index))
            .collect::<Vec<_>>();
        last_indices.sort_unstable_by_key(|(a, _)| *a);

        // Unwind the account history index in the primary database.
        let mut cursor = self.tx.cursor_write::<tables::AccountsHistory>()?;
        for &(address, rem_index) in &last_indices {
            let partial_shard = unwind_history_shards::<_, tables::AccountsHistory, _>(
                &mut cursor,
                ShardedKey::last(address),
                rem_index,
                |sharded_key| sharded_key.key == address,
            )?;

            // Check the last returned partial shard.
            // If it's not empty, the shard needs to be reinserted.
            if !partial_shard.is_empty() {
                cursor.insert(
                    ShardedKey::last(address),
                    &BlockNumberList::new_pre_sorted(partial_shard),
                )?;
            }
        }

        let changesets = last_indices.len();
        Ok(changesets)
    }

    fn insert_account_history_index(
        &self,
        account_transitions: impl IntoIterator<Item = (Address, impl IntoIterator<Item = u64>)>,
    ) -> ProviderResult<()> {
        self.append_history_index::<_, tables::AccountsHistory>(
            account_transitions,
            ShardedKey::new,
        )
    }

    fn unwind_storage_history_indices(
        &self,
        changesets: impl Iterator<Item = (BlockNumberAddress, StorageEntry)>,
    ) -> ProviderResult<usize> {
        let mut storage_changesets = changesets
            .into_iter()
            .map(|(BlockNumberAddress((bn, address)), storage)| (address, storage.key, bn))
            .collect::<Vec<_>>();
        storage_changesets.sort_unstable_by_key(|(address, key, _)| (*address, *key));

        // Unwind the storage history index in the primary database.
        let mut cursor = self.tx.cursor_write::<tables::StoragesHistory>()?;
        for &(address, storage_key, rem_index) in &storage_changesets {
            let partial_shard = unwind_history_shards::<_, tables::StoragesHistory, _>(
                &mut cursor,
                StorageShardedKey::last(address, storage_key),
                rem_index,
                |storage_sharded_key| {
                    storage_sharded_key.address == address &&
                        storage_sharded_key.sharded_key.key == storage_key
                },
            )?;

            // Check the last returned partial shard.
            // If it's not empty, the shard needs to be reinserted.
            if !partial_shard.is_empty() {
                cursor.insert(
                    StorageShardedKey::last(address, storage_key),
                    &BlockNumberList::new_pre_sorted(partial_shard),
                )?;
            }
        }

        let changesets = storage_changesets.len();
        Ok(changesets)
    }

    fn insert_storage_history_index(
        &self,
        storage_transitions: impl IntoIterator<Item = ((Address, B256), impl IntoIterator<Item = u64>)>,
    ) -> ProviderResult<()> {
        self.append_history_index::<_, tables::StoragesHistory>(
            storage_transitions,
            |(address, storage_key), highest_block_number| {
                StorageShardedKey::new(address, storage_key, highest_block_number)
            },
        )
    }

    #[instrument(level = "debug", target = "providers::db", skip_all)]
    fn update_history_indices(&self, range: RangeInclusive<BlockNumber>) -> ProviderResult<()> {
        let storage_settings = self.cached_storage_settings();
        if storage_settings.persist_account_history_index() {
            let indices = self.changed_accounts_and_blocks_with_range(range.clone())?;
            self.insert_account_history_index(indices)?;
        }

        if storage_settings.persist_storage_history_index() {
            let indices = self.changed_storages_and_blocks_with_range(range)?;
            self.insert_storage_history_index(indices)?;
        }

        Ok(())
    }
}

impl<TX: DbTxMut + DbTx + 'static, N: NodeTypesForProvider> BlockExecutionWriter
    for DatabaseProvider<TX, N>
{
    fn remove_block_and_execution_above(&self, block: BlockNumber) -> ProviderResult<()> {
        self.unwind_trie_state_from(block + 1)?;

        // remove execution res
        self.remove_state_above(block)?;

        // remove block bodies it is needed for both get block range and get block execution results
        // that is why it is deleted afterwards.
        self.remove_blocks_above(block)?;

        // Update pipeline progress
        self.update_pipeline_stages(block, true)?;

        Ok(())
    }
}

impl<TX: DbTxMut + DbTx + 'static, N: NodeTypesForProvider> BlockWriter
    for DatabaseProvider<TX, N>
{
    type Block = BlockTy<N>;
    type Receipt = ReceiptTy<N>;

    fn remove_blocks_above(&self, block: BlockNumber) -> ProviderResult<()> {
        let last_block_number = self.last_block_number()?;

        // First transaction to be removed: the next tx number of the last surviving block.
        // `block_body_indices` is file-tier aware, so this resolves for engine-written heights.
        let unwind_tx_from = self
            .block_body_indices(block)?
            .map(|b| b.next_tx_num())
            .ok_or(ProviderError::BlockBodyIndicesNotFound(block))?;

        // Last transaction to be removed. The engine write path stores no `Blocks` row
        // (`save_block_state_and_index` writes the index + state only), so reading the tip's
        // body indices from `tables::Blocks` failed for every engine-written chain.
        // `TxLocations` is the numbering authority; its last key is the highest assigned
        // transaction number, which is the range end. An empty table yields an empty range.
        let unwind_tx_to = self
            .tx
            .cursor_read::<tables::TxLocations>()?
            .last()?
            .map(|(tx_num, _)| tx_num)
            .unwrap_or_else(|| unwind_tx_from.saturating_sub(1));

        // Collect the hashes of the canonical blocks being removed, then drop the canonical
        // index entries, the (legacy) block payload rows, and the hash→number index rows.
        // `HeaderNumbers` is written by `save_block_state_and_index` and has no other cleanup.
        let removed_hashes = self.canonical_hashes_range(block + 1, last_block_number + 1)?;
        self.remove::<tables::CanonicalChain>(block + 1..)?;
        for hash in removed_hashes {
            self.tx.delete::<tables::HeaderNumbers>(hash, None)?;
        }

        // `TxLocations` is the tx-number index (and the numbering authority): delete the whole
        // range at and above the unwind point so the counter rewinds with the unwound blocks and
        // no rows are leaked.
        self.remove::<tables::TxLocations>(unwind_tx_from..)?;

        if unwind_tx_from <= unwind_tx_to {
            // Legacy/Fjall heights still carry `Transactions` rows; the file tier carries none,
            // so this only deletes the hash→number index for rows that exist. File-tier heights
            // re-write their (rewound) entries on re-sync.
            let hashes = self.transaction_hashes_by_range(unwind_tx_from..(unwind_tx_to + 1))?;
            self.with_aux_db_batch(|batch| {
                let mut writer = EitherWriter::new_transaction_hash_numbers(self, batch)?;
                for (hash, _) in hashes {
                    writer.delete_transaction_hash_number(hash)?;
                }
                Ok(((), writer.into_raw_aux_db_batch()))
            })?;
        }

        // Sender fields are cleared by the sender-recovery stage unwind (it runs
        // before the bodies unwind); the row deletion below removes the rows
        // entirely, which subsumes the old sender-table prune.

        self.remove_bodies_above(block)?;

        Ok(())
    }

    fn remove_bodies_above(&self, block: BlockNumber) -> ProviderResult<()> {
        // Clears the body parts (ommmers/withdrawals) of the still-existing canonical
        // rows above the unwind point; the headers stage unwind deletes the rows.
        self.storage.writer().remove_block_bodies_above(self, block)?;

        // First transaction to be removed
        let unwind_tx_from = self
            .block_body_indices(block)?
            .map(|b| b.next_tx_num())
            .ok_or(ProviderError::BlockBodyIndicesNotFound(block))?;

        // Drop the tx-number index rows above the unwind point; the payload lives in the
        // file tier and is truncated by the recovery path.
        self.remove::<tables::TxLocations>(unwind_tx_from..)?;

        Ok(())
    }
}

/// Test-only fixture writers. The engine is the only production block writer
/// (`save_block_state_and_index` writes the index + state and the payload lives in the file tier);
/// these methods seed the index into Fjall for fixtures across the workspace. They are compiled
/// only for tests or the `test-utils` feature — never in production.
#[cfg(any(test, feature = "test-utils"))]
impl<TX: DbTxMut + DbTx + 'static, N: NodeTypesForProvider> DatabaseProvider<TX, N> {
    /// Inserts the block's index rows into the database (test-only fixture).
    ///
    /// No payload is written: transaction locations and hash lookups point at the file tier.
    pub fn insert_block(
        &self,
        block: &RecoveredBlock<BlockTy<N>>,
    ) -> ProviderResult<StoredBlockBodyIndices> {
        let block_number = block.number();
        let tx_count = block.body().transaction_count() as u64;
        let first_tx_num = self.next_tx_number()?;
        let write_tx_hash_numbers =
            self.prune_modes.transaction_lookup.is_none_or(|m| !m.is_full());

        self.tx.put::<tables::CanonicalChain>(block_number, block.hash())?;
        self.tx.put::<tables::HeaderNumbers>(block.hash(), block_number)?;
        for (idx, tx) in block.body().transactions().iter().enumerate() {
            let tx_num = first_tx_num + idx as u64;
            self.tx.put::<tables::TxLocations>(
                tx_num,
                StoredTransactionLocation::new(block_number, idx as u64),
            )?;
            if write_tx_hash_numbers {
                self.tx.put::<tables::TransactionHashNumbers>(*tx.tx_hash(), tx_num)?;
            }
        }

        Ok(StoredBlockBodyIndices { first_tx_num, tx_count })
    }

    /// Appends blocks with their execution state to the database (test-only fixture).
    ///
    /// History indices are written to the appropriate backend based on storage settings:
    /// the primary database when `*_history_in_aux_db` is false, `aux DB` when true.
    pub fn append_blocks_with_state(
        &self,
        blocks: Vec<RecoveredBlock<BlockTy<N>>>,
        execution_outcome: &ExecutionOutcome<ReceiptTy<N>>,
        hashed_state: HashedPostStateSorted,
    ) -> ProviderResult<()> {
        if blocks.is_empty() {
            debug!(target: "providers::db", "Attempted to append empty block range");
            return Ok(());
        }

        // Blocks are not empty, so no need to handle the case of `blocks.first()` being
        // `None`.
        let first_number = blocks[0].number();

        // Blocks are not empty, so no need to handle the case of `blocks.last()` being
        // `None`.
        let last_block_number = blocks[blocks.len() - 1].number();

        let mut durations_recorder = metrics::DurationsRecorder::new(&self.metrics);

        // Extract account and storage transitions from the bundle reverts BEFORE writing state.
        // Changesets live in the primary database
        // whose index isn't updated until commit, making them invisible to subsequent reads
        // within the same transaction.
        let (account_transitions, storage_transitions) = {
            let mut account_transitions: BTreeMap<Address, Vec<u64>> = BTreeMap::new();
            let mut storage_transitions: BTreeMap<(Address, B256), Vec<u64>> = BTreeMap::new();
            for (block_idx, block_reverts) in execution_outcome.bundle.reverts.iter().enumerate() {
                let block_number = first_number + block_idx as u64;
                for (address, account_revert) in block_reverts {
                    account_transitions.entry(*address).or_default().push(block_number);
                    for storage_key in account_revert.storage.keys() {
                        let key = B256::from(storage_key.to_be_bytes());
                        storage_transitions.entry((*address, key)).or_default().push(block_number);
                    }
                }
            }
            (account_transitions, storage_transitions)
        };

        // Insert the blocks
        for block in blocks {
            self.insert_block(&block)?;
            durations_recorder.record_relative(metrics::Action::InsertBlock);
        }

        self.write_state(execution_outcome, OriginalValuesKnown::No, StateWriteConfig::default())?;
        durations_recorder.record_relative(metrics::Action::InsertState);

        // insert hashes and intermediate merkle nodes
        self.write_hashed_state(&hashed_state)?;
        durations_recorder.record_relative(metrics::Action::InsertHashes);

        // Use pre-computed transitions for history indices since
        // writes aren't visible until commit.
        let storage_settings = self.cached_storage_settings();
        if storage_settings.persist_account_history_index() {
            self.insert_account_history_index(account_transitions)?;
        }
        if storage_settings.persist_storage_history_index() {
            self.insert_storage_history_index(storage_transitions)?;
        }
        durations_recorder.record_relative(metrics::Action::InsertHistoryIndices);

        // Update pipeline progress
        self.update_pipeline_stages(last_block_number, false)?;
        durations_recorder.record_relative(metrics::Action::UpdatePipelineStages);

        debug!(target: "providers::db", range = ?first_number..=last_block_number, actions = ?durations_recorder.actions, "Appended blocks");

        Ok(())
    }
}

impl<TX: DbTx + 'static, N: NodeTypes> PruneCheckpointReader for DatabaseProvider<TX, N> {
    fn get_prune_checkpoint(
        &self,
        segment: PruneSegment,
    ) -> ProviderResult<Option<PruneCheckpoint>> {
        Ok(self.tx.get::<tables::PruneCheckpoints>(segment)?)
    }

    fn get_prune_checkpoints(&self) -> ProviderResult<Vec<(PruneSegment, PruneCheckpoint)>> {
        Ok(PruneSegment::variants()
            .filter_map(|segment| {
                self.tx
                    .get::<tables::PruneCheckpoints>(segment)
                    .transpose()
                    .map(|chk| chk.map(|chk| (segment, chk)))
            })
            .collect::<Result<_, _>>()?)
    }
}

impl<TX: DbTxMut, N: NodeTypes> PruneCheckpointWriter for DatabaseProvider<TX, N> {
    fn save_prune_checkpoint(
        &self,
        segment: PruneSegment,
        checkpoint: PruneCheckpoint,
    ) -> ProviderResult<()> {
        Ok(self.tx.put::<tables::PruneCheckpoints>(segment, checkpoint)?)
    }
}

impl<TX: DbTx + 'static, N: NodeTypesForProvider> StatsReader for DatabaseProvider<TX, N> {
    fn count_entries<T: Table>(&self) -> ProviderResult<usize> {
        // The primary database holds the authoritative rows for every table that is mirrored
        // stored once, so counting both stores would double count.
        Ok(self.tx.entries::<T>()?)
    }
}

impl<TX: DbTx + 'static, N: NodeTypes> ChainStateBlockReader for DatabaseProvider<TX, N> {
    fn last_finalized_block_number(&self) -> ProviderResult<Option<BlockNumber>> {
        let mut finalized_blocks = self
            .tx
            .cursor_read::<tables::ChainState>()?
            .walk(Some(tables::ChainStateKey::LastFinalizedBlock))?
            .take(1)
            .collect::<Result<BTreeMap<tables::ChainStateKey, BlockNumber>, _>>()?;

        let last_finalized_block_number = finalized_blocks.pop_first().map(|pair| pair.1);
        Ok(last_finalized_block_number)
    }

    fn last_safe_block_number(&self) -> ProviderResult<Option<BlockNumber>> {
        let mut finalized_blocks = self
            .tx
            .cursor_read::<tables::ChainState>()?
            .walk(Some(tables::ChainStateKey::LastSafeBlock))?
            .take(1)
            .collect::<Result<BTreeMap<tables::ChainStateKey, BlockNumber>, _>>()?;

        let last_finalized_block_number = finalized_blocks.pop_first().map(|pair| pair.1);
        Ok(last_finalized_block_number)
    }
}

impl<TX: DbTxMut, N: NodeTypes> ChainStateBlockWriter for DatabaseProvider<TX, N> {
    fn save_finalized_block_number(&self, block_number: BlockNumber) -> ProviderResult<()> {
        Ok(self
            .tx
            .put::<tables::ChainState>(tables::ChainStateKey::LastFinalizedBlock, block_number)?)
    }

    fn save_safe_block_number(&self, block_number: BlockNumber) -> ProviderResult<()> {
        Ok(self.tx.put::<tables::ChainState>(tables::ChainStateKey::LastSafeBlock, block_number)?)
    }
}

impl<TX: DbTx + 'static, N: NodeTypes + 'static> DBProvider for DatabaseProvider<TX, N> {
    type Tx = TX;

    fn tx_ref(&self) -> &Self::Tx {
        &self.tx
    }

    fn tx_mut(&mut self) -> &mut Self::Tx {
        &mut self.tx
    }

    fn into_tx(self) -> Self::Tx {
        self.tx
    }

    fn prune_modes_ref(&self) -> &PruneModes {
        self.prune_modes_ref()
    }

    /// Commit database transaction and pending `aux DB` batches.
    #[instrument(
        name = "DatabaseProvider::commit",
        level = "debug",
        target = "providers::db",
        skip_all
    )]
    fn commit(self) -> ProviderResult<()> {
        let mut timings = metrics::CommitTimings::default();

        let start = Instant::now();
        let batches = std::mem::take(&mut *self.pending_aux_db_batches.lock());
        if let Some(aux_db_provider) = &self.aux_db_provider {
            for batch in batches {
                aux_db_provider.commit_batch(batch)?;
            }
        }
        timings.aux_db = start.elapsed();

        let start = Instant::now();
        self.tx.commit()?;
        timings.db = start.elapsed();

        self.metrics.record_commit(&timings);

        Ok(())
    }
}

impl<TX: DbTx, N: NodeTypes> MetadataProvider for DatabaseProvider<TX, N> {
    fn get_metadata(&self, key: &str) -> ProviderResult<Option<Vec<u8>>> {
        self.tx.get::<tables::Metadata>(key.to_string()).map_err(Into::into)
    }
}

impl<TX: DbTxMut, N: NodeTypes> MetadataWriter for DatabaseProvider<TX, N> {
    fn write_metadata(&self, key: &str, value: Vec<u8>) -> ProviderResult<()> {
        self.tx.put::<tables::Metadata>(key.to_string(), value).map_err(Into::into)
    }
}

impl<TX: Send, N: NodeTypes> StorageSettingsCache for DatabaseProvider<TX, N> {
    fn cached_storage_settings(&self) -> StorageSettings {
        *self.storage_settings.read()
    }

    fn set_storage_settings_cache(&self, settings: StorageSettings) {
        *self.storage_settings.write() = settings;
    }
}

impl<TX: Send, N: NodeTypes> StoragePath for DatabaseProvider<TX, N> {
    fn storage_path(&self) -> PathBuf {
        self.db_path.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_utils::{
            blocks::BlockchainTestData, create_test_provider_factory, MockNodeTypesWithDB,
        },
        ProviderFactory,
    };
    use alloy_primitives::{
        map::{AddressMap, B256Map},
        U256,
    };
    use smartnet_chain_state::{ComputedTrieData, ExecutedBlock};
    use smartnet_db_api::models::{StorageSettings, StoredTransactionLocation};
    use smartnet_execution_types::{AccountRevertInit, BlockExecutionOutput, BlockExecutionResult};
    use smartnet_primitives_traits::{Account, StorageEntry};
    use smartnet_prune_types::PruneMode;
    use smartnet_storage_api::{DatabaseProviderFactory, StorageChangeSetReader};
    use smartnet_testing_utils::generators::{self, random_block, BlockParams};
    use smartnet_trie::{Nibbles, StoredNibbles, StoredNibblesSubKey};

    #[test]
    fn test_receipts_by_block_range_empty_range() {
        let factory = create_test_provider_factory();
        let provider = factory.provider().unwrap();

        // empty range should return empty vec
        let start = 10u64;
        let end = 9u64;
        let result = provider.receipts_by_block_range(start..=end).unwrap();
        assert_eq!(result, Vec::<Vec<smartnet_ethereum_primitives::Receipt>>::new());
    }

    #[test]
    fn test_receipts_by_block_range_nonexistent_blocks() {
        let factory = create_test_provider_factory();
        let provider = factory.provider().unwrap();

        // non-existent blocks should return empty vecs for each block
        let result = provider.receipts_by_block_range(10..=12).unwrap();
        assert_eq!(result, vec![vec![], vec![], vec![]]);
    }

    #[test]
    fn test_receipts_by_block_range_all_empty_blocks() {
        let factory = create_test_provider_factory();
        let mut rng = generators::rng();

        // create blocks with no transactions
        let mut blocks = Vec::new();
        for i in 0..3 {
            let block =
                random_block(&mut rng, i, BlockParams { tx_count: Some(0), ..Default::default() });
            blocks.push(block);
        }

        let provider_rw = factory.provider_rw().unwrap();
        for block in blocks {
            provider_rw.insert_block(&block.try_recover().unwrap()).unwrap();
        }
        provider_rw.commit().unwrap();

        let provider = factory.provider().unwrap();
        let result = provider.receipts_by_block_range(1..=3).unwrap();

        assert_eq!(result.len(), 3);
        for block_receipts in result {
            assert_eq!(block_receipts.len(), 0);
        }
    }

    #[test]
    fn test_receipts_by_block_range_consistency_with_individual_calls() {
        let factory = create_test_provider_factory();
        let data = BlockchainTestData::default();

        let provider_rw = factory.provider_rw().unwrap();
        provider_rw.insert_block(&data.genesis.try_recover().unwrap()).unwrap();
        provider_rw
            .write_state(
                &ExecutionOutcome { first_block: 0, receipts: vec![vec![]], ..Default::default() },
                crate::OriginalValuesKnown::No,
                StateWriteConfig::default(),
            )
            .unwrap();
        for (recovered, outcome) in &data.blocks {
            provider_rw.insert_block(recovered).unwrap();
            provider_rw
                .write_state(outcome, crate::OriginalValuesKnown::No, StateWriteConfig::default())
                .unwrap();
        }
        provider_rw.commit().unwrap();

        let provider = factory.provider().unwrap();

        // get receipts using block range method
        let range_result = provider.receipts_by_block_range(1..=3).unwrap();

        // get receipts using individual block calls
        let mut individual_results = Vec::new();
        for block_num in 1..=3 {
            let receipts =
                provider.receipts_by_block(block_num.into()).unwrap().unwrap_or_default();
            individual_results.push(receipts);
        }

        assert_eq!(range_result, individual_results);
    }

    #[test]
    fn test_write_trie_updates_sorted() {
        use smartnet_trie::{
            updates::{StorageTrieUpdatesSorted, TrieUpdatesSorted},
            BranchNodeCompact, StorageTrieEntry,
        };

        let factory = create_test_provider_factory();
        let provider_rw = factory.provider_rw().unwrap();

        // Pre-populate account trie with data that will be deleted
        {
            let tx = provider_rw.tx_ref();
            let mut cursor = tx.cursor_write::<tables::AccountsTrie>().unwrap();

            // Add account node that will be deleted
            let to_delete = StoredNibbles(Nibbles::from_nibbles([0x3, 0x4]));
            cursor
                .upsert(
                    to_delete,
                    &BranchNodeCompact::new(
                        0b1010_1010_1010_1010, // state_mask
                        0b0000_0000_0000_0000, // tree_mask
                        0b0000_0000_0000_0000, // hash_mask
                        vec![],
                        None,
                    ),
                )
                .unwrap();

            // Add account node that will be updated
            let to_update = StoredNibbles(Nibbles::from_nibbles([0x1, 0x2]));
            cursor
                .upsert(
                    to_update,
                    &BranchNodeCompact::new(
                        0b0101_0101_0101_0101, // old state_mask (will be updated)
                        0b0000_0000_0000_0000, // tree_mask
                        0b0000_0000_0000_0000, // hash_mask
                        vec![],
                        None,
                    ),
                )
                .unwrap();
        }

        // Pre-populate storage tries with data
        let storage_address1 = B256::from([1u8; 32]);
        let storage_address2 = B256::from([2u8; 32]);
        {
            let tx = provider_rw.tx_ref();
            let mut storage_cursor = tx.cursor_dup_write::<tables::StoragesTrie>().unwrap();

            // Add storage nodes for address1 (one will be deleted)
            storage_cursor
                .upsert(
                    storage_address1,
                    &StorageTrieEntry {
                        nibbles: StoredNibblesSubKey(Nibbles::from_nibbles([0x2, 0x0])),
                        node: BranchNodeCompact::new(
                            0b0011_0011_0011_0011, // will be deleted
                            0b0000_0000_0000_0000,
                            0b0000_0000_0000_0000,
                            vec![],
                            None,
                        ),
                    },
                )
                .unwrap();

            // Add storage nodes for address2 (will be wiped)
            storage_cursor
                .upsert(
                    storage_address2,
                    &StorageTrieEntry {
                        nibbles: StoredNibblesSubKey(Nibbles::from_nibbles([0xa, 0xb])),
                        node: BranchNodeCompact::new(
                            0b1100_1100_1100_1100, // will be wiped
                            0b0000_0000_0000_0000,
                            0b0000_0000_0000_0000,
                            vec![],
                            None,
                        ),
                    },
                )
                .unwrap();
            storage_cursor
                .upsert(
                    storage_address2,
                    &StorageTrieEntry {
                        nibbles: StoredNibblesSubKey(Nibbles::from_nibbles([0xc, 0xd])),
                        node: BranchNodeCompact::new(
                            0b0011_1100_0011_1100, // will be wiped
                            0b0000_0000_0000_0000,
                            0b0000_0000_0000_0000,
                            vec![],
                            None,
                        ),
                    },
                )
                .unwrap();
        }

        // Create sorted account trie updates
        let account_nodes = vec![
            (
                Nibbles::from_nibbles([0x1, 0x2]),
                Some(BranchNodeCompact::new(
                    0b1111_1111_1111_1111, // state_mask (updated)
                    0b0000_0000_0000_0000, // tree_mask
                    0b0000_0000_0000_0000, // hash_mask (no hashes)
                    vec![],
                    None,
                )),
            ),
            (Nibbles::from_nibbles([0x3, 0x4]), None), // Deletion
            (
                Nibbles::from_nibbles([0x5, 0x6]),
                Some(BranchNodeCompact::new(
                    0b1111_1111_1111_1111, // state_mask
                    0b0000_0000_0000_0000, // tree_mask
                    0b0000_0000_0000_0000, // hash_mask (no hashes)
                    vec![],
                    None,
                )),
            ),
        ];

        // Create sorted storage trie updates
        let storage_trie1 = StorageTrieUpdatesSorted {
            is_deleted: false,
            storage_nodes: vec![
                (
                    Nibbles::from_nibbles([0x1, 0x0]),
                    Some(BranchNodeCompact::new(
                        0b1111_0000_0000_0000, // state_mask
                        0b0000_0000_0000_0000, // tree_mask
                        0b0000_0000_0000_0000, // hash_mask (no hashes)
                        vec![],
                        None,
                    )),
                ),
                (Nibbles::from_nibbles([0x2, 0x0]), None), // Deletion of existing node
            ],
        };

        let storage_trie2 = StorageTrieUpdatesSorted {
            is_deleted: true, // Wipe all storage for this address
            storage_nodes: vec![],
        };

        let mut storage_tries = B256Map::default();
        storage_tries.insert(storage_address1, storage_trie1);
        storage_tries.insert(storage_address2, storage_trie2);

        let trie_updates = TrieUpdatesSorted::new(account_nodes, storage_tries);

        // Write the sorted trie updates
        let num_entries = provider_rw.write_trie_updates_sorted(&trie_updates).unwrap();

        // We should have 2 account insertions + 1 account deletion + 1 storage insertion + 1
        // storage deletion = 5
        assert_eq!(num_entries, 5);

        // Verify account trie updates were written correctly
        let tx = provider_rw.tx_ref();
        let mut cursor = tx.cursor_read::<tables::AccountsTrie>().unwrap();

        // Check first account node was updated
        let nibbles1 = StoredNibbles(Nibbles::from_nibbles([0x1, 0x2]));
        let entry1 = cursor.seek_exact(nibbles1).unwrap();
        assert!(entry1.is_some(), "Updated account node should exist");
        let expected_mask = smartnet_trie::TrieMask::new(0b1111_1111_1111_1111);
        assert_eq!(
            entry1.unwrap().1.state_mask,
            expected_mask,
            "Account node should have updated state_mask"
        );

        // Check deleted account node no longer exists
        let nibbles2 = StoredNibbles(Nibbles::from_nibbles([0x3, 0x4]));
        let entry2 = cursor.seek_exact(nibbles2).unwrap();
        assert!(entry2.is_none(), "Deleted account node should not exist");

        // Check new account node exists
        let nibbles3 = StoredNibbles(Nibbles::from_nibbles([0x5, 0x6]));
        let entry3 = cursor.seek_exact(nibbles3).unwrap();
        assert!(entry3.is_some(), "New account node should exist");

        // Verify storage trie updates were written correctly
        let mut storage_cursor = tx.cursor_dup_read::<tables::StoragesTrie>().unwrap();

        // Check storage for address1
        let storage_entries1: Vec<_> = storage_cursor
            .walk_dup(Some(storage_address1), None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            storage_entries1.len(),
            1,
            "Storage address1 should have 1 entry after deletion"
        );
        assert_eq!(
            storage_entries1[0].1.nibbles.0,
            Nibbles::from_nibbles([0x1, 0x0]),
            "Remaining entry should be [0x1, 0x0]"
        );

        // Check storage for address2 was wiped
        let storage_entries2: Vec<_> = storage_cursor
            .walk_dup(Some(storage_address2), None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(storage_entries2.len(), 0, "Storage address2 should be empty after wipe");

        provider_rw.commit().unwrap();
    }

    #[test]
    fn test_try_into_history_rejects_unexecuted_blocks() {
        use smartnet_storage_api::TryIntoHistoricalStateProvider;

        let factory = create_test_provider_factory();

        // Insert genesis block to have some data
        let data = BlockchainTestData::default();
        let provider_rw = factory.provider_rw().unwrap();
        provider_rw.insert_block(&data.genesis.try_recover().unwrap()).unwrap();
        provider_rw
            .write_state(
                &ExecutionOutcome { first_block: 0, receipts: vec![vec![]], ..Default::default() },
                crate::OriginalValuesKnown::No,
                StateWriteConfig::default(),
            )
            .unwrap();
        provider_rw.commit().unwrap();

        // Get a fresh provider - Execution checkpoint is 0, no receipts written beyond genesis
        let provider = factory.provider().unwrap();

        // Requesting historical state for block 0 (executed) should succeed
        let result = provider.try_into_history_at_block(0);
        assert!(result.is_ok(), "Block 0 should be available");

        // Get another provider and request state for block 100 (not executed)
        let provider = factory.provider().unwrap();
        let result = provider.try_into_history_at_block(100);

        // Should fail with BlockNotExecuted error
        match result {
            Err(ProviderError::BlockNotExecuted { requested: 100, .. }) => {}
            Err(e) => panic!("Expected BlockNotExecuted error, got: {e:?}"),
            Ok(_) => panic!("Expected error, got Ok"),
        }
    }

    #[test]
    fn test_unwind_storage_hashing_with_hashed_state() {
        let factory = create_test_provider_factory();

        let address = Address::random();
        let hashed_address = keccak256(address);

        let plain_slot = B256::random();
        let hashed_slot = keccak256(plain_slot);

        let current_value = U256::from(100);
        let old_value = U256::from(42);

        let provider_rw = factory.provider_rw().unwrap();
        provider_rw
            .tx
            .cursor_dup_write::<tables::HashedStorages>()
            .unwrap()
            .upsert(hashed_address, &StorageEntry { key: hashed_slot, value: current_value })
            .unwrap();

        let changesets = vec![(
            BlockNumberAddress((1, address)),
            StorageEntry { key: plain_slot, value: old_value },
        )];

        let result = provider_rw.unwind_storage_hashing(changesets.into_iter()).unwrap();

        assert_eq!(result.len(), 1);
        assert!(result.contains_key(&hashed_address));
        assert!(result[&hashed_address].contains(&hashed_slot));

        let mut cursor = provider_rw.tx.cursor_dup_read::<tables::HashedStorages>().unwrap();
        let entry = cursor
            .seek_by_key_subkey(hashed_address, hashed_slot)
            .unwrap()
            .expect("entry should exist");
        assert_eq!(entry.key, hashed_slot);
        assert_eq!(entry.value, old_value);
    }

    #[test]
    fn test_write_and_remove_state_roundtrip_legacy() {
        let factory = create_test_provider_factory();
        let storage_settings = StorageSettings::v1();
        factory.set_storage_settings_cache(storage_settings);

        let address = Address::with_last_byte(1);
        let hashed_address = keccak256(address);
        let slot = U256::from(5);
        let slot_key = B256::from(slot);
        let hashed_slot = keccak256(slot_key);

        let mut rng = generators::rng();
        let block0 =
            random_block(&mut rng, 0, BlockParams { tx_count: Some(0), ..Default::default() });
        let block1 =
            random_block(&mut rng, 1, BlockParams { tx_count: Some(0), ..Default::default() });

        {
            let provider_rw = factory.provider_rw().unwrap();
            provider_rw.insert_block(&block0.try_recover().unwrap()).unwrap();
            provider_rw.insert_block(&block1.try_recover().unwrap()).unwrap();
            provider_rw
                .tx
                .cursor_write::<tables::PlainAccountState>()
                .unwrap()
                .upsert(address, &Account { nonce: 0, balance: U256::ZERO, bytecode_hash: None })
                .unwrap();
            provider_rw.commit().unwrap();
        }

        let provider_rw = factory.provider_rw().unwrap();

        let mut state_init: BundleStateInit = AddressMap::default();
        let mut storage_map: B256Map<(U256, U256)> = B256Map::default();
        storage_map.insert(slot_key, (U256::ZERO, U256::from(10)));
        state_init.insert(
            address,
            (
                Some(Account { nonce: 0, balance: U256::ZERO, bytecode_hash: None }),
                Some(Account { nonce: 1, balance: U256::ZERO, bytecode_hash: None }),
                storage_map,
            ),
        );

        let mut reverts_init: RevertsInit = HashMap::default();
        let mut block_reverts: AddressMap<AccountRevertInit> = AddressMap::default();
        block_reverts.insert(
            address,
            (
                Some(Some(Account { nonce: 0, balance: U256::ZERO, bytecode_hash: None })),
                vec![StorageEntry { key: slot_key, value: U256::ZERO }],
            ),
        );
        reverts_init.insert(1, block_reverts);

        let execution_outcome =
            ExecutionOutcome::new_init(state_init, reverts_init, [], vec![vec![]], 1, vec![]);

        provider_rw
            .write_state(
                &execution_outcome,
                OriginalValuesKnown::Yes,
                StateWriteConfig {
                    write_receipts: false,
                    write_account_changesets: true,
                    write_storage_changesets: true,
                },
            )
            .unwrap();

        let hashed_state =
            execution_outcome.hash_state_slow::<smartnet_trie::KeccakKeyHasher>().into_sorted();
        provider_rw.write_hashed_state(&hashed_state).unwrap();

        let account = provider_rw
            .tx
            .cursor_read::<tables::PlainAccountState>()
            .unwrap()
            .seek_exact(address)
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(account.nonce, 1);

        let storage_entry = provider_rw
            .tx
            .cursor_dup_read::<tables::PlainStorageState>()
            .unwrap()
            .seek_by_key_subkey(address, slot_key)
            .unwrap()
            .unwrap();
        assert_eq!(storage_entry.key, slot_key);
        assert_eq!(storage_entry.value, U256::from(10));

        let hashed_entry = provider_rw
            .tx
            .cursor_dup_read::<tables::HashedStorages>()
            .unwrap()
            .seek_by_key_subkey(hashed_address, hashed_slot)
            .unwrap()
            .unwrap();
        assert_eq!(hashed_entry.key, hashed_slot);
        assert_eq!(hashed_entry.value, U256::from(10));

        let account_cs_entries = provider_rw
            .tx
            .cursor_dup_read::<tables::AccountChangeSets>()
            .unwrap()
            .walk(Some(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(!account_cs_entries.is_empty());

        let storage_cs_entries = provider_rw
            .tx
            .cursor_read::<tables::StorageChangeSets>()
            .unwrap()
            .walk(Some(BlockNumberAddress((1, address))))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(!storage_cs_entries.is_empty());
        assert_eq!(storage_cs_entries[0].1.key, slot_key);

        provider_rw.remove_state_above(0).unwrap();

        let restored_account = provider_rw
            .tx
            .cursor_read::<tables::PlainAccountState>()
            .unwrap()
            .seek_exact(address)
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(restored_account.nonce, 0);

        let storage_gone = provider_rw
            .tx
            .cursor_dup_read::<tables::PlainStorageState>()
            .unwrap()
            .seek_by_key_subkey(address, slot_key)
            .unwrap();
        assert!(storage_gone.is_none() || storage_gone.unwrap().key != slot_key);

        let account_cs_after = provider_rw
            .tx
            .cursor_dup_read::<tables::AccountChangeSets>()
            .unwrap()
            .walk(Some(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(account_cs_after.is_empty());

        let storage_cs_after = provider_rw
            .tx
            .cursor_read::<tables::StorageChangeSets>()
            .unwrap()
            .walk(Some(BlockNumberAddress((1, address))))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(storage_cs_after.is_empty());
    }

    #[test]
    fn test_unwind_storage_hashing_legacy() {
        let factory = create_test_provider_factory();
        let storage_settings = StorageSettings::v1();
        factory.set_storage_settings_cache(storage_settings);

        let address = Address::random();
        let hashed_address = keccak256(address);

        let plain_slot = B256::random();
        let hashed_slot = keccak256(plain_slot);

        let current_value = U256::from(100);
        let old_value = U256::from(42);

        let provider_rw = factory.provider_rw().unwrap();
        provider_rw
            .tx
            .cursor_dup_write::<tables::HashedStorages>()
            .unwrap()
            .upsert(hashed_address, &StorageEntry { key: hashed_slot, value: current_value })
            .unwrap();

        let changesets = vec![(
            BlockNumberAddress((1, address)),
            StorageEntry { key: plain_slot, value: old_value },
        )];

        let result = provider_rw.unwind_storage_hashing(changesets.into_iter()).unwrap();

        assert_eq!(result.len(), 1);
        assert!(result.contains_key(&hashed_address));
        assert!(result[&hashed_address].contains(&hashed_slot));

        let mut cursor = provider_rw.tx.cursor_dup_read::<tables::HashedStorages>().unwrap();
        let entry = cursor
            .seek_by_key_subkey(hashed_address, hashed_slot)
            .unwrap()
            .expect("entry should exist");
        assert_eq!(entry.key, hashed_slot);
        assert_eq!(entry.value, old_value);
    }

    /// 3A.1: `save_blocks` mirrors the ledger payload (headers, canonical hashes,
    /// transactions, receipts) into the primary database.
    #[test]
    fn test_save_blocks_writes_db_payload_tables() {
        let factory = create_test_provider_factory();
        factory.set_storage_settings_cache(StorageSettings::v1());

        let data = BlockchainTestData::default();

        let genesis_hash = data.genesis.hash();
        let genesis_executed = ExecutedBlock::new(
            Arc::new(data.genesis.clone().try_recover().unwrap()),
            Arc::new(BlockExecutionOutput {
                result: BlockExecutionResult {
                    receipts: vec![],
                    requests: Default::default(),
                    gas_used: 0,
                    blob_gas_used: 0,
                },
                state: Default::default(),
            }),
            ComputedTrieData::default(),
        );

        let mut blocks = Vec::with_capacity(data.blocks.len());
        for (recovered, outcome) in &data.blocks {
            blocks.push(ExecutedBlock::new(
                Arc::new(recovered.clone()),
                Arc::new(BlockExecutionOutput {
                    result: BlockExecutionResult {
                        receipts: outcome.receipts.first().cloned().unwrap_or_default(),
                        requests: outcome.requests.first().cloned().unwrap_or_default(),
                        gas_used: 0,
                        blob_gas_used: 0,
                    },
                    state: outcome.bundle.clone(),
                }),
                ComputedTrieData::default(),
            ));
        }

        let mut expected_canonical = vec![(0u64, genesis_hash)];
        for (recovered, _) in &data.blocks {
            expected_canonical.push((recovered.number(), recovered.hash()));
        }

        let provider_rw = factory.provider_rw().unwrap();
        provider_rw.save_blocks(vec![genesis_executed], SaveBlocksMode::Full).unwrap();
        provider_rw.commit().unwrap();

        let provider_rw = factory.provider_rw().unwrap();
        provider_rw.save_blocks(blocks, SaveBlocksMode::Full).unwrap();
        provider_rw.commit().unwrap();

        let provider = factory.provider().unwrap();
        assert_eq!(provider.table::<tables::CanonicalChain>().unwrap(), expected_canonical);
    }

    /// Writes one account changeset and one storage changeset row for `block`.
    fn insert_changeset_rows(
        factory: &ProviderFactory<MockNodeTypesWithDB>,
        block: BlockNumber,
        address: Address,
        slot: B256,
    ) {
        let provider_rw = factory.database_provider_rw().unwrap();
        let account_info =
            Some(Account { balance: U256::from(block), nonce: block, bytecode_hash: None });
        provider_rw
            .tx_ref()
            .put::<tables::AccountChangeSets>(
                block,
                AccountBeforeTx { address, info: account_info },
            )
            .unwrap();
        provider_rw
            .tx_ref()
            .put::<tables::StorageChangeSets>(
                BlockNumberAddress((block, address)),
                StorageEntry { key: slot, value: U256::from(block) },
            )
            .unwrap();
        provider_rw.commit().unwrap();
    }

    /// The low-end limiter must delete every row below `floor` in both changeset tables, keep
    /// every row at or above it, and be idempotent.
    #[test]
    fn test_remove_changesets_below_bounds_both_tables() {
        let factory = create_test_provider_factory();
        let address = Address::with_last_byte(0x11);
        let slot = B256::with_last_byte(0x22);

        // Blocks 0..=20 get one account row and one storage row each.
        for block in 0..=20u64 {
            insert_changeset_rows(&factory, block, address, slot);
        }

        // floor = 8 deletes blocks 0..=7 (8 account rows + 8 storage rows).
        let provider_rw = factory.provider_rw().unwrap();
        let removed = provider_rw.remove_changesets_below(8).unwrap();
        provider_rw.commit().unwrap();
        assert_eq!(removed, 16);

        let provider = factory.provider().unwrap();
        let accounts = provider
            .account_changesets_range(..)
            .unwrap()
            .into_iter()
            .map(|(block, _)| block)
            .collect::<Vec<_>>();
        assert_eq!(accounts, (8..=20u64).collect::<Vec<_>>());

        let storage = provider
            .storage_changesets_range(..)
            .unwrap()
            .into_iter()
            .map(|(bna, _)| bna.block_number())
            .collect::<Vec<_>>();
        assert_eq!(storage, (8..=20u64).collect::<Vec<_>>());

        // Idempotent: nothing left below floor 8, so a second pass removes nothing.
        let provider_rw = factory.provider_rw().unwrap();
        assert_eq!(provider_rw.remove_changesets_below(8).unwrap(), 0);
        provider_rw.commit().unwrap();

        // floor = 0 is a no-op guard, not a whole-table wipe.
        let provider_rw = factory.provider_rw().unwrap();
        assert_eq!(provider_rw.remove_changesets_below(0).unwrap(), 0);
        provider_rw.commit().unwrap();

        let provider = factory.provider().unwrap();
        assert_eq!(provider.account_changesets_range(..).unwrap().len(), 13);
        assert_eq!(provider.storage_changesets_range(..).unwrap().len(), 13);
    }

    /// A `floor` above the head deletes every row. The tables are height-keyed, so the whole
    /// table sits strictly below the floor.
    #[test]
    fn test_remove_changesets_below_above_head_clears_tables() {
        let factory = create_test_provider_factory();
        let address = Address::with_last_byte(0x33);
        let slot = B256::with_last_byte(0x44);

        for block in 1..=5u64 {
            insert_changeset_rows(&factory, block, address, slot);
        }

        let provider_rw = factory.provider_rw().unwrap();
        assert_eq!(provider_rw.remove_changesets_below(1_000).unwrap(), 10);
        provider_rw.commit().unwrap();

        let provider = factory.provider().unwrap();
        assert!(provider.account_changesets_range(..).unwrap().is_empty());
        assert!(provider.storage_changesets_range(..).unwrap().is_empty());
    }

    /// Several addresses per block must all be removed together: `remove` walks every dupsort
    /// value under a matching key, not just the first.
    #[test]
    fn test_remove_changesets_below_removes_all_dups_per_block() {
        let factory = create_test_provider_factory();
        let slot = B256::with_last_byte(0x55);

        for block in 1..=6u64 {
            // Three distinct addresses per block.
            for byte in 0..3u8 {
                insert_changeset_rows(&factory, block, Address::with_last_byte(byte), slot);
            }
        }

        // floor = 4 removes blocks 1..=3: 3 blocks x 3 addresses x 2 tables = 18 rows.
        let provider_rw = factory.provider_rw().unwrap();
        assert_eq!(provider_rw.remove_changesets_below(4).unwrap(), 18);
        provider_rw.commit().unwrap();

        let provider = factory.provider().unwrap();
        let accounts = provider
            .account_changesets_range(..)
            .unwrap()
            .into_iter()
            .map(|(block, _)| block)
            .collect::<Vec<_>>();
        assert_eq!(accounts.len(), 9);
        assert!(accounts.iter().all(|block| *block >= 4));

        let storage = provider
            .storage_changesets_range(..)
            .unwrap()
            .into_iter()
            .map(|(bna, _)| bna.block_number())
            .collect::<Vec<_>>();
        assert_eq!(storage.len(), 9);
        assert!(storage.iter().all(|block| *block >= 4));
    }

    /// Writes the changeset-window floor to metadata and advances the Finish checkpoint so
    /// `best_block_number` reflects a head at `best`. Mirrors what the Stage 5 limiter persists
    /// in the same transaction that trims the changeset tables.
    fn set_changeset_floor(factory: &ProviderFactory<MockNodeTypesWithDB>, floor: u64, best: u64) {
        let provider_rw = factory.provider_rw().unwrap();
        provider_rw.write_metadata(CHANGESET_WINDOW_FLOOR, floor.to_be_bytes().to_vec()).unwrap();
        provider_rw.update_pipeline_stages(best, false).unwrap();
        provider_rw.commit().unwrap();
    }

    /// Out-of-window historical queries (block below the floor) must fail loudly with
    /// `StateAtBlockPruned`, never fall through to plain state and return the current value.
    #[test]
    fn test_try_into_history_below_floor_is_refused() {
        use smartnet_storage_api::TryIntoHistoricalStateProvider;

        let factory = create_test_provider_factory();
        // floor = 10, best = 20: blocks 0..=9 are outside the retention window.
        set_changeset_floor(&factory, 10, 20);

        let provider = factory.provider().unwrap();
        // Best-block early-return still wins for block 20 itself.
        assert!(provider.try_into_history_at_block(20).is_ok(), "best block is always available");

        // A block below the floor must be refused, not silently answered from plain state.
        let provider = factory.provider().unwrap();
        match provider.try_into_history_at_block(9) {
            Err(ProviderError::StateAtBlockPruned(9)) => {}
            Err(e) => panic!("expected StateAtBlockPruned(9), got {e:?}"),
            Ok(_) => panic!("block below the changeset floor must be refused, got Ok"),
        }
    }

    /// At or above the floor the request must proceed past the guard (it may still fail later for
    /// unrelated reasons, but not with `StateAtBlockPruned` from the window guard).
    #[test]
    fn test_try_into_at_or_above_floor_passes_guard() {
        use smartnet_storage_api::TryIntoHistoricalStateProvider;

        let factory = create_test_provider_factory();
        set_changeset_floor(&factory, 10, 20);

        for block in [10u64, 15, 19] {
            let provider = factory.provider().unwrap();
            if let Err(ProviderError::StateAtBlockPruned(n)) =
                provider.try_into_history_at_block(block)
            {
                panic!("block {block} is at/above the floor but was refused (pruned at {n})")
            }
            // Any other outcome (Ok, or a non-window error) means the guard let it through.
        }
    }

    /// A saturating floor of 0 (head has not yet advanced past the window) must leave every
    /// historical query unguarded — the whole chain is still retained.
    #[test]
    fn test_try_into_with_saturated_floor_is_permissive() {
        use smartnet_storage_api::TryIntoHistoricalStateProvider;

        let factory = create_test_provider_factory();
        // floor = 0: nothing has aged out yet.
        set_changeset_floor(&factory, 0, 20);

        let provider = factory.provider().unwrap();
        if let Err(ProviderError::StateAtBlockPruned(n)) = provider.try_into_history_at_block(5) {
            panic!("saturated floor of 0 must not refuse block 5 (pruned at {n})")
        }
    }

    /// No floor persisted (fresh/legacy DB that has never run the limiter) must behave the same
    /// as a saturated floor: no window guard, so historical queries are not spuriously refused.
    #[test]
    fn test_try_into_without_floor_metadata_is_permissive() {
        use smartnet_storage_api::TryIntoHistoricalStateProvider;

        let factory = create_test_provider_factory();
        let provider_rw = factory.provider_rw().unwrap();
        provider_rw.update_pipeline_stages(20, false).unwrap();
        provider_rw.commit().unwrap();

        let provider = factory.provider().unwrap();
        if let Err(ProviderError::StateAtBlockPruned(n)) = provider.try_into_history_at_block(5) {
            panic!("missing floor metadata must not refuse block 5 (pruned at {n})")
        }
    }

    /// 4.11: a block write may fill only one of the two numbering tables — the payload writer
    /// inserts `Transactions` rows, the engine's index-only write inserts `TxLocations`. The
    /// floor must be one past whichever is higher: reading `TxLocations` alone hands the
    /// payload writer a number it has already used (and vice versa).
    #[test]
    fn next_tx_number_is_one_past_the_higher_of_both_indexes() {
        let factory = create_test_provider_factory();
        let data = BlockchainTestData::default();
        let provider_rw = factory.provider_rw().unwrap();

        assert_eq!(provider_rw.next_tx_number().unwrap(), 0, "an empty database starts at zero");

        // Payload path: `Transactions` rows only, no `TxLocations`.
        let genesis = data.genesis.try_recover().unwrap();
        provider_rw.insert_block(&genesis).unwrap();
        let block1 = &data.blocks[0].0;
        let indices1 = provider_rw.insert_block(block1).unwrap();
        assert!(indices1.tx_count > 0, "block 1 must carry a transaction");
        let payload_floor = indices1.first_tx_num + indices1.tx_count;
        assert_eq!(
            provider_rw.next_tx_number().unwrap(),
            payload_floor,
            "with only `Transactions` populated the floor is one past its last key"
        );

        // Index path: a `TxLocations` row above the payload floor must win.
        let indexed_floor = payload_floor + 7;
        provider_rw
            .tx_ref()
            .put::<tables::TxLocations>(indexed_floor, StoredTransactionLocation::new(1, 0))
            .unwrap();
        assert_eq!(provider_rw.next_tx_number().unwrap(), indexed_floor + 1);

        // A later payload write resumes above the index row, after which `Transactions` is the
        // higher of the two and must be the one answering — `TxLocations` alone would still
        // say `indexed_floor + 1` and collide with the rows just written.
        let (block2, _) = data.blocks[1..]
            .iter()
            .find(|(b, _)| b.body().transaction_count() > 0)
            .expect("a later block carries a transaction");
        let indices2 = provider_rw.insert_block(block2).unwrap();
        assert_eq!(
            indices2.first_tx_num,
            indexed_floor + 1,
            "numbering resumes above the index row"
        );
        assert_eq!(
            provider_rw.next_tx_number().unwrap(),
            indexed_floor + 1 + indices2.tx_count,
            "the payload writer's rows must raise the floor above an older index row"
        );
    }

    /// 4.12: the engine's index-only write obeys `transaction_lookup` exactly like `save_blocks`
    /// and `insert_block` — under `full` the pruning stage wipes that index, so writing it here
    /// would leave rows the operator's prune mode forbids. `TxLocations` must survive either
    /// way: pruning the hash index must never break transaction numbering.
    #[test]
    fn save_block_state_and_index_honours_transaction_lookup_prune_mode() {
        let report = |factory: ProviderFactory<MockNodeTypesWithDB>, expect_hash_numbers: bool| {
            let data = BlockchainTestData::default();
            let provider_rw = factory.provider_rw().unwrap();
            let (recovered, outcome) = &data.blocks[0];
            let tx_hash = recovered.body().transactions[0].recalculate_hash();
            let expected_num = provider_rw.next_tx_number().unwrap();

            let executed = ExecutedBlock::new(
                Arc::new(recovered.clone()),
                Arc::new(BlockExecutionOutput {
                    result: BlockExecutionResult {
                        receipts: outcome.receipts.first().cloned().unwrap_or_default(),
                        requests: outcome.requests.first().cloned().unwrap_or_default(),
                        gas_used: 0,
                        blob_gas_used: 0,
                    },
                    state: outcome.bundle.clone(),
                }),
                ComputedTrieData::default(),
            );
            provider_rw.save_block_state_and_index(vec![executed]).unwrap();
            provider_rw.commit().unwrap();

            let provider = factory.provider().unwrap();
            assert_eq!(
                provider.next_tx_number().unwrap(),
                expected_num + 1,
                "numbering must be recorded regardless of the hash index"
            );
            assert!(
                provider.tx_ref().get::<tables::TxLocations>(expected_num).unwrap().is_some(),
                "TxLocations locates the transaction and is never pruned"
            );
            assert_eq!(
                provider.transaction_id(tx_hash).unwrap(),
                expect_hash_numbers.then_some(expected_num),
                "TransactionHashNumbers must follow the transaction_lookup prune mode"
            );
        };

        report(create_test_provider_factory(), true);
        report(
            create_test_provider_factory().with_prune_modes(PruneModes {
                transaction_lookup: Some(PruneMode::Full),
                ..PruneModes::default()
            }),
            false,
        );
    }
}
