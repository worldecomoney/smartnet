use crate::metrics::PersistenceMetrics;
use alloy_eips::BlockNumHash;
use crossbeam_channel::Sender as CrossbeamSender;
use smartnet_ethereum_primitives::EthPrimitives;
use smartnet_primitives_traits::{FastInstant as Instant, NodePrimitives};
use smartnet_prune::{PrunerError, PrunerWithFactory};
use smartnet_stages_api::{MetricEvent, MetricEventsSender};
use smartnet_chain_state::ExecutedBlock;
use smartnet_errors::ProviderError;
use smartnet_file_tier::{FileTierError, SignatureAccess};
use smartnet_provider::{
    providers::ProviderNodeTypes, DatabaseProviderFactory, DBProvider, ProviderFactory,
    StorageEngine, StorageError,
};
use smartnet_tasks::spawn_os_thread;
use smartnet_timing::fmt_wall;
use std::{
    path::{Path, PathBuf},
    sync::{
        mpsc::{Receiver, SendError, Sender},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};
use thiserror::Error;
use tracing::{debug, error, instrument, warn};

/// The boot-time clean-shutdown check (RECOVERY.md §4): read `SHUTDOWN_CLEAN` from the DB,
/// clear it, and report whether the last exit was clean.
///
/// Thin wrapper over [`StorageEngine::take_shutdown_clean`] (kept for existing call sites
/// and tests; the protocol lives in the engine).
pub fn take_shutdown_clean<N>(provider: &ProviderFactory<N>) -> bool
where
    N: ProviderNodeTypes,
{
    StorageEngine::new(provider.clone(), None, 0).take_shutdown_clean()
}

/// The boot-time crash-recovery protocol (RECOVERY.md §5) — the sanctioned
/// `restore_unwind(2)` call site. Thin wrapper over [`StorageEngine::restore_unwind`].
pub fn recover_file_tier<N>(
    provider: &ProviderFactory<N>,
    file_tier_base: Option<&Path>,
) -> Result<u64, PersistenceError>
where
    N: ProviderNodeTypes,
{
    let Some(base) = file_tier_base else { return Ok(0) };
    Ok(StorageEngine::new(provider.clone(), None, 0).restore_unwind(base)?)
}

/// Unified result of any persistence operation.
#[derive(Debug)]
pub struct PersistenceResult {
    /// The last block that was persisted, if any.
    pub last_block: Option<BlockNumHash>,
    /// The commit duration, only available for save-blocks operations.
    pub commit_duration: Option<Duration>,
}

/// Writes parts of the node's in-memory tree state to the database and the file tier.
///
/// This is meant to be a spawned service that listens for various incoming persistence operations,
/// performing those actions on disk, and returning the result in a channel.
///
/// The actual write/recovery protocol lives in [`StorageEngine`] (both stores, one
/// sequence); this service owns the channel loop, metrics, deferred finalized/safe
/// buffering, and the pruner (which runs post-commit in its own transaction).
///
/// This should be spawned in its own thread with [`std::thread::spawn`], since this performs
/// blocking I/O operations in an endless loop.
#[derive(Debug)]
pub struct PersistenceService<N>
where
    N: ProviderNodeTypes,
{
    /// The encapsulated storage engine (FS + DB write protocol).
    engine: StorageEngine<N>,
    /// Incoming requests
    incoming: Receiver<PersistenceAction<N::Primitives>>,
    /// The pruner (post-commit, separate transaction)
    pruner: PrunerWithFactory<ProviderFactory<N>>,
    /// metrics
    metrics: PersistenceMetrics,
    /// Sender for sync metrics - we only submit sync metrics for persisted blocks
    sync_metrics_tx: MetricEventsSender,
    /// Pending finalized block number to be committed with the next block save.
    /// This avoids triggering a separate fsync for each finalized block update.
    pending_finalized_block: Option<u64>,
    /// Pending safe block number to be committed with the next block save.
    /// This avoids triggering a separate fsync for each safe block update.
    pending_safe_block: Option<u64>,
}

impl<N> PersistenceService<N>
where
    N: ProviderNodeTypes,
{
    /// Create a new persistence service
    pub fn new(
        provider: ProviderFactory<N>,
        incoming: Receiver<PersistenceAction<N::Primitives>>,
        pruner: PrunerWithFactory<ProviderFactory<N>>,
        sync_metrics_tx: MetricEventsSender,
        changeset_window: u64,
        file_tier_base: Option<PathBuf>,
    ) -> Self {
        Self {
            engine: StorageEngine::new(provider, file_tier_base, changeset_window),
            incoming,
            pruner,
            metrics: PersistenceMetrics::default(),
            sync_metrics_tx,
            pending_finalized_block: None,
            pending_safe_block: None,
        }
    }
}

impl<N> PersistenceService<N>
where
    N: ProviderNodeTypes,
    <N::Primitives as NodePrimitives>::SignedTx: SignatureAccess,
{
    /// This is the main loop, that will listen to database events and perform the requested
    /// database actions
    ///
    /// A failed operation replies with `last_block: None` (the engine treats that as "nothing
    /// persisted" and retries on its next advance) and the service keeps running — a transient
    /// error (lock contention, ENOSPC, EIO) must never kill the persistence thread (XCURR H-S1).
    /// The loop ends only when every sender has disconnected (clean shutdown).
    pub fn run(mut self) -> Result<(), PersistenceError> {
        // If the receiver errors then senders have disconnected, so the loop should then end.
        while let Ok(action) = self.incoming.recv() {
            match action {
                PersistenceAction::SaveBlocks(blocks, sender) => {
                    let result = match self.on_save_blocks(blocks) {
                        Ok(result) => result,
                        Err(err) => {
                            error!(
                                target: "engine::persistence",
                                %err,
                                "save-blocks failed — replying with no result; the engine will retry"
                            );
                            let _ = sender.send(PersistenceResult {
                                last_block: None,
                                commit_duration: None,
                            });
                            continue;
                        }
                    };
                    let result_number = result.last_block.map(|b| b.number);

                    let _ = sender.send(result);

                    if let Some(block_number) = result_number {
                        // send new sync metrics based on saved blocks
                        let _ = self
                            .sync_metrics_tx
                            .send(MetricEvent::SyncHeight { height: block_number });
                    }
                }
                PersistenceAction::SaveFinalizedBlock(finalized_block) => {
                    self.pending_finalized_block = Some(finalized_block);
                }
                PersistenceAction::SaveSafeBlock(safe_block) => {
                    self.pending_safe_block = Some(safe_block);
                }
            }
        }
        // Wait for in-flight background seal compressions before the service stops, so a clean
        // shutdown does not leave a half-written seal behind (a crash still can, which
        // `reconcile_seals` finishes on the next boot).
        self.engine.join_seal_tasks();
        Ok(())
    }

    /// Orchestrates one save: single-block warn → engine protocol (FS append + DB commit)
    /// → deferred finalized/safe re-queue → post-commit pruner → metrics.
    ///
    /// The write protocol itself lives in [`StorageEngine::save_blocks`]; this method
    /// only adds the service-level concerns (pending markers, pruner, metrics).
    #[instrument(level = "debug", target = "engine::persistence", skip_all, fields(block_count = blocks.len()))]
    fn on_save_blocks(
        &mut self,
        blocks: Vec<ExecutedBlock<N::Primitives>>,
    ) -> Result<PersistenceResult, PersistenceError>
    where
        <N::Primitives as NodePrimitives>::SignedTx: SignatureAccess,
    {
        let block_count = blocks.len();

        // Protocol rule: one block per save — no block batches (only tx batches are
        // supported). The drop-2 file-tier recovery invariant assumes at most one
        // block line (+ a possible partial tail) per append; anything more is a bug
        // upstream of this service and would corrupt payload on a crash.
        if block_count > 1 {
            warn!(
                target: "engine::persistence",
                block_count,
                "save-blocks received more than one block — protocol rule is one block per save"
            );
        }

        let pending_finalized = self.pending_finalized_block.take();
        let pending_safe = self.pending_safe_block.take();

        let start_time = Instant::now();
        let outcome = self.engine.save_blocks(blocks, pending_finalized, pending_safe)?;

        // Re-queue markers that were above this batch's last block.
        if outcome.deferred_finalized.is_some() {
            self.pending_finalized_block = outcome.deferred_finalized;
        }
        if outcome.deferred_safe.is_some() {
            self.pending_safe_block = outcome.deferred_safe;
        }

        // Post-commit pruner: separate provider/transaction so it reads the committed
        // state written above.
        let mut seg_prune = Duration::ZERO;
        if let Some(last) = outcome.last_block
            && self.pruner.is_pruning_needed(last.number)
        {
            debug!(target: "engine::persistence", block_num=?last.number, "Running pruner");
            let prune_start = Instant::now();
            let provider_rw = self.engine.provider().database_provider_rw()?;
            let _ = self.pruner.run_with_provider(&provider_rw, last.number)?;
            provider_rw.commit()?;
            seg_prune = prune_start.elapsed();
            debug!(target: "engine::persistence", tip=?last.number, "Finished pruning after saving blocks");
            self.metrics.prune_before_duration_seconds.record(seg_prune);
        }

        debug!(
            target: "engine::persistence",
            first=?outcome.last_block,
            last=?outcome.last_block,
            file_tier_on = outcome.file_tier_on,
            total = %fmt_wall(outcome.total),
            fs_append = %fmt_wall(outcome.fs_append),
            state_buffer = %fmt_wall(outcome.state_buffer),
            commit_fsync = %fmt_wall(outcome.commit_fsync),
            prune_commit = %fmt_wall(seg_prune),
            "Saved range of blocks"
        );

        let elapsed = start_time.elapsed();
        self.metrics.save_blocks_batch_size.record(block_count as f64);
        self.metrics.save_blocks_duration_seconds.record(elapsed);

        Ok(PersistenceResult { last_block: outcome.last_block, commit_duration: Some(elapsed) })
    }
}

/// One of the errors that can happen when using the persistence service.
#[derive(Debug, Error)]
pub enum PersistenceError {
    /// A pruner error
    #[error(transparent)]
    PrunerError(#[from] PrunerError),

    /// A provider error
    #[error(transparent)]
    ProviderError(#[from] ProviderError),

    /// A file-tier (gz payload files) error
    #[error(transparent)]
    FileTier(#[from] FileTierError),

    /// A storage-engine protocol error (FS append / DB commit sequence)
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// A signal to the persistence service that part of the tree state can be persisted.
#[derive(Debug)]
pub enum PersistenceAction<N: NodePrimitives = EthPrimitives> {
    /// The section of tree state that should be persisted. These blocks are expected in order of
    /// increasing block number.
    ///
    /// First, header, transaction, and receipt-related payload is appended to the file tier;
    /// then the state + index rows are committed to the database.
    SaveBlocks(Vec<ExecutedBlock<N>>, CrossbeamSender<PersistenceResult>),

    /// Update the persisted finalized block on disk
    SaveFinalizedBlock(u64),

    /// Update the persisted safe block on disk
    SaveSafeBlock(u64),
}

/// A handle to the persistence service
#[derive(Debug, Clone)]
pub struct PersistenceHandle<N: NodePrimitives = EthPrimitives> {
    /// The channel used to communicate with the persistence service
    sender: Sender<PersistenceAction<N>>,
    /// Guard that joins the service thread when all handles are dropped.
    /// Uses `Arc` so the handle remains `Clone`.
    _service_guard: Arc<ServiceGuard>,
    /// Test-mode flag: the mock delivery composes the persistence result at the handle
    /// itself and sends it before posting the action, so the synchronous seal-cycle
    /// (`save completes inside the calling advance`) works in tests without any replier
    /// thread. The action is still posted for the tests to peek at.
    auto_reply: bool,
}

impl<T: NodePrimitives> PersistenceHandle<T> {
    /// Create a new [`PersistenceHandle`] from a [`Sender<PersistenceAction>`].
    ///
    /// This is intended for testing purposes where you want to mock the persistence service.
    /// For production use, prefer [`spawn_service`](Self::spawn_service).
    pub fn new(sender: Sender<PersistenceAction<T>>) -> Self {
        Self { sender, _service_guard: Arc::new(ServiceGuard(None)), auto_reply: false }
    }

    /// Test-mode handle: the mock delivery replies **before** posting the action, with the
    /// last submitted block as the persisted tip.
    ///
    /// This matches the production contract (the save completes inside the calling
    /// `advance_persistence`) without threading a replier through the tests; tests keep the
    /// action channel to peek at exactly what was submitted.
    pub fn new_auto_reply(sender: Sender<PersistenceAction<T>>) -> Self {
        Self { sender, _service_guard: Arc::new(ServiceGuard(None)), auto_reply: true }
    }

    /// Create a new [`PersistenceHandle`], and spawn the persistence service.
    ///
    /// The returned handle can be cloned and shared. When all clones are dropped, the service
    /// thread will be joined, ensuring graceful shutdown before resources (like `aux DB`) are
    /// released.
    pub fn spawn_service<N>(
        provider_factory: ProviderFactory<N>,
        pruner: PrunerWithFactory<ProviderFactory<N>>,
        sync_metrics_tx: MetricEventsSender,
        changeset_window: u64,
        file_tier_base: Option<PathBuf>,
    ) -> PersistenceHandle<N::Primitives>
    where
        N: ProviderNodeTypes,
        <N::Primitives as NodePrimitives>::SignedTx: SignatureAccess,
    {
        // create the initial channels
        let (db_service_tx, db_service_rx) = std::sync::mpsc::channel();

        // spawn the persistence service
        let db_service = PersistenceService::new(
            provider_factory,
            db_service_rx,
            pruner,
            sync_metrics_tx,
            changeset_window,
            file_tier_base,
        );
        let join_handle = spawn_os_thread("persistence", || {
            if let Err(err) = db_service.run() {
                error!(target: "engine::persistence", ?err, "Persistence service failed");
            }
        });

        PersistenceHandle {
            sender: db_service_tx,
            _service_guard: Arc::new(ServiceGuard(Some(join_handle))),
            auto_reply: false,
        }
    }

    /// Sends a specific [`PersistenceAction`] in the contained channel. The caller is responsible
    /// for creating any channels for the given action.
    pub fn send_action(
        &self,
        action: PersistenceAction<T>,
    ) -> Result<(), SendError<PersistenceAction<T>>> {
        self.sender.send(action)
    }

    /// Tells the persistence service to save a certain list of finalized blocks. The blocks are
    /// assumed to be ordered by block number.
    ///
    /// This returns the latest hash that has been saved, allowing removal of that block and any
    /// previous blocks from in-memory data structures. This value is returned in the receiver end
    /// of the sender argument.
    ///
    /// If there are no blocks to persist, then `None` is sent in the sender.
    pub fn save_blocks(
        &self,
        blocks: Vec<ExecutedBlock<T>>,
        tx: CrossbeamSender<PersistenceResult>,
    ) -> Result<(), SendError<PersistenceAction<T>>> {
        if self.auto_reply {
            let _ = tx.send(PersistenceResult {
                last_block: blocks.last().map(|b| b.recovered_block().num_hash()),
                commit_duration: None,
            });
        }
        self.send_action(PersistenceAction::SaveBlocks(blocks, tx))
    }

    /// Queues the finalized block number to be persisted on disk.
    ///
    /// The update is deferred and will be committed together with the next [`Self::save_blocks`]
    /// call to avoid triggering a separate fsync for each update.
    pub fn save_finalized_block_number(
        &self,
        finalized_block: u64,
    ) -> Result<(), SendError<PersistenceAction<T>>> {
        self.send_action(PersistenceAction::SaveFinalizedBlock(finalized_block))
    }

    /// Queues the safe block number to be persisted on disk.
    ///
    /// The update is deferred and will be committed together with the next [`Self::save_blocks`]
    /// call to avoid triggering a separate fsync for each update.
    pub fn save_safe_block_number(
        &self,
        safe_block: u64,
    ) -> Result<(), SendError<PersistenceAction<T>>> {
        self.send_action(PersistenceAction::SaveSafeBlock(safe_block))
    }
}

/// Guard that joins the persistence service thread when dropped.
///
/// This ensures graceful shutdown - the service thread completes before resources like
/// `aux DB` are released. Stored in an `Arc` inside [`PersistenceHandle`] so the handle
/// can be cloned while sharing the same guard.
struct ServiceGuard(Option<JoinHandle<()>>);

impl std::fmt::Debug for ServiceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ServiceGuard").field(&self.0.as_ref().map(|_| "...")).finish()
    }
}

impl Drop for ServiceGuard {
    fn drop(&mut self) {
        if let Some(join_handle) = self.0.take() {
            let _ = join_handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, U256};
    use smartnet_chain_state::FinishedExExHeight;
    use smartnet_prune::Pruner;
    use smartnet_chain_state::test_utils::TestBlockBuilder;
    use smartnet_file_tier::{BlockRecords, FileTierWriter, ShardId};
    use smartnet_provider::{
        test_utils::{
            create_test_provider_factory, create_test_provider_factory_at, MockNodeTypesWithDB,
        },
        BlockExecutionWriter, BlockHashReader, BlockNumReader, DatabaseProviderFactory,
        SaveBlocksMode, StorageSettings, StorageSettingsCache,
    };
    use tokio::sync::mpsc::unbounded_channel;

    /// Matches `smartnet_config::DEFAULT_CHANGESET_WINDOW`. engine-tree does not depend on the
    /// config crate, so the test uses the same literal value.
    const TEST_CHANGESET_WINDOW: u64 = 10_000;

    fn default_persistence_handle() -> PersistenceHandle<EthPrimitives> {
        let provider = create_test_provider_factory();

        let (_finished_exex_height_tx, finished_exex_height_rx) =
            tokio::sync::watch::channel(FinishedExExHeight::NoExExs);

        let pruner =
            Pruner::new_with_factory(provider.clone(), vec![], 5, 0, None, finished_exex_height_rx);

        let (sync_metrics_tx, _sync_metrics_rx) = unbounded_channel();
        PersistenceHandle::<EthPrimitives>::spawn_service(
            provider,
            pruner,
            sync_metrics_tx,
            TEST_CHANGESET_WINDOW,
            None,
        )
    }

    #[test]
    fn test_save_blocks_empty() {
        smartnet_tracing::init_test_tracing();
        let handle = default_persistence_handle();

        let blocks = vec![];
        let (tx, rx) = crossbeam_channel::bounded(1);

        handle.save_blocks(blocks, tx).unwrap();

        let result = rx.recv().unwrap();
        assert!(result.last_block.is_none());
    }

    #[test]
    fn test_save_blocks_single_block() {
        smartnet_tracing::init_test_tracing();
        let handle = default_persistence_handle();
        let block_number = 0;
        let mut test_block_builder = TestBlockBuilder::eth();
        let executed =
            test_block_builder.get_executed_block_with_number(block_number, B256::random());
        let block_hash = executed.recovered_block().hash();

        let blocks = vec![executed];
        let (tx, rx) = crossbeam_channel::bounded(1);

        handle.save_blocks(blocks, tx).unwrap();

        let result = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("test timed out");

        assert_eq!(block_hash, result.last_block.unwrap().hash);
    }

    /// A reorg (unwind + append) must leave headers, historical changesets and the latest
    /// state consistent when observed through the primary factory.
    ///
    /// A second read-only factory cannot be opened while the primary holds the database:
    /// the backend takes an exclusive lock on the database directory, and held read
    /// transactions are not snapshot-isolated, so the cross-factory snapshot assertions of
    /// the former concurrent read-only variant do not apply to this backend.
    #[test]
    fn test_reorg_updates_headers_and_history() {
        smartnet_tracing::init_test_tracing();

        let provider_factory = create_test_provider_factory();
        // Historical reads below the tip require history index rows, which are only
        // written under the archive profile.
        provider_factory.set_storage_settings_cache(StorageSettings::archive());

        // --- Phase 1: Write blocks 0..3 ---
        let mut test_block_builder = TestBlockBuilder::eth().with_state();
        let signer = test_block_builder.signer;
        let blocks_a: Vec<_> = test_block_builder.get_executed_blocks(0..3).collect();
        let hash_a1 = blocks_a[1].recovered_block().hash();
        let hash_a2 = blocks_a[2].recovered_block().hash();

        // Compute expected signer state after each block from tx counts.
        let single_cost = TestBlockBuilder::<EthPrimitives>::single_tx_cost();
        let initial_balance = U256::from(10).pow(U256::from(18));
        let txs_in_block0 = blocks_a[0].recovered_block().body().transactions.len() as u64;
        let txs_in_block1 = blocks_a[1].recovered_block().body().transactions.len() as u64;

        let balance_after_block1 =
            initial_balance - single_cost * U256::from(txs_in_block0 + txs_in_block1);
        let nonce_after_block1 = txs_in_block0 + txs_in_block1;

        {
            let provider_rw = provider_factory.database_provider_rw().unwrap();
            provider_rw.save_blocks(blocks_a, SaveBlocksMode::Full).unwrap();
            provider_rw.commit().unwrap();
        }

        // Primary sees all 3 blocks.
        {
            let provider = provider_factory.provider().unwrap();
            assert_eq!(
                provider.block_hash(2).unwrap(),
                Some(hash_a2),
                "primary must see block 2 after initial append"
            );
        }

        // Historical state at block 1 is accessible via history index + changesets.
        {
            let state_at_1 = provider_factory.history_by_block_number(1).unwrap();
            let account_at_1 =
                state_at_1.basic_account(&signer).unwrap().expect("signer must exist at block 1");
            assert_eq!(account_at_1.balance, balance_after_block1, "signer balance at block 1");
            assert_eq!(account_at_1.nonce, nonce_after_block1, "signer nonce at block 1");
        }

        // --- Phase 2: Reorg - remove block 2 and append a different block 2 ---
        let block_b2 = test_block_builder.get_executed_block_with_number(2, hash_a1);
        let hash_b2 = block_b2.recovered_block().hash();
        let txs_in_block_b2 = block_b2.recovered_block().body().transactions.len() as u64;
        assert_ne!(hash_a2, hash_b2, "reorg block must differ");

        let balance_after_reorg_block2 =
            balance_after_block1 - single_cost * U256::from(txs_in_block_b2);
        let nonce_after_reorg_block2 = nonce_after_block1 + txs_in_block_b2;

        {
            let provider_rw = provider_factory.database_provider_rw().unwrap();
            provider_rw.remove_block_and_execution_above(1).unwrap();
            provider_rw.commit().unwrap();

            let provider_rw = provider_factory.database_provider_rw().unwrap();
            provider_rw.save_blocks(vec![block_b2], SaveBlocksMode::Full).unwrap();
            provider_rw.commit().unwrap();
        }

        // The reorged chain replaces block 2; block 1 is untouched.
        {
            let provider = provider_factory.provider().unwrap();
            assert_eq!(
                provider.block_hash(2).unwrap(),
                Some(hash_b2),
                "must see the reorged block 2, not the old one"
            );
            assert_eq!(
                provider.block_hash(1).unwrap(),
                Some(hash_a1),
                "block 1 must still be the original"
            );
        }

        // Historical state at block 1 survives the reorg.
        {
            let state_at_1 = provider_factory.history_by_block_number(1).unwrap();
            let account_at_1 = state_at_1
                .basic_account(&signer)
                .unwrap()
                .expect("signer must exist at block 1 after reorg");
            assert_eq!(
                account_at_1.balance, balance_after_block1,
                "signer balance at block 1 must survive reorg"
            );
            assert_eq!(
                account_at_1.nonce, nonce_after_block1,
                "signer nonce at block 1 must survive reorg"
            );
        }

        // The latest state (at block 2) reflects the reorged execution.
        {
            let state_at_2 = provider_factory.history_by_block_number(2).unwrap();
            let account_at_2 = state_at_2
                .basic_account(&signer)
                .unwrap()
                .expect("signer must exist at block 2 after reorg");
            assert_eq!(
                account_at_2.balance, balance_after_reorg_block2,
                "signer balance at block 2 must reflect reorged execution"
            );
            assert_eq!(
                account_at_2.nonce, nonce_after_reorg_block2,
                "signer nonce at block 2 must reflect reorged execution"
            );
        }
    }

    /// One synthetic block's records: bt only, no txs — all the seal path checks is the bt count.
    fn block_records(b: u64) -> BlockRecords {
        BlockRecords {
            bt: smartnet_file_tier::BtRecord {
                b,
                h: format!("0x{b:064x}"),
                ph: format!("0x{:064x}", b.wrapping_sub(1)),
                ts: 1,
                tc: 0,
                gu: 0,
                gl: 30_000_000,
                m: "0x0000000000000000000000000000000000000000".into(),
                ex: "0x".into(),
                ..Default::default()
            },
            txs: vec![],
            logs: None,
            erc4626: None,
        }
    }

    /// One `BlockRecords` per block of sub-shard (0,0): blocks `0..10_000`. txs/4626 are empty,
    /// which is fine — seal only verifies the bt count.
    fn full_shard_records() -> Vec<BlockRecords> {
        (0..smartnet_file_tier::shard::SHARD_Z_SIZE).map(block_records).collect()
    }

    /// Stage 6A seal path: appending a full sub-shard's worth of records through
    /// `append_file_tier_and_seal` compresses the active `.jsonl` into `.gz` (the atomic seal).
    /// Pure FS — no Fjall watermarks are written, because the payload never lives in the
    /// database to be handed off.
    #[test]
    fn test_file_tier_seal_at_boundary() {
        smartnet_tracing::init_test_tracing();
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = std::sync::mpsc::channel();
        let provider_factory = create_test_provider_factory();
        let mut service = PersistenceService::<MockNodeTypesWithDB>::new(
            provider_factory,
            rx,
            Pruner::new_with_factory(
                create_test_provider_factory(),
                vec![],
                5,
                0,
                None,
                tokio::sync::watch::channel(FinishedExExHeight::NoExExs).1,
            ),
            unbounded_channel().0,
            TEST_CHANGESET_WINDOW,
            Some(dir.path().to_path_buf()),
        );

        let records = full_shard_records();
        service
            .engine
            .append_file_tier_and_seal(&records, std::time::Instant::now())
            .unwrap();
        // Compression runs on a background thread; join it before asserting the sealed files.
        service.engine.join_seal_tasks();

        // The shard is sealed: the final `b.gz` exists and the active `.jsonl` is gone. The
        // fixture blocks are all empty (tc: 0), so only `b` ever existed — txs/logs were never
        // created (missing = empty).
        let sealed_b = dir.path().join("blocks/0/00/b.gz");
        assert!(sealed_b.exists(), "b.gz must exist after seal");
        assert!(!dir.path().join("blocks/0/00/b.jsonl").exists());
        assert!(!dir.path().join("blocks/0/00/txs.gz").exists());
        assert!(!dir.path().join("blocks/0/00/txs.jsonl").exists());
    }

    /// Crash recovery (RECOVERY.md §5): the last 2 lines are removed and `BX` is the block of
    /// the last surviving record. The DB truncation is a no-op on a fresh test DB.
    #[test]
    fn test_file_tier_recovery_drops_last_two_lines() {
        use smartnet_file_tier::verify_bt_records;

        smartnet_tracing::init_test_tracing();
        let dir = tempfile::tempdir().unwrap();
        let provider_factory = create_test_provider_factory();

        {
            let mut writer = FileTierWriter::new(dir.path());
            for b in 1..=3u64 {
                writer.files_for_block(b).unwrap().append_bt(&block_records(b).bt).unwrap();
            }
            writer.finish_active().unwrap();
        }

        let bx = recover_file_tier(&provider_factory, Some(dir.path())).unwrap();
        assert_eq!(bx, 1, "blocks 2 and 3 removed; BX is block 1");
        assert!(
            verify_bt_records(dir.path(), ShardId { s: 0, y: 0 }, 1).unwrap(),
            "one record (block 1) survives"
        );
    }

    /// Crash recovery on a chain written by the **engine** path. `save_block_state_and_index`
    /// stores the index + state but no `Blocks` row, so the DB truncation must not read the
    /// tip's body indices from `tables::Blocks`. Regression for the `BlockBodyIndicesNotFound`
    /// that aborted recovery — and therefore node restart — for every file-tier chain.
    #[test]
    fn test_recovery_truncates_engine_written_chain() {
        smartnet_tracing::init_test_tracing();
        let dir = tempfile::tempdir().unwrap();
        // Root the database under the file-tier base: production invariant — the file-tier
        // base always contains the database's datadir (a provider honors a base only when
        // its db lives under it; a foreign base from a parallel test is ignored).
        let provider_factory = create_test_provider_factory_at(dir.path());

        // The engine resolves file-tier heights through the process-global base. This is the
        // only engine-tree test that sets it.
        assert!(smartnet_file_tier::set_base(dir.path()), "file-tier base must be unset yet");
        smartnet_file_tier::bootstrap_network_settings(
            Some(1),
            Some(vec![alloy_primitives::Address::repeat_byte(0x11)]),
        )
        .unwrap();

        // Blocks 0..=2, written through the engine path (file tier + index + state, no `Blocks`).
        let blocks: Vec<_> =
            TestBlockBuilder::eth().with_state().get_executed_blocks(0..3).collect();

        {
            let provider_rw = provider_factory.provider_rw().unwrap();
            // Genesis keeps its Fjall `Blocks` row (block 0 always reads from Fjall); the engine
            // write path below stores no `Blocks` row for blocks 1..=2.
            provider_rw.insert_block(blocks[0].recovered_block()).unwrap();
            provider_rw.commit().unwrap();
        }

        let (_tx, rx) = std::sync::mpsc::channel();
        let mut service = PersistenceService::<MockNodeTypesWithDB>::new(
            provider_factory.clone(),
            rx,
            Pruner::new_with_factory(
                create_test_provider_factory(),
                vec![],
                5,
                0,
                None,
                tokio::sync::watch::channel(FinishedExExHeight::NoExExs).1,
            ),
            unbounded_channel().0,
            TEST_CHANGESET_WINDOW,
            Some(dir.path().to_path_buf()),
        );
        service.on_save_blocks(blocks).unwrap();

        assert_eq!(
            provider_factory.provider().unwrap().last_block_number().unwrap(),
            2,
            "engine path persisted blocks 0..=2"
        );

        // Unclean exit: drop the last 2 file records and truncate the DB to BX. Before the fix,
        // the recovery path read the tip's `Blocks` row (absent here) and errored.
        let bx = recover_file_tier(&provider_factory, Some(dir.path())).unwrap();
        assert_eq!(bx, 0, "blocks 1 and 2 dropped; BX is block 0");

        let provider = provider_factory.provider().unwrap();
        assert_eq!(provider.last_block_number().unwrap(), 0, "DB truncated to BX");
        assert!(provider.block_hash(2).unwrap().is_none(), "block 2 index row is gone");
    }

    // NOTE: the Stage 6B-i kill-9 crash tests (child seeds Fjall payload rows, drives the
    // file tier, SIGKILLs, parent re-runs recovery and diffs point reads) were removed when
    // the payload stopped living in Fjall, so the old seed + read-from-Fjall baseline no
    // longer applies. `test_recovery_truncates_engine_written_chain` above covers the DB-side
    // truncation of the current model (one DB commit per batch + remove-2-lines/truncate-to-BX);
    // a kill-9 variant that SIGKILLs mid-batch is still a fresh design.
}
