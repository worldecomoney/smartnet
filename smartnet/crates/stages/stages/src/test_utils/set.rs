use super::TEST_STAGE_ID;
use crate::{StageSet, StageSetBuilder};
use smartnet_stages_api::{test_utils::TestStage, ExecOutput, StageError};
use std::collections::VecDeque;

#[derive(Default, Debug)]
pub struct TestStages {
    exec_outputs: VecDeque<Result<ExecOutput, StageError>>,
}

impl TestStages {
    pub const fn new(exec_outputs: VecDeque<Result<ExecOutput, StageError>>) -> Self {
        Self { exec_outputs }
    }
}

impl<Provider> StageSet<Provider> for TestStages {
    fn builder(self) -> StageSetBuilder<Provider> {
        StageSetBuilder::default().add_stage(
            TestStage::new(TEST_STAGE_ID).with_exec(self.exec_outputs),
        )
    }
}
