//! Configure `SmartNet` RPC.
//!
//! This crate contains several builder and config types that allow to configure the selection of
//! [`SmartnetRpcModule`] per transport. **RPC = HTTP, nothing else** (operator ruling,
//! 2026-11-19): JSON-RPC is served over HTTP only; `WebSocket` carries the mesh binary
//! protocol (`doc/MESH-PAYLOAD-PROTOCOL.md`, RLP frames) and never RPC. The node never
//! mounts WS as an RPC transport (`config.rs`), and the `ws` plumbing below is legacy.
//!
//! The [`RpcModuleBuilder`] is the main entrypoint for configuring all RPC modules. It takes
//! instances of components required to start the servers, such as provider impls, network and
//! transaction pool. [`RpcModuleBuilder::build`] returns a [`TransportRpcModules`] which contains
//! the transport specific config (what APIs are available via this transport).
//!
//! The [`RpcServerConfig`] is used to assemble and start the HTTP server (the ws machinery
//! is legacy, see above), it requires the [`TransportRpcModules`] so it can start the server
//! with the configured modules.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

use crate::{
    engine_rpc_module::AuthRpcModule, error::WsHttpSamePortError, metrics::RpcRequestMetrics,
};
use alloy_network::{Ethereum, IntoWallet};
use alloy_provider::{fillers::RecommendedFillers, Provider, ProviderBuilder};
use core::marker::PhantomData;
use error::{ConflictingModules, RpcError, ServerKind};
use jsonrpsee::{
    core::RegisterMethodError,
    server::{middleware::rpc::RpcServiceBuilder, AlreadyStoppedError, IdProvider, ServerHandle},
    Methods, RpcModule,
};
use smartnet_evm::ConfigureEvm;
use smartnet_network_api::{noop::NoopNetwork, NetworkInfo, Peers};
use smartnet_primitives_traits::{NodePrimitives, TxTy};
use smartnet_transaction_pool::{noop::NoopTransactionPool, TransactionPool};
use serde::{Deserialize, Serialize};
use smartnet_chainspec::{ChainSpecProvider, EthereumHardforks};
use smartnet_consensus::FullConsensus;
use smartnet_engine_primitives::{ConsensusEngineEvent, ConsensusEngineHandle};
use smartnet_payload_primitives::PayloadTypes;
use smartnet_rpc::{
    AdminApi, DebugApi, EngineEthApi, EthApi, EthApiBuilder, EthBundle, MinerApi, NetApi,
    OperatorExtensionApi, OtterscanApi, RPCApi, TraceApi, TxPoolApi, Web3Api,
};
use smartnet_rpc_api::servers::*;
use smartnet_rpc_engine_api::merge_engine_extension_rpc;
use smartnet_rpc_eth_api::{
    helpers::{
        pending_block::PendingEnvBuilder, Call, EthApiSpec, EthTransactions, LoadPendingBlock,
    },
    node::RpcNodeCoreAdapter,
    EthApiServer, EthApiTypes, FullEthApiServer, FullEthApiTypes, RpcBlock, RpcConvert,
    RpcConverter, RpcHeader, RpcNodeCore, RpcReceipt, RpcTransaction, RpcTxReq,
};
use smartnet_rpc_eth_types::{receipt::EthReceiptConverter, EthConfig, EthSubscriptionIdProvider};
pub use smartnet_rpc_server_types::SmartnetRpcModule;
use smartnet_storage_api::{
    AccountReader, BlockReader, ChangeSetReader, FullRpcProvider, NodePrimitivesProvider,
    StateProviderFactory,
};
use smartnet_tasks::{pool::BlockingTaskGuard, Runtime};
use smartnet_tasks::EventSender;
use std::{
    collections::HashMap,
    fmt::Debug,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::PathBuf,
};
use tower_http::cors::CorsLayer;

pub use cors::CorsDomainError;

// re-export for convenience
pub use jsonrpsee::server::ServerBuilder;
use jsonrpsee::server::ServerConfigBuilder;
pub use smartnet_rpc_server_types::{constants, RpcModuleSelection};
pub use tower::layer::util::{Identity, Stack};

/// Merged engine RPC module + noop handle (SmartNet serves `engine_*` on the unified
/// loopback JSON-RPC transport — there is no separate authrpc listener and no JWT).
pub mod engine_rpc_module;

/// RPC server utilities.
pub mod config;

/// The operator's RPC batch admission gate (class budgets; better-than-deny).
pub mod batch_policy;
/// Utils for installing Rpc middleware.
pub mod middleware;

/// Cors utilities.
mod compression_layer;
pub use compression_layer::CompressionLayer;

mod cors;

/// Rpc error utilities.
pub mod error;

/// Eth utils
pub mod eth;
pub use eth::EthHandlers;

// Rpc server metrics
mod metrics;
use crate::middleware::SmartnetRpcMiddleware;
pub use metrics::{MeteredBatchRequestsFuture, MeteredRequestFuture, RpcRequestMetricsService};
use smartnet_chain_state::{
    CanonStateSubscriptions, ForkChoiceSubscriptions, PersistedBlockSubscriptions,
};
use smartnet_rpc::eth::sim_bundle::EthSimBundle;

/// A builder type to configure the RPC module: See [`RpcModule`]
///
/// This is the main entrypoint and the easiest way to configure an RPC server.
#[derive(Debug, Clone)]
pub struct RpcModuleBuilder<N, Provider, Pool, Network, EvmConfig, Consensus> {
    /// The Provider type to when creating all rpc handlers
    provider: Provider,
    /// The Pool type to when creating all rpc handlers
    pool: Pool,
    /// The Network type to when creating all rpc handlers
    network: Network,
    /// How additional tasks are spawned, for example in the eth pubsub namespace
    executor: Option<Runtime>,
    /// Defines how the EVM should be configured before execution.
    evm_config: EvmConfig,
    /// The consensus implementation.
    consensus: Consensus,
    /// Node data primitives.
    _primitives: PhantomData<N>,
}

// === impl RpcBuilder ===

