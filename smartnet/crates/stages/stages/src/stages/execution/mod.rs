use crate::stages::MERKLE_STAGE_DEFAULT_INCREMENTAL_THRESHOLD;
use alloy_consensus::BlockHeader;
use alloy_primitives::BlockNumber;
use num_traits::Zero;
use smartnet_evm::{execute::Executor, metrics::ExecutorMetrics, ConfigureEvm};
use smartnet_execution_types::Chain;
use smartnet_chain_state::{ExExManagerHandle, ExExNotification, ExExNotificationSource};
use smartnet_primitives_traits::{format_gas_throughput, BlockBody, NodePrimitives};
use smartnet_revm::database::StateProviderDatabase;
use smartnet_stages_api::{
    BlockErrorKind, CheckpointBlockRange, EntitiesCheckpoint, ExecInput, ExecOutput,
    ExecutionCheckpoint, ExecutionStageThresholds, Stage, StageCheckpoint, StageError, StageId,
};
use smartnet_chainspec::{ChainSpecProvider, EthereumHardforks};
use smartnet_config::config::ExecutionConfig;
use smartnet_consensus::FullConsensus;
use smartnet_db::tables;
use smartnet_db_api::{cursor::DbCursorRO, transaction::DbTx};
use smartnet_provider::{
    BlockHashReader, BlockReader, DBProvider, ExecutionOutcome, HeaderProvider,
    LatestStateProviderRef, OriginalValuesKnown, ProviderError, StateWriteConfig, StateWriter,
    StatsReader, StoragePath, StorageSettingsCache, TransactionVariant,
};
use std::{
    cmp::max,
    collections::BTreeMap,
    ops::RangeInclusive,
    sync::Arc,
    task::{ready, Context, Poll},
    time::{Duration, Instant},
};
use tracing::*;

/// The execution stage executes all transactions and
/// update history indexes.
///
/// Input tables:
/// - [`tables::CanonicalHeaders`] get next block to execute.
/// - [`tables::Headers`] get for revm environment variables.
/// - [`tables::BlockBodyIndices`] to get tx number
/// - [`tables::Transactions`] to execute
///
/// For state access [`LatestStateProviderRef`] provides us latest state and history state
/// For latest most recent state [`LatestStateProviderRef`] would need (Used for execution Stage):
/// - [`tables::PlainAccountState`]
/// - [`tables::Bytecodes`]
/// - [`tables::PlainStorageState`]
///
/// Tables updated after state finishes execution:
/// - [`tables::PlainAccountState`]
/// - [`tables::PlainStorageState`]
/// - [`tables::Bytecodes`]
/// - [`tables::AccountChangeSets`]
/// - [`tables::StorageChangeSets`]
///
/// For unwinds we are accessing:
/// - [`tables::BlockBodyIndices`] get tx index to know what needs to be unwinded
/// - [`tables::AccountsHistory`] to remove change set and apply old values to
/// - [`tables::PlainAccountState`] [`tables::StoragesHistory`] to remove change set and apply old
///   values to [`tables::PlainStorageState`]
// false positive, we cannot derive it if !DB: Debug.
#[derive(Debug)]
pub struct ExecutionStage<E>
where
    E: ConfigureEvm,
{
    /// The stage's internal block executor
    evm_config: E,
    /// The consensus instance for validating blocks.
    consensus: Arc<dyn FullConsensus<E::Primitives>>,
    /// The commit thresholds of the execution stage.
    thresholds: ExecutionStageThresholds,
    /// The highest threshold (in number of blocks) for switching between incremental
    /// and full calculations across [`super::MerkleStage`], [`super::AccountHashingStage`] and
    /// [`super::StorageHashingStage`]. This is required to figure out if can prune or not
    /// changesets on subsequent pipeline runs.
    external_clean_threshold: u64,
    /// Input for the post execute commit hook.
    /// Set after every [`ExecutionStage::execute`] and cleared after
    /// [`ExecutionStage::post_execute_commit`].
    post_execute_commit_input: Option<Chain<E::Primitives>>,
    /// Handle to communicate with `ExEx` manager.
    exex_manager_handle: ExExManagerHandle<E::Primitives>,
    /// Executor metrics.
    metrics: ExecutorMetrics,
}

