//! Ethereum Node types config.

pub use crate::{payload::EthereumPayloadBuilder, EthereumEngineValidator};
use crate::{validator::InProcValidatorIo, EthEngineTypes, EthEvmConfig};
use alloy_eips::{eip7840::BlobParams, merge::EPOCH_SLOTS};
use alloy_network::Ethereum;
use alloy_rpc_types_engine::ExecutionData;
use smartnet_eth_wire_types::BasicNetworkPrimitives;
use smartnet_ethereum_primitives::{EthPrimitives, TransactionSigned};
use smartnet_evm::{
    eth::spec::{DisableBlockRewards, EthExecutorSpec},
    ConfigureEvm, EvmFactory, EvmFactoryFor, NextBlockEnvAttributes,
};
use smartnet_network_api::noop::NoopNetwork;
use smartnet_transaction_pool::{
    blobstore::DiskFileBlobStore, EthTransactionPool, PoolPooledTx, PoolTransaction,
    TransactionPool, TransactionValidationTaskExecutor,
};
use revm::context::TxEnv;
use smartnet_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks, Hardforks};
use smartnet_engine_primitives::EngineTypes;
use smartnet_ethereum_consensus::EthBeaconConsensus;
use smartnet_engine_primitives::ethereum::{EthBuiltPayload, EthPayloadAttributes};
use smartnet_mesh::sync_gate;
use smartnet_node_api::{
    AddOnsContext, FullNodeComponents, HeaderTy, NodeAddOns, NodePrimitives, PrimitivesTy, TxTy,
};
use smartnet_node_builder::{
    components::{
        BasicPayloadServiceBuilder, ComponentsBuilder, ConsensusBuilder, ExecutorBuilder,
        NetworkBuilder, PoolBuilder, TxPoolBuilder,
    },
    node::{FullNodeTypes, NodeTypes},
    rpc::{
        BasicEngineApiBuilder, BasicEngineValidatorBuilder, Either, EngineApiBuilder,
        EngineValidatorAddOn, EngineValidatorBuilder, EthApiBuilder, EthApiCtx, Identity,
        PayloadValidatorBuilder, SmartnetAuthHttpMiddleware, SmartnetRpcAddOns, SmartnetRpcMiddleware,
        RpcAddOns, RpcHandle, Stack,
    },
    BuilderContext, Node, NodeAdapter,
};
use smartnet_payload_primitives::PayloadTypes;
use smartnet_provider::{providers::ProviderFactoryBuilder, EthStorage};
use smartnet_rpc::{
    eth::core::{EthApiFor, EthRpcConverterFor},
    ValidationApi,
};
use smartnet_rpc_api::servers::BlockSubmissionValidationApiServer;
use smartnet_rpc_builder::config::SmartnetRpcServerConfig;
use smartnet_rpc_eth_api::{
    helpers::{
        config::{EthConfigApiServer, EthConfigHandler},
        pending_block::BuildPendingEnv,
    },
    EthApiServer, FullEthApiServer, RpcConvert, RpcTypes, SignableTxRequest,
};
use smartnet_rpc_eth_types::{error::FromEvmError, EthApiError};
use smartnet_rpc_server_types::SmartnetRpcModule;
use smartnet_tracing::tracing::{debug, info, warn};
use smartnet_validator::ValidatorDaemon;
use std::{
    marker::PhantomData,
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

/// Type configuration for a regular Ethereum node.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct EthereumNode;

impl EthereumNode {
    /// Returns a [`ComponentsBuilder`] configured for a regular Ethereum node.
    pub fn components<Node>() -> ComponentsBuilder<
        Node,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        EthereumExecutorBuilder,
        EthereumConsensusBuilder,
    >
    where
        Node: FullNodeTypes<
            Types: NodeTypes<
                ChainSpec: Hardforks + EthereumHardforks + EthExecutorSpec + DisableBlockRewards,
                Primitives = EthPrimitives,
            >,
        >,
        <Node::Types as NodeTypes>::Payload:
            PayloadTypes<BuiltPayload = EthBuiltPayload, PayloadAttributes = EthPayloadAttributes>,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(EthereumPoolBuilder::default())
            .executor(EthereumExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::default())
            .network(EthereumNetworkBuilder::default())
            .consensus(EthereumConsensusBuilder::default())
    }

    /// Instantiates the [`ProviderFactoryBuilder`] for an ethereum node.
    ///
    /// # Open a Providerfactory in read-only mode from a datadir
    ///
    /// See also: [`ProviderFactoryBuilder`] and
    /// [`ReadOnlyConfig`](smartnet_provider::providers::ReadOnlyConfig).
    ///
    /// ```no_run
    /// use smartnet_chainspec::MAINNET;
    /// use smartnet_node_ethereum::EthereumNode;
    ///
    /// fn demo(runtime: smartnet_tasks::Runtime) {
    ///     let factory = EthereumNode::provider_factory_builder()
    ///         .open_read_only(MAINNET.clone(), "datadir", runtime)
    ///         .unwrap();
    /// }
    /// ```
    ///
    /// See also [`ProviderFactory::new`](smartnet_provider::ProviderFactory::new) for constructing
    /// a [`ProviderFactory`](smartnet_provider::ProviderFactory) manually with all required
    /// components.
    pub fn provider_factory_builder() -> ProviderFactoryBuilder<Self> {
        ProviderFactoryBuilder::default()
    }
}