impl<N, Provider, Pool, Network, EvmConfig, Consensus>
    RpcModuleBuilder<N, Provider, Pool, Network, EvmConfig, Consensus>
{
    /// Create a new instance of the builder
    pub const fn new(
        provider: Provider,
        pool: Pool,
        network: Network,
        executor: Runtime,
        evm_config: EvmConfig,
        consensus: Consensus,
    ) -> Self {
        Self {
            provider,
            pool,
            network,
            executor: Some(executor),
            evm_config,
            consensus,
            _primitives: PhantomData,
        }
    }

    /// Configure the provider instance.
    pub fn with_provider<P>(
        self,
        provider: P,
    ) -> RpcModuleBuilder<N, P, Pool, Network, EvmConfig, Consensus> {
        let Self { pool, network, executor, evm_config, consensus, _primitives, .. } = self;
        RpcModuleBuilder { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Configure the transaction pool instance.
    pub fn with_pool<P>(
        self,
        pool: P,
    ) -> RpcModuleBuilder<N, Provider, P, Network, EvmConfig, Consensus> {
        let Self { provider, network, executor, evm_config, consensus, _primitives, .. } = self;
        RpcModuleBuilder { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Configure a [`NoopTransactionPool`] instance.
    ///
    /// Caution: This will configure a pool API that does absolutely nothing.
    /// This is only intended for allow easier setup of namespaces that depend on the
    /// [`EthApi`] which requires a [`TransactionPool`] implementation.
    pub fn with_noop_pool(
        self,
    ) -> RpcModuleBuilder<N, Provider, NoopTransactionPool, Network, EvmConfig, Consensus> {
        let Self { provider, executor, network, evm_config, consensus, _primitives, .. } = self;
        RpcModuleBuilder {
            provider,
            executor,
            network,
            evm_config,
            pool: NoopTransactionPool::default(),
            consensus,
            _primitives,
        }
    }

    /// Configure the network instance.
    pub fn with_network<Net>(
        self,
        network: Net,
    ) -> RpcModuleBuilder<N, Provider, Pool, Net, EvmConfig, Consensus> {
        let Self { provider, pool, executor, evm_config, consensus, _primitives, .. } = self;
        RpcModuleBuilder { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Configure a [`NoopNetwork`] instance.
    ///
    /// Caution: This will configure a network API that does absolutely nothing.
    /// This is only intended for allow easier setup of namespaces that depend on the
    /// [`EthApi`] which requires a [`NetworkInfo`] implementation.
    pub fn with_noop_network(
        self,
    ) -> RpcModuleBuilder<N, Provider, Pool, NoopNetwork, EvmConfig, Consensus> {
        let Self { provider, pool, executor, evm_config, consensus, _primitives, .. } = self;
        RpcModuleBuilder {
            provider,
            pool,
            executor,
            network: NoopNetwork::default(),
            evm_config,
            consensus,
            _primitives,
        }
    }

    /// Configure the task executor to use for additional tasks.
    pub fn with_executor(self, executor: Runtime) -> Self {
        let Self { pool, network, provider, evm_config, consensus, _primitives, .. } = self;
        Self {
            provider,
            network,
            pool,
            executor: Some(executor),
            evm_config,
            consensus,
            _primitives,
        }
    }

    /// Configure the evm configuration type
    pub fn with_evm_config<E>(
        self,
        evm_config: E,
    ) -> RpcModuleBuilder<N, Provider, Pool, Network, E, Consensus> {
        let Self { provider, pool, executor, network, consensus, _primitives, .. } = self;
        RpcModuleBuilder { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Configure the consensus implementation.
    pub fn with_consensus<C>(
        self,
        consensus: C,
    ) -> RpcModuleBuilder<N, Provider, Pool, Network, EvmConfig, C> {
        let Self { provider, network, pool, executor, evm_config, _primitives, .. } = self;
        RpcModuleBuilder { provider, network, pool, executor, evm_config, consensus, _primitives }
    }

    /// Instantiates a new [`EthApiBuilder`] from the configured components.
    #[expect(clippy::type_complexity)]
    pub fn eth_api_builder<ChainSpec>(
        &self,
    ) -> EthApiBuilder<
        RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>,
        RpcConverter<Ethereum, EvmConfig, EthReceiptConverter<ChainSpec>>,
    >
    where
        Provider: Clone,
        Pool: Clone,
        Network: Clone,
        EvmConfig: Clone,
        RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>:
            RpcNodeCore<Provider: ChainSpecProvider<ChainSpec = ChainSpec>, Evm = EvmConfig>,
    {
        EthApiBuilder::new(
            self.provider.clone(),
            self.pool.clone(),
            self.network.clone(),
            self.evm_config.clone(),
        )
    }

    /// Initializes a new [`EthApiServer`] with the configured components and default settings.
    ///
    /// Note: This spawns all necessary tasks.
    ///
    /// See also [`EthApiBuilder`].
    #[expect(clippy::type_complexity)]
    pub fn bootstrap_eth_api<ChainSpec>(
        &self,
    ) -> EthApi<
        RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>,
        RpcConverter<Ethereum, EvmConfig, EthReceiptConverter<ChainSpec>>,
    >
    where
        Provider: Clone,
        Pool: Clone,
        Network: Clone,
        EvmConfig: ConfigureEvm + Clone,
        RpcNodeCoreAdapter<Provider, Pool, Network, EvmConfig>:
            RpcNodeCore<Provider: ChainSpecProvider<ChainSpec = ChainSpec>, Evm = EvmConfig>,
        RpcConverter<Ethereum, EvmConfig, EthReceiptConverter<ChainSpec>>: RpcConvert,
        (): PendingEnvBuilder<EvmConfig>,
    {
        self.eth_api_builder().build()
    }
}

impl<N, Provider, Pool, Network, EvmConfig, Consensus>
    RpcModuleBuilder<N, Provider, Pool, Network, EvmConfig, Consensus>
where
    N: NodePrimitives,
    Provider: FullRpcProvider<Block = N::Block, Receipt = N::Receipt, Header = N::BlockHeader>
        + CanonStateSubscriptions<Primitives = N>
        + ForkChoiceSubscriptions<Header = N::BlockHeader>
        + PersistedBlockSubscriptions
        + AccountReader
        + ChangeSetReader,
    Pool: TransactionPool + Clone + 'static,
    Network: NetworkInfo + Peers + Clone + 'static,
    EvmConfig: ConfigureEvm<Primitives = N> + 'static,
    Consensus: FullConsensus<N> + Clone + 'static,
{
    /// Configures all [`RpcModule`]s specific to the given [`TransportRpcModuleConfig`] which can
    /// be used to start the transport server(s).
    ///
    /// This behaves exactly as [`RpcModuleBuilder::build`] for the [`TransportRpcModules`], but
    /// also configures the auth (engine api) server, which exposes a subset of the `eth_`
    /// namespace.
    /// Build transport RPC modules plus merged engine / `wem_*` / `smartnet_*` module (unified
    /// HTTP/WS).
    pub fn build_with_engine_rpc<EthApi, Payload>(
        self,
        module_config: TransportRpcModuleConfig,
        engine: impl IntoEngineApiRpcModule,
        eth: EthApi,
        engine_events: EventSender<ConsensusEngineEvent<N>>,
        beacon_engine_handle: ConsensusEngineHandle<Payload>,
    ) -> (
        TransportRpcModules,
        AuthRpcModule,
        RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>,
    )
    where
        EthApi: FullEthApiServer<Provider = Provider, Pool = Pool>,
        Payload: PayloadTypes,
    {
        let config = module_config.config.clone().unwrap_or_default();

        let mut registry = self.into_registry(config, eth, engine_events);
        let modules = registry.create_transport_rpc_modules(module_config);
        let auth_module = registry.create_auth_module(engine, beacon_engine_handle);

        (modules, auth_module, registry)
    }

    /// Converts the builder into a [`RpcRegistryInner`] which can be used to create all
    /// components.
    ///
    /// This is useful for getting access to API handlers directly
    pub fn into_registry<EthApi>(
        self,
        config: RpcModuleConfig,
        eth: EthApi,
        engine_events: EventSender<ConsensusEngineEvent<N>>,
    ) -> RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    where
        EthApi: FullEthApiServer<Provider = Provider, Pool = Pool>,
    {
        let Self { provider, pool, network, executor, consensus, evm_config, .. } = self;
        let executor =
            executor.expect("RpcModuleBuilder requires a Runtime to be set via `with_executor`");
        RpcRegistryInner::new(
            provider,
            pool,
            network,
            executor,
            consensus,
            config,
            evm_config,
            eth,
            engine_events,
        )
    }

    /// Configures all [`RpcModule`]s specific to the given [`TransportRpcModuleConfig`] which can
    /// be used to start the transport server(s).
    pub fn build<EthApi>(
        self,
        module_config: TransportRpcModuleConfig,
        eth: EthApi,
        engine_events: EventSender<ConsensusEngineEvent<N>>,
    ) -> TransportRpcModules<()>
    where
        EthApi: FullEthApiServer<Provider = Provider, Pool = Pool>,
    {
        if module_config.is_empty() {
            TransportRpcModules::default()
        } else {
            let config = module_config.config.clone().unwrap_or_default();
            let mut registry = self.into_registry(config, eth, engine_events);
            registry.create_transport_rpc_modules(module_config)
        }
    }
}

impl<N: NodePrimitives> Default for RpcModuleBuilder<N, (), (), (), (), ()> {
    fn default() -> Self {
        Self {
            provider: (),
            pool: (),
            network: (),
            executor: None,
            evm_config: (),
            consensus: (),
            _primitives: PhantomData,
        }
    }
}

/// Bundles settings for modules
#[derive(Debug, Default, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RpcModuleConfig {
    /// `eth` namespace settings
    eth: EthConfig,
    /// Mesh peers file used by the `admin` namespace (`admin_nodeInfo`).
    mesh_peers_file: Option<PathBuf>,
    /// Whether the `txpool` namespace may be served (operator key `RPC_MEMPOOL`, default off).
    /// Only meaningful together with the registry allowlist; see
    /// [`SmartnetRpcModule::is_allowed`].
    #[serde(default)]
    txpool_namespace: bool,
    /// Whether `web3_nodeInfo` is exposed (operator key `RPC_NODEINFO`, default off).
    #[serde(default)]
    nodeinfo: bool,
}

// === impl RpcModuleConfig ===

impl RpcModuleConfig {
    /// Convenience method to create a new [`RpcModuleConfigBuilder`]
    pub fn builder() -> RpcModuleConfigBuilder {
        RpcModuleConfigBuilder::default()
    }

    /// Returns a new RPC module config given the eth namespace config
    pub const fn new(eth: EthConfig) -> Self {
        Self { eth, mesh_peers_file: None, txpool_namespace: false, nodeinfo: false }
    }

    /// Sets the mesh peers file used by the `admin` namespace.
    pub fn with_mesh_peers_file(mut self, mesh_peers_file: Option<PathBuf>) -> Self {
        self.mesh_peers_file = mesh_peers_file;
        self
    }

    /// Sets whether the `txpool` namespace may be served (`RPC_MEMPOOL`, default off).
    pub const fn with_txpool_namespace(mut self, enabled: bool) -> Self {
        self.txpool_namespace = enabled;
        self
    }

    /// Sets whether `web3_nodeInfo` is exposed (`RPC_NODEINFO`, default off).
    pub const fn with_nodeinfo(mut self, enabled: bool) -> Self {
        self.nodeinfo = enabled;
        self
    }

    /// Get the mesh peers file, if configured.
    pub const fn mesh_peers_file(&self) -> Option<&PathBuf> {
        self.mesh_peers_file.as_ref()
    }

    /// Returns true if the `txpool` namespace is enabled (`RPC_MEMPOOL`).
    pub const fn txpool_namespace(&self) -> bool {
        self.txpool_namespace
    }

    /// Returns true if `web3_nodeInfo` is exposed (`RPC_NODEINFO`).
    pub const fn nodeinfo(&self) -> bool {
        self.nodeinfo
    }

    /// Get a reference to the eth namespace config
    pub const fn eth(&self) -> &EthConfig {
        &self.eth
    }

    /// Get a mutable reference to the eth namespace config
    pub const fn eth_mut(&mut self) -> &mut EthConfig {
        &mut self.eth
    }
}

/// Configures [`RpcModuleConfig`]
#[derive(Clone, Debug, Default)]
pub struct RpcModuleConfigBuilder {
    eth: Option<EthConfig>,
}

// === impl RpcModuleConfigBuilder ===

impl RpcModuleConfigBuilder {
    /// Configures a custom eth namespace config
    pub fn eth(mut self, eth: EthConfig) -> Self {
        self.eth = Some(eth);
        self
    }

    /// Consumes the type and creates the [`RpcModuleConfig`]
    pub fn build(self) -> RpcModuleConfig {
        let Self { eth } = self;
        RpcModuleConfig {
            eth: eth.unwrap_or_default(),
            mesh_peers_file: None,
            txpool_namespace: false,
            nodeinfo: false,
        }
    }

    /// Get a reference to the eth namespace config, if any
    pub const fn get_eth(&self) -> Option<&EthConfig> {
        self.eth.as_ref()
    }

    /// Get a mutable reference to the eth namespace config, if any
    pub const fn eth_mut(&mut self) -> &mut Option<EthConfig> {
        &mut self.eth
    }

    /// Get the eth namespace config, creating a default if none is set
    pub fn eth_mut_or_default(&mut self) -> &mut EthConfig {
        self.eth.get_or_insert_with(EthConfig::default)
    }
}

/// A Helper type the holds instances of the configured modules.
#[derive(Debug)]
pub struct RpcRegistryInner<Provider, Pool, Network, EthApi: EthApiTypes, EvmConfig, Consensus> {
    provider: Provider,
    pool: Pool,
    network: Network,
    executor: Runtime,
    evm_config: EvmConfig,
    consensus: Consensus,
    /// Holds all `eth_` namespace handlers
    eth: EthHandlers<EthApi>,
    /// to put trace calls behind semaphore
    blocking_pool_guard: BlockingTaskGuard,
    /// Contains the [Methods] of a module
    modules: HashMap<SmartnetRpcModule, Methods>,
    /// eth config settings
    eth_config: EthConfig,
    /// Mesh peers file used by the `admin` namespace (`admin_nodeInfo`).
    mesh_peers_file: Option<PathBuf>,
    /// Whether the `txpool` namespace may be served (operator key `RPC_MEMPOOL`, default off).
    txpool_enabled: bool,
    /// Whether `web3_nodeInfo` is exposed (operator key `RPC_NODEINFO`, default off).
    nodeinfo_enabled: bool,
    /// Notification channel for engine API events
    engine_events:
        EventSender<ConsensusEngineEvent<<EthApi::RpcConvert as RpcConvert>::Primitives>>,
}

// === impl RpcRegistryInner ===

impl<N, Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    N: NodePrimitives,
    Provider: StateProviderFactory
        + CanonStateSubscriptions<Primitives = N>
        + BlockReader<Block = N::Block, Receipt = N::Receipt>
        + Clone
        + Unpin
        + 'static,
    Pool: Send + Sync + Clone + 'static,
    Network: Clone + 'static,
    EthApi: FullEthApiTypes + 'static,
    EvmConfig: ConfigureEvm<Primitives = N>,
{
    /// Creates a new, empty instance.
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        provider: Provider,
        pool: Pool,
        network: Network,
        executor: Runtime,
        consensus: Consensus,
        config: RpcModuleConfig,
        evm_config: EvmConfig,
        eth_api: EthApi,
        engine_events: EventSender<
            ConsensusEngineEvent<<EthApi::Provider as NodePrimitivesProvider>::Primitives>,
        >,
    ) -> Self
    where
        EvmConfig: ConfigureEvm<Primitives = N>,
    {
        let blocking_pool_guard = BlockingTaskGuard::new(config.eth.max_tracing_requests);

        let eth = EthHandlers::bootstrap(config.eth.clone(), executor.clone(), eth_api);

        if config.txpool_namespace {
            tracing::warn!(
                target: "smartnet::rpc",
                "txpool namespace ENABLED via RPC_MEMPOOL. This is a temporary sync-debugging aid \
                 (read-only, but leaks pending-tx data and is a DoS surface). Disable before any \
                 public launch."
            );
        }

        Self {
            provider,
            pool,
            network,
            eth,
            executor,
            consensus,
            modules: Default::default(),
            blocking_pool_guard,
            eth_config: config.eth,
            mesh_peers_file: config.mesh_peers_file,
            txpool_enabled: config.txpool_namespace,
            nodeinfo_enabled: config.nodeinfo,
            evm_config,
            engine_events,
        }
    }
}

impl<Provider, Pool, Network, EthApi, Evm, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, Evm, Consensus>
where
    EthApi: EthApiTypes,
{
    /// Returns a reference to the installed [`EthApi`].
    pub const fn eth_api(&self) -> &EthApi {
        &self.eth.api
    }

    /// Returns a reference to the installed [`EthHandlers`].
    pub const fn eth_handlers(&self) -> &EthHandlers<EthApi> {
        &self.eth
    }

    /// Returns a reference to the pool
    pub const fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Returns a reference to the tasks type
    pub const fn tasks(&self) -> &Runtime {
        &self.executor
    }

    /// Returns a reference to the provider
    pub const fn provider(&self) -> &Provider {
        &self.provider
    }

    /// Returns a reference to the evm config
    pub const fn evm_config(&self) -> &Evm {
        &self.evm_config
    }

    /// Returns all installed methods
    pub fn methods(&self) -> Vec<Methods> {
        self.modules.values().cloned().collect()
    }

    /// Returns a merged `RpcModule`
    pub fn module(&self) -> RpcModule<()> {
        let mut module = RpcModule::new(());
        for methods in self.modules.values().cloned() {
            module.merge(methods).expect("No conflicts");
        }
        module
    }
}

impl<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    Network: NetworkInfo + Clone + 'static,
    EthApi: EthApiTypes,
    Provider: BlockReader + ChainSpecProvider<ChainSpec: EthereumHardforks>,
    EvmConfig: ConfigureEvm,
{
    /// Instantiates `AdminApi`
    pub fn admin_api(&self) -> AdminApi<Network, Pool>
    where
        Network: Peers,
        Pool: TransactionPool + Clone + 'static,
    {
        AdminApi::new(self.network.clone(), self.pool.clone(), self.mesh_peers_file.clone())
    }

    /// Instantiates `Web3Api`
    pub fn web3_api(&self) -> Web3Api<Network> {
        Web3Api::new(self.network.clone())
    }

    /// Register Admin Namespace
    pub fn register_admin(&mut self) -> &mut Self
    where
        Network: Peers,
        Pool: TransactionPool + Clone + 'static,
    {
        let adminapi = self.admin_api();
        self.modules.insert(SmartnetRpcModule::Admin, adminapi.into_rpc().into());
        self
    }

    /// Register Web3 Namespace
    pub fn register_web3(&mut self) -> &mut Self {
        let web3api = self.web3_api();
        self.modules.insert(SmartnetRpcModule::Web3, web3api.into_rpc().into());
        self
    }
}

impl<N, Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    N: NodePrimitives,
    Provider: FullRpcProvider<
            Header = N::BlockHeader,
            Block = N::Block,
            Receipt = N::Receipt,
            Transaction = N::SignedTx,
        > + AccountReader
        + ChangeSetReader
        + CanonStateSubscriptions<Primitives = N>
        + ForkChoiceSubscriptions<Header = N::BlockHeader>
        + PersistedBlockSubscriptions,
    Network: NetworkInfo + Peers + Clone + 'static,
    EthApi: EthApiServer<
            RpcTxReq<EthApi::NetworkTypes>,
            RpcTransaction<EthApi::NetworkTypes>,
            RpcBlock<EthApi::NetworkTypes>,
            RpcReceipt<EthApi::NetworkTypes>,
            RpcHeader<EthApi::NetworkTypes>,
            TxTy<N>,
        > + EthApiTypes,
    EvmConfig: ConfigureEvm<Primitives = N> + 'static,
{
    /// Register Eth Namespace
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn register_eth(&mut self) -> &mut Self {
        let eth_api = self.eth_api().clone();
        self.modules.insert(SmartnetRpcModule::Eth, eth_api.into_rpc().into());
        self
    }

    /// Register Otterscan Namespace
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn register_ots(&mut self) -> &mut Self
    where
        EthApi: FullEthApiTypes + 'static,
    {
        let otterscan_api = self.otterscan_api();
        self.modules.insert(SmartnetRpcModule::Ots, otterscan_api.into_rpc().into());
        self
    }

    /// Register Debug Namespace
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn register_debug(&mut self) -> &mut Self
    where
        EthApi: FullEthApiTypes + 'static,
    {
        let debug_api = self.debug_api();
        self.modules.insert(SmartnetRpcModule::Debug, debug_api.into_rpc().into());
        self
    }

    /// Register Trace Namespace
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn register_trace(&mut self) -> &mut Self
    where
        EthApi: FullEthApiTypes + 'static,
    {
        let trace_api = self.trace_api();
        self.modules.insert(SmartnetRpcModule::Trace, trace_api.into_rpc().into());
        self
    }

    /// Register Net Namespace
    ///
    /// See also [`Self::eth_api`]
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime.
    pub fn register_net(&mut self) -> &mut Self
    where
        EthApi: EthApiSpec + 'static,
    {
        let netapi = self.net_api();
        self.modules.insert(SmartnetRpcModule::Net, netapi.into_rpc().into());
        self
    }

    /// Instantiates `OtterscanApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn otterscan_api(&self) -> OtterscanApi<EthApi> {
        let eth_api = self.eth_api().clone();
        OtterscanApi::new(eth_api)
    }
}

impl<N, Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    N: NodePrimitives,
    Provider: FullRpcProvider<
            Block = N::Block,
            Header = N::BlockHeader,
            Transaction = N::SignedTx,
            Receipt = N::Receipt,
        > + AccountReader
        + ChangeSetReader,
    Network: NetworkInfo + Peers + Clone + 'static,
    EthApi: EthApiTypes,
    EvmConfig: ConfigureEvm<Primitives = N>,
{
    /// Instantiates `TraceApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn trace_api(&self) -> TraceApi<EthApi> {
        TraceApi::new(
            self.eth_api().clone(),
            self.blocking_pool_guard.clone(),
            self.eth_config.clone(),
        )
    }

    /// Instantiates [`EthBundle`] Api
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn bundle_api(&self) -> EthBundle<EthApi>
    where
        EthApi: EthTransactions + LoadPendingBlock + Call,
    {
        let eth_api = self.eth_api().clone();
        EthBundle::new(eth_api, self.blocking_pool_guard.clone())
    }

    /// Instantiates `DebugApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn debug_api(&self) -> DebugApi<EthApi>
    where
        EthApi: FullEthApiTypes,
    {
        DebugApi::new(
            self.eth_api().clone(),
            self.blocking_pool_guard.clone(),
            self.tasks(),
            self.engine_events.new_listener(),
        )
    }

    /// Instantiates `NetApi`
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn net_api(&self) -> NetApi<Network, EthApi>
    where
        EthApi: EthApiSpec + 'static,
    {
        let eth_api = self.eth_api().clone();
        NetApi::new(self.network.clone(), eth_api)
    }

    /// Instantiates `OperatorExtensionApi`
    pub fn operator_extension_api(&self) -> OperatorExtensionApi<Provider, EvmConfig> {
        OperatorExtensionApi::new(
            self.provider.clone(),
            self.evm_config.clone(),
            self.blocking_pool_guard.clone(),
            self.executor.clone(),
        )
    }
}

impl<N, Provider, Pool, Network, EthApi, EvmConfig, Consensus>
    RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    N: NodePrimitives,
    Provider: FullRpcProvider<Block = N::Block>
        + CanonStateSubscriptions<Primitives = N>
        + ForkChoiceSubscriptions<Header = N::BlockHeader>
        + PersistedBlockSubscriptions
        + AccountReader
        + ChangeSetReader,
    Pool: TransactionPool + Clone + 'static,
    Network: NetworkInfo + Peers + Clone + 'static,
    EthApi: FullEthApiServer,
    EvmConfig: ConfigureEvm<Primitives = N> + 'static,
    Consensus: FullConsensus<N> + Clone + 'static,
{
    /// Register operator extension RPC (`smartnet_*` canonical + `wem_*` on one module).
    ///
    /// Inserts the same merged module under both [`SmartnetRpcModule::Smartnet`] and
    /// [`SmartnetRpcModule::Wem`] so CLI `--http.api smartnet` or `wem` resolves identically.
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime.
    pub fn register_operator_extensions(&mut self) -> &mut Self {
        let module: Methods = self.operator_extension_rpc_module().into();
        self.modules.insert(SmartnetRpcModule::Smartnet, module.clone());
        self.modules.insert(SmartnetRpcModule::Wem, module);
        self
    }

    /// Canonical `smartnet_*` + legacy `wem_*` operator/debug RPC on one [`RpcModule`].
    pub fn operator_extension_rpc_module(&self) -> jsonrpsee::RpcModule<()> {
        use smartnet_rpc_api::{SmartnetApiServer, WemApiServer};
        let api = OperatorExtensionApi::new(
            self.provider.clone(),
            self.evm_config.clone(),
            self.blocking_pool_guard.clone(),
            self.executor.clone(),
        );
        let mut module = jsonrpsee::RpcModule::new(());
        module
            .merge(WemApiServer::into_rpc(api.clone()).remove_context())
            .expect("wem_* methods");
        module
            .merge(SmartnetApiServer::into_rpc(api).remove_context())
            .expect("smartnet_* methods");
        module
    }

    /// Configures the auth module that includes the
    ///   * `engine_` namespace
    ///   * `wem_` namespace
    ///   * `api_` namespace
    ///
    /// Note: This does _not_ register the `engine_` in this registry.
    pub fn create_auth_module<Payload>(
        &self,
        engine_api: impl IntoEngineApiRpcModule,
        beacon_engine_handle: ConsensusEngineHandle<Payload>,
    ) -> AuthRpcModule
    where
        Payload: PayloadTypes,
    {
        let mut module = engine_api.into_rpc_module();

        merge_engine_extension_rpc(&mut module, beacon_engine_handle)
            .expect("No conflicting engine extension methods");

        // also merge a subset of `eth_` handlers
        let eth_handlers = self.eth_handlers();
        let engine_eth = EngineEthApi::new(eth_handlers.api.clone(), eth_handlers.filter.clone());

        module.merge(engine_eth.into_rpc()).expect("No conflicting methods");

        AuthRpcModule { inner: module }
    }

    /// Helper function to create a [`RpcModule`] if it's not `None`
    fn maybe_module(&mut self, config: Option<&RpcModuleSelection>) -> Option<RpcModule<()>> {
        config.map(|config| self.module_for(config))
    }

    /// Configure a [`TransportRpcModules`] using the current registry. This
    /// creates [`RpcModule`] instances for the modules selected by the
    /// `config`.
    pub fn create_transport_rpc_modules(
        &mut self,
        mut config: TransportRpcModuleConfig,
    ) -> TransportRpcModules<()> {
        // Harden the stored selections against the production allowlist before anything reads
        // them. `module_for` already refuses to build a blocked namespace, but the add-on merges
        // (`merge_if_module_configured`, e.g. `Testing`/`Flashbots`) gate on
        // `contains_http`/`contains_ws`, which consult these selections directly — normalizing
        // them here closes that side path for every transport at once.
        let txpool_enabled = self.txpool_enabled;
        config.http = config.http.take().map(|sel| sel.filter_allowed(txpool_enabled));
        config.ws = config.ws.take().map(|sel| sel.filter_allowed(txpool_enabled));

        let mut modules = TransportRpcModules::default();
        let http = self.maybe_module(config.http.as_ref());
        let ws = self.maybe_module(config.ws.as_ref());

        modules.config = config;
        modules.http = http;
        modules.ws = ws;
        modules
    }

    /// Populates a new [`RpcModule`] based on the selected [`SmartnetRpcModule`]s in the given
    /// [`RpcModuleSelection`]
    pub fn module_for(&mut self, config: &RpcModuleSelection) -> RpcModule<()> {
        let mut module = RpcModule::new(());
        // Production allowlist: drop every namespace not permitted here so the blocked APIs are
        // never even constructed. This is the single choke point for both HTTP and WS (each goes
        // through `maybe_module` → `module_for`), so it holds regardless of `--http.api all`.
        let txpool_enabled = self.txpool_enabled;
        let namespaces: Vec<_> = config
            .iter_selection()
            .filter(|ns| {
                let allowed = ns.is_allowed(txpool_enabled);
                if !allowed {
                    tracing::debug!(
                        target: "smartnet::rpc",
                        namespace = %ns,
                        "RPC namespace blocked by production allowlist"
                    );
                }
                allowed
            })
            .collect();
        let all_methods = self.namespace_methods(namespaces.iter().cloned());
        // `Wem` and `Smartnet` both resolve to the identical operator-extension
        // method set (see `register_operator_extensions`); merge it only once so
        // selections like `All` do not trip `AlreadyRegistered` on every method.
        let mut merged_operator_extension = false;
        for (namespace, methods) in namespaces.iter().zip(all_methods) {
            let is_operator_extension =
                matches!(namespace, SmartnetRpcModule::Wem | SmartnetRpcModule::Smartnet);
            if is_operator_extension && merged_operator_extension {
                continue;
            }
            merged_operator_extension |= is_operator_extension;
            module.merge(methods).expect("No conflicts");
        }
        module
    }

    /// Returns the [Methods] for the given [`SmartnetRpcModule`]
    ///
    /// If this is the first time the namespace is requested, a new instance of API implementation
    /// will be created.
    ///
    /// # Panics
    ///
    /// If called outside of the tokio runtime. See also [`Self::eth_api`]
    pub fn namespace_methods(
        &mut self,
        namespaces: impl Iterator<Item = SmartnetRpcModule>,
    ) -> Vec<Methods> {
        let EthHandlers { api: eth_api, filter: eth_filter, pubsub: eth_pubsub, .. } =
            self.eth_handlers().clone();

        // Create a copy, so we can list out all the methods for rpc_ api
        let namespaces: Vec<_> = namespaces.collect();
        let operator_extension_methods = namespaces
            .iter()
            .any(|ns| matches!(ns, SmartnetRpcModule::Wem | SmartnetRpcModule::Smartnet))
            .then(|| self.operator_extension_rpc_module().into());
        namespaces
            .iter()
            .map(|namespace| {
                self.modules
                    .entry(namespace.clone())
                    .or_insert_with(|| match namespace.clone() {
                        SmartnetRpcModule::Admin => AdminApi::new(
                            self.network.clone(),
                            self.pool.clone(),
                            self.mesh_peers_file.clone(),
                        )
                        .into_rpc()
                        .into(),
                        SmartnetRpcModule::Debug => DebugApi::new(
                            eth_api.clone(),
                            self.blocking_pool_guard.clone(),
                            &self.executor,
                            self.engine_events.new_listener(),
                        )
                        .into_rpc()
                        .into(),
                        SmartnetRpcModule::Eth => {
                            // merge all eth handlers
                            let mut module = eth_api.clone().into_rpc();
                            module.merge(eth_filter.clone().into_rpc()).expect("No conflicts");
                            module.merge(eth_pubsub.clone().into_rpc()).expect("No conflicts");
                            module
                                .merge(
                                    EthBundle::new(
                                        eth_api.clone(),
                                        self.blocking_pool_guard.clone(),
                                    )
                                    .into_rpc(),
                                )
                                .expect("No conflicts");

                            module.into()
                        }
                        SmartnetRpcModule::Net => {
                            NetApi::new(self.network.clone(), eth_api.clone()).into_rpc().into()
                        }
                        SmartnetRpcModule::Trace => TraceApi::new(
                            eth_api.clone(),
                            self.blocking_pool_guard.clone(),
                            self.eth_config.clone(),
                        )
                        .into_rpc()
                        .into(),
                        SmartnetRpcModule::Web3 => {
                            let mut module = Web3Api::new(self.network.clone()).into_rpc();
                            // `web3_nodeInfo` (operator key `RPC_NODEINFO`, default off): the
                            // closed mesh roster — `admin_nodeInfo`'s data re-published under
                            // the allowed `web3` namespace. Registered only when enabled so the
                            // method is absent from the routing table otherwise. Reveals mesh
                            // membership labels/IPs/ports; the node's own public key is not
                            // reported yet (identity is by listen port until the keypair lands).
                            if self.nodeinfo_enabled {
                                let peers_file = self.mesh_peers_file.clone();
                                module
                                    .register_async_method("web3_nodeInfo", move |_, _, _| {
                                        let peers_file = peers_file.clone();
                                        async move { smartnet_rpc::mesh_node_info(peers_file).await }
                                    })
                                    .expect("web3_nodeInfo registration");
                            }
                            module.into()
                        }
                        SmartnetRpcModule::Txpool => TxPoolApi::new(
                            self.eth.api.pool().clone(),
                            dyn_clone::clone(self.eth.api.converter()),
                        )
                        .into_rpc()
                        .into(),
                        SmartnetRpcModule::Rpc => RPCApi::new(
                            namespaces
                                .iter()
                                .map(|module| (module.to_string(), "1.0".to_string()))
                                .collect(),
                        )
                        .into_rpc()
                        .into(),
                        SmartnetRpcModule::Ots => OtterscanApi::new(eth_api.clone()).into_rpc().into(),
                        SmartnetRpcModule::Wem | SmartnetRpcModule::Smartnet => {
                            operator_extension_methods.clone().expect("built above")
                        }
                        SmartnetRpcModule::Miner => MinerApi::default().into_rpc().into(),
                        SmartnetRpcModule::Mev => {
                            EthSimBundle::new(eth_api.clone(), self.blocking_pool_guard.clone())
                                .into_rpc()
                                .into()
                        }
                        // these are implementation specific and need to be handled during
                        // initialization and should be registered via extend_rpc_modules in the
                        // nodebuilder rpc addon stack
                        SmartnetRpcModule::Flashbots |
                        SmartnetRpcModule::Testing |
                        SmartnetRpcModule::Other(_) => Default::default(),
                    })
                    .clone()
            })
            .collect::<Vec<_>>()
    }
}

impl<Provider, Pool, Network, EthApi, EvmConfig, Consensus> Clone
    for RpcRegistryInner<Provider, Pool, Network, EthApi, EvmConfig, Consensus>
where
    EthApi: EthApiTypes,
    Provider: Clone,
    Pool: Clone,
    Network: Clone,
    EvmConfig: Clone,
    Consensus: Clone,
{
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            pool: self.pool.clone(),
            network: self.network.clone(),
            executor: self.executor.clone(),
            evm_config: self.evm_config.clone(),
            consensus: self.consensus.clone(),
            eth: self.eth.clone(),
            blocking_pool_guard: self.blocking_pool_guard.clone(),
            modules: self.modules.clone(),
            eth_config: self.eth_config.clone(),
            mesh_peers_file: self.mesh_peers_file.clone(),
            txpool_enabled: self.txpool_enabled,
            nodeinfo_enabled: self.nodeinfo_enabled,
            engine_events: self.engine_events.clone(),
        }
    }
}

/// A builder type for configuring and launching the servers that will handle RPC requests.
///
/// Supported server transports are:
///    - http
///    - ws
///
/// Http and WS share the same settings: [`ServerBuilder`].
///
/// Once the [`RpcModule`] is built via [`RpcModuleBuilder`] the servers can be started, See also
/// [`ServerBuilder::build`] and [`Server::start`](jsonrpsee::server::Server::start).
#[derive(Debug)]
pub struct RpcServerConfig<RpcMiddleware = Identity> {
    /// Configs for JSON-RPC Http.
    http_server_config: Option<ServerConfigBuilder>,
    /// Allowed CORS Domains for http
    http_cors_domains: Option<String>,
    /// Address where to bind the http server to
    http_addr: Option<SocketAddr>,
    /// Control whether http responses should be compressed
    http_disable_compression: bool,
    /// Configs for WS server
    ws_server_config: Option<ServerConfigBuilder>,
    /// Allowed CORS Domains for ws.
    ws_cors_domains: Option<String>,
    /// Address where to bind the ws server to
    ws_addr: Option<SocketAddr>,
    /// Configurable RPC middleware
    rpc_middleware: RpcMiddleware,
}

// === impl RpcServerConfig ===

impl Default for RpcServerConfig<Identity> {
    /// Create a new config instance
    fn default() -> Self {
        Self {
            http_server_config: None,
            http_cors_domains: None,
            http_addr: None,
            http_disable_compression: false,
            ws_server_config: None,
            ws_cors_domains: None,
            ws_addr: None,
            rpc_middleware: Default::default(),
        }
    }
}

impl RpcServerConfig {
    /// Creates a new config with only http set
    pub fn http(config: ServerConfigBuilder) -> Self {
        Self::default().with_http(config)
    }

    /// Creates a new config with only ws set
    pub fn ws(config: ServerConfigBuilder) -> Self {
        Self::default().with_ws(config)
    }

    /// Configures the http server
    ///
    /// Note: this always configures an [`EthSubscriptionIdProvider`] [`IdProvider`] for
    /// convenience. To set a custom [`IdProvider`], please use [`Self::with_id_provider`].
    pub fn with_http(mut self, config: ServerConfigBuilder) -> Self {
        self.http_server_config =
            Some(config.set_id_provider(EthSubscriptionIdProvider::default()));
        self
    }

    /// Configures the ws server
    ///
    /// Note: this always configures an [`EthSubscriptionIdProvider`] [`IdProvider`] for
    /// convenience. To set a custom [`IdProvider`], please use [`Self::with_id_provider`].
    pub fn with_ws(mut self, config: ServerConfigBuilder) -> Self {
        self.ws_server_config = Some(config.set_id_provider(EthSubscriptionIdProvider::default()));
        self
    }
}

impl<RpcMiddleware> RpcServerConfig<RpcMiddleware> {
    /// Configure rpc middleware
    pub fn set_rpc_middleware<T>(self, rpc_middleware: T) -> RpcServerConfig<T> {
        RpcServerConfig {
            http_server_config: self.http_server_config,
            http_cors_domains: self.http_cors_domains,
            http_addr: self.http_addr,
            http_disable_compression: self.http_disable_compression,
            ws_server_config: self.ws_server_config,
            ws_cors_domains: self.ws_cors_domains,
            ws_addr: self.ws_addr,
            rpc_middleware,
        }
    }

    /// Configure the cors domains for http _and_ ws
    pub fn with_cors(self, cors_domain: Option<String>) -> Self {
        self.with_http_cors(cors_domain.clone()).with_ws_cors(cors_domain)
    }

    /// Configure the cors domains for WS
    pub fn with_ws_cors(mut self, cors_domain: Option<String>) -> Self {
        self.ws_cors_domains = cors_domain;
        self
    }

    /// Configure whether HTTP responses should be compressed
    pub const fn with_http_disable_compression(mut self, http_disable_compression: bool) -> Self {
        self.http_disable_compression = http_disable_compression;
        self
    }

    /// Configure the cors domains for HTTP
    pub fn with_http_cors(mut self, cors_domain: Option<String>) -> Self {
        self.http_cors_domains = cors_domain;
        self
    }

    /// Configures the [`SocketAddr`] of the http server
    ///
    /// Default is [`Ipv4Addr::LOCALHOST`] and
    /// [`smartnet_rpc_server_types::constants::DEFAULT_HTTP_RPC_PORT`]
    pub const fn with_http_address(mut self, addr: SocketAddr) -> Self {
        self.http_addr = Some(addr);
        self
    }

    /// Configures the [`SocketAddr`] of the ws server
    ///
    /// Default is [`Ipv4Addr::LOCALHOST`] and
    /// [`smartnet_rpc_server_types::constants::DEFAULT_WS_RPC_PORT`]
    pub const fn with_ws_address(mut self, addr: SocketAddr) -> Self {
        self.ws_addr = Some(addr);
        self
    }

    /// Sets a custom [`IdProvider`] for all configured transports.
    ///
    /// By default all transports use [`EthSubscriptionIdProvider`]
    pub fn with_id_provider<I>(mut self, id_provider: I) -> Self
    where
        I: IdProvider + Clone + 'static,
    {
        if let Some(config) = self.http_server_config {
            self.http_server_config = Some(config.set_id_provider(id_provider.clone()));
        }
        if let Some(config) = self.ws_server_config {
            self.ws_server_config = Some(config.set_id_provider(id_provider));
        }

        self
    }

    /// Configures a custom tokio runtime for the rpc server.
    pub fn with_tokio_runtime(mut self, tokio_runtime: Option<tokio::runtime::Handle>) -> Self {
        let Some(tokio_runtime) = tokio_runtime else { return self };
        if let Some(http_server_config) = self.http_server_config {
            self.http_server_config =
                Some(http_server_config.custom_tokio_runtime(tokio_runtime.clone()));
        }
        if let Some(ws_server_config) = self.ws_server_config {
            self.ws_server_config = Some(ws_server_config.custom_tokio_runtime(tokio_runtime));
        }
        self
    }

    /// Returns true if any server is configured.
    ///
    /// If no server is configured, no server will be launched on [`RpcServerConfig::start`].
    pub const fn has_server(&self) -> bool {
        self.http_server_config.is_some() || self.ws_server_config.is_some()
    }

    /// Returns the [`SocketAddr`] of the http server
    pub const fn http_address(&self) -> Option<SocketAddr> {
        self.http_addr
    }

    /// Returns the [`SocketAddr`] of the ws server
    pub const fn ws_address(&self) -> Option<SocketAddr> {
        self.ws_addr
    }

    /// Creates the [`CorsLayer`] if any
    fn maybe_cors_layer(cors: Option<String>) -> Result<Option<CorsLayer>, CorsDomainError> {
        cors.as_deref().map(cors::create_cors_layer).transpose()
    }

    /// Returns a [`CompressionLayer`] that adds compression support (gzip, deflate, brotli, zstd)
    /// based on the client's `Accept-Encoding` header
    fn maybe_compression_layer(disable_compression: bool) -> Option<CompressionLayer> {
        if disable_compression {
            None
        } else {
            Some(CompressionLayer::new())
        }
    }

    /// Builds and starts the configured server(s): http and ws.
    ///
    /// If both http and ws are on the same port, they are combined into one server.
    ///
    /// Returns the [`RpcServerHandle`] with the handle to the started servers.
    pub async fn start(self, modules: &TransportRpcModules) -> Result<RpcServerHandle, RpcError>
    where
        RpcMiddleware: SmartnetRpcMiddleware,
    {
        let mut http_handle = None;
        let mut ws_handle = None;

        let http_socket_addr = self.http_addr.unwrap_or(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            constants::DEFAULT_HTTP_RPC_PORT,
        )));

        let ws_socket_addr = self.ws_addr.unwrap_or(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            constants::DEFAULT_WS_RPC_PORT,
        )));

        // If both are configured on the same port, we combine them into one server.
        if self.http_addr == self.ws_addr &&
            self.http_server_config.is_some() &&
            self.ws_server_config.is_some()
        {
            let cors = match (self.ws_cors_domains.as_ref(), self.http_cors_domains.as_ref()) {
                (Some(ws_cors), Some(http_cors)) => {
                    if ws_cors.trim() != http_cors.trim() {
                        return Err(WsHttpSamePortError::ConflictingCorsDomains {
                            http_cors_domains: Some(http_cors.clone()),
                            ws_cors_domains: Some(ws_cors.clone()),
                        }
                        .into());
                    }
                    Some(ws_cors)
                }
                (a, b) => a.or(b),
            }
            .cloned();

            // we merge this into one server using the http setup
            modules.config.ensure_ws_http_identical()?;

            if let Some(config) = self.http_server_config {
                let server = ServerBuilder::new()
                    .set_http_middleware(
                        tower::ServiceBuilder::new()
                            .option_layer(Self::maybe_cors_layer(cors)?)
                            .option_layer(Self::maybe_compression_layer(
                                self.http_disable_compression,
                            )),
                    )
                    .set_rpc_middleware(
                        RpcServiceBuilder::default()
                            .layer(
                                modules
                                    .http
                                    .as_ref()
                                    .or(modules.ws.as_ref())
                                    .map(RpcRequestMetrics::same_port)
                                    .unwrap_or_default(),
                            )
                            .layer(batch_policy::BatchLimitsLayer)
                            .layer(self.rpc_middleware.clone()),
                    )
                    .set_config(config.build())
                    .build(http_socket_addr)
                    .await
                    .map_err(|err| {
                        RpcError::server_error(err, ServerKind::WsHttp(http_socket_addr))
                    })?;
                let addr = server.local_addr().map_err(|err| {
                    RpcError::server_error(err, ServerKind::WsHttp(http_socket_addr))
                })?;
                if let Some(module) = modules.http.as_ref().or(modules.ws.as_ref()) {
                    let handle = server.start(module.clone());
                    http_handle = Some(handle.clone());
                    ws_handle = Some(handle);
                }
                return Ok(RpcServerHandle {
                    http_local_addr: Some(addr),
                    ws_local_addr: Some(addr),
                    http: http_handle,
                    ws: ws_handle,
                });
            }
        }

        let mut ws_local_addr = None;
        let mut ws_server = None;
        let mut http_local_addr = None;
        let mut http_server = None;

        if let Some(config) = self.ws_server_config {
            let server = ServerBuilder::new()
                .set_config(config.ws_only().build())
                .set_http_middleware(
                    tower::ServiceBuilder::new()
                        .option_layer(Self::maybe_cors_layer(self.ws_cors_domains.clone())?),
                )
                .set_rpc_middleware(
                    RpcServiceBuilder::default()
                        .layer(modules.ws.as_ref().map(RpcRequestMetrics::ws).unwrap_or_default())
                        .layer(batch_policy::BatchLimitsLayer)
                        .layer(self.rpc_middleware.clone()),
                )
                .build(ws_socket_addr)
                .await
                .map_err(|err| RpcError::server_error(err, ServerKind::WS(ws_socket_addr)))?;

            let addr = server
                .local_addr()
                .map_err(|err| RpcError::server_error(err, ServerKind::WS(ws_socket_addr)))?;

            ws_local_addr = Some(addr);
            ws_server = Some(server);
        }

        if let Some(config) = self.http_server_config {
            let server = ServerBuilder::new()
                .set_config(config.http_only().build())
                .set_http_middleware(
                    tower::ServiceBuilder::new()
                        .option_layer(Self::maybe_cors_layer(self.http_cors_domains.clone())?)
                        .option_layer(Self::maybe_compression_layer(self.http_disable_compression)),
                )
                .set_rpc_middleware(
                    RpcServiceBuilder::default()
                        .layer(
                            modules.http.as_ref().map(RpcRequestMetrics::http).unwrap_or_default(),
                        )
                        .layer(batch_policy::BatchLimitsLayer)
                        .layer(self.rpc_middleware.clone()),
                )
                .build(http_socket_addr)
                .await
                .map_err(|err| RpcError::server_error(err, ServerKind::Http(http_socket_addr)))?;
            let local_addr = server
                .local_addr()
                .map_err(|err| RpcError::server_error(err, ServerKind::Http(http_socket_addr)))?;
            http_local_addr = Some(local_addr);
            http_server = Some(server);
        }

        http_handle = http_server
            .map(|http_server| http_server.start(modules.http.clone().expect("http server error")));
        ws_handle = ws_server
            .map(|ws_server| ws_server.start(modules.ws.clone().expect("ws server error")));
        Ok(RpcServerHandle { http_local_addr, ws_local_addr, http: http_handle, ws: ws_handle })
    }
}