impl<E> ExecutionStage<E>
where
    E: ConfigureEvm,
{
    /// Create new execution stage with specified config.
    pub fn new(
        evm_config: E,
        consensus: Arc<dyn FullConsensus<E::Primitives>>,
        thresholds: ExecutionStageThresholds,
        external_clean_threshold: u64,
        exex_manager_handle: ExExManagerHandle<E::Primitives>,
    ) -> Self {
        Self {
            external_clean_threshold,
            evm_config,
            consensus,
            thresholds,
            post_execute_commit_input: None,
            exex_manager_handle,
            metrics: ExecutorMetrics::default(),
        }
    }

    /// Create an execution stage with the provided executor.
    ///
    /// The commit threshold will be set to [`MERKLE_STAGE_DEFAULT_INCREMENTAL_THRESHOLD`].
    pub fn new_with_executor(
        evm_config: E,
        consensus: Arc<dyn FullConsensus<E::Primitives>>,
    ) -> Self {
        Self::new(
            evm_config,
            consensus,
            ExecutionStageThresholds::default(),
            MERKLE_STAGE_DEFAULT_INCREMENTAL_THRESHOLD,
            ExExManagerHandle::empty(),
        )
    }

    /// Create new instance of [`ExecutionStage`] from configuration.
    pub fn from_config(
        evm_config: E,
        consensus: Arc<dyn FullConsensus<E::Primitives>>,
        config: ExecutionConfig,
        external_clean_threshold: u64,
    ) -> Self {
        Self::new(
            evm_config,
            consensus,
            config.into(),
            external_clean_threshold,
            ExExManagerHandle::empty(),
        )
    }

    /// Returns whether we can perform pruning of [`tables::AccountChangeSets`] and
    /// [`tables::StorageChangeSets`].
    ///
    /// This function verifies whether the [`super::MerkleStage`] or Hashing stages will run from
    /// scratch. If at least one stage isn't starting anew, it implies that pruning of
    /// changesets cannot occur. This is determined by checking the highest clean threshold
    /// (`self.external_clean_threshold`) across the stages.
    ///
    /// Given that `start_block` changes with each checkpoint, it's necessary to inspect
    /// [`tables::AccountsTrie`] to ensure that [`super::MerkleStage`] hasn't
    /// been previously executed.
    fn can_prune_changesets(
        &self,
        provider: impl StatsReader,
        start_block: u64,
        max_block: u64,
    ) -> Result<bool, StageError> {
        // We can only prune changesets if we're not executing MerkleStage from scratch (by
        // threshold or first-sync)
        Ok(max_block - start_block > self.external_clean_threshold ||
            provider.count_entries::<tables::AccountsTrie>()?.is_zero())
    }
}