impl NodeTypes for EthereumNode {
    type Primitives = EthPrimitives;
    type ChainSpec = ChainSpec;
    type Storage = EthStorage;
    type Payload = EthEngineTypes;
}

/// Builds [`EthApi`](smartnet_rpc::EthApi) for Ethereum.
#[derive(Debug)]
pub struct EthereumEthApiBuilder<NetworkT = Ethereum>(PhantomData<NetworkT>);

impl<NetworkT> Default for EthereumEthApiBuilder<NetworkT> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<N, NetworkT> EthApiBuilder<N> for EthereumEthApiBuilder<NetworkT>
where
    N: FullNodeComponents<
        Types: NodeTypes<ChainSpec: Hardforks + EthereumHardforks>,
        Evm: ConfigureEvm<NextBlockEnvCtx: BuildPendingEnv<HeaderTy<N::Types>>>,
    >,
    NetworkT: RpcTypes<TransactionRequest: SignableTxRequest<TxTy<N::Types>>>,
    EthRpcConverterFor<N, NetworkT>: RpcConvert<
        Primitives = PrimitivesTy<N::Types>,
        Error = EthApiError,
        Network = NetworkT,
        Evm = N::Evm,
    >,
    EthApiError: FromEvmError<N::Evm>,
{
    type EthApi = EthApiFor<N, NetworkT>;

    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        Ok(ctx.eth_api_builder().map_converter(|r| r.with_network()).build())
    }
}

/// Add-ons w.r.t. l1 ethereum.
#[derive(Debug)]
pub struct EthereumAddOns<
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
    PVB,
    EB = BasicEngineApiBuilder<PVB>,
    EVB = BasicEngineValidatorBuilder<PVB>,
    RpcMiddleware = Identity,
    AuthHttpMiddleware = Identity,