/// Holds modules to be installed per transport type
///
/// # Example
///
/// Configure a http transport only
///
/// ```
/// use smartnet_rpc_builder::{SmartnetRpcModule, TransportRpcModuleConfig};
/// let config = TransportRpcModuleConfig::default()
///     .with_http([SmartnetRpcModule::Eth, SmartnetRpcModule::Admin]);
/// ```
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct TransportRpcModuleConfig {
    /// http module configuration
    http: Option<RpcModuleSelection>,
    /// ws module configuration
    ws: Option<RpcModuleSelection>,
    /// Config for the modules
    config: Option<RpcModuleConfig>,
}

// === impl TransportRpcModuleConfig ===

impl TransportRpcModuleConfig {
    /// Creates a new config with only http set
    pub fn set_http(http: impl Into<RpcModuleSelection>) -> Self {
        Self::default().with_http(http)
    }

    /// Creates a new config with only ws set
    pub fn set_ws(ws: impl Into<RpcModuleSelection>) -> Self {
        Self::default().with_ws(ws)
    }

    /// Sets the [`RpcModuleSelection`] for the http transport.
    pub fn with_http(mut self, http: impl Into<RpcModuleSelection>) -> Self {
        self.http = Some(http.into());
        self
    }

    /// Sets the [`RpcModuleSelection`] for the ws transport.
    pub fn with_ws(mut self, ws: impl Into<RpcModuleSelection>) -> Self {
        self.ws = Some(ws.into());
        self
    }

