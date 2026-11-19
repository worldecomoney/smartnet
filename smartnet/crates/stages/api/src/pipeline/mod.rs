mod ctrl;
mod event;
pub use crate::pipeline::ctrl::ControlFlow;
use crate::{PipelineTarget, StageCheckpoint, StageId};
use alloy_primitives::{BlockNumber, B256};
pub use event::*;
use futures_util::Future;
use smartnet_provider::{
    providers::ProviderNodeTypes, DBProvider, DatabaseProviderFactory, ProviderFactory,
    StageCheckpointReader, StageCheckpointWriter,
};
use smartnet_tasks::{EventSender, EventStream};
use std::{
    pin::Pin,
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tracing::*;

mod builder;
mod progress;
mod set;

use crate::{
    BlockErrorKind, ExecInput, ExecOutput, MetricEvent, MetricEventsSender, PipelineError, Stage,
    StageError, StageExt,
};
pub use builder::*;
use progress::*;
pub use set::*;

/// A container for a queued stage.
pub(crate) type BoxedStage<DB> = Box<dyn Stage<DB>>;

/// The future that returns the owned pipeline and the result of the pipeline run. See
/// [`Pipeline::run_as_fut`].
pub type PipelineFut<N> = Pin<Box<dyn Future<Output = PipelineWithResult<N>> + Send>>;

/// The pipeline type itself with the result of [`Pipeline::run_as_fut`]
pub type PipelineWithResult<N> = (Pipeline<N>, Result<ControlFlow, PipelineError>);

#[cfg_attr(doc, aquamarine::aquamarine)]
/// A staged sync pipeline.
///
/// The pipeline executes queued [stages][Stage] serially. An external component determines the tip
/// of the chain and the pipeline then executes each stage in order from the current local chain tip
/// and the external chain tip. When a stage is executed, it will run until it reaches the chain
/// tip.
///
/// After the entire pipeline has been run, it will run again unless asked to stop (see
/// [`Pipeline::set_max_block`]).
///
/// `include_mmd!("docs/mermaid/pipeline.mmd`")
///
/// # No unwinding
///
/// `SmartNet` has no stage unwind: a recoverable stage error retries the stage; any other
/// stage error fails this pipeline run (drop). Only `restore_unwind(2)` may unwind, on
/// cold-shutdown recovery.
///
/// # Defaults
///
/// The [`DefaultStages`](crate::sets::DefaultStages) are used to fully sync the node.
pub struct Pipeline<N: ProviderNodeTypes> {
    /// Provider factory.
    provider_factory: ProviderFactory<N>,
    /// All configured stages in the order they will be executed.
    stages: Vec<BoxedStage<<ProviderFactory<N> as DatabaseProviderFactory>::ProviderRW>>,
    /// The maximum block number to sync to.
    max_block: Option<BlockNumber>,
    /// Sender for events the pipeline emits.
    event_sender: EventSender<PipelineEvent>,
    /// Keeps track of the progress of the pipeline.
    progress: PipelineProgress,
    /// A Sender for the current chain tip to sync to.
    ///
    /// This is used to notify the headers stage about a new sync target.
    tip_tx: Option<watch::Sender<B256>>,
    metrics_tx: Option<MetricEventsSender>,
}

impl<N: ProviderNodeTypes> Pipeline<N> {
    /// Construct a pipeline using a [`PipelineBuilder`].
    pub fn builder() -> PipelineBuilder<<ProviderFactory<N> as DatabaseProviderFactory>::ProviderRW>
    {
        PipelineBuilder::default()
    }

    /// Return the minimum block number achieved by
    /// any stage during the execution of the pipeline.
    pub const fn minimum_block_number(&self) -> Option<u64> {
        self.progress.minimum_block_number
    }

    /// Set tip for reverse sync.
    #[track_caller]
    pub fn set_tip(&self, tip: B256) {
        let _ = self.tip_tx.as_ref().expect("tip sender is set").send(tip).map_err(|_| {
            warn!(target: "sync::pipeline", "Chain tip channel closed");
        });
    }

    /// Listen for events on the pipeline.
    pub fn events(&self) -> EventStream<PipelineEvent> {
        self.event_sender.new_listener()
    }

    /// Get a mutable reference to a stage by index.
    pub fn stage(
        &mut self,
        idx: usize,
    ) -> &mut dyn Stage<<ProviderFactory<N> as DatabaseProviderFactory>::ProviderRW> {
        &mut self.stages[idx]
    }
}

impl<N: ProviderNodeTypes> Pipeline<N> {
    /// Registers progress metrics for each registered stage
    pub fn register_metrics(&mut self) -> Result<(), PipelineError> {
        let Some(metrics_tx) = &mut self.metrics_tx else { return Ok(()) };
        let provider = self.provider_factory.provider()?;

        for stage in &self.stages {
            let stage_id = stage.id();
            let _ = metrics_tx.send(MetricEvent::StageCheckpoint {
                stage_id,
                checkpoint: provider.get_stage_checkpoint(stage_id)?.unwrap_or_default(),
                max_block_number: None,
                elapsed: Duration::default(),
            });
        }
        Ok(())
    }

    /// Consume the pipeline and run it until it reaches the provided tip, if set. Return the
    /// pipeline and its result as a future.
    #[track_caller]
    pub fn run_as_fut(mut self, target: Option<PipelineTarget>) -> PipelineFut<N> {
        let _ = self.register_metrics();
        Box::pin(async move {
            // NOTE: the tip should only be None if we are in continuous sync mode.
            if let Some(PipelineTarget::Sync(tip)) = target {
                self.set_tip(tip);
            }

            let result = self.run_loop().await;
            trace!(target: "sync::pipeline", ?target, ?result, "Pipeline finished");
            (self, result)
        })
    }

    /// Run the pipeline in an infinite loop. Will terminate early if the user has specified
    /// a `max_block` in the pipeline.
    pub async fn run(&mut self) -> Result<(), PipelineError> {
        let _ = self.register_metrics(); // ignore error

        loop {
            let next_action = self.run_loop().await?;

            // Terminate the loop early if it's reached the maximum user
            // configured block.
            if next_action.should_continue() &&
                self.progress
                    .minimum_block_number
                    .zip(self.max_block)
                    .is_some_and(|(progress, target)| progress >= target)
            {
                trace!(
                    target: "sync::pipeline",
                    ?next_action,
                    minimum_block_number = ?self.progress.minimum_block_number,
                    max_block = ?self.max_block,
                    "Terminating pipeline."
                );
                return Ok(());
            }
        }
    }

    /// Performs one pass of the pipeline across all stages. After successful
    /// execution of each stage, it proceeds to commit it to the database.
    ///
    /// If any stage is unsuccessful at execution, a recoverable error retries the stage;
    /// any other error fails this pipeline run (drop — no unwind).
    ///
    /// Returns the control flow after it ran the pipeline.
    /// This will be [`ControlFlow::Continue`] or [`ControlFlow::NoProgress`] of the _last_ stage
    /// in the pipeline (for example the `Finish` stage).
    pub async fn run_loop(&mut self) -> Result<ControlFlow, PipelineError> {
        let mut previous_stage = None;
        for stage_index in 0..self.stages.len() {
            let stage = &self.stages[stage_index];
            let stage_id = stage.id();

            trace!(target: "sync::pipeline", stage = %stage_id, "Executing stage");
            let next = self.execute_stage_to_completion(previous_stage, stage_index).await?;

            trace!(target: "sync::pipeline", stage = %stage_id, ?next, "Completed stage");

            match next {
                ControlFlow::NoProgress { block_number } => {
                    if let Some(block_number) = block_number {
                        self.progress.update(block_number);
                    }
                }
                ControlFlow::Continue { block_number } => self.progress.update(block_number),
            }

            previous_stage = Some(
                self.provider_factory
                    .provider()?
                    .get_stage_checkpoint(stage_id)?
                    .unwrap_or_default()
                    .block_number,
            );
        }

        Ok(self.progress.next_ctrl())
    }

    async fn execute_stage_to_completion(
        &mut self,
        previous_stage: Option<BlockNumber>,
        stage_index: usize,
    ) -> Result<ControlFlow, PipelineError> {
        let total_stages = self.stages.len();

        let stage_id = self.stage(stage_index).id();
        let mut made_progress = false;
        let target = self.max_block.or(previous_stage);

        loop {
            let prev_checkpoint = self.provider_factory.get_stage_checkpoint(stage_id)?;

            let stage_reached_max_block = prev_checkpoint
                .zip(self.max_block)
                .is_some_and(|(prev_progress, target)| prev_progress.block_number >= target);
            if stage_reached_max_block {
                warn!(
                    target: "sync::pipeline",
                    stage = %stage_id,
                    max_block = self.max_block,
                    prev_block = prev_checkpoint.map(|progress| progress.block_number),
                    "Stage reached target block, skipping."
                );
                self.event_sender.notify(PipelineEvent::Skipped { stage_id });

                // We reached the maximum block, so we skip the stage
                return Ok(ControlFlow::NoProgress {
                    block_number: prev_checkpoint.map(|progress| progress.block_number),
                });
            }

            let exec_input = ExecInput { target, checkpoint: prev_checkpoint };

            self.event_sender.notify(PipelineEvent::Prepare {
                pipeline_stages_progress: PipelineStagesProgress {
                    current: stage_index + 1,
                    total: total_stages,
                },
                stage_id,
                checkpoint: prev_checkpoint,
                target,
            });

            if let Err(err) = self.stage(stage_index).execute_ready(exec_input).await {
                self.event_sender.notify(PipelineEvent::Error { stage_id });
                self.on_stage_error(stage_id, prev_checkpoint, err)?;
                continue;
            }

            let stage_started_at = Instant::now();
            let provider_rw = self.provider_factory.database_provider_rw()?;

            self.event_sender.notify(PipelineEvent::Run {
                pipeline_stages_progress: PipelineStagesProgress {
                    current: stage_index + 1,
                    total: total_stages,
                },
                stage_id,
                checkpoint: prev_checkpoint,
                target,
            });

            match self.stage(stage_index).execute(&provider_rw, exec_input) {
                Ok(out @ ExecOutput { checkpoint, done }) => {
                    // Update stage checkpoint.
                    provider_rw.save_stage_checkpoint(stage_id, checkpoint)?;

                    // Commit processed data to the database.
                    provider_rw.commit()?;

                    // Invoke stage post commit hook.
                    self.stage(stage_index).post_execute_commit()?;

                    // Notify event listeners and update metrics.
                    self.event_sender.notify(PipelineEvent::Ran {
                        pipeline_stages_progress: PipelineStagesProgress {
                            current: stage_index + 1,
                            total: total_stages,
                        },
                        stage_id,
                        result: out,
                    });
                    if let Some(metrics_tx) = &mut self.metrics_tx {
                        let _ = metrics_tx.send(MetricEvent::StageCheckpoint {
                            stage_id,
                            checkpoint,
                            max_block_number: target,
                            elapsed: stage_started_at.elapsed(),
                        });
                    }

                    let block_number = checkpoint.block_number;
                    let prev_block_number = prev_checkpoint.unwrap_or_default().block_number;
                    made_progress |= block_number != prev_block_number;
                    if done {
                        return Ok(if made_progress {
                            ControlFlow::Continue { block_number }
                        } else {
                            ControlFlow::NoProgress { block_number: Some(block_number) }
                        });
                    }
                }
                Err(err) => {
                    drop(provider_rw);
                    self.event_sender.notify(PipelineEvent::Error { stage_id });

                    self.on_stage_error(stage_id, prev_checkpoint, err)?;
                }
            }
        }
    }

    /// Handles a stage error.
    ///
    /// Returns `Ok(())` to retry the stage (recoverable error — drop the failed transaction
    /// and run the stage again), or `Err` to fail this pipeline run (drop — no unwind).
    fn on_stage_error(
        &self,
        stage_id: StageId,
        prev_checkpoint: Option<StageCheckpoint>,
        err: StageError,
    ) -> Result<(), PipelineError> {
        // Linear-chain invariant (fjall2 / D1): `StageError::DetachedHead` is not handled
        // here — it is fatal (see `StageError::is_fatal`). On a single-chair linear chain a
        // detached head is impossible in normal operation, so it signals corruption or a
        // malicious peer; the pipeline halts loudly.
        if let StageError::Block { block, error } = &err {
            match error {
                BlockErrorKind::Validation(validation_error) => {
                    error!(
                        target: "sync::pipeline",
                        stage = %stage_id,
                        bad_block = %block.block.number,
                        "Stage encountered a validation error: {validation_error}"
                    );

                    // Phase 4b semantics (doc/ark/DONE.md): on error the stage's transaction
                    // is dropped — a rollback, no unwind — which is correct for partial data
                    // writes. MerkleExecute is the exception that needs the reset below: it
                    // clears its inner checkpoint *inside* execute (merkle.rs) just before the
                    // final root check, so a validation failure would roll that clear back and
                    // a restart would resume from stale inner progress. Re-do the clear in a
                    // fresh transaction — kept for safe restart.
                    if stage_id == StageId::MerkleExecute {
                        let provider_rw = self.provider_factory.database_provider_rw()?;
                        provider_rw
                            .save_stage_checkpoint_progress(StageId::MerkleExecute, vec![])?;
                        provider_rw.save_stage_checkpoint(
                            StageId::MerkleExecute,
                            prev_checkpoint.unwrap_or_default(),
                        )?;

                        provider_rw.commit()?;
                    }
                }
                BlockErrorKind::Execution(execution_error) => {
                    error!(
                        target: "sync::pipeline",
                        stage = %stage_id,
                        bad_block = %block.block.number,
                        "Stage encountered an execution error: {execution_error}"
                    );
                }
            }

            // No unwind: drop this pipeline run (SmartNet Phase 4b).
            return Err(err.into());
        }

        if err.is_fatal() {
            error!(target: "sync::pipeline", stage = %stage_id, "Stage encountered a fatal error: {err}");
            Err(err.into())
        } else {
            // On other errors we assume they are recoverable if we discard the
            // transaction and run the stage again.
            warn!(
                target: "sync::pipeline",
                stage = %stage_id,
                "Stage encountered a non-fatal error: {err}. Retrying..."
            );
            Ok(())
        }
    }
}

impl<N: ProviderNodeTypes> std::fmt::Debug for Pipeline<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("stages", &self.stages.iter().map(|stage| stage.id()).collect::<Vec<StageId>>())
            .field("max_block", &self.max_block)
            .field("event_sender", &self.event_sender)
            .finish()
    }
}