> {
    inner: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware>,
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware>
    EthereumAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
{
    /// Creates a new instance from the inner `RpcAddOns`.
    pub const fn new(
        inner: RpcAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware>,
    ) -> Self {
        Self { inner }
    }
}

impl<N> Default for EthereumAddOns<N, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: EthereumHardforks + Clone + 'static,
            Payload: EngineTypes<ExecutionData = ExecutionData>
                         + PayloadTypes<PayloadAttributes = EthPayloadAttributes>,
            Primitives = EthPrimitives,
        >,
    >,
    EthereumEthApiBuilder: EthApiBuilder<N>,
{
    fn default() -> Self {
        Self::new(RpcAddOns::new(
            EthereumEthApiBuilder::default(),
            EthereumEngineValidatorBuilder::default(),
            BasicEngineApiBuilder::default(),
            BasicEngineValidatorBuilder::default(),
            Default::default(),
            Identity::new(),
        ))
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware>
    EthereumAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware>
where
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
{
    /// Replace the engine API builder.
    pub fn with_engine_api<T>(
        self,
        engine_api_builder: T,
    ) -> EthereumAddOns<N, EthB, PVB, T, EVB, RpcMiddleware, AuthHttpMiddleware>
    where
        T: Send,
    {
        let Self { inner } = self;
        EthereumAddOns::new(inner.with_engine_api(engine_api_builder))
    }

    /// Replace the payload validator builder.
    pub fn with_payload_validator<V, T>(
        self,
        payload_validator_builder: T,
    ) -> EthereumAddOns<N, EthB, T, EB, EVB, RpcMiddleware, AuthHttpMiddleware> {
        let Self { inner } = self;
        EthereumAddOns::new(inner.with_payload_validator(payload_validator_builder))
    }

    /// Sets rpc middleware
    pub fn with_rpc_middleware<T>(
        self,
        rpc_middleware: T,
    ) -> EthereumAddOns<N, EthB, PVB, EB, EVB, T, AuthHttpMiddleware>
    where
        T: Send,
    {
        let Self { inner } = self;
        EthereumAddOns::new(inner.with_rpc_middleware(rpc_middleware))
    }

    /// Configures the HTTP transport middleware for the auth / Engine API server.
    pub fn with_auth_http_middleware<T>(
        self,
        auth_http_middleware: T,
    ) -> EthereumAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware, T>
    where
        T: Send,
    {
        let Self { inner } = self;
        EthereumAddOns::new(inner.with_auth_http_middleware(auth_http_middleware))
    }

    /// Stacks an additional HTTP transport middleware layer for the auth / Engine API server.
    pub fn layer_auth_http_middleware<T>(
        self,
        layer: T,
    ) -> EthereumAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware, Stack<AuthHttpMiddleware, T>> {
        let Self { inner } = self;
        EthereumAddOns::new(inner.layer_auth_http_middleware(layer))
    }

    /// Conditionally stacks an HTTP transport middleware layer for the auth / Engine API server.
    #[expect(clippy::type_complexity)]
    pub fn option_layer_auth_http_middleware<T>(
        self,
        layer: Option<T>,
    ) -> EthereumAddOns<
        N,
        EthB,
        PVB,
        EB,
        EVB,
        RpcMiddleware,
        Stack<AuthHttpMiddleware, Either<T, Identity>>,
    > {
        let Self { inner } = self;
        EthereumAddOns::new(inner.option_layer_auth_http_middleware(layer))
    }

    /// Sets the tokio runtime for the RPC servers.
    ///
    /// Caution: This runtime must not be created from within asynchronous context.
    pub fn with_tokio_runtime(self, tokio_runtime: Option<tokio::runtime::Handle>) -> Self {
        let Self { inner } = self;
        Self { inner: inner.with_tokio_runtime(tokio_runtime) }
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware> NodeAddOns<N>
    for EthereumAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: Hardforks + EthereumHardforks,
            Primitives = EthPrimitives,
            Payload: EngineTypes<ExecutionData = ExecutionData>,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    >,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EB::EngineApi: Clone,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
    RpcMiddleware: SmartnetRpcMiddleware,
    AuthHttpMiddleware: SmartnetAuthHttpMiddleware<Identity>,
    EthB::EthApi: FullEthApiServer<NetworkTypes = Ethereum, Primitives = EthPrimitives>
        + Send
        + Sync
        + 'static,
{
    type Handle = RpcHandle<N, EthB::EthApi>;

    async fn launch_add_ons(
        self,
        ctx: smartnet_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        // RPC catch-up service REMOVED (user-in.md #5): node-to-node sync is mesh-only.
        // `RPC_SYNC`, `SYNC_PEERS`, `SYNC_BATCH` and `SYNC_POLL_INTERVAL` are gone from
        // the schema — a leftover key now fails the boot (fail-closed), and there is no
        // catch-up loop to start. Block acquisition is mesh gossip + RLP import.

        // Validator heartbeat cadence (`HEARTBEAT_MS`). The validator section is still
        // the on/off switch: a malformed validator section aborts boot (fail-closed),
        // exactly as before. The mesh needs no heartbeat — each peer keeps its own
        // persistent link with PING/PONG keepalive and reconnect backoff.
        //
        // The launch flag is checked first: `smartnet import` clears it, so the import
        // run never arms the validator — no heartbeat task, no seal pass, and the
        // config's validator section is not even read (import and the sync path both
        // feed the engine directly; sealing belongs to a live producing node only).
        let config_path = ctx.config.config.clone();
        let validator_enabled = ctx.config.validator_daemon &&
            match config_path.as_deref() {
                Some(path) => smartnet_validator::validator_daemon_enabled(path)?,
                None => false,
            };
        let heartbeat = Duration::from_millis(
            ctx.config.rpc.heartbeat_ms.unwrap_or(smartnet_validator::DEFAULT_HEARTBEAT_MS),
        );
        let chain_id = ctx.config.chain.chain_id();
        let task_executor = ctx.node.task_executor().clone();

        let validation_api = ValidationApi::<_, _, <N::Types as NodeTypes>::Payload>::new(
            ctx.node.provider().clone(),
            Arc::new(ctx.node.consensus().clone()),
            ctx.node.evm_config().clone(),
            ctx.config.rpc.flashbots_config(),
            ctx.node.task_executor().clone(),
            Arc::new(EthereumEngineValidator::new(ctx.config.chain.clone())),
        );

        let eth_config =
            EthConfigHandler::new(ctx.node.provider().clone(), ctx.node.evm_config().clone());

        // Stage the RPC modules WITHOUT binding the transport: the HTTP/WS listener
        // comes up only after the mesh sync gate passes or times out (engine → mesh →
        // sync → rumor → RPC). A timeout is fail-open, but boot catch-up is then
        // reported as unconfirmed — RPC availability alone is not sync readiness.
        let staged = self
            .inner
            .prepare_add_ons_with(ctx.clone(), move |container| {
                container.modules.merge_if_module_configured(
                    SmartnetRpcModule::Flashbots,
                    validation_api.into_rpc(),
                )?;

                container
                    .modules
                    .merge_if_module_configured(SmartnetRpcModule::Eth, eth_config.into_rpc())?;

                // testing_buildBlockV1 is intentionally not wired here:
                // `SmartnetRpcModule::Testing` is hard-blocked by the production
                // allowlist (`SmartnetRpcModule::is_allowed`), so
                // `create_transport_rpc_modules` filters it out of every transport selection before
                // `modules.config` is stored. A
                // `merge_if_module_configured(SmartnetRpcModule::Testing, …)` would therefore
                // always be a no-op — the gated module can never be "configured".
                // Wiring `testing_buildBlockV1` again requires an explicit decision
                // to relax that allowlist, not dead plumbing that looks like a
                // working opt-in.

                Ok(())
            })
            .await?;

        // The node's own RPC objects, once the modules exist: tx inject goes through the
        // live EthApi, block inject through the engine — in-process, no loopback socket.
        let eth_api = staged.eth_api().clone();

        // Mesh service: closed-list all-to-all gossip (txs + blocks over the binary
        // WebSocket). Bound and preconnected inside — every peer is dialed now (links
        // used to spawn lazily on first spread, so a quiet chain never caught up at
        // boot), and the engine loop is already running so those in-process injects are
        // answered. The gate receiver below is how boot learns catch-up finished.
        let mesh = crate::mesh::spawn_mesh_service(&ctx, eth_api.clone()).await?;
        let mesh_enabled = mesh.enabled;
        // The status line's "X nodes connected / boot catch-up" readings — taken here
        // because `mesh.ready` is moved into the sync-gate wait just below; `eth_api`
        // and `boot_sync` move into the heartbeat task further down, so their handles
        // are taken here too.
        let status_spreader = mesh.spreader.clone();
        let status_api = eth_api.clone();

        // Sync gate: wait until every peer link completed a bootstrap exchange, the
        // local tip has reached the highest reported peer tip, and no ordered pull is
        // armed — or the boot timeout fires (warn and proceed: a dead peer must not
        // brick the RPC). The boot result is carried into the heartbeat as `boot_sync`;
        // a timeout stays unconfirmed and is never reported as full synchronization.
        let boot_sync = if sync_gate::wait_ready(mesh.ready, sync_gate::BOOT_READY_TIMEOUT).await {
            info!(
                target: "smartnet::mesh",
                boot_sync = "confirmed",
                "mesh sync gate passed — boot catch-up confirmed; binding RPC transport"
            );
            "confirmed"
        } else {
            warn!(
                target: "smartnet::mesh",
                boot_sync = "unconfirmed",
                "mesh sync gate timed out — serving RPC with unconfirmed catch-up; not fully synced"
            );
            "unconfirmed"
        };

        // Fully up: bind the HTTP/WS transport, then start the heartbeat.
        let handle = staged.serve().await?;
        let engine = handle.beacon_engine_handle.clone();

        // Validator heartbeat. On each beat: one validator seal pass (only when
        // `SEAL = true`). The task is critical but a failed beat is logged and retried —
        // it never takes the node down.
        // ONE heartbeat per node (user-in #1): the task is unconditional — the mesh,
        // the validator and the node duties all re-use this single interval, and the
        // 10-second status line lives on it.
        task_executor.spawn_critical_task("node-heartbeat", async move {
                let validator = if validator_enabled {
                    match config_path {
                        Some(path) => {
                            let io = InProcValidatorIo::new(eth_api, engine, chain_id);
                            match ValidatorDaemon::load(path, chain_id, io).await {
                                Ok(validator) => validator,
                                Err(err) => {
                                    warn!(
                                        target: "smartnet_validator",
                                        %err,
                                        "merged validator disabled for this run — no seal \
                                         beats this boot"
                                    );
                                    None
                                }
                            }
                        }
                        None => None,
                    }
                } else {
                    None
                };

                info!(
                    target: "smartnet::heartbeat",
                    interval_ms = heartbeat.as_millis() as u64,
                    validator = validator.is_some(),
                    mesh = mesh_enabled,
                    "node heartbeat started (single multi-purpose HB)"
                );

                let mut tick = tokio::time::interval(heartbeat);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // The second heartbeat (user-in #1b): ONE standard line per
                // `REPORT_INTERVAL` — `blockNr - X nodes connected - boot catch-up
                // confirmed|unconfirmed`. Nothing else is throttled: every other event
                // prints as emitted (the verbosity gate is gone, operator order
                // 2026-11-18). While an import streams it prints its own line instead.
                let mut status_at: Option<Instant> = None;
                loop {
                    tick.tick().await;
                    if let Some(validator) = validator.as_ref() {
                        validator.beat().await;
                    }
                    if status_at.is_none_or(|at| at.elapsed() >= smartnet_tracing::REPORT_INTERVAL)
                        && !crate::import::import_active()
                    {
                        status_at = Some(Instant::now());
                        let nodes = status_spreader.connected_count();
                        let tip = EthApiServer::block_number(&status_api)
                            .ok()
                            .and_then(|nr| u64::try_from(nr).ok())
                            .map_or_else(|| "?".to_string(), |nr| nr.to_string());
                        info!(
                            target: smartnet_tracing::STATUS_TARGET,
                            block = %tip,
                            nodes,
                            boot_sync,
                            "blockNr = {tip} - {nodes} nodes connected (boot catch-up {boot_sync})"
                        );
                    }
                }
            });

        Ok(handle)
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware> SmartnetRpcAddOns<N>
    for EthereumAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: Hardforks + EthereumHardforks,
            Primitives = EthPrimitives,
            Payload: EngineTypes<ExecutionData = ExecutionData>,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    >,
    EthB: EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N>,
    EB: EngineApiBuilder<N>,
    EB::EngineApi: Clone,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
    RpcMiddleware: SmartnetRpcMiddleware,
    AuthHttpMiddleware: SmartnetAuthHttpMiddleware<Identity>,
    EthB::EthApi: FullEthApiServer<NetworkTypes = Ethereum, Primitives = EthPrimitives>
        + Send
        + Sync
        + 'static,
{
    type EthApi = EthB::EthApi;

    fn hooks_mut(&mut self) -> &mut smartnet_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.inner.hooks_mut()
    }
}

impl<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware> EngineValidatorAddOn<N>
    for EthereumAddOns<N, EthB, PVB, EB, EVB, RpcMiddleware, AuthHttpMiddleware>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec: EthChainSpec + EthereumHardforks,
            Primitives = EthPrimitives,
            Payload: EngineTypes<ExecutionData = ExecutionData>,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    >,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EB: EngineApiBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthApiError: FromEvmError<N::Evm>,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = TxEnv>,
    RpcMiddleware: Send,
    AuthHttpMiddleware: Send,
    EthB::EthApi: FullEthApiServer<NetworkTypes = Ethereum, Primitives = EthPrimitives>
        + Send
        + Sync
        + 'static,
{
    type ValidatorBuilder = EVB;

    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        self.inner.engine_validator_builder()
    }
}

impl<N> Node<N> for EthereumNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        EthereumPoolBuilder,
        BasicPayloadServiceBuilder<EthereumPayloadBuilder>,
        EthereumNetworkBuilder,
        EthereumExecutorBuilder,
        EthereumConsensusBuilder,
    >;

    type AddOns =
        EthereumAddOns<NodeAdapter<N>, EthereumEthApiBuilder, EthereumEngineValidatorBuilder>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        Self::components()
    }

    fn add_ons(&self) -> Self::AddOns {
        EthereumAddOns::default()
    }
}

/// A regular ethereum evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct EthereumExecutorBuilder;

impl<Types, Node> ExecutorBuilder<Node> for EthereumExecutorBuilder
where
    Types: NodeTypes<
        ChainSpec: Hardforks + EthExecutorSpec + EthereumHardforks + DisableBlockRewards,
        Primitives = EthPrimitives,
    >,
    Node: FullNodeTypes<Types = Types>,
{
    type EVM = EthEvmConfig<Types::ChainSpec>;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        Ok(EthEvmConfig::new(ctx.chain_spec()))
    }
}

/// A basic ethereum transaction pool.
///
/// This contains various settings that can be configured and take precedence over the node's
/// config.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct EthereumPoolBuilder {
    // TODO add options for txpool args
}

impl<Types, Node, Evm> PoolBuilder<Node, Evm> for EthereumPoolBuilder
where
    Types: NodeTypes<
        ChainSpec: EthereumHardforks,
        Primitives: NodePrimitives<SignedTx = TransactionSigned>,
    >,
    Node: FullNodeTypes<Types = Types>,
    Evm: ConfigureEvm<Primitives = PrimitivesTy<Types>> + Clone + 'static,
{
    type Pool = EthTransactionPool<Node::Provider, DiskFileBlobStore, Evm>;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: Evm,
    ) -> eyre::Result<Self::Pool> {
        let pool_config = ctx.pool_config();

        // This build has no KZG verifier, so blob transactions can never be accepted.
        let blobs_disabled = true;

        let blob_cache_size = if let Some(blob_cache_size) = pool_config.blob_cache_size {
            Some(blob_cache_size)
        } else {
            // get the current blob params for the current timestamp, fallback to default Cancun
            // params
            let current_timestamp =
                SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
            let blob_params = ctx
                .chain_spec()
                .blob_params_at_timestamp(current_timestamp)
                .unwrap_or_else(BlobParams::cancun);

            // Derive the blob cache size from the target blob count, to auto scale it by
            // multiplying it with the slot count for 2 epochs: 384 for pectra
            Some((blob_params.target_blob_count * EPOCH_SLOTS * 2) as u32)
        };

        let blob_store =
            smartnet_node_builder::components::create_blob_store_with_cache(ctx, blob_cache_size)?;

        let validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .set_eip4844(!blobs_disabled)
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .with_local_transactions_config(pool_config.local_transactions_config.clone())
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
                .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());

        let transaction_pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build_and_spawn_maintenance_task(blob_store, pool_config)?;

        info!(target: "smartnet::cli", "Transaction pool initialized");
        debug!(target: "smartnet::cli", "Spawned txpool maintenance task");

        Ok(transaction_pool)
    }
}

