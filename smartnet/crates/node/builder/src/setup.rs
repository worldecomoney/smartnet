//! Helpers for setting up parts of the node.

use std::sync::Arc;

use alloy_primitives::B256;
use smartnet_chain_state::ExExManagerHandle;
use smartnet_config::{config::StageConfig, PruneConfig};
use smartnet_consensus::FullConsensus;
use smartnet_evm::ConfigureEvm;
use smartnet_provider::{providers::ProviderNodeTypes, ProviderFactory};
use smartnet_stages::{prelude::DefaultStages, stages::ExecutionStage, Pipeline, StageSet};
use smartnet_tracing::tracing::debug;
use tokio::sync::watch;

/// Builds the [Pipeline] with the given [`ProviderFactory`].
///
/// The pipeline carries the offline (execution / hashing / indexing) stages plus the finish
/// stage. There is no network download path — blocks reach storage through the engine
/// (`new_payload` via mesh inject, RPC sync, or RLP import), so the pipeline is only used
/// for unwind and consistency repair.
#[expect(clippy::too_many_arguments)]
pub fn build_pipeline<N, Evm>(
    provider_factory: ProviderFactory<N>,
    stage_config: &StageConfig,
    consensus: Arc<dyn FullConsensus<N::Primitives>>,
    max_block: Option<u64>,
    metrics_tx: smartnet_stages::MetricEventsSender,
    prune_config: PruneConfig,
    evm_config: Evm,
    exex_manager_handle: ExExManagerHandle<N::Primitives>,
) -> eyre::Result<Pipeline<N>>
where
    N: ProviderNodeTypes,
    Evm: ConfigureEvm<Primitives = N::Primitives> + 'static,
{
    let mut builder = Pipeline::<N>::builder();

    if let Some(max_block) = max_block {
        debug!(target: "smartnet::cli", max_block, "Configuring builder to use max block");
        builder = builder.with_max_block(max_block)
    }

    let (tip_tx, _tip_rx) = watch::channel(B256::ZERO);

    let pipeline = builder
        .with_tip_sender(tip_tx)
        .with_metrics_tx(metrics_tx)
        .add_stages(
            DefaultStages::new(
                evm_config.clone(),
                Arc::clone(&consensus),
                stage_config.clone(),
                prune_config.segments,
            )
            .set(ExecutionStage::new(
                evm_config,
                consensus,
                stage_config.execution.into(),
                stage_config.execution_external_clean_threshold(),
                exex_manager_handle,
            )),
        )
        .build(provider_factory);

    Ok(pipeline)
}