impl<E, Provider> Stage<Provider> for ExecutionStage<E>
where
    E: ConfigureEvm,
    Provider: DBProvider
        + BlockReader<
            Block = <E::Primitives as NodePrimitives>::Block,
            Header = <E::Primitives as NodePrimitives>::BlockHeader,
        > + StatsReader
        + BlockHashReader
        + StateWriter<Receipt = <E::Primitives as NodePrimitives>::Receipt>
        + StorageSettingsCache
        + StoragePath
        + ChainSpecProvider<ChainSpec: EthereumHardforks>,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::Execution
    }

    fn poll_execute_ready(
        &mut self,
        cx: &mut Context<'_>,
        _: ExecInput,
    ) -> Poll<Result<(), StageError>> {
        ready!(self.exex_manager_handle.poll_ready(cx));

        Poll::Ready(Ok(()))
    }

    /// Execute the stage
    fn execute(&mut self, provider: &Provider, input: ExecInput) -> Result<ExecOutput, StageError> {
        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()));
        }

        let start_block = input.next_block();
        let max_block = input.target();

        let db = StateProviderDatabase(LatestStateProviderRef::new(provider));
        let mut executor = self.evm_config.batch_executor(db);

        // Progress tracking
        let mut stage_progress = start_block;
        let mut stage_checkpoint =
            execution_checkpoint(provider, start_block, max_block, input.checkpoint())?;

        let mut fetch_block_duration = Duration::default();
        let mut execution_duration = Duration::default();

        let mut last_block = start_block;
        let mut last_execution_duration = Duration::default();
        let mut last_cumulative_gas = 0;
        let mut last_log_instant = Instant::now();
        let log_duration = Duration::from_secs(10);

        debug!(target: "sync::stages::execution", start = start_block, end = max_block, "Executing range");

        // Execute block range
        let mut cumulative_gas = 0;
        let batch_start = Instant::now();

        let mut blocks = Vec::new();
        let mut results = Vec::new();
        for block_number in start_block..=max_block {
            // Fetch the block
            let fetch_block_start = Instant::now();

            // we need the block's transactions but we don't need the transaction hashes
            let block = provider
                .recovered_block(block_number.into(), TransactionVariant::NoHash)?
                .ok_or_else(|| ProviderError::HeaderNotFound(block_number.into()))?;

            fetch_block_duration += fetch_block_start.elapsed();

            cumulative_gas += block.header().gas_used();

            // Configure the executor to use the current state.
            trace!(target: "sync::stages::execution", number = block_number, txs = block.body().transactions().len(), "Executing block");

            // Execute the block
            let execute_start = Instant::now();

            let result = self.metrics.metered_one(&block, |input| {
                executor.execute_one(input).map_err(|error| StageError::Block {
                    block: Box::new(block.block_with_parent()),
                    error: BlockErrorKind::Execution(error),
                })
            })?;

            if let Err(err) = self.consensus.validate_block_post_execution(&block, &result, None) {
                return Err(StageError::Block {
                    block: Box::new(block.block_with_parent()),
                    error: BlockErrorKind::Validation(err),
                });
            }
            results.push(result);

            execution_duration += execute_start.elapsed();

            // Log execution throughput
            if last_log_instant.elapsed() >= log_duration {
                info!(
                    target: "sync::stages::execution",
                    start = last_block,
                    end = block_number,
                    throughput = format_gas_throughput(cumulative_gas - last_cumulative_gas, execution_duration - last_execution_duration),
                    "Executed block range"
                );

                last_block = block_number + 1;
                last_execution_duration = execution_duration;
                last_cumulative_gas = cumulative_gas;
                last_log_instant = Instant::now();
            }

            stage_progress = block_number;
            stage_checkpoint.progress.processed += block.header().gas_used();

            // If we have ExExes we need to save the block in memory for later
            if self.exex_manager_handle.has_exexs() {
                blocks.push(block);
            }

            // Check if we should commit now
            if self.thresholds.is_end_of_batch(
                block_number - start_block,
                executor.size_hint() as u64,
                cumulative_gas,
                batch_start.elapsed(),
            ) {
                break;
            }
        }

        // prepare execution output for writing
        let time = Instant::now();
        let mut state = ExecutionOutcome::from_blocks(
            start_block,
            executor.into_state().take_bundle(),
            results,
        );
        let write_preparation_duration = time.elapsed();

        // log the gas per second for the range we just executed
        debug!(
            target: "sync::stages::execution",
            start = start_block,
            end = stage_progress,
            throughput = format_gas_throughput(cumulative_gas, execution_duration),
            "Finished executing block range"
        );

        // Prepare the input for post execute commit hook, where an `ExExNotification` will be sent.
        //
        // Note: Since we only write to `blocks` if there are any ExExes, we don't need to perform
        // the `has_exexs` check here as well
        if !blocks.is_empty() {
            let previous_input = self.post_execute_commit_input.replace(Chain::new(
                blocks,
                state.clone(),
                BTreeMap::new(),
            ));

            if previous_input.is_some() {
                // Not processing the previous post execute commit input is a critical error, as it
                // means that we didn't send the notification to ExExes
                return Err(StageError::PostExecuteCommit(
                    "Previous post execute commit input wasn't processed",
                ));
            }
        }

        let time = Instant::now();

        if self.can_prune_changesets(provider, start_block, max_block)? {
            let prune_modes = provider.prune_modes_ref();

            // Iterate over all reverts and clear them if pruning is configured.
            for block_number in start_block..=max_block {
                let Some(reverts) =
                    state.bundle.reverts.get_mut((block_number - start_block) as usize)
                else {
                    break;
                };

                // If both account history and storage history pruning is configured, clear reverts
                // for this block.
                if prune_modes
                    .account_history
                    .is_some_and(|m| m.should_prune(block_number, max_block)) &&
                    prune_modes
                        .storage_history
                        .is_some_and(|m| m.should_prune(block_number, max_block))
                {
                    reverts.clear();
                }
            }
        }

        // Write output.
        provider.write_state(&state, OriginalValuesKnown::Yes, StateWriteConfig::default())?;

        let db_write_duration = time.elapsed();
        debug!(
            target: "sync::stages::execution",
            block_fetch = ?fetch_block_duration,
            execution = ?execution_duration,
            write_preparation = ?write_preparation_duration,
            write = ?db_write_duration,
            "Execution time"
        );

        let done = stage_progress == max_block;
        Ok(ExecOutput {
            checkpoint: StageCheckpoint::new(stage_progress)
                .with_execution_stage_checkpoint(stage_checkpoint),
            done,
        })
    }

    fn post_execute_commit(&mut self) -> Result<(), StageError> {
        let Some(chain) = self.post_execute_commit_input.take() else { return Ok(()) };

        // NOTE: We can ignore the error here, since an error means that the channel is closed,
        // which means the manager has died, which then in turn means the node is shutting down.
        let _ = self.exex_manager_handle.send(
            ExExNotificationSource::Pipeline,
            ExExNotification::ChainCommitted { new: Arc::new(chain) },
        );

        Ok(())
    }
}