/// Network component for `SmartNet`: inert (devp2p/kademlia removed, mesh carries gossip).
#[derive(Debug, Default, Clone, Copy)]
pub struct EthereumNetworkBuilder {
    // TODO add closure to modify network
}

impl<Node, Pool> NetworkBuilder<Node, Pool> for EthereumNetworkBuilder
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec: Hardforks>>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TxTy<Node::Types>>>
        + Unpin
        + 'static,
{
    type Network =
        NoopNetwork<BasicNetworkPrimitives<PrimitivesTy<Node::Types>, PoolPooledTx<Pool>>>;

    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        _pool: Pool,
    ) -> eyre::Result<Self::Network> {
        // Devp2p/kademlia neutralized (p2p2mesh.md Stage 2): no discovery, no 30303
        // listener, no "P2P networking initialized". Gossip runs on the mesh
        // (smartnet-mesh) and block acquisition on rpc-sync; this component stays an
        // inert Noop so FullNodeComponents, net_* RPC, events and the pipeline
        // wiring keep compiling until Stage 3 detaches them for good.
        debug!(
            target: "smartnet::cli",
            chain_id = ctx.chain_spec().chain_id(),
            "network component is noop; gossip runs on the mesh"
        );
        Ok(NoopNetwork::new().with_chain_id(ctx.chain_spec().chain_id()))
    }
}

