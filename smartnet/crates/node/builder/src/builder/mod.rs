//! Customizable node builder.

#![expect(clippy::type_complexity)]
#![allow(missing_debug_implementations)]

use crate::{
    common::WithConfigs,
    components::NodeComponentsBuilder,
    node::FullNode,
    rpc::{RpcContext, SmartnetRpcAddOns, SmartnetRpcServerHandles},
    EngineNodeLauncher, LaunchNode, Node,
};
use smartnet_chainspec::{EthChainSpec, EthereumHardforks};
use smartnet_db_api::{database::Database, database_metrics::DatabaseMetrics};
use smartnet_node_api::{
    FullNodeTypes, FullNodeTypesAdapter, NodeAddOns, NodeTypes, NodeTypesWithDBAdapter,
};
use smartnet_node_core::{
    cli::config::{PayloadBuilderConfig, SmartnetTransactionPoolConfig},
    node_config::NodeConfig,
    primitives::Head,
};
use smartnet_provider::{
    providers::{AuxDbProvider, BlockchainProvider, NodeTypesForProvider},
    ChainSpecProvider, FullProvider,
};
use smartnet_tasks::TaskExecutor;
use smartnet_transaction_pool::PoolConfig;
use std::sync::Arc;

pub mod add_ons;

mod states;
pub use states::*;

/// Canonical full-node adapter (DB + blockchain provider).
// Note: we need to hardcode this because custom components might depend on it in associated types.
pub type SmartnetFullAdapter<DB, Types> =
    FullNodeTypesAdapter<Types, DB, BlockchainProvider<NodeTypesWithDBAdapter<Types, DB>>>;