fn execution_checkpoint<Provider>(
    provider: &Provider,
    start_block: BlockNumber,
    max_block: BlockNumber,
    checkpoint: StageCheckpoint,
) -> Result<ExecutionCheckpoint, ProviderError>
where
    Provider: DBProvider + HeaderProvider,
{
    Ok(match checkpoint.execution_stage_checkpoint() {
        // If checkpoint block range fully matches our range,
        // we take the previously used stage checkpoint as-is.
        Some(stage_checkpoint @ ExecutionCheckpoint { block_range, .. })
            if block_range == CheckpointBlockRange::from(start_block..=max_block) =>
        {
            stage_checkpoint
        }
        // If checkpoint block range precedes our range seamlessly, we take the previously used
        // stage checkpoint and add the amount of gas from our range to the checkpoint total.
        Some(ExecutionCheckpoint {
            block_range: CheckpointBlockRange { to, .. },
            progress: EntitiesCheckpoint { processed, total },
        }) if to == start_block - 1 => ExecutionCheckpoint {
            block_range: CheckpointBlockRange { from: start_block, to: max_block },
            progress: EntitiesCheckpoint {
                processed,
                total: total + calculate_gas_used_from_headers(provider, start_block..=max_block)?,
            },
        },
        // If checkpoint block range ends on the same block as our range, we take the previously
        // used stage checkpoint.
        Some(ExecutionCheckpoint { block_range: CheckpointBlockRange { to, .. }, progress })
            if to == max_block =>
        {
            ExecutionCheckpoint {
                block_range: CheckpointBlockRange { from: start_block, to: max_block },
                progress,
            }
        }
        // If there's any other non-empty checkpoint, we calculate the remaining amount of total gas
        // to be processed not including the checkpoint range.
        Some(ExecutionCheckpoint { progress: EntitiesCheckpoint { processed, .. }, .. }) => {
            let after_checkpoint_block_number =
                calculate_gas_used_from_headers(provider, checkpoint.block_number + 1..=max_block)?;

            ExecutionCheckpoint {
                block_range: CheckpointBlockRange { from: start_block, to: max_block },
                progress: EntitiesCheckpoint {
                    processed,
                    total: processed + after_checkpoint_block_number,
                },
            }
        }
        // Otherwise, we recalculate the whole stage checkpoint including the amount of gas
        // already processed, if there's any.
        _ => {
            // Stage 3B.1: headers live only in the primary database, so the genesis block
            // is the first canonical chain entry.
            let genesis_block_number = provider
                .tx_ref()
                .cursor_read::<tables::CanonicalChain>()?
                .first()?
                .map(|(number, _)| number)
                .unwrap_or_default();
            let processed = calculate_gas_used_from_headers(
                provider,
                genesis_block_number..=max(start_block - 1, genesis_block_number),
            )?;

            ExecutionCheckpoint {
                block_range: CheckpointBlockRange { from: start_block, to: max_block },
                progress: EntitiesCheckpoint {
                    processed,
                    total: processed +
                        calculate_gas_used_from_headers(provider, start_block..=max_block)?,
                },
            }
        }
    })
}

/// Calculates the total amount of gas used from the headers in the given range.
pub fn calculate_gas_used_from_headers<Provider>(
    provider: &Provider,
    range: RangeInclusive<BlockNumber>,
) -> Result<u64, ProviderError>
where
    Provider: HeaderProvider,
{
    debug!(target: "sync::stages::execution", ?range, "Calculating gas used from headers");

    let mut gas_total = 0;

    let start = Instant::now();

    // Stage 3 (3A.3): headers live only in the primary database.
    for header in provider.headers_range(range.clone())? {
        gas_total += header.gas_used();
    }

    let duration = start.elapsed();
    debug!(target: "sync::stages::execution", ?range, ?duration, "Finished calculating gas used from headers");

    Ok(gas_total)
}