/// A basic ethereum consensus builder.
#[derive(Debug, Default, Clone, Copy)]
pub struct EthereumConsensusBuilder {
    // TODO add closure to modify consensus
}

impl<Node> ConsensusBuilder<Node> for EthereumConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<ChainSpec: EthChainSpec + EthereumHardforks, Primitives = EthPrimitives>,
    >,
{
    type Consensus = Arc<EthBeaconConsensus<<Node::Types as NodeTypes>::ChainSpec>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        // The registry path is operator data: read it from `VAL_FILE` in the same config.toml
        // the node booted with. No directory is inferred from HOME or the binary location.
        let val_file = match ctx.config().config.as_deref() {
            Some(config_path) => Some(crate::validator_registry::load_val_file(config_path)?),
            None => None,
        };
        build_eth_consensus(ctx.chain_spec(), val_file.as_deref())
    }
}

/// Builder for [`EthereumEngineValidator`].
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct EthereumEngineValidatorBuilder;

impl<Node, Types> PayloadValidatorBuilder<Node> for EthereumEngineValidatorBuilder
where
    Types: NodeTypes<
        ChainSpec: Hardforks + EthereumHardforks + Clone + 'static,
        Payload: EngineTypes<ExecutionData = ExecutionData>
                     + PayloadTypes<PayloadAttributes = EthPayloadAttributes>,
        Primitives = EthPrimitives,
    >,
    Node: FullNodeComponents<Types = Types>,
{
    type Validator = EthereumEngineValidator<Types::ChainSpec>;

    async fn build(self, ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(EthereumEngineValidator::new(ctx.config.chain.clone()))
    }
}