    /// Sets a custom [`RpcModuleConfig`] for the configured modules.
    pub fn with_config(mut self, config: RpcModuleConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Get a mutable reference to the http module configuration.
    pub const fn http_mut(&mut self) -> &mut Option<RpcModuleSelection> {
        &mut self.http
    }

    /// Get a mutable reference to the ws module configuration.
    pub const fn ws_mut(&mut self) -> &mut Option<RpcModuleSelection> {
        &mut self.ws
    }

    /// Get a mutable reference to the rpc module configuration.
    pub const fn config_mut(&mut self) -> &mut Option<RpcModuleConfig> {
        &mut self.config
    }

    /// Returns true if no transports are configured
    pub const fn is_empty(&self) -> bool {
        self.http.is_none() && self.ws.is_none()
    }

    /// Returns the [`RpcModuleSelection`] for the http transport
    pub const fn http(&self) -> Option<&RpcModuleSelection> {
        self.http.as_ref()
    }

    /// Returns the [`RpcModuleSelection`] for the ws transport
    pub const fn ws(&self) -> Option<&RpcModuleSelection> {
        self.ws.as_ref()
    }

    /// Returns the [`RpcModuleConfig`] for the configured modules
    pub const fn config(&self) -> Option<&RpcModuleConfig> {
        self.config.as_ref()
    }

    /// Returns true if the given module is configured for any transport.
    pub fn contains_any(&self, module: &SmartnetRpcModule) -> bool {
        self.contains_http(module) || self.contains_ws(module)
    }

    /// Returns true if the given module is configured for the http transport.
    pub fn contains_http(&self, module: &SmartnetRpcModule) -> bool {
        self.http.as_ref().is_some_and(|http| http.contains(module))
    }

    /// Returns true if the given module is configured for the ws transport.
    pub fn contains_ws(&self, module: &SmartnetRpcModule) -> bool {
        self.ws.as_ref().is_some_and(|ws| ws.contains(module))
    }

    /// Ensures that both http and ws are configured and that they are configured to use the same
    /// port.
    fn ensure_ws_http_identical(&self) -> Result<(), WsHttpSamePortError> {
        if RpcModuleSelection::are_identical(self.http.as_ref(), self.ws.as_ref()) {
            Ok(())
        } else {
            let http_modules =
                self.http.as_ref().map(RpcModuleSelection::to_selection).unwrap_or_default();
            let ws_modules =
                self.ws.as_ref().map(RpcModuleSelection::to_selection).unwrap_or_default();

            let http_not_ws = http_modules.difference(&ws_modules).cloned().collect();
            let ws_not_http = ws_modules.difference(&http_modules).cloned().collect();
            let overlap = http_modules.intersection(&ws_modules).cloned().collect();

            Err(WsHttpSamePortError::ConflictingModules(Box::new(ConflictingModules {
                overlap,
                http_not_ws,
                ws_not_http,
            })))
        }
    }
}

/// Holds installed modules per transport type.
#[derive(Debug, Clone, Default)]
pub struct TransportRpcModules<Context = ()> {
    /// The original config
    config: TransportRpcModuleConfig,
    /// rpcs module for http
    http: Option<RpcModule<Context>>,
    /// rpcs module for ws
    ws: Option<RpcModule<Context>>,
}

// === impl TransportRpcModules ===

impl TransportRpcModules {
    /// Sets a custom [`TransportRpcModuleConfig`] for the configured modules.
    /// This will overwrite current configuration, if any.
    pub fn with_config(mut self, config: TransportRpcModuleConfig) -> Self {
        self.config = config;
        self
    }

