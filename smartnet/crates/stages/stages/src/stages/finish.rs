use smartnet_stages_api::{ExecInput, ExecOutput, Stage, StageCheckpoint, StageError, StageId};

/// The finish stage.
///
/// This stage does not write anything; it's checkpoint is used to denote the highest fully synced
/// block.
#[derive(Default, Debug, Clone)]
#[non_exhaustive]
pub struct FinishStage;

impl<Provider> Stage<Provider> for FinishStage {
    fn id(&self) -> StageId {
        StageId::Finish
    }

    fn execute(
        &mut self,
        _provider: &Provider,
        input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        Ok(ExecOutput { checkpoint: StageCheckpoint::new(input.target()), done: true })
    }
}
