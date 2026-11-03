use smartnet_primitives_traits::NodePrimitives;
use smartnet_prune::{
    PruneMode, PruneModes, PruneSegment, PrunerBuilder, SegmentOutput, SegmentOutputCheckpoint,
};
use smartnet_stages_api::{ExecInput, ExecOutput, Stage, StageCheckpoint, StageError, StageId};
use smartnet_db_api::{table::Value, transaction::DbTxMut};
use smartnet_provider::{
    AuxDbProviderFactory, BlockReader, ChainStateBlockReader, DBProvider, NodePrimitivesProvider,
    PruneCheckpointReader, PruneCheckpointWriter, StageCheckpointReader, TransactionsProvider,
};
use smartnet_storage_api::{ChangeSetReader, StorageChangeSetReader, StorageSettingsCache};
use tracing::info;

/// The prune stage that runs the pruner with the provided prune modes.
///
/// There are two main reasons to have this stage when running a full node:
/// - Sender Recovery stage inserts a lot of data into the database that's only needed for the
///   Execution stage. Pruner will clean up the unneeded recovered senders.
/// - Pruning during the live sync can take a significant amount of time, especially history
///   segments. If we can prune as much data as possible in one go before starting the live sync, we
///   should do it.
///
/// `commit_threshold` is the maximum number of entries to prune before committing
/// progress to the database.
#[derive(Debug)]
pub struct PruneStage {
    prune_modes: PruneModes,
    commit_threshold: usize,
}

impl PruneStage {
    /// Crate new prune stage with the given prune modes and commit threshold.
    pub const fn new(prune_modes: PruneModes, commit_threshold: usize) -> Self {
        Self { prune_modes, commit_threshold }
    }
}

impl<Provider> Stage<Provider> for PruneStage
where
    Provider: DBProvider<Tx: DbTxMut>
        + PruneCheckpointReader
        + PruneCheckpointWriter
        + BlockReader
        + ChainStateBlockReader
        + StageCheckpointReader
        + NodePrimitivesProvider<
            Primitives: NodePrimitives<SignedTx: Value, Receipt: Value, BlockHeader: Value>,
        > + StorageSettingsCache
        + ChangeSetReader
        + StorageChangeSetReader
        + AuxDbProviderFactory
        + TransactionsProvider<Transaction: smartnet_codecs::Compact>,
{
    fn id(&self) -> StageId {
        StageId::Prune
    }

    fn execute(&mut self, provider: &Provider, input: ExecInput) -> Result<ExecOutput, StageError> {
        let mut pruner = PrunerBuilder::default()
            .segments(self.prune_modes.clone())
            .delete_limit(self.commit_threshold)
            .build::<Provider>();

        let result = pruner.run_with_provider(provider, input.target())?;
        if result.progress.is_finished() {
            Ok(ExecOutput { checkpoint: StageCheckpoint::new(input.target()), done: true })
        } else {
            if let Some((last_segment, last_segment_output)) = result.segments.last() {
                match last_segment_output {
                    SegmentOutput {
                        progress,
                        pruned,
                        checkpoint:
                            checkpoint @ Some(SegmentOutputCheckpoint { block_number: Some(_), .. }),
                    } => {
                        info!(
                            target: "sync::stages::prune::exec",
                            ?last_segment,
                            ?progress,
                            ?pruned,
                            ?checkpoint,
                            "Last segment has more data to prune"
                        )
                    }
                    SegmentOutput { progress, pruned, checkpoint: _ } => {
                        info!(
                            target: "sync::stages::prune::exec",
                            ?last_segment,
                            ?progress,
                            ?pruned,
                            "Last segment has more data to prune"
                        )
                    }
                }
            }
            // We cannot set the checkpoint yet, because prune segments may have different highest
            // pruned block numbers
            Ok(ExecOutput { checkpoint: input.checkpoint(), done: false })
        }
    }
}

/// The prune sender recovery stage that runs the pruner with the provided `PruneMode` for the
/// `SenderRecovery` segment.
///
/// Under the hood, this stage has the same functionality as [`PruneStage`].
///
/// Should be run right after `Execution`, unlike [`PruneStage`] which runs at the end.
/// This lets subsequent stages reuse the freed pages instead of growing the freelist.
#[derive(Debug)]
pub struct PruneSenderRecoveryStage(PruneStage);

impl PruneSenderRecoveryStage {
    /// Create new prune sender recovery stage with the given prune mode and commit threshold.
    pub fn new(prune_mode: PruneMode, commit_threshold: usize) -> Self {
        Self(PruneStage::new(
            PruneModes { sender_recovery: Some(prune_mode), ..PruneModes::default() },
            commit_threshold,
        ))
    }
}

impl<Provider> Stage<Provider> for PruneSenderRecoveryStage
where
    Provider: DBProvider<Tx: DbTxMut>
        + PruneCheckpointReader
        + PruneCheckpointWriter
        + BlockReader
        + ChainStateBlockReader
        + StageCheckpointReader
        + NodePrimitivesProvider<
            Primitives: NodePrimitives<SignedTx: Value, Receipt: Value, BlockHeader: Value>,
        > + StorageSettingsCache
        + ChangeSetReader
        + StorageChangeSetReader
        + AuxDbProviderFactory
        + TransactionsProvider<Transaction: smartnet_codecs::Compact>,
{
    fn id(&self) -> StageId {
        StageId::PruneSenderRecovery
    }

    fn execute(&mut self, provider: &Provider, input: ExecInput) -> Result<ExecOutput, StageError> {
        let mut result = self.0.execute(provider, input)?;

        // Adjust the checkpoint to the highest pruned block number of the Sender Recovery segment
        if !result.done {
            let checkpoint = provider
                .get_prune_checkpoint(PruneSegment::SenderRecovery)?
                .ok_or(StageError::MissingPruneCheckpoint(PruneSegment::SenderRecovery))?;

            // `unwrap_or_default` is safe because we know that genesis block doesn't have any
            // transactions and senders
            result.checkpoint = StageCheckpoint::new(checkpoint.block_number.unwrap_or_default());
        }

        Ok(result)
    }
}