    /// Sets the [`RpcModule`] for the http transport.
    /// This will overwrite current module, if any.
    pub fn with_http(mut self, http: RpcModule<()>) -> Self {
        self.http = Some(http);
        self
    }

    /// Sets the [`RpcModule`] for the ws transport.
    /// This will overwrite current module, if any.
    pub fn with_ws(mut self, ws: RpcModule<()>) -> Self {
        self.ws = Some(ws);
        self
    }

    /// Returns the [`TransportRpcModuleConfig`] used to configure this instance.
    pub const fn module_config(&self) -> &TransportRpcModuleConfig {
        &self.config
    }

    /// Merge the given [`Methods`] in all configured transport modules if the given
    /// [`SmartnetRpcModule`] is configured for the transport.
    ///
    /// Fails if any of the methods in other is present already.
    pub fn merge_if_module_configured(
        &mut self,
        module: SmartnetRpcModule,
        other: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        let other = other.into();
        if self.module_config().contains_http(&module) {
            self.merge_http(other.clone())?;
        }
        if self.module_config().contains_ws(&module) {
            self.merge_ws(other)?;
        }

        Ok(())
    }

    /// Merge the given [`Methods`] in all configured transport modules if the given
    /// [`SmartnetRpcModule`] is configured for the transport, using a closure to lazily
    /// create the methods only when needed.
    ///
    /// The closure is only called if at least one transport has the module configured.
    /// Fails if any of the methods in the closure result is present already.
    pub fn merge_if_module_configured_with<F>(
        &mut self,
        module: SmartnetRpcModule,
        f: F,
    ) -> Result<(), RegisterMethodError>
    where
        F: FnOnce() -> Methods,
    {
        // Early return if module not configured for any transport
        if !self.module_config().contains_any(&module) {
            return Ok(());
        }
        self.merge_if_module_configured(module, f())
    }