#[expect(clippy::doc_markdown)]
#[cfg_attr(doc, aquamarine::aquamarine)]
/// Declaratively construct a node.
///
/// [`NodeBuilder`] provides a [builder-like interface][builder] for composing
/// components of a node.
///
/// ## Order
///
/// Configuring a node starts out with a [`NodeConfig`] (this can be obtained from cli arguments for
/// example) and then proceeds to configure the core static types of the node:
/// [`NodeTypes`], these include the node's primitive types and the node's engine
/// types.
///
/// Next all stateful components of the node are configured, these include all the
/// components of the node that are downstream of those types, these include:
///
///  - The EVM and Executor configuration: [`ExecutorBuilder`](crate::components::ExecutorBuilder)
///  - The transaction pool: [`PoolBuilder`](crate::components::PoolBuilder)
///  - The network: [`NetworkBuilder`](crate::components::NetworkBuilder)
///  - The payload builder: [`PayloadBuilder`](crate::components::PayloadServiceBuilder)
///
/// Once all the components are configured, the node is ready to be launched.
///
/// On launch the builder returns a fully type aware [`NodeHandle`] that has access to all the
/// configured components and can interact with the node.
///
/// There are convenience functions for networks that come with a preset of types and components via
/// the [`Node`] trait, see `smartnet_node_ethereum::EthereumNode`.
///
/// The [`NodeBuilder::node`] function configures the node's types and components in one step.
///
/// ## Components
///
/// All components are configured with a [`NodeComponentsBuilder`] that is responsible for actually
/// creating the node components during the launch process. The
/// [`ComponentsBuilder`](crate::components::ComponentsBuilder) is a general purpose implementation
/// of the [`NodeComponentsBuilder`] trait that can be used to configure the executor, network,
/// transaction pool and payload builder of the node. It enforces the correct order of
/// configuration, for example the network and the payload builder depend on the transaction pool
/// type that is configured first.
///
/// All builder traits are generic over the node types and are invoked with the [`BuilderContext`]
/// that gives access to internals of the that are needed to configure the components. This include
/// the original config, chain spec, the database provider and the task executor,
///
/// ## Hooks
///
/// Once all the components are configured, the builder can be used to set hooks that are run at
/// specific points in the node's lifecycle. This way custom services can be spawned before the node
/// is launched [`NodeBuilderWithComponents::on_component_initialized`], or once the rpc server(s)
/// are launched [`NodeBuilderWithComponents::on_rpc_started`]. The
/// [`NodeBuilderWithComponents::extend_rpc_modules`] can be used to inject custom rpc modules into
/// the rpc server before it is launched. See also [`RpcContext`] All hooks accept a closure that is
/// then invoked at the appropriate time in the node's launch process.
///
/// ## Flow
///
/// The [`NodeBuilder`] is intended to sit behind a CLI that provides the necessary [`NodeConfig`]
/// input: [`NodeBuilder::new`]
///
/// From there the builder is configured with the node's types, components, and hooks, then launched
/// with the [`WithLaunchContext::launch`] method. On launch all the builtin internals, such as the
/// `Database` and its providers [`BlockchainProvider`] are initialized before the configured
/// [`NodeComponentsBuilder`] is invoked with the [`BuilderContext`] to create the transaction pool,
/// network, and payload builder components. When the RPC is configured, the corresponding hooks are
/// invoked to allow for custom rpc modules to be injected into the rpc server:
/// [`NodeBuilderWithComponents::extend_rpc_modules`]
///
/// Finally all components are created and all services are launched and a [`NodeHandle`] is
/// returned that can be used to interact with the node: [`FullNode`]
///
/// The following diagram shows the flow of the node builder from CLI to a launched node.
///
/// include_mmd!("docs/mermaid/builder.mmd")
///
/// ## Internals
///
/// The node builder is fully type safe, it uses the [`NodeTypes`] trait to enforce that
/// all components are configured with the correct types. However the database types and with that
/// the provider trait implementations are currently created by the builder itself during the launch
/// process, hence the database type is not part of the [`NodeTypes`] trait and the node's
/// components, that depend on the database, are configured separately. In order to have a nice
/// trait that encapsulates the entire node the
/// [`FullNodeComponents`](smartnet_node_api::FullNodeComponents) trait was introduced. This
/// trait has convenient associated types for all the components of the node. After
/// [`WithLaunchContext::launch`] the [`NodeHandle`] contains an instance of [`FullNode`] that
/// implements the [`FullNodeComponents`](smartnet_node_api::FullNodeComponents) trait and has
/// access to all the components of the node. Internally the node builder uses several generic
/// adapter types that are then map to traits with associated types for ease of use.
///
/// ### Limitations
///
/// Currently the launch process is limited to ethereum nodes and requires all the components
/// specified above. It also expects beacon consensus with the ethereum engine API that is
/// configured by the builder itself during launch. This might change in the future.
///
/// [builder]: https://doc.rust-lang.org/1.0.0/style/ownership/builders.html
pub struct NodeBuilder<DB, ChainSpec> {
    /// All settings for how the node should be configured.
    config: NodeConfig<ChainSpec>,
    /// The configured database for the node.
    database: DB,
    /// An optional [`AuxDbProvider`] to use instead of creating one during launch.
    aux_db_provider: Option<AuxDbProvider>,
}

impl<ChainSpec> NodeBuilder<(), ChainSpec> {
    /// Create a new [`NodeBuilder`].
    pub const fn new(config: NodeConfig<ChainSpec>) -> Self {
        Self { config, database: (), aux_db_provider: None }
    }
}

impl<DB, ChainSpec> NodeBuilder<DB, ChainSpec> {
    /// Returns a reference to the node builder's config.
    pub const fn config(&self) -> &NodeConfig<ChainSpec> {
        &self.config
    }

    /// Returns a mutable reference to the node builder's config.
    pub const fn config_mut(&mut self) -> &mut NodeConfig<ChainSpec> {
        &mut self.config
    }

    /// Returns a reference to the node's database
    pub const fn db(&self) -> &DB {
        &self.database
    }

    /// Returns a mutable reference to the node's database
    pub const fn db_mut(&mut self) -> &mut DB {
        &mut self.database
    }

    /// Applies a fallible function to the builder.
    pub fn try_apply<F, R>(self, f: F) -> Result<Self, R>
    where
        F: FnOnce(Self) -> Result<Self, R>,
    {
        f(self)
    }

