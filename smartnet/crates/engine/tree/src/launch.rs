//! Engine orchestrator launch helper.
//!
//! Provides [`build_engine_orchestrator`](crate::launch::build_engine_orchestrator) which wires
//! together all engine components and returns a
//! [`ChainOrchestrator`](crate::chain::ChainOrchestrator) ready to be polled as a `Stream`.

use crate::{
    backfill::PipelineSync,
    chain::ChainOrchestrator,
    engine::{EngineApiKind, EngineApiRequest, EngineApiRequestHandler, EngineHandler},
    persistence::PersistenceHandle,
    tree::{EngineApiTreeHandler, EngineValidator, TreeConfig, WaitForCaches},
};
use futures::Stream;
use smartnet_evm::ConfigureEvm;
use smartnet_primitives_traits::NodePrimitives;
use smartnet_prune::PrunerWithFactory;
use smartnet_stages_api::{MetricEventsSender, Pipeline};
use smartnet_trie_db::ChangesetCache;
use smartnet_consensus::FullConsensus;
use smartnet_engine_primitives::BeaconEngineMessage;
use smartnet_file_tier::SignatureAccess;
use smartnet_payload_builder::PayloadBuilderHandle;
use smartnet_provider::{
    providers::{BlockchainProvider, ProviderNodeTypes},
    ProviderFactory,
};
use smartnet_tasks::Runtime;
use std::{path::PathBuf, sync::Arc};

/// Builds the engine [`ChainOrchestrator`] that drives the chain forward.
///
/// This spawns and wires together the following components:
///
/// - **[`PersistenceHandle`]** — spawns the persistence service on a background thread for writing
///   blocks and performing pruning outside the critical consensus path.
/// - **[`EngineApiTreeHandler`]** — spawns the tree handler that processes engine API requests
///   (`newPayload`, `forkchoiceUpdated`) and maintains the in-memory chain state.
/// - **[`EngineApiRequestHandler`]** + **[`EngineHandler`]** — glue that routes incoming CL
///   messages to the tree handler.
/// - **[`PipelineSync`]** — wraps the staged sync [`Pipeline`] for backfill sync when the node
///   needs to catch up over large block ranges.
///
/// There is no block downloader: blocks reach the engine through mesh inject, RPC sync, or
/// RLP import — all of which call `new_payload` directly.
///
/// The returned orchestrator implements [`Stream`] and yields
/// [`ChainEvent`]s.
///
/// [`ChainEvent`]: crate::chain::ChainEvent
#[expect(clippy::too_many_arguments, clippy::type_complexity)]
pub fn build_engine_orchestrator<N, S, V, C>(
    engine_kind: EngineApiKind,
    consensus: Arc<dyn FullConsensus<N::Primitives>>,
    incoming_requests: S,
    pipeline: Pipeline<N>,
    pipeline_task_spawner: Runtime,
    provider: ProviderFactory<N>,
    blockchain_db: BlockchainProvider<N>,
    pruner: PrunerWithFactory<ProviderFactory<N>>,
    payload_builder: PayloadBuilderHandle<N::Payload>,
    payload_validator: V,
    tree_config: TreeConfig,
    sync_metrics_tx: MetricEventsSender,
    evm_config: C,
    changeset_cache: ChangesetCache,
    changeset_window: u64,
    file_tier_base: Option<PathBuf>,
    runtime: Runtime,
) -> Result<
    ChainOrchestrator<
        EngineHandler<
            EngineApiRequestHandler<EngineApiRequest<N::Payload, N::Primitives>, N::Primitives>,
            S,
        >,
        PipelineSync<N>,
    >,
    smartnet_errors::ProviderError,
>
where
    N: ProviderNodeTypes,
    <N::Primitives as NodePrimitives>::SignedTx: SignatureAccess,
    S: Stream<Item = BeaconEngineMessage<N::Payload>> + Send + Sync + Unpin + 'static,
    V: EngineValidator<N::Payload> + WaitForCaches,
    C: ConfigureEvm<Primitives = N::Primitives> + 'static,
{
    let persistence_handle = PersistenceHandle::<N::Primitives>::spawn_service(
        provider,
        pruner,
        sync_metrics_tx,
        changeset_window,
        file_tier_base,
    );

    let canonical_in_memory_state = blockchain_db.canonical_in_memory_state();

    let (to_tree_tx, from_tree) = EngineApiTreeHandler::spawn_new(
        blockchain_db,
        consensus,
        payload_validator,
        persistence_handle,
        payload_builder,
        canonical_in_memory_state,
        tree_config,
        engine_kind,
        evm_config,
        changeset_cache,
        runtime,
    )?;

    let engine_handler = EngineApiRequestHandler::new(to_tree_tx, from_tree);
    let handler = EngineHandler::new(engine_handler, incoming_requests);

    let backfill_sync = PipelineSync::new(pipeline, pipeline_task_spawner);

    Ok(ChainOrchestrator::new(handler, backfill_sync))
}