/// Builds the node's consensus instance from the chain spec — the ONE place the
/// Calibri-Lite runtime profile is applied (max extraData, the profile detected from
/// the genesis, the London/Paris shape warnings).
///
/// `val_file` is the registry path from the operator's `VAL_FILE` key. The key may be absent
/// or blank — the node-owned scaffold then looks in the launch directory and under
/// `HOME_DIR_PATH` and errors if neither holds the file. A keyed path that cannot be read is
/// also a hard boot error. `None` is only for in-process test builders that carry no config
/// file; it leaves the gate unloaded.
pub fn build_eth_consensus<ChainSpec>(
    chain_spec: Arc<ChainSpec>,
    val_file: Option<&Path>,
) -> eyre::Result<Arc<EthBeaconConsensus<ChainSpec>>>
where
    ChainSpec: EthChainSpec + EthereumHardforks + core::fmt::Debug + Send + Sync + 'static,
{
    // The node-owned registry (statuses included) is the authority the gate answers from —
    // loaded ONCE at bootstrap from `VAL_FILE` so every block this node sees is gate-checked.
    // A configured registry that cannot be read is a hard boot error; the node must not run
    // with the seal gate silently unloaded.
    if let Some(registry_path) = val_file {
        crate::validator_registry::load_validators(registry_path).map_err(|err| {
            eyre::eyre!(
                "validator registry unreadable ({}) : {err} — refusing to start",
                registry_path.display()
            )
        })?;
        // The bootstrap guard already loaded and labelled the registry; this load replaces
        // the RAM array, so re-apply the inert self-label.
        if let Some(address) = crate::bootstrap_files::self_address() {
            calibri_lite_consensus::registry::mark_self(address);
        }
    }

    let mut consensus = EthBeaconConsensus::new(chain_spec.clone());
    let genesis_cfg = &chain_spec.genesis().config;
    // The Calibri-Lite consensus setup keys on the genesis carrying a validator section in
    // `extraData` (the legacy `config.clique` object is inert; see `GENESIS-LEGACY-CONFIG.md`).
    if smartnet_chainspec::is_smartnet_genesis(chain_spec.genesis()) {
        // No hard-coded Istanbul→London boundary: wallet/EVM compatibility (type-0/type-2) is
        // uniform, and import/sync picks the EVM spec from each header's own shape.
        if genesis_cfg.terminal_total_difficulty.is_some() {
            warn!(
                target: "smartnet::cli",
                event = "CALIBRI_LITE_GENESIS_MERGE_FIELDS",
                "Genesis includes Merge-era fields; Calibri Lite runtime profile skips merge header checks — remove terminalTotalDifficulty* for a clean genesis if desired"
            );
        }
        consensus =
            consensus.with_max_extra_data_size(smartnet_chainspec::CALIBRI_LITE_MAX_EXTRA_DATA_SIZE);
        let profile = calibri_lite_consensus::SmartNetRuntimeProfile::detect(
            chain_spec.chain().id(),
            chain_spec.genesis(),
        );
        if let Some(profile) = profile {
            consensus = consensus.with_smartnet_runtime_profile(profile);
        }
    }

    Ok(Arc::new(consensus))
}