    /// Applies a fallible function to the builder, if the condition is `true`.
    pub fn try_apply_if<F, R>(self, cond: bool, f: F) -> Result<Self, R>
    where
        F: FnOnce(Self) -> Result<Self, R>,
    {
        if cond {
            f(self)
        } else {
            Ok(self)
        }
    }

    /// Apply a function to the builder
    pub fn apply<F>(self, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        f(self)
    }

    /// Apply a function to the builder, if the condition is `true`.
    pub fn apply_if<F>(self, cond: bool, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        if cond {
            f(self)
        } else {
            self
        }
    }
}

impl<DB, ChainSpec: EthChainSpec> NodeBuilder<DB, ChainSpec> {
    /// Configures the underlying database that the node will use.
    pub fn with_database<D>(self, database: D) -> NodeBuilder<D, ChainSpec> {
        NodeBuilder { config: self.config, database, aux_db_provider: self.aux_db_provider }
    }

    /// Sets the [`AuxDbProvider`] to use instead of creating one during launch.
    pub const fn with_aux_db_provider(mut self, aux_db_provider: AuxDbProvider) -> Self {
        self.aux_db_provider = Some(aux_db_provider);
        self
    }

    /// Preconfigure the builder with the context to launch the node.
    ///
    /// This provides the task executor and the data directory for the node.
    pub const fn with_launch_context(self, task_executor: TaskExecutor) -> WithLaunchContext<Self> {
        WithLaunchContext { builder: self, task_executor }
    }

    /// Creates an _ephemeral_ preconfigured node for testing purposes.
    #[cfg(feature = "test-utils")]
    pub fn testing_node(
        self,
        task_executor: TaskExecutor,
    ) -> WithLaunchContext<
        NodeBuilder<
            Arc<smartnet_db::test_utils::TempDatabase<smartnet_db::DatabaseEnv>>,
            ChainSpec,
        >,
    > {
        let path = smartnet_db::test_utils::tempdir_path();
        self.testing_node_with_datadir(task_executor, path)
    }

    /// Creates a preconfigured node for testing purposes with a specific datadir.
    ///
    /// The entire `datadir` will be cleaned up when the node is dropped.
    #[cfg(feature = "test-utils")]
    pub fn testing_node_with_datadir(
        mut self,
        task_executor: TaskExecutor,
        datadir: impl Into<std::path::PathBuf>,
    ) -> WithLaunchContext<
        NodeBuilder<
            Arc<smartnet_db::test_utils::TempDatabase<smartnet_db::DatabaseEnv>>,
            ChainSpec,
        >,
    > {
        // Scope the given datadir as `<datadir>/data` so the engine segment
        // (`<parent>/<engine>/<name>`) lands inside the caller's directory — test callers
        // pass tempdirs that must contain everything this node writes.
        let path = smartnet_node_core::dirs::MaybePlatformPath::<
            smartnet_node_core::dirs::DataDirPath,
        >::from(datadir.into().join("data"));
        self.config = self.config.with_datadir_args(smartnet_node_core::args::DatadirArgs {
            datadir: path.clone(),
            ..Default::default()
        });

        let data_dir =
            path.unwrap_or_chain_default(self.config.chain.chain(), self.config.datadir.clone());

        let db = smartnet_db::test_utils::create_test_rw_db_with_datadir(data_dir.data_dir());

        WithLaunchContext { builder: self.with_database(db), task_executor }
    }
}

impl<DB, ChainSpec> NodeBuilder<DB, ChainSpec>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    ChainSpec: EthChainSpec + EthereumHardforks,
{
    /// Configures the types of the node.
    pub fn with_types<T>(self) -> NodeBuilderWithTypes<SmartnetFullAdapter<DB, T>>
    where
        T: NodeTypesForProvider<ChainSpec = ChainSpec>,
    {
        self.with_types_and_provider()
    }

