//! Engine node related functionality.

use crate::{
    common::{Attached, LaunchContextWith, WithConfigs},
    engine_util::EngineMessageStreamExt,
    events::node,
    hooks::NodeHooks,
    rpc::{
        EngineShutdown, EngineValidatorAddOn, EngineValidatorBuilder, RpcHandle, SmartnetRpcAddOns,
    },
    setup::build_pipeline,
    AddOns, AddOnsContext, FullNode, LaunchContext, LaunchNode, NodeAdapter,
    NodeBuilderWithComponents, NodeComponents, NodeComponentsBuilder, NodeHandle, NodeTypesAdapter,
};
use futures::{stream::FusedStream, stream_select, FutureExt, StreamExt};
use smartnet_chain_state::ExExManagerHandle;
use smartnet_chainspec::EthChainSpec;
use smartnet_engine_tree::{
    chain::{ChainEvent, FromOrchestrator},
    engine::{EngineApiKind, EngineApiRequest, EngineRequestHandler},
    launch::build_engine_orchestrator,
    tree::TreeConfig,
};
use smartnet_file_tier::SignatureAccess;
use smartnet_node_api::{
    BuiltPayload, ConsensusEngineHandle, FullNodeTypes, NodeTypes, NodeTypesWithDBAdapter,
};
use smartnet_node_core::{
    dirs::{ChainPath, DataDirPath},
    exit::NodeExitFuture,
};
use smartnet_primitives_traits::NodePrimitives;
use smartnet_provider::{
    providers::{BlockchainProvider, NodeTypesForProvider, ProviderNodeTypes},
    ProviderFactory, StorageEngine, StorageSettingsCache,
};
use smartnet_tasks::{EventSender, TaskExecutor};
use smartnet_tracing::tracing::{debug, error, info};
use smartnet_trie_db::ChangesetCache;
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::{mpsc::unbounded_channel, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;

/// THE final shutdown protocol, shared by the command-success, command-error and ctrl+c paths:
/// after the graceful signal, wait until every **other** graceful task has finalized, then
/// flush the file tier and write `SHUTDOWN_CLEAN` (flag + timestamp) **last**. The runtime is
/// dropped only after this returns. A timeout leaves no marker, so the next boot unwinds.
async fn final_protocol<N: ProviderNodeTypes>(tasks: TaskExecutor, factory: ProviderFactory<N>) {
    // This task owns one guard; `1` means every other graceful task is done.
    while tasks.graceful_tasks_remaining() > 1 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let engine = StorageEngine::new(factory, None, 0);
    if let Some(base) = smartnet_file_tier::base() {
        match engine.shutdown_clean(base) {
            Ok(()) => debug!(target: "smartnet::cli", "flushed FS and recorded SHUTDOWN_CLEAN"),
            Err(err) => error!(target: "smartnet::cli", %err, "failed to record clean shutdown"),
        }
    } else {
        error!(target: "smartnet::cli", "file tier base is unset; cannot record SHUTDOWN_CLEAN");
    }
}

/// The engine node launcher.
#[derive(Debug)]
pub struct EngineNodeLauncher {
    /// The task executor for the node.
    pub ctx: LaunchContext,

    /// Temporary configuration for engine tree.
    /// After engine is stabilized, this should be configured through node builder.
    pub engine_tree_config: TreeConfig,
}

impl EngineNodeLauncher {
    /// Create a new instance of the ethereum node launcher.
    pub const fn new(
        task_executor: TaskExecutor,
        data_dir: ChainPath<DataDirPath>,
        engine_tree_config: TreeConfig,
    ) -> Self {
        Self { ctx: LaunchContext::new(task_executor, data_dir), engine_tree_config }
    }

    async fn launch_node<T, CB, AO>(
        self,
        target: NodeBuilderWithComponents<T, CB, AO>,
    ) -> eyre::Result<NodeHandle<NodeAdapter<T, CB::Components>, AO>>
    where
        T: FullNodeTypes<
            Types: NodeTypesForProvider,
            Provider = BlockchainProvider<
                NodeTypesWithDBAdapter<<T as FullNodeTypes>::Types, <T as FullNodeTypes>::DB>,
            >,
        >,
        CB: NodeComponentsBuilder<T>,
        AO: SmartnetRpcAddOns<NodeAdapter<T, CB::Components>>
            + EngineValidatorAddOn<NodeAdapter<T, CB::Components>>,
        <<T::Types as NodeTypes>::Primitives as NodePrimitives>::SignedTx: SignatureAccess,
    {
        let Self { ctx, engine_tree_config } = self;
        let NodeBuilderWithComponents {
            adapter: NodeTypesAdapter { database },
            aux_db_provider,
            components_builder,
            add_ons: AddOns { hooks, add_ons },
            config,
        } = target;
        let NodeHooks { on_component_initialized, on_node_started, .. } = hooks;

        // Captured before `config` is moved into the launch context: the identity source for a
        // programmatic launch is the chain spec passed to it (the operator CLI path bootstraps
        // DB-only before this point — operator `config.toml` is never a chain-identity source).
        let spec_chain_id = config.chain.chain_id();
        let spec_validators =
            smartnet_provider::init::validators_from_extra_data(&config.chain.genesis().extra_data);

        // Create changeset cache that will be shared across the engine
        let changeset_cache = ChangesetCache::new();

        // 6B: the provider's aged-block read path resolves records relative to the
        // process-global file-tier base (decision 6B-d.4, set once, first caller wins).
        // The file tree lives beside the engine dir — `<datadir>/blocks/{S}/{Y:02}/` — not
        // inside the database's own directory.
        //
        // Set it BEFORE the blockchain db is built below: a file-tier block keeps its
        // header/payload only in the file tree — the DB holds the index (CanonicalChain,
        // HeaderNumbers) but no `Blocks[]` row — so the very first tip read in
        // `BlockchainProvider::new` can only resolve the header through the file tier.
        // Setting it after the provider is constructed leaves `in_file_tier` false for that
        // read, the DB lookup misses the header row, and startup fails with
        // `HeaderNotFound(<tip>)`. It is also set before the engine can hand the first shard
        // off — the floor only advances afterwards, so every below-floor read finds a base.
        let file_tier_base = ctx
            .data_dir
            .data_dir()
            .parent()
            .expect("engine datadir is a strict child of the chain datadir")
            .to_path_buf();
        smartnet_file_tier::set_base(file_tier_base.clone());

        // setup the launch context
        let ctx = ctx
            .with_configured_globals(engine_tree_config.reserved_cpu_cores())
            // load the toml config
            .with_loaded_toml_config(config)?
            // attach the database
            .attach(database.clone())
            // ensure certain settings take effect
            .with_adjusted_configs()
            // Create the provider factory with changeset cache
            .with_provider_factory::<_, <CB::Components as NodeComponents<T>>::Evm>(changeset_cache.clone(), aux_db_provider).await?
            .inspect(|_| {
                info!(target: "smartnet::cli", "Database opened");
            })
            .inspect(|this| {
                debug!(target: "smartnet::cli", chain=%this.chain_id(), genesis=?this.genesis_hash(), "Initializing genesis");
            })
            .with_genesis()?
            .inspect(|this: &LaunchContextWith<Attached<WithConfigs<<T::Types as NodeTypes>::ChainSpec>, _>>| {
                info!(target: "smartnet::cli", "execution rulebook: MARS (single baked-in EIP set)");
                let settings = this.provider_factory().cached_storage_settings();
                info!(target: "smartnet::cli", ?settings, "Loaded storage settings");
            });

        // The network settings are bootstrapped once, before anything reads them. The operator
        // CLI bootstraps first (database-only); this is the fallback for a programmatic launch,
        // and its identity source is the chain spec passed to this launch. Operator `config.toml`
        // is never a chain-identity source. `bootstrap_network_settings...` is a no-op once the
        // globals are set.
        if !smartnet_file_tier::network_settings_initialised() {
            let (id, signers) =
                smartnet_provider::init::read_network_settings(&database).unwrap_or((None, None));
            smartnet_file_tier::bootstrap_network_settings_with_fallback(
                id,
                signers,
                spec_chain_id,
                spec_validators,
            )
            .map_err(|err| eyre::eyre!("{err}"))?;
        }

        bootstrap_seq_20_recovery(&ctx, file_tier_base.as_path())?;

        // Register the final protocol immediately after recovery, before any later bootstrap
        // step can fail. Recovery has just cleared the marker; waiting until components are
        // attached leaves a window where `with_blockchain_db` / `with_components` can fail with
        // no task left to record a clean shutdown, and the next boot would wrongly unwind.
        // The runner fires the graceful signal on success, command error and ctrl+c alike;
        // `final_protocol` writes the marker only after every other graceful task has finished.
        let shutdown_marker_factory = ctx.provider_factory().clone();
        let shutdown_marker_tasks = ctx.task_executor().clone();
        let final_protocol_tasks = shutdown_marker_tasks.clone();
        shutdown_marker_tasks.spawn_critical_with_graceful_shutdown_signal(
            "shutdown final_protocol",
            move |on_graceful_shutdown| async move {
                let _guard = on_graceful_shutdown.await;
                final_protocol(final_protocol_tasks, shutdown_marker_factory).await;
            },
        );

        let ctx = ctx
            .with_metrics_task()
            // passing FullNodeTypes as type parameter here so that we can build
            // later the components.
            .with_blockchain_db::<T, _>(move |provider_factory| {
                Ok(BlockchainProvider::new(provider_factory)?)
            })?
            .with_components(components_builder, on_component_initialized)
            .await?;

        let exex_manager_handle = ExExManagerHandle::empty();

        // create pipeline
        let (consensus_engine_tx, consensus_engine_rx) = unbounded_channel();

        let node_config = ctx.node_config();

        let max_block = ctx.max_block();

        let consensus = Arc::new(ctx.components().consensus().clone());

        let pipeline = build_pipeline(
            ctx.provider_factory().clone(),
            &ctx.toml_config().stages,
            consensus.clone(),
            max_block,
            ctx.sync_metrics_tx(),
            ctx.prune_config(),
            ctx.components().evm_config().clone(),
            exex_manager_handle.clone(),
        )?;

        let pipeline_events = pipeline.events();

        let mut pruner_builder = ctx.pruner_builder();
        pruner_builder = pruner_builder.finished_exex_height(exex_manager_handle.finished_height());
        let pruner = pruner_builder.build_with_provider_factory(ctx.provider_factory().clone());
        let pruner_events = pruner.events();
        info!(target: "smartnet::cli", prune_config=?ctx.prune_config(), "Pruner initialized");

        let event_sender = EventSender::default();

        let beacon_engine_handle = ConsensusEngineHandle::new(consensus_engine_tx.clone());

        let add_ons_ctx = AddOnsContext {
            node: ctx.node_adapter().clone(),
            config: ctx.node_config(),
            beacon_engine_handle: beacon_engine_handle.clone(),
            engine_events: event_sender.clone(),
        };
        let validator_builder = add_ons.engine_validator_builder();

        // Build the engine validator with all required components
        let engine_validator = validator_builder
            .clone()
            .build_tree_validator(&add_ons_ctx, engine_tree_config.clone(), changeset_cache.clone())
            .await?;

        // Create the consensus engine stream with optional reorg
        let consensus_engine_stream = UnboundedReceiverStream::from(consensus_engine_rx)
            .maybe_skip_fcu(node_config.debug.skip_fcu)
            .maybe_skip_new_payload(node_config.debug.skip_new_payload)
            .maybe_reorg(
                ctx.blockchain_db().clone(),
                ctx.components().evm_config().clone(),
                || async {
                    // Create a separate cache for reorg validator (not shared with main engine)
                    let reorg_cache = ChangesetCache::new();
                    validator_builder
                        .build_tree_validator(&add_ons_ctx, engine_tree_config.clone(), reorg_cache)
                        .await
                },
                node_config.debug.reorg_frequency,
                node_config.debug.reorg_depth,
            )
            .await?
            // Store messages _after_ skipping so that `replay-engine` command
            // would replay only the messages that were observed by the engine
            // during this run.
            .maybe_store_messages(node_config.debug.engine_api_store.clone());

        let engine_kind = if ctx.chain_spec().is_optimism() {
            EngineApiKind::OpStack
        } else {
            EngineApiKind::Ethereum
        };

        let mut orchestrator = build_engine_orchestrator(
            engine_kind,
            consensus.clone(),
            Box::pin(consensus_engine_stream),
            pipeline,
            ctx.task_executor().clone(),
            ctx.provider_factory().clone(),
            ctx.blockchain_db().clone(),
            pruner,
            ctx.components().payload_builder_handle().clone(),
            engine_validator,
            engine_tree_config,
            ctx.sync_metrics_tx(),
            ctx.components().evm_config().clone(),
            changeset_cache,
            ctx.prune_config().changeset_window,
            Some(file_tier_base),
            ctx.task_executor().clone(),
        )?;

        info!(target: "smartnet::cli", "Consensus engine initialized");

        #[expect(clippy::needless_continue)]
        let events = stream_select!(
            event_sender.new_listener().map(Into::into),
            pipeline_events.map(Into::into),
            ctx.consensus_layer_events(),
            pruner_events.map(Into::into),
        );

        ctx.task_executor().spawn_critical_task(
            "events task",
            node::handle_events(
                Some(Box::new(ctx.components().network().clone())),
                Some(ctx.head().number),
                events,
            ),
        );

        // Engine first: the mesh (staged inside the add-ons) injects through this
        // loop in-process, so it must be running before add-ons launch anything.

        // Create engine shutdown handle
        let (engine_shutdown, shutdown_rx) = EngineShutdown::new();

        // Run consensus engine to completion
        let initial_target = ctx.initial_backfill_target()?;
        let mut built_payloads = ctx
            .components()
            .payload_builder_handle()
            .subscribe()
            .await
            .map_err(|e| eyre::eyre!("Failed to subscribe to payload builder events: {:?}", e))?
            .into_built_payload_stream()
            .fuse();

        let (exit, rx) = oneshot::channel();
        let terminate_after_backfill = ctx.terminate_after_initial_backfill();

        info!(target: "smartnet::cli", "Starting {} consensus engine", calibri_lite_consensus::CONSENSUS_NAME);
        let consensus_engine = move |mut on_graceful_shutdown| async move {
            if let Some(initial_target) = initial_target {
                debug!(target: "smartnet::cli", %initial_target,  "start backfill sync");
                orchestrator.start_backfill_sync(initial_target);
            }

            let mut res = Ok(());
            let mut shutdown_rx = shutdown_rx.fuse();

            // advance the chain and await payloads built locally to add into the engine api
            // tree handler to prevent re-execution if that block is received as payload from
            // the CL
            loop {
                tokio::select! {
                    event = orchestrator.next() => {
                        let Some(event) = event else { break };
                        debug!(target: "smartnet::cli", "Event: {event}");
                        match event {
                            ChainEvent::BackfillSyncFinished => {
                                if terminate_after_backfill {
                                    debug!(target: "smartnet::cli", "Terminating after initial backfill");
                                    break
                                }
                            }
                            ChainEvent::BackfillSyncStarted => {}
                            ChainEvent::FatalError => {
                                error!(target: "smartnet::cli", "Fatal error in consensus engine");
                                res = Err(eyre::eyre!("Fatal error in consensus engine"));
                                break
                            }
                            ChainEvent::Handler(ev) => {
                                event_sender.notify(ev);
                            }
                        }
                    }
                    payload = built_payloads.select_next_some(), if !built_payloads.is_terminated() => {
                        if let Some(executed_block) = payload.executed_block() {
                            debug!(target: "smartnet::cli", block=?executed_block.recovered_block.num_hash(),  "inserting built payload");
                            orchestrator.handler_mut().handler_mut().on_event(EngineApiRequest::InsertExecutedBlock(executed_block.into_executed_payload()).into());
                        }
                    }
                    shutdown_req = &mut shutdown_rx => {
                        if let Ok(req) = shutdown_req {
                            debug!(target: "smartnet::cli", "received engine shutdown request");
                            orchestrator.handler_mut().handler_mut().on_event(
                                FromOrchestrator::Terminate { tx: req.done_tx }.into()
                            );
                        }
                    }
                    _guard = &mut on_graceful_shutdown => {
                        // Shutdown signal received.
                        // Send Terminate so the engine OS thread can exit cleanly before we
                        // drop the orchestrator.
                        debug!(target: "smartnet::cli", "shutdown signal received, terminating engine");
                        let (done_tx, done_rx) = oneshot::channel();
                        orchestrator.handler_mut().handler_mut().on_event(
                            FromOrchestrator::Terminate { tx: done_tx }.into()
                        );
                        let _ = done_rx.await;
                        break;
                    }
                }
            }

            let _ = exit.send(res);
        };
        ctx.task_executor()
            .spawn_critical_with_graceful_shutdown_signal("consensus engine", consensus_engine);

        let RpcHandle {
            rpc_server_handles,
            rpc_registry,
            engine_events,
            beacon_engine_handle,
            engine_shutdown: _,
        } = add_ons.launch_add_ons(add_ons_ctx).await?;


        let full_node = FullNode {
            evm_config: ctx.components().evm_config().clone(),
            pool: ctx.components().pool().clone(),
            network: ctx.components().network().clone(),
            provider: ctx.node_adapter().provider.clone(),
            payload_builder_handle: ctx.components().payload_builder_handle().clone(),
            task_executor: ctx.task_executor().clone(),
            config: ctx.node_config().clone(),
            data_dir: ctx.data_dir().clone(),
            add_ons_handle: RpcHandle {
                rpc_server_handles,
                rpc_registry,
                engine_events,
                beacon_engine_handle,
                engine_shutdown,
            },
        };
        // Notify on node started
        on_node_started.on_event(FullNode::clone(&full_node))?;


        let handle = NodeHandle {
            node_exit_future: NodeExitFuture::new(async { rx.await? }),
            node: full_node,
        };

        Ok(handle)
    }
}

/// Bootstrap phase 20 — crash recovery (RECOVERY.md §4–5).
///
/// The clean-shutdown flag decides whether recovery runs; both the check and the
/// recovery must happen before anything reads the DB head (the blockchain provider,
/// and `ctx.max_block`). Delegates to [`StorageEngine::recover_on_boot`]: reconcile
/// half-finished seals, then on a cold restart run `restore_unwind(2)` (the only
/// unwind) — drop the last 2 FS lines, truncate the DB to `BX`, re-sync from
/// `BX + 1`. Fatal on failure: reconcile only fails on genuine on-disk corruption
/// (a lone `.gz.tmp` with neither its `.jsonl` source nor a sealed `.gz`), and it
/// runs before a single read or write of the file tier or the DB.
fn bootstrap_seq_20_recovery<T>(
    ctx: &LaunchContextWith<Attached<WithConfigs<T::ChainSpec>, ProviderFactory<T>>>,
    file_tier_base: &std::path::Path,
) -> eyre::Result<()>
where
    T: smartnet_provider::providers::ProviderNodeTypes,
{
    let engine = StorageEngine::new(ctx.provider_factory().clone(), None, 0);
    let bx = engine
        .recover_on_boot(file_tier_base)
        .map_err(|err| eyre::eyre!("crash recovery failed: {err}"))?;
    info!(target: "smartnet::cli", bx, "recovery complete; head at BX, re-syncing from BX + 1");
    Ok(())
}

impl<T, CB, AO> LaunchNode<NodeBuilderWithComponents<T, CB, AO>> for EngineNodeLauncher
where
    T: FullNodeTypes<
        Types: NodeTypesForProvider,
        Provider = BlockchainProvider<
            NodeTypesWithDBAdapter<<T as FullNodeTypes>::Types, <T as FullNodeTypes>::DB>,
        >,
    >,
    CB: NodeComponentsBuilder<T> + 'static,
    AO: SmartnetRpcAddOns<NodeAdapter<T, CB::Components>>
        + EngineValidatorAddOn<NodeAdapter<T, CB::Components>>
        + 'static,
    <<T::Types as NodeTypes>::Primitives as NodePrimitives>::SignedTx: SignatureAccess,
{
    type Node = NodeHandle<NodeAdapter<T, CB::Components>, AO>;
    type Future = Pin<Box<dyn Future<Output = eyre::Result<Self::Node>> + Send>>;

    fn launch_node(self, target: NodeBuilderWithComponents<T, CB, AO>) -> Self::Future {
        Box::pin(self.launch_node(target))
    }
}