    /// Merge the given [Methods] in the configured http methods.
    ///
    /// Fails if any of the methods in other is present already.
    ///
    /// Returns [Ok(false)] if no http transport is configured.
    pub fn merge_http(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        if let Some(ref mut http) = self.http {
            return http.merge(other.into()).map(|_| true);
        }
        Ok(false)
    }

    /// Merge the given [Methods] in the configured ws methods.
    ///
    /// Fails if any of the methods in other is present already.
    ///
    /// Returns [Ok(false)] if no ws transport is configured.
    pub fn merge_ws(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        if let Some(ref mut ws) = self.ws {
            return ws.merge(other.into()).map(|_| true);
        }
        Ok(false)
    }

    /// Merge the given [`Methods`] in all configured methods.
    ///
    /// Fails if any of the methods in other is present already.
    pub fn merge_configured(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        let other = other.into();
        self.merge_http(other.clone())?;
        self.merge_ws(other)?;
        Ok(())
    }

    /// Returns all unique endpoints installed for the given module.
    ///
    /// Note: In case of duplicate method names this only record the first occurrence.
    pub fn methods_by_module(&self, module: SmartnetRpcModule) -> Methods {
        self.methods_by(|name| name.starts_with(module.as_str()))
    }

    /// Returns all unique endpoints installed in any of the configured modules.
    ///
    /// Note: In case of duplicate method names this only record the first occurrence.
    pub fn methods_by<F>(&self, mut filter: F) -> Methods
    where
        F: FnMut(&str) -> bool,
    {
        let mut methods = Methods::new();

        // filter that matches the given filter and also removes duplicates we already have
        let mut f =
            |name: &str, mm: &Methods| filter(name) && !mm.method_names().any(|m| m == name);

        if let Some(m) = self.http_methods(|name| f(name, &methods)) {
            let _ = methods.merge(m);
        }
        if let Some(m) = self.ws_methods(|name| f(name, &methods)) {
            let _ = methods.merge(m);
        }
        methods
    }