    /// Configures the types of the node and the provider type that will be used by the node.
    pub fn with_types_and_provider<T, P>(
        self,
    ) -> NodeBuilderWithTypes<FullNodeTypesAdapter<T, DB, P>>
    where
        T: NodeTypesForProvider<ChainSpec = ChainSpec>,
        P: FullProvider<NodeTypesWithDBAdapter<T, DB>>,
    {
        NodeBuilderWithTypes::new(self.config, self.database, self.aux_db_provider)
    }

    /// Preconfigures the node with a specific node implementation.
    ///
    /// This is a convenience method that sets the node's types and components in one call.
    pub fn node<N>(
        self,
        node: N,
    ) -> NodeBuilderWithComponents<SmartnetFullAdapter<DB, N>, N::ComponentsBuilder, N::AddOns>
    where
        N: Node<SmartnetFullAdapter<DB, N>, ChainSpec = ChainSpec> + NodeTypesForProvider,
    {
        self.with_types().with_components(node.components_builder()).with_add_ons(node.add_ons())
    }
}

/// A [`NodeBuilder`] with its launch context already configured.
///
/// This exposes the same methods as [`NodeBuilder`] but with the launch context already configured,
/// See [`WithLaunchContext::launch`]
pub struct WithLaunchContext<Builder> {
    builder: Builder,
    task_executor: TaskExecutor,
}

impl<Builder> WithLaunchContext<Builder> {
    /// Returns a reference to the task executor.
    pub const fn task_executor(&self) -> &TaskExecutor {
        &self.task_executor
    }
}

impl<DB, ChainSpec> WithLaunchContext<NodeBuilder<DB, ChainSpec>> {
    /// Returns a reference to the node builder's config.
    pub const fn config(&self) -> &NodeConfig<ChainSpec> {
        self.builder.config()
    }

    /// Returns a mutable reference to the node builder's config.
    pub const fn config_mut(&mut self) -> &mut NodeConfig<ChainSpec> {
        self.builder.config_mut()
    }
}

impl<DB, ChainSpec> WithLaunchContext<NodeBuilder<DB, ChainSpec>>
where
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
    ChainSpec: EthChainSpec + EthereumHardforks,
{
    /// Sets the [`AuxDbProvider`] to use instead of creating one during launch.
    pub const fn with_aux_db_provider(mut self, aux_db_provider: AuxDbProvider) -> Self {
        self.builder.aux_db_provider = Some(aux_db_provider);
        self
    }

    /// Configures the types of the node.
    pub fn with_types<T>(
        self,
    ) -> WithLaunchContext<NodeBuilderWithTypes<SmartnetFullAdapter<DB, T>>>
    where
        T: NodeTypesForProvider<ChainSpec = ChainSpec>,
    {
        WithLaunchContext { builder: self.builder.with_types(), task_executor: self.task_executor }
    }

    /// Configures the types of the node and the provider type that will be used by the node.
    pub fn with_types_and_provider<T, P>(
        self,
    ) -> WithLaunchContext<NodeBuilderWithTypes<FullNodeTypesAdapter<T, DB, P>>>
    where
        T: NodeTypesForProvider<ChainSpec = ChainSpec>,
        P: FullProvider<NodeTypesWithDBAdapter<T, DB>>,
    {
        WithLaunchContext {
            builder: self.builder.with_types_and_provider(),
            task_executor: self.task_executor,
        }
    }

    /// Preconfigures the node with a specific node implementation.
    ///
    /// This is a convenience method that sets the node's types and components in one call.
    pub fn node<N>(
        self,
        node: N,
    ) -> WithLaunchContext<
        NodeBuilderWithComponents<SmartnetFullAdapter<DB, N>, N::ComponentsBuilder, N::AddOns>,
    >
    where
        N: Node<SmartnetFullAdapter<DB, N>, ChainSpec = ChainSpec> + NodeTypesForProvider,
    {
        self.with_types().with_components(node.components_builder()).with_add_ons(node.add_ons())
    }

    /// Launches a preconfigured [Node]
    ///
    /// This bootstraps the node internals, creates all the components with the given [Node]
    ///
    /// Returns a [`NodeHandle`](crate::NodeHandle) that can be used to interact with the node.
    pub async fn launch_node<N>(
        self,
        node: N,
    ) -> eyre::Result<
        <EngineNodeLauncher as LaunchNode<
            NodeBuilderWithComponents<SmartnetFullAdapter<DB, N>, N::ComponentsBuilder, N::AddOns>,
        >>::Node,
    >
    where
        N: Node<SmartnetFullAdapter<DB, N>, ChainSpec = ChainSpec> + NodeTypesForProvider,
        N::AddOns: SmartnetRpcAddOns<
            NodeAdapter<
                SmartnetFullAdapter<DB, N>,
                <N::ComponentsBuilder as NodeComponentsBuilder<SmartnetFullAdapter<DB, N>>>::Components,
            >,
        >,
        EngineNodeLauncher: LaunchNode<
            NodeBuilderWithComponents<SmartnetFullAdapter<DB, N>, N::ComponentsBuilder, N::AddOns>,
        >,
{
        self.node(node).launch().await
    }
}

