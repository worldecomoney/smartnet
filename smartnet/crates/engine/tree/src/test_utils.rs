use alloy_primitives::B256;
use smartnet_stages::{test_utils::TestStages, ExecOutput, StageError};
use std::sync::Arc;
use std::collections::VecDeque;
use smartnet_stages_api::Pipeline;
use smartnet_chainspec::ChainSpec;
use smartnet_provider::test_utils::{
    create_test_provider_factory_with_chain_spec, MockNodeTypesWithDB,
};
use tokio::sync::watch;

/// Test pipeline builder.
#[derive(Default, Debug)]
pub struct TestPipelineBuilder {
    pipeline_exec_outputs: VecDeque<Result<ExecOutput, StageError>>,
}

impl TestPipelineBuilder {
    /// Create a new [`TestPipelineBuilder`].
    pub const fn new() -> Self {
        Self { pipeline_exec_outputs: VecDeque::new() }
    }

    /// Set the pipeline execution outputs to use for the test consensus engine.
    pub fn with_pipeline_exec_outputs(
        mut self,
        pipeline_exec_outputs: VecDeque<Result<ExecOutput, StageError>>,
    ) -> Self {
        self.pipeline_exec_outputs = pipeline_exec_outputs;
        self
    }

    /// Set the executor results to use for the test consensus engine.
    #[deprecated(
        note = "no-op: executor results are not used and will be removed in a future release"
    )]
    pub fn with_executor_results(
        self,
        executor_results: Vec<smartnet_provider::ExecutionOutcome>,
    ) -> Self {
        let _ = executor_results;
        self
    }

    /// Builds the pipeline.
    pub fn build(self, chain_spec: Arc<ChainSpec>) -> Pipeline<MockNodeTypesWithDB> {
        smartnet_tracing::init_test_tracing();

        // Setup pipeline
        let (tip_tx, _tip_rx) = watch::channel(B256::default());
        let pipeline = Pipeline::<MockNodeTypesWithDB>::builder()
            .add_stages(TestStages::new(self.pipeline_exec_outputs))
            .with_tip_sender(tip_tx);

        let provider_factory = create_test_provider_factory_with_chain_spec(chain_spec);

        pipeline.build(provider_factory)
    }
}