    /// Returns all [`Methods`] installed for the http server based in the given closure.
    ///
    /// Returns `None` if no http support is configured.
    pub fn http_methods<F>(&self, filter: F) -> Option<Methods>
    where
        F: FnMut(&str) -> bool,
    {
        self.http.as_ref().map(|module| methods_by(module, filter))
    }

    /// Returns all [`Methods`] installed for the ws server based in the given closure.
    ///
    /// Returns `None` if no ws support is configured.
    pub fn ws_methods<F>(&self, filter: F) -> Option<Methods>
    where
        F: FnMut(&str) -> bool,
    {
        self.ws.as_ref().map(|module| methods_by(module, filter))
    }

    /// Removes the method with the given name from the configured http methods.
    ///
    /// Returns `true` if the method was found and removed, `false` otherwise.
    ///
    /// Be aware that a subscription consist of two methods, `subscribe` and `unsubscribe` and
    /// it's the caller responsibility to remove both `subscribe` and `unsubscribe` methods for
    /// subscriptions.
    pub fn remove_http_method(&mut self, method_name: &'static str) -> bool {
        if let Some(http_module) = &mut self.http {
            http_module.remove_method(method_name).is_some()
        } else {
            false
        }
    }

    /// Removes the given methods from the configured http methods.
    pub fn remove_http_methods(&mut self, methods: impl IntoIterator<Item = &'static str>) {
        for name in methods {
            self.remove_http_method(name);
        }
    }

    /// Removes the method with the given name from the configured ws methods.
    ///
    /// Returns `true` if the method was found and removed, `false` otherwise.
    ///
    /// Be aware that a subscription consist of two methods, `subscribe` and `unsubscribe` and
    /// it's the caller responsibility to remove both `subscribe` and `unsubscribe` methods for
    /// subscriptions.
    pub fn remove_ws_method(&mut self, method_name: &'static str) -> bool {
        if let Some(ws_module) = &mut self.ws {
            ws_module.remove_method(method_name).is_some()
        } else {
            false
        }
    }

    /// Removes the given methods from the configured ws methods.
    pub fn remove_ws_methods(&mut self, methods: impl IntoIterator<Item = &'static str>) {
        for name in methods {
            self.remove_ws_method(name);
        }
    }

    /// Removes the method with the given name from all configured transports.
    ///
    /// Returns `true` if the method was found and removed, `false` otherwise.
    pub fn remove_method_from_configured(&mut self, method_name: &'static str) -> bool {
        let http_removed = self.remove_http_method(method_name);
        let ws_removed = self.remove_ws_method(method_name);

        http_removed || ws_removed
    }

    /// Renames a method in all configured transports by:
    /// 1. Removing the old method name.
    /// 2. Adding the new method.
    pub fn rename(
        &mut self,
        old_name: &'static str,
        new_method: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        // Remove the old method from all configured transports
        self.remove_method_from_configured(old_name);

        // Merge the new method into the configured transports
        self.merge_configured(new_method)
    }

    /// Replace the given [`Methods`] in the configured http methods.
    ///
    /// Fails if any of the methods in other is present already or if the method being removed is
    /// not present
    ///
    /// Returns [Ok(false)] if no http transport is configured.
    pub fn replace_http(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_http_methods(other.method_names());
        self.merge_http(other)
    }

    /// Replace the given [Methods] in the configured ws methods.
    ///
    /// Fails if any of the methods in other is present already or if the method being removed is
    /// not present
    ///
    /// Returns [Ok(false)] if no ws transport is configured.
    pub fn replace_ws(&mut self, other: impl Into<Methods>) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_ws_methods(other.method_names());
        self.merge_ws(other)
    }

    /// Replaces the method with the given name from all configured transports.
    ///
    /// Returns `true` if the method was found and replaced, `false` otherwise
    pub fn replace_configured(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.replace_http(other.clone())?;
        self.replace_ws(other)?;
        Ok(true)
    }

    /// Adds or replaces given [`Methods`] in http module.
    ///
    /// Returns `true` if the methods were replaced or added, `false` otherwise.
    pub fn add_or_replace_http(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_http_methods(other.method_names());
        self.merge_http(other)
    }

    /// Adds or replaces given [`Methods`] in ws module.
    ///
    /// Returns `true` if the methods were replaced or added, `false` otherwise.
    pub fn add_or_replace_ws(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<bool, RegisterMethodError> {
        let other = other.into();
        self.remove_ws_methods(other.method_names());
        self.merge_ws(other)
    }

    /// Adds or replaces given [`Methods`] in all configured network modules.
    pub fn add_or_replace_configured(
        &mut self,
        other: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        let other = other.into();
        self.add_or_replace_http(other.clone())?;
        self.add_or_replace_ws(other)?;
        Ok(())
    }
    /// Adds or replaces the given [`Methods`] in the transport modules where the specified
    /// [`SmartnetRpcModule`] is configured.
    pub fn add_or_replace_if_module_configured(
        &mut self,
        module: SmartnetRpcModule,
        other: impl Into<Methods>,
    ) -> Result<(), RegisterMethodError> {
        let other = other.into();
        if self.module_config().contains_http(&module) {
            self.add_or_replace_http(other.clone())?;
        }
        if self.module_config().contains_ws(&module) {
            self.add_or_replace_ws(other)?;
        }
        Ok(())
    }
}

/// Returns the methods installed in the given module that match the given filter.
fn methods_by<T, F>(module: &RpcModule<T>, mut filter: F) -> Methods
where
    F: FnMut(&str) -> bool,
{
    let mut methods = Methods::new();
    let method_names = module.method_names().filter(|name| filter(name));

    for name in method_names {
        if let Some(matched_method) = module.method(name).cloned() {
            let _ = methods.verify_and_insert(name, matched_method);
        }
    }

    methods
}

/// A handle to the spawned servers.
///
/// When this type is dropped or [`RpcServerHandle::stop`] has been called the server will be
/// stopped.
#[derive(Clone, Debug)]
#[must_use = "Server stops if dropped"]
pub struct RpcServerHandle {
    /// The address of the http/ws server
    http_local_addr: Option<SocketAddr>,
    ws_local_addr: Option<SocketAddr>,
    http: Option<ServerHandle>,
    ws: Option<ServerHandle>,
}

// === impl RpcServerHandle ===

impl RpcServerHandle {
    /// Returns the [`SocketAddr`] of the http server if started.
    pub const fn http_local_addr(&self) -> Option<SocketAddr> {
        self.http_local_addr
    }

    /// Returns the [`SocketAddr`] of the ws server if started.
    pub const fn ws_local_addr(&self) -> Option<SocketAddr> {
        self.ws_local_addr
    }