impl<T: FullNodeTypes> WithLaunchContext<NodeBuilderWithTypes<T>> {
    /// Advances the state of the node builder to the next state where all components are configured
    pub fn with_components<CB>(
        self,
        components_builder: CB,
    ) -> WithLaunchContext<NodeBuilderWithComponents<T, CB, ()>>
    where
        CB: NodeComponentsBuilder<T>,
    {
        WithLaunchContext {
            builder: self.builder.with_components(components_builder),
            task_executor: self.task_executor,
        }
    }
}

impl<T, CB> WithLaunchContext<NodeBuilderWithComponents<T, CB, ()>>
where
    T: FullNodeTypes,
    CB: NodeComponentsBuilder<T>,
{
    /// Advances the state of the node builder to the next state where all customizable
    /// [`NodeAddOns`] types are configured.
    pub fn with_add_ons<AO>(
        self,
        add_ons: AO,
    ) -> WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>
    where
        AO: NodeAddOns<NodeAdapter<T, CB::Components>>,
    {
        WithLaunchContext {
            builder: self.builder.with_add_ons(add_ons),
            task_executor: self.task_executor,
        }
    }
}

impl<T, CB, AO> WithLaunchContext<NodeBuilderWithComponents<T, CB, AO>>
where
    T: FullNodeTypes,
    CB: NodeComponentsBuilder<T>,
    AO: SmartnetRpcAddOns<NodeAdapter<T, CB::Components>>,
{
    /// Returns a reference to the node builder's config.
    pub const fn config(&self) -> &NodeConfig<<T::Types as NodeTypes>::ChainSpec> {
        &self.builder.config
    }

    /// Returns a mutable reference to the node builder's config.
    pub const fn config_mut(&mut self) -> &mut NodeConfig<<T::Types as NodeTypes>::ChainSpec> {
        &mut self.builder.config
    }

    /// Returns a reference to node's database.
    pub const fn db(&self) -> &T::DB {
        &self.builder.adapter.database
    }

    /// Returns a mutable reference to node's database.
    pub const fn db_mut(&mut self) -> &mut T::DB {
        &mut self.builder.adapter.database
    }

    /// Applies a fallible function to the builder.
    pub fn try_apply<F, R>(self, f: F) -> Result<Self, R>
    where
        F: FnOnce(Self) -> Result<Self, R>,
    {
        f(self)
    }

    /// Applies a fallible function to the builder, if the condition is `true`.
    pub fn try_apply_if<F, R>(self, cond: bool, f: F) -> Result<Self, R>
    where
        F: FnOnce(Self) -> Result<Self, R>,
    {
        if cond {
            f(self)
        } else {
            Ok(self)
        }
    }

    /// Apply a function to the builder
    pub fn apply<F>(self, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        f(self)
    }

    /// Apply a function to the builder, if the condition is `true`.
    pub fn apply_if<F>(self, cond: bool, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        if cond {
            f(self)
        } else {
            self
        }
    }

    /// Sets the hook that is run once the node's components are initialized.
    pub fn on_component_initialized<F>(self, hook: F) -> Self
    where
        F: FnOnce(NodeAdapter<T, CB::Components>) -> eyre::Result<()> + Send + 'static,
    {
        Self {
            builder: self.builder.on_component_initialized(hook),
            task_executor: self.task_executor,
        }
    }

    /// Sets the hook that is run once the node has started.
    pub fn on_node_started<F>(self, hook: F) -> Self
    where
        F: FnOnce(FullNode<NodeAdapter<T, CB::Components>, AO>) -> eyre::Result<()>
            + Send
            + 'static,
    {
        Self { builder: self.builder.on_node_started(hook), task_executor: self.task_executor }
    }

    /// Modifies the addons with the given closure.
    ///
    /// This method provides access to methods on the addons type that don't have
    /// direct builder methods. It's useful for advanced configuration scenarios
    /// where you need to call addon-specific methods.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use tower::layer::util::Identity;
    ///
    /// let builder = NodeBuilder::new(config)
    ///     .with_types::<EthereumNode>()
    ///     .with_components(EthereumNode::components())
    ///     .with_add_ons(EthereumAddOns::default())
    ///     .map_add_ons(|addons| addons.with_rpc_middleware(Identity::default()));
    /// ```
    ///
    /// # See also
    ///
    /// - [`NodeAddOns`] trait for available addon types
    /// - [`crate::NodeBuilderWithComponents::extend_rpc_modules`] for RPC module configuration
    pub fn map_add_ons<F>(self, f: F) -> Self
    where
        F: FnOnce(AO) -> AO,
    {
        Self { builder: self.builder.map_add_ons(f), task_executor: self.task_executor }
    }

    /// Sets the hook that is run once the rpc server is started.
    pub fn on_rpc_started<F>(self, hook: F) -> Self
    where
        F: FnOnce(
                RpcContext<'_, NodeAdapter<T, CB::Components>, AO::EthApi>,
                SmartnetRpcServerHandles,
            ) -> eyre::Result<()>
            + Send
            + 'static,
    {
        Self { builder: self.builder.on_rpc_started(hook), task_executor: self.task_executor }
    }

    /// Sets the hook that is run to configure the rpc modules.
    ///
    /// This hook can obtain the node's components (txpool, provider, etc.) and can modify the
    /// modules that the RPC server installs.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use jsonrpsee::{core::RpcResult, proc_macros::rpc};
    ///
    /// #[derive(Clone)]
    /// struct CustomApi<Pool> { pool: Pool }
    ///
    /// #[rpc(server, namespace = "custom")]
    /// impl CustomApi {
    ///     #[method(name = "hello")]
    ///     async fn hello(&self) -> RpcResult<String> {
    ///         Ok("World".to_string())
    ///     }
    /// }
    ///
    /// let node = NodeBuilder::new(config)
    ///     .node(EthereumNode::default())
    ///     .extend_rpc_modules(|ctx| {
    ///         // Access node components, so they can used by the CustomApi
    ///         let pool = ctx.pool().clone();
    ///
    ///         // Add custom RPC namespace
    ///         ctx.modules.merge_configured(CustomApi { pool }.into_rpc())?;
    ///
    ///         Ok(())
    ///     })
    ///     .build()?;
    /// ```
    pub fn extend_rpc_modules<F>(self, hook: F) -> Self
    where
        F: FnOnce(RpcContext<'_, NodeAdapter<T, CB::Components>, AO::EthApi>) -> eyre::Result<()>
            + Send
            + 'static,
    {
        Self { builder: self.builder.extend_rpc_modules(hook), task_executor: self.task_executor }
    }

    /// Launches the node with the given launcher.
    pub async fn launch_with<L>(self, launcher: L) -> eyre::Result<L::Node>
    where
        L: LaunchNode<NodeBuilderWithComponents<T, CB, AO>>,
    {
        launcher.launch_node(self.builder).await
    }

    /// Launches the node with the given closure.
    pub fn launch_with_fn<L, R>(self, launcher: L) -> R
    where
        L: FnOnce(Self) -> R,
    {
        launcher(self)
    }

    /// Check that the builder can be launched
    ///
    /// This is useful when writing tests to ensure that the builder is configured correctly.
    pub const fn check_launch(self) -> Self {
        self
    }

    /// Launches the node with the [`EngineNodeLauncher`] that sets up engine API consensus and rpc
    pub async fn launch(
        self,
    ) -> eyre::Result<<EngineNodeLauncher as LaunchNode<NodeBuilderWithComponents<T, CB, AO>>>::Node>
    where
        EngineNodeLauncher: LaunchNode<NodeBuilderWithComponents<T, CB, AO>>,
    {
        let launcher = self.engine_api_launcher();
        self.builder.launch_with(launcher).await
    }

    /// Returns an [`EngineNodeLauncher`] that can be used to launch the node with engine API
    /// support.
    pub fn engine_api_launcher(&self) -> EngineNodeLauncher {
        let engine_tree_config = self.builder.config.engine.tree_config();
        EngineNodeLauncher::new(
            self.task_executor.clone(),
            self.builder.config.datadir(),
            engine_tree_config,
        )
    }
}

/// Captures the necessary context for building the components of the node.
pub struct BuilderContext<Node: FullNodeTypes> {
    /// The current head of the blockchain at launch.
    pub(crate) head: Head,
    /// The configured provider to interact with the blockchain.
    pub(crate) provider: Node::Provider,
    /// The executor of the node.
    pub(crate) executor: TaskExecutor,
    /// Config container
    pub(crate) config_container: WithConfigs<<Node::Types as NodeTypes>::ChainSpec>,
}

impl<Node: FullNodeTypes> BuilderContext<Node> {
    /// Create a new instance of [`BuilderContext`]
    pub const fn new(
        head: Head,
        provider: Node::Provider,
        executor: TaskExecutor,
        config_container: WithConfigs<<Node::Types as NodeTypes>::ChainSpec>,
    ) -> Self {
        Self { head, provider, executor, config_container }
    }

    /// Returns the configured provider to interact with the blockchain.
    pub const fn provider(&self) -> &Node::Provider {
        &self.provider
    }

    /// Returns the current head of the blockchain at launch.
    pub const fn head(&self) -> Head {
        self.head
    }

    /// Returns the config of the node.
    pub const fn config(&self) -> &NodeConfig<<Node::Types as NodeTypes>::ChainSpec> {
        &self.config_container.config
    }

    /// Returns a mutable reference to the config of the node.
    pub const fn config_mut(&mut self) -> &mut NodeConfig<<Node::Types as NodeTypes>::ChainSpec> {
        &mut self.config_container.config
    }

    /// Returns the loaded reh.toml config.
    pub const fn smartnet_config(&self) -> &smartnet_config::Config {
        &self.config_container.toml_config
    }

    /// Returns the executor of the node.
    ///
    /// This can be used to execute async tasks or functions during the setup.
    pub const fn task_executor(&self) -> &TaskExecutor {
        &self.executor
    }

    /// Returns the chain spec of the node.
    pub fn chain_spec(&self) -> Arc<<Node::Types as NodeTypes>::ChainSpec> {
        self.provider().chain_spec()
    }

    /// Returns the transaction pool config of the node.
    pub fn pool_config(&self) -> PoolConfig {
        self.config().txpool.pool_config()
    }

    /// Returns the config for payload building.
    pub fn payload_builder_config(&self) -> impl PayloadBuilderConfig {
        self.config().builder.clone()
    }
}

impl<Node: FullNodeTypes> std::fmt::Debug for BuilderContext<Node> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuilderContext")
            .field("head", &self.head)
            .field("provider", &std::any::type_name::<Node::Provider>())
            .field("executor", &self.executor)
            .field("config", &self.config())
            .finish()
    }
}
