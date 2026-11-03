use smartnet_stages_api::StageId;

mod test_db;
pub use test_db::{StorageKind, TestStageDB};

mod set;
pub use set::TestStages;

/// The test stage id
pub const TEST_STAGE_ID: StageId = StageId::Other("TestStage");