    /// Tell the server to stop without waiting for the server to stop.
    pub fn stop(self) -> Result<(), AlreadyStoppedError> {
        if let Some(handle) = self.http {
            handle.stop()?
        }

        if let Some(handle) = self.ws {
            handle.stop()?
        }

        Ok(())
    }

    /// Returns the url to the http server
    pub fn http_url(&self) -> Option<String> {
        self.http_local_addr.map(|addr| format!("http://{addr}"))
    }

    /// Returns the url to the ws server
    pub fn ws_url(&self) -> Option<String> {
        self.ws_local_addr.map(|addr| format!("ws://{addr}"))
    }

    /// Returns a http client connected to the server.
    pub fn http_client(&self) -> Option<jsonrpsee::http_client::HttpClient> {
        let url = self.http_url()?;
        let client = jsonrpsee::http_client::HttpClientBuilder::default().build(url);
        client.expect("failed to create http client").into()
    }

    /// Returns a ws client connected to the server.
    pub async fn ws_client(&self) -> Option<jsonrpsee::ws_client::WsClient> {
        let url = self.ws_url()?;
        let client = jsonrpsee::ws_client::WsClientBuilder::default()
            .build(url)
            .await
            .expect("failed to create ws client");
        Some(client)
    }

    /// Returns a new [`alloy_network::Ethereum`] http provider with its recommended fillers.
    pub fn eth_http_provider(
        &self,
    ) -> Option<impl Provider<alloy_network::Ethereum> + Clone + Unpin + 'static> {
        self.new_http_provider_for()
    }

    /// Returns a new [`alloy_network::Ethereum`] http provider with its recommended fillers and
    /// installed wallet.
    pub fn eth_http_provider_with_wallet<W>(
        &self,
        wallet: W,
    ) -> Option<impl Provider<alloy_network::Ethereum> + Clone + Unpin + 'static>
    where
        W: IntoWallet<alloy_network::Ethereum, NetworkWallet: Clone + Unpin + 'static>,
    {
        let rpc_url = self.http_url()?;
        let provider =
            ProviderBuilder::new().wallet(wallet).connect_http(rpc_url.parse().expect("valid url"));
        Some(provider)
    }

    /// Returns an http provider from the rpc server handle for the
    /// specified [`alloy_network::Network`].
    ///
    /// This installs the recommended fillers: [`RecommendedFillers`]
    pub fn new_http_provider_for<N>(&self) -> Option<impl Provider<N> + Clone + Unpin + 'static>
    where
        N: RecommendedFillers<RecommendedFillers: Unpin>,
    {
        let rpc_url = self.http_url()?;
        let provider = ProviderBuilder::default()
            .with_recommended_fillers()
            .connect_http(rpc_url.parse().expect("valid url"));
        Some(provider)
    }

    /// Returns a new [`alloy_network::Ethereum`] websocket provider with its recommended fillers.
    pub async fn eth_ws_provider(
        &self,
    ) -> Option<impl Provider<alloy_network::Ethereum> + Clone + Unpin + 'static> {
        self.new_ws_provider_for().await
    }

    /// Returns a new [`alloy_network::Ethereum`] ws provider with its recommended fillers and
    /// installed wallet.
    pub async fn eth_ws_provider_with_wallet<W>(
        &self,
        wallet: W,
    ) -> Option<impl Provider<alloy_network::Ethereum> + Clone + Unpin + 'static>
    where
        W: IntoWallet<alloy_network::Ethereum, NetworkWallet: Clone + Unpin + 'static>,
    {
        let rpc_url = self.ws_url()?;
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect(&rpc_url)
            .await
            .expect("failed to create ws client");
        Some(provider)
    }

    /// Returns an ws provider from the rpc server handle for the
    /// specified [`alloy_network::Network`].
    ///
    /// This installs the recommended fillers: [`RecommendedFillers`]
    pub async fn new_ws_provider_for<N>(&self) -> Option<impl Provider<N> + Clone + Unpin + 'static>
    where
        N: RecommendedFillers<RecommendedFillers: Unpin>,
    {
        let rpc_url = self.ws_url()?;
        let provider = ProviderBuilder::default()
            .with_recommended_fillers()
            .connect(&rpc_url)
            .await
            .expect("failed to create ws client");
        Some(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_rpc_module_config() {
        let selection = vec!["eth", "admin"];
        let config = RpcModuleSelection::try_from_selection(selection).unwrap();
        assert_eq!(
            config,
            RpcModuleSelection::Selection(
                [SmartnetRpcModule::Eth, SmartnetRpcModule::Admin].into()
            )
        );
    }

    #[test]
    fn test_configure_transport_config() {
        let config = TransportRpcModuleConfig::default()
            .with_http([SmartnetRpcModule::Eth, SmartnetRpcModule::Admin]);
        assert_eq!(
            config,
            TransportRpcModuleConfig {
                http: Some(RpcModuleSelection::Selection(
                    [SmartnetRpcModule::Eth, SmartnetRpcModule::Admin].into()
                )),
                ws: None,
                config: None,
            }
        )
    }

    #[test]
    fn test_configure_transport_config_none() {
        let config = TransportRpcModuleConfig::default().with_http(Vec::<SmartnetRpcModule>::new());
        assert_eq!(
            config,
            TransportRpcModuleConfig {
                http: Some(RpcModuleSelection::Selection(Default::default())),
                ws: None,
                config: None,
            }
        )
    }

    fn create_test_module() -> RpcModule<()> {
        let mut module = RpcModule::new(());
        module.register_method("anything", |_, _, _| "succeed").unwrap();
        module
    }

    #[test]
    fn test_remove_method_from_configured() {
        let mut modules = TransportRpcModules {
            http: Some(create_test_module()),
            ws: Some(create_test_module()),
            ..Default::default()
        };

        // Remove a method that exists
        assert!(modules.remove_method_from_configured("anything"));

        // Remove a method that was just removed (it does not exist anymore)
        assert!(!modules.remove_method_from_configured("anything"));

        // Remove a method that does not exist
        assert!(!modules.remove_method_from_configured("non_existent_method"));

        // Verify that the method was removed from all transports
        assert!(modules.http.as_ref().unwrap().method("anything").is_none());
        assert!(modules.ws.as_ref().unwrap().method("anything").is_none());
    }

    #[test]
    fn test_replace_configured() {
        let mut modules = TransportRpcModules {
            http: Some(create_test_module()),
            ws: Some(create_test_module()),
            ..Default::default()
        };
        let mut other_module = RpcModule::new(());
        other_module.register_method("something", |_, _, _| "fails").unwrap();

        assert!(modules.replace_configured(other_module).unwrap());

        // Verify that the other_method was added
        assert!(modules.http.as_ref().unwrap().method("something").is_some());
        assert!(modules.ws.as_ref().unwrap().method("something").is_some());

        assert!(modules.http.as_ref().unwrap().method("anything").is_some());
        assert!(modules.ws.as_ref().unwrap().method("anything").is_some());
    }

    #[test]
    fn test_add_or_replace_if_module_configured() {
        // Create a config that enables SmartnetRpcModule::Eth for HTTP and WS
        let config = TransportRpcModuleConfig::default()
            .with_http([SmartnetRpcModule::Eth])
            .with_ws([SmartnetRpcModule::Eth]);

        // Create HTTP module with an existing method (to test "replace")
        let mut http_module = RpcModule::new(());
        http_module.register_method("eth_existing", |_, _, _| "original").unwrap();

        // Create WS module with the same existing method
        let mut ws_module = RpcModule::new(());
        ws_module.register_method("eth_existing", |_, _, _| "original").unwrap();

        // Set up TransportRpcModules with the config and modules
        let mut modules =
            TransportRpcModules { config, http: Some(http_module), ws: Some(ws_module) };

        // Create new methods: one to replace an existing method, one to add a new one
        let mut new_module = RpcModule::new(());
        new_module.register_method("eth_existing", |_, _, _| "replaced").unwrap(); // Replace
        new_module.register_method("eth_new", |_, _, _| "added").unwrap(); // Add
        let new_methods: Methods = new_module.into();

        // Call the function for SmartnetRpcModule::Eth
        let result =
            modules.add_or_replace_if_module_configured(SmartnetRpcModule::Eth, new_methods);
        assert!(result.is_ok(), "Function should succeed");

        // Verify HTTP: existing method still exists (replaced), new method added
        let http = modules.http.as_ref().unwrap();
        assert!(http.method("eth_existing").is_some());
        assert!(http.method("eth_new").is_some());

        // Verify WS: existing method still exists (replaced), new method added
        let ws = modules.ws.as_ref().unwrap();
        assert!(ws.method("eth_existing").is_some());
        assert!(ws.method("eth_new").is_some());
    }

    #[test]
    fn test_merge_if_module_configured_with_lazy_evaluation() {
        // Create a config that enables SmartnetRpcModule::Eth for HTTP only
        let config = TransportRpcModuleConfig::default().with_http([SmartnetRpcModule::Eth]);

        let mut modules = TransportRpcModules { config, http: Some(RpcModule::new(())), ws: None };

        // Track whether closure was called
        let mut closure_called = false;

        // Test with configured module - closure should be called
        let result = modules.merge_if_module_configured_with(SmartnetRpcModule::Eth, || {
            closure_called = true;
            let mut methods = RpcModule::new(());
            methods.register_method("eth_test", |_, _, _| "test").unwrap();
            methods.into()
        });

        assert!(result.is_ok());
        assert!(closure_called, "Closure should be called when module is configured");
        assert!(modules.http.as_ref().unwrap().method("eth_test").is_some());

        // Reset and test with unconfigured module - closure should NOT be called
        closure_called = false;
        let result = modules.merge_if_module_configured_with(SmartnetRpcModule::Debug, || {
            closure_called = true;
            RpcModule::new(()).into()
        });

        assert!(result.is_ok());
        assert!(!closure_called, "Closure should NOT be called when module is not configured");
    }
}
