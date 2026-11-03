use smartnet_prune_types::PruneMode;
use smartnet_stages_api::{ExecInput, ExecOutput, Stage, StageCheckpoint, StageError, StageId};
use smartnet_config::config::SenderRecoveryConfig;

/// The sender recovery stage is retained for pipeline compatibility. Transaction senders are
/// recovered and written to the file tier by the engine, so the stage is a no-op (3B.2).
#[derive(Clone, Debug)]
pub struct SenderRecoveryStage {
    /// The size of inserted items after which the control
    /// flow will be returned to the pipeline for commit
    pub commit_threshold: u64,
    /// Prune mode for sender recovery.
    pub prune_mode: Option<PruneMode>,
}

impl SenderRecoveryStage {
    /// Create new instance of [`SenderRecoveryStage`].
    pub const fn new(config: SenderRecoveryConfig, prune_mode: Option<PruneMode>) -> Self {
        Self { commit_threshold: config.commit_threshold, prune_mode }
    }
}

impl Default for SenderRecoveryStage {
    fn default() -> Self {
        Self { commit_threshold: 5_000_000, prune_mode: None }
    }
}

impl<Provider> Stage<Provider> for SenderRecoveryStage {
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::SenderRecovery
    }

    /// Senders are recovered and written to the file tier by the engine, so there is no database
    /// payload to annotate here.
    fn execute(
        &mut self,
        _provider: &Provider,
        input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        let target = input.target();
        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()));
        }
        Ok(ExecOutput { checkpoint: StageCheckpoint::new(target), done: true })
    }
}
