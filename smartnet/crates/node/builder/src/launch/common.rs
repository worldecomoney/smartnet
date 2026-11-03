//! Helper types that can be used by launchers.
//!
//! ## Launch Context Type System
//!
//! The node launch process uses a type-state pattern to ensure correct initialization
//! order at compile time. Methods are only available when their prerequisites are met.
//!
//! ### Core Types
//!
//! - [`LaunchContext`]: Base context with executor and data directory
//! - [`LaunchContextWith<T>`]: Context with an attached value of type `T`
//! - [`Attached<L, R>`]: Pairs values, preserving both previous (L) and new (R) state
//!
//! ### Helper Attachments
//!
//! - [`WithConfigs`]: Node config + TOML config
//! - [`WithMeteredProvider`]: Provider factory with metrics
//! - [`WithMeteredProviders`]: Provider factory + blockchain provider
//! - [`WithComponents`]: Final form with all components
//!
//! ### Method Availability
//!
//! Methods are implemented on specific type combinations:
//! - `impl<T> LaunchContextWith<T>`: Generic methods available for any attachment
//! - `impl LaunchContextWith<WithConfigs>`: Config-specific methods
//! - `impl LaunchContextWith<Attached<WithConfigs, DB>>`: Database operations
//! - `impl LaunchContextWith<Attached<WithConfigs, ProviderFactory>>`: Provider operations
//! - etc.
//!
//! This ensures correct initialization order without runtime checks.

use crate::{
    components::NodeComponentsBuilder,
    hooks::OnComponentInitializedHook,
    BuilderContext, NodeAdapter, PrimitivesTy,
};
use alloy_eips::eip2124::Head;
use alloy_primitives::{BlockNumber, B256};
use eyre::Context;
use rayon::ThreadPoolBuilder;
use smartnet_chainspec::{Chain, EthChainSpec, EthereumHardforks};
use smartnet_config::{config::EtlConfig, PruneConfig};
use smartnet_db_api::{database::Database, database_metrics::DatabaseMetrics};
use smartnet_evm::ConfigureEvm;
use smartnet_fs_util as fs;
use smartnet_node_api::{FullNodeTypes, NodeTypes, NodeTypesWithDB, NodeTypesWithDBAdapter};
use smartnet_node_core::{
    dirs::{ChainPath, DataDirPath},
    node_config::NodeConfig,
};
use smartnet_provider::{
    init::{init_genesis_with_settings, InitStorageError},
    log_calibri_lite_storage_profile,
    providers::{AuxDbProvider, NodeTypesForProvider, ProviderNodeTypes},
    BlockHashReader, BlockNumReader, MetadataProvider, ProviderError, ProviderFactory,
    ProviderResult, StageCheckpointReader, StorageSettings, StorageSettingsCache,
};
use smartnet_prune::{PruneModes, PrunerBuilder};
use smartnet_stages::{MetricEvent, StageId};
use smartnet_tasks::TaskExecutor;
use smartnet_tracing::tracing::{debug, error, info, warn};
use smartnet_trie_db::ChangesetCache;
use std::{num::NonZeroUsize, sync::Arc, thread::available_parallelism};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::events::{cl::ConsensusLayerHealthEvents, node::NodeEvent};
use futures::{future::Either, stream, Stream, StreamExt};

/// Reusable setup for launching a node.
///
/// This is the entry point for the node launch process. It implements a builder
/// pattern using type-state programming to enforce correct initialization order.
///
/// ## Type Evolution
///
/// Starting from `LaunchContext`, each method transforms the type to reflect
/// accumulated state:
///
/// ```text
/// LaunchContext
///   └─> LaunchContextWith<WithConfigs>
///       └─> LaunchContextWith<Attached<WithConfigs, DB>>
///           └─> LaunchContextWith<Attached<WithConfigs, ProviderFactory>>
///               └─> LaunchContextWith<Attached<WithConfigs, WithMeteredProviders>>
///                   └─> LaunchContextWith<Attached<WithConfigs, WithComponents>>
/// ```
#[derive(Debug, Clone)]
pub struct LaunchContext {
    /// The task executor for the node.
    pub task_executor: TaskExecutor,
    /// The data directory for the node.
    pub data_dir: ChainPath<DataDirPath>,
}

impl LaunchContext {
    /// Create a new instance of the default node launcher.
    pub const fn new(task_executor: TaskExecutor, data_dir: ChainPath<DataDirPath>) -> Self {
        Self { task_executor, data_dir }
    }

    /// Create launch context with attachment.
    pub const fn with<T>(self, attachment: T) -> LaunchContextWith<T> {
        LaunchContextWith { inner: self, attachment }
    }

    /// Loads the node config with the configured `data_dir` and overrides settings according to the
    /// `config`.
    ///
    /// Attaches both the `NodeConfig` and the loaded `reth.toml` config to the launch context.
    pub fn with_loaded_toml_config<ChainSpec>(
        self,
        config: NodeConfig<ChainSpec>,
    ) -> eyre::Result<LaunchContextWith<WithConfigs<ChainSpec>>>
    where
        ChainSpec: EthChainSpec + smartnet_chainspec::EthereumHardforks,
    {
        let toml_config = self.load_toml_config(&config)?;
        Ok(self.with(WithConfigs { config, toml_config }))
    }

    /// Loads the node config with the configured `data_dir` and overrides settings according to the
    /// `config`.
    ///
    /// This is async because the trusted peers may have to be resolved.
    pub fn load_toml_config<ChainSpec>(
        &self,
        config: &NodeConfig<ChainSpec>,
    ) -> eyre::Result<smartnet_config::Config>
    where
        ChainSpec: EthChainSpec + smartnet_chainspec::EthereumHardforks,
    {
        let config_path = config
            .config
            .clone()
            .map(|p| smartnet_node_core::dirs::resolve_config_file(&p))
            .unwrap_or_else(|| self.data_dir.resolve_config());

        // Matches the CLI, which creates the config file with default values on first use
        // (`Config::from_path`), so a fresh datadir can be launched directly.
        let mut toml_config = smartnet_config::Config::from_path(&config_path)
            .wrap_err_with(|| format!("Could not load config file {config_path:?}"))?;

        Self::save_pruning_config(&mut toml_config, config, &config_path)?;

        info!(target: "smartnet::cli", path = ?config_path, "Configuration loaded");

        Ok(toml_config)
    }

    /// Save prune config to the toml file if node is a full node or has custom pruning CLI
    /// arguments. Also migrates deprecated prune config values to new defaults.
    fn save_pruning_config<ChainSpec>(
        smartnet_config: &mut smartnet_config::Config,
        config: &NodeConfig<ChainSpec>,
        config_path: impl AsRef<std::path::Path>,
    ) -> eyre::Result<()>
    where
        ChainSpec: EthChainSpec + smartnet_chainspec::EthereumHardforks,
    {
        let mut should_save = smartnet_config.prune.segments.migrate();

        if let Some(prune_config) = config.prune_config() {
            if smartnet_config.prune != prune_config {
                smartnet_config.set_prune_config(prune_config);
                should_save = true;
            }
        } else if !smartnet_config.prune.is_default() {
            info!(target: "smartnet::cli", "Pruning configuration is present in the config file, but no CLI arguments are provided. Using config from file.");
        }

        if should_save {
            info!(target: "smartnet::cli", "Saving prune config to toml file");
            smartnet_config.save(config_path.as_ref())?;
        }

        Ok(())
    }

    /// Convenience function to [`Self::configure_globals`]
    pub fn with_configured_globals(self, reserved_cpu_cores: usize) -> Self {
        self.configure_globals(reserved_cpu_cores);
        self
    }

    /// Configure global settings this includes:
    ///
    /// - Raising the file descriptor limit
    /// - Configuring the global rayon thread pool for implicit `par_iter` usage
    pub fn configure_globals(&self, reserved_cpu_cores: usize) {
        // Raise the fd limit of the process.
        // Does not do anything on windows.
        match fdlimit::raise_fd_limit() {
            Ok(fdlimit::Outcome::LimitRaised { from, to }) => {
                debug!(from, to, "Raised file descriptor limit");
            }
            Ok(fdlimit::Outcome::Unsupported) => {}
            Err(err) => warn!(%err, "Failed to raise file descriptor limit"),
        }

        // Configure the implicit global rayon pool for `par_iter` usage.
        // TODO: reserved_cpu_cores is currently ignored because subtracting from thread pool
        // sizes doesn't actually reserve CPU cores for other processes.
        let _ = reserved_cpu_cores;
        let num_threads = available_parallelism().map_or(1, NonZeroUsize::get);
        if let Err(err) = ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(|i| format!("rayon-{i:02}"))
            .build_global()
        {
            warn!(%err, "Failed to build global thread pool")
        }
    }
}

/// A [`LaunchContext`] along with an additional value.
///
/// The type parameter `T` represents the current state of the launch process.
/// Methods are conditionally implemented based on `T`, ensuring operations
/// are only available when their prerequisites are met.
///
/// For example:
/// - Config methods when `T = WithConfigs<ChainSpec>`
/// - Database operations when `T = Attached<WithConfigs<ChainSpec>, DB>`
/// - Provider operations when `T = Attached<WithConfigs<ChainSpec>, ProviderFactory<N>>`
#[derive(Debug, Clone)]
pub struct LaunchContextWith<T> {
    /// The wrapped launch context.
    pub inner: LaunchContext,
    /// The additional attached value.
    pub attachment: T,
}

impl<T> LaunchContextWith<T> {
    /// Configure global settings this includes:
    ///
    /// - Raising the file descriptor limit
    /// - Configuring the global rayon thread pool
    pub fn configure_globals(&self, reserved_cpu_cores: u64) {
        self.inner.configure_globals(reserved_cpu_cores.try_into().unwrap());
    }

    /// Returns the data directory.
    pub const fn data_dir(&self) -> &ChainPath<DataDirPath> {
        &self.inner.data_dir
    }

    /// Returns the task executor.
    pub const fn task_executor(&self) -> &TaskExecutor {
        &self.inner.task_executor
    }

    /// Attaches another value to the launch context.
    pub fn attach<A>(self, attachment: A) -> LaunchContextWith<Attached<T, A>> {
        LaunchContextWith {
            inner: self.inner,
            attachment: Attached::new(self.attachment, attachment),
        }
    }

    /// Consumes the type and calls a function with a reference to the context.
    // Returns the context again
    pub fn inspect<F>(self, f: F) -> Self
    where
        F: FnOnce(&Self),
    {
        f(&self);
        self
    }
}

impl<L, R> LaunchContextWith<Attached<L, R>> {
    /// Get a reference to the left value.
    pub const fn left(&self) -> &L {
        &self.attachment.left
    }

    /// Get a reference to the right value.
    pub const fn right(&self) -> &R {
        &self.attachment.right
    }

    /// Get a mutable reference to the left value.
    pub const fn left_mut(&mut self) -> &mut L {
        &mut self.attachment.left
    }

    /// Get a mutable reference to the right value.
    pub const fn right_mut(&mut self) -> &mut R {
        &mut self.attachment.right
    }
}
impl<R, ChainSpec: EthChainSpec> LaunchContextWith<Attached<WithConfigs<ChainSpec>, R>> {
    /// Adjust certain settings in the config to make sure they are set correctly
    ///
    /// This includes:
    /// - Making sure the ETL dir is set to the datadir
    /// - RPC settings are adjusted to the correct port
    pub fn with_adjusted_configs(self) -> Self {
        self.ensure_etl_datadir().with_adjusted_instance_ports()
    }

    /// Make sure ETL uses the configured datadir rather than a fixed system temp location.
    pub fn ensure_etl_datadir(mut self) -> Self {
        if self.toml_config_mut().stages.etl.dir.is_none() {
            let etl_path = EtlConfig::from_datadir(self.data_dir().data_dir());
            if etl_path.exists() {
                // Remove etl-path files on launch
                if let Err(err) = fs::remove_dir_all(&etl_path) {
                    warn!(target: "smartnet::cli", ?etl_path, %err, "Failed to remove ETL path on launch");
                }
            }
            self.toml_config_mut().stages.etl.dir = Some(etl_path);
        }

        self
    }

    /// Change rpc port numbers based on the instance number.
    pub fn with_adjusted_instance_ports(mut self) -> Self {
        self.node_config_mut().adjust_instance_ports();
        self
    }

    /// Returns the container for all config types
    pub const fn configs(&self) -> &WithConfigs<ChainSpec> {
        self.attachment.left()
    }

    /// Returns the attached [`NodeConfig`].
    pub const fn node_config(&self) -> &NodeConfig<ChainSpec> {
        &self.left().config
    }

    /// Returns the attached [`NodeConfig`].
    pub const fn node_config_mut(&mut self) -> &mut NodeConfig<ChainSpec> {
        &mut self.left_mut().config
    }

    /// Returns the attached toml config [`smartnet_config::Config`].
    pub const fn toml_config(&self) -> &smartnet_config::Config {
        &self.left().toml_config
    }

    /// Returns the attached toml config [`smartnet_config::Config`].
    pub const fn toml_config_mut(&mut self) -> &mut smartnet_config::Config {
        &mut self.left_mut().toml_config
    }

    /// Returns the configured chain spec.
    pub fn chain_spec(&self) -> Arc<ChainSpec> {
        self.node_config().chain.clone()
    }

    /// Get the hash of the genesis block.
    pub fn genesis_hash(&self) -> B256 {
        self.node_config().chain.genesis_hash()
    }

    /// Returns the chain identifier of the node.
    pub fn chain_id(&self) -> Chain {
        self.node_config().chain.chain()
    }

    /// Returns the configured [`PruneConfig`]
    ///
    /// Any configuration set in CLI will take precedence over those set in toml
    pub fn prune_config(&self) -> PruneConfig
    where
        ChainSpec: smartnet_chainspec::EthereumHardforks,
    {
        let Some(mut node_prune_config) = self.node_config().prune_config() else {
            // No CLI config is set, use the toml config.
            return self.toml_config().prune.clone();
        };

        // Otherwise, use the CLI configuration and merge with toml config.
        node_prune_config.merge(self.toml_config().prune.clone());
        node_prune_config
    }

    /// Returns the configured [`PruneModes`], returning the default if no config was available.
    pub fn prune_modes(&self) -> PruneModes
    where
        ChainSpec: smartnet_chainspec::EthereumHardforks,
    {
        self.prune_config().segments
    }

    /// Returns an initialized [`PrunerBuilder`] based on the configured [`PruneConfig`]
    pub fn pruner_builder(&self) -> PrunerBuilder
    where
        ChainSpec: smartnet_chainspec::EthereumHardforks,
    {
        PrunerBuilder::new(self.prune_config())
    }
}

impl<DB, ChainSpec> LaunchContextWith<Attached<WithConfigs<ChainSpec>, DB>>
where
    DB: Database + Clone + 'static,
    ChainSpec: EthChainSpec + EthereumHardforks + 'static,
{
    /// Returns the [`ProviderFactory`] for the attached storage after executing a consistent check
    /// between the database and static files.
    pub async fn create_provider_factory<N, Evm>(
        &self,
        changeset_cache: ChangesetCache,
        aux_db_provider: Option<AuxDbProvider>,
    ) -> eyre::Result<ProviderFactory<N>>
    where
        N: ProviderNodeTypes<DB = DB, ChainSpec = ChainSpec>,
        Evm: ConfigureEvm<Primitives = N::Primitives> + 'static,
    {
        // Legacy cleanup: static files (MDBX-era store) are retired — remove any leftover
        // `static_files/` directory from an old datadir so it never confuses operators or
        // wastes disk. The node never recreates it.
        let legacy_static_files = self.data_dir().data_dir().join("static_files");
        if legacy_static_files.exists() {
            match std::fs::remove_dir_all(&legacy_static_files) {
                Ok(()) => info!(
                    target: "smartnet::cli",
                    path = %legacy_static_files.display(),
                    "removed legacy static_files directory (subsystem retired)"
                ),
                Err(err) => warn!(
                    target: "smartnet::cli",
                    path = %legacy_static_files.display(),
                    %err, "failed to remove legacy static_files directory — ignoring"
                ),
            }
        }

        let prune_config = self.prune_config();
        let factory = ProviderFactory::new(
            self.right().clone(),
            self.chain_spec(),
            aux_db_provider,
            self.task_executor().clone(),
        )?
        .with_prune_modes(prune_config.segments)
        .with_minimum_pruning_distance(prune_config.minimum_pruning_distance)
        .with_changeset_cache(changeset_cache);

        // Log effective persistence profile (persisted metadata overrides factory default).
        let effective_settings = if let Ok(provider) = factory.provider() {
            if let Ok(Some(stored)) = provider.storage_settings() {
                factory.set_storage_settings_cache(stored);
                stored
            } else {
                factory.cached_storage_settings()
            }
        } else {
            factory.cached_storage_settings()
        };
        log_calibri_lite_storage_profile(effective_settings);

        // Heal chain-state block numbers that may be ahead of the highest header
        // (legacy <=1.10.2 datadirs).
        factory.check_consistency()?;

        Ok(factory)
    }

    /// Creates a new [`ProviderFactory`] and attaches it to the launch context.
    pub async fn with_provider_factory<N, Evm>(
        self,
        changeset_cache: ChangesetCache,
        aux_db_provider: Option<AuxDbProvider>,
    ) -> eyre::Result<LaunchContextWith<Attached<WithConfigs<ChainSpec>, ProviderFactory<N>>>>
    where
        N: ProviderNodeTypes<DB = DB, ChainSpec = ChainSpec>,
        Evm: ConfigureEvm<Primitives = N::Primitives> + 'static,
    {
        let factory =
            self.create_provider_factory::<N, Evm>(changeset_cache, aux_db_provider).await?;
        let ctx = LaunchContextWith {
            inner: self.inner,
            attachment: self.attachment.map_right(|_| factory),
        };

        Ok(ctx)
    }
}

impl<T> LaunchContextWith<Attached<WithConfigs<T::ChainSpec>, ProviderFactory<T>>>
where
    T: ProviderNodeTypes,
{
    /// Returns access to the underlying database.
    pub const fn database(&self) -> &T::DB {
        self.right().db_ref()
    }

    /// Returns the configured `ProviderFactory`.
    pub const fn provider_factory(&self) -> &ProviderFactory<T> {
        self.right()
    }

    /// Convenience function to [`Self::init_genesis`]
    pub fn with_genesis(self) -> Result<Self, InitStorageError> {
        init_genesis_with_settings(
            self.provider_factory(),
            StorageSettings::calibri_lite_runtime(),
            None,
        )?;
        Ok(self)
    }

    /// Write the genesis block and state if it has not already been written
    pub fn init_genesis(&self) -> Result<B256, InitStorageError> {
        init_genesis_with_settings(
            self.provider_factory(),
            StorageSettings::calibri_lite_runtime(),
            None,
        )
    }

    /// Creates a new `WithMeteredProvider` container and attaches it to the
    /// launch context.
    ///
    /// This spawns a metrics task that listens for metrics related events and feeds the
    /// internal metrics facade (transport-independent; no exporter is attached).
    pub fn with_metrics_task(
        self,
    ) -> LaunchContextWith<Attached<WithConfigs<T::ChainSpec>, WithMeteredProvider<T>>> {
        let (metrics_sender, metrics_receiver) = unbounded_channel();

        let with_metrics =
            WithMeteredProvider { provider_factory: self.right().clone(), metrics_sender };

        debug!(target: "smartnet::cli", "Spawning stages metrics listener task");
        let sync_metrics_listener = smartnet_stages::MetricsListener::new(metrics_receiver);
        self.task_executor()
            .spawn_critical_task("stages metrics listener task", sync_metrics_listener);

        LaunchContextWith {
            inner: self.inner,
            attachment: self.attachment.map_right(|_| with_metrics),
        }
    }
}

impl<N, DB>
    LaunchContextWith<
        Attached<WithConfigs<N::ChainSpec>, WithMeteredProvider<NodeTypesWithDBAdapter<N, DB>>>,
    >
where
    N: NodeTypes,
    DB: Database + DatabaseMetrics + Clone + Unpin + 'static,
{
    /// Returns the configured `ProviderFactory`.
    const fn provider_factory(&self) -> &ProviderFactory<NodeTypesWithDBAdapter<N, DB>> {
        &self.right().provider_factory
    }

    /// Returns the metrics sender.
    fn sync_metrics_tx(&self) -> UnboundedSender<MetricEvent> {
        self.right().metrics_sender.clone()
    }

    /// Creates a `BlockchainProvider` and attaches it to the launch context.
    #[expect(clippy::complexity)]
    pub fn with_blockchain_db<T, F>(
        self,
        create_blockchain_provider: F,
    ) -> eyre::Result<LaunchContextWith<Attached<WithConfigs<N::ChainSpec>, WithMeteredProviders<T>>>>
    where
        T: FullNodeTypes<Types = N, DB = DB>,
        F: FnOnce(ProviderFactory<NodeTypesWithDBAdapter<N, DB>>) -> eyre::Result<T::Provider>,
    {
        let blockchain_db = create_blockchain_provider(self.provider_factory().clone())?;

        let metered_providers = WithMeteredProviders {
            db_provider_container: WithMeteredProvider {
                provider_factory: self.provider_factory().clone(),
                metrics_sender: self.sync_metrics_tx(),
            },
            blockchain_db,
        };

        let ctx = LaunchContextWith {
            inner: self.inner,
            attachment: self.attachment.map_right(|_| metered_providers),
        };

        Ok(ctx)
    }
}

impl<T>
    LaunchContextWith<
        Attached<WithConfigs<<T::Types as NodeTypes>::ChainSpec>, WithMeteredProviders<T>>,
    >
where
    T: FullNodeTypes<Types: NodeTypesForProvider>,
{
    /// Returns access to the underlying database.
    pub const fn database(&self) -> &T::DB {
        self.provider_factory().db_ref()
    }

    /// Returns the configured `ProviderFactory`.
    pub const fn provider_factory(
        &self,
    ) -> &ProviderFactory<NodeTypesWithDBAdapter<T::Types, T::DB>> {
        &self.right().db_provider_container.provider_factory
    }

    /// Fetches the head block from the database.
    ///
    /// If the database is empty, returns the genesis block.
    pub fn lookup_head(&self) -> eyre::Result<Head> {
        self.node_config()
            .lookup_head(self.provider_factory())
            .wrap_err("the head block is missing")
    }

    /// Returns the metrics sender.
    pub fn sync_metrics_tx(&self) -> UnboundedSender<MetricEvent> {
        self.right().db_provider_container.metrics_sender.clone()
    }

    /// Returns a reference to the blockchain provider.
    pub const fn blockchain_db(&self) -> &T::Provider {
        &self.right().blockchain_db
    }

    /// Creates a `NodeAdapter` and attaches it to the launch context.
    pub async fn with_components<CB>(
        self,
        components_builder: CB,
        on_component_initialized: Box<
            dyn OnComponentInitializedHook<NodeAdapter<T, CB::Components>>,
        >,
    ) -> eyre::Result<
        LaunchContextWith<
            Attached<WithConfigs<<T::Types as NodeTypes>::ChainSpec>, WithComponents<T, CB>>,
        >,
    >
    where
        CB: NodeComponentsBuilder<T>,
    {
        // fetch the head block from the database
        let head = self.lookup_head()?;

        let builder_ctx = BuilderContext::new(
            head,
            self.blockchain_db().clone(),
            self.task_executor().clone(),
            self.configs().clone(),
        );

        debug!(target: "smartnet::cli", "creating components");
        let components = components_builder.build_components(&builder_ctx).await?;

        let blockchain_db = self.blockchain_db().clone();

        let node_adapter = NodeAdapter {
            components,
            task_executor: self.task_executor().clone(),
            provider: blockchain_db,
        };

        debug!(target: "smartnet::cli", "calling on_component_initialized hook");
        on_component_initialized.on_event(node_adapter.clone())?;

        let components_container = WithComponents {
            db_provider_container: WithMeteredProvider {
                provider_factory: self.provider_factory().clone(),
                metrics_sender: self.sync_metrics_tx(),
            },
            node_adapter,
            head,
        };

        let ctx = LaunchContextWith {
            inner: self.inner,
            attachment: self.attachment.map_right(|_| components_container),
        };

        Ok(ctx)
    }
}

impl<T, CB>
    LaunchContextWith<
        Attached<WithConfigs<<T::Types as NodeTypes>::ChainSpec>, WithComponents<T, CB>>,
    >
where
    T: FullNodeTypes<Types: NodeTypesForProvider>,
    CB: NodeComponentsBuilder<T>,
{
    /// Returns the configured `ProviderFactory`.
    pub const fn provider_factory(
        &self,
    ) -> &ProviderFactory<NodeTypesWithDBAdapter<T::Types, T::DB>> {
        &self.right().db_provider_container.provider_factory
    }

    /// Returns the max block that the node should run to.
    ///
    /// This is a pure config lookup — the former network-tip fetch is gone with p2p.
    pub const fn max_block(&self) -> Option<BlockNumber> {
        self.node_config().max_block()
    }

    /// Returns the current head block.
    pub const fn head(&self) -> Head {
        self.right().head
    }

    /// Returns the configured `NodeAdapter`.
    pub const fn node_adapter(&self) -> &NodeAdapter<T, CB::Components> {
        &self.right().node_adapter
    }

    /// Returns mutable reference to the configured `NodeAdapter`.
    pub const fn node_adapter_mut(&mut self) -> &mut NodeAdapter<T, CB::Components> {
        &mut self.right_mut().node_adapter
    }

    /// Returns a reference to the blockchain provider.
    pub const fn blockchain_db(&self) -> &T::Provider {
        &self.node_adapter().provider
    }

    /// Returns the initial backfill to sync to at launch.
    ///
    /// This returns the configured `debug.tip` if set, otherwise it will check if backfill was
    /// previously interrupted and returns the block hash of the last checkpoint, see also
    /// [`Self::check_pipeline_consistency`]
    pub fn initial_backfill_target(&self) -> ProviderResult<Option<B256>> {
        let mut initial_target = self.node_config().debug.tip;

        if initial_target.is_none() {
            initial_target = self.check_pipeline_consistency()?;
        }

        Ok(initial_target)
    }

    /// Returns true if the node should terminate after the initial backfill run.
    ///
    /// This is the case if any of these configs are set:
    ///  `--debug.max-block`
    ///  `--debug.terminate`
    pub const fn terminate_after_initial_backfill(&self) -> bool {
        self.node_config().debug.terminate || self.node_config().debug.max_block.is_some()
    }

    /// Ensures that the database matches chain-specific requirements.
    ///
    /// This checks for OP-Mainnet and ensures we have all the necessary data to progress (past
    /// bedrock height)
    fn ensure_chain_specific_db_checks(&self) -> ProviderResult<()> {
        if self.chain_spec().is_optimism() && self.chain_id() == Chain::optimism_mainnet() {
            let latest = self.blockchain_db().last_block_number()?;
            // bedrock height
            if latest < 105235063 {
                error!(
                    "Op-mainnet has been launched without importing the pre-Bedrock state. The chain can't progress without this. See also https://reth.rs/run/sync-op-mainnet.html?minimal-bootstrap-recommended"
                );
                return Err(ProviderError::BestBlockNotFound);
            }
        }

        Ok(())
    }

    /// Check if the pipeline is consistent (all stages have the checkpoint block numbers no less
    /// than the checkpoint of the first stage).
    ///
    /// This will return the pipeline target if:
    ///  * the pipeline was interrupted during its previous run
    ///  * a new stage was added
    ///  * stage data was dropped manually through maintenance tooling
    ///
    /// # Returns
    ///
    /// A target block hash if the pipeline is inconsistent, otherwise `None`.
    pub fn check_pipeline_consistency(&self) -> ProviderResult<Option<B256>> {
        // The era stage was removed together with the p2p/era import subsystem.
        let mut all_stages = StageId::ALL.into_iter().filter(|id| id != &StageId::Era);

        // Get the expected first stage based on config.
        let first_stage = all_stages.next().expect("there must be at least one stage");

        // If no target was provided, check if the stages are congruent - check if the
        // checkpoint of the last stage matches the checkpoint of the first.
        let first_stage_checkpoint = self
            .blockchain_db()
            .get_stage_checkpoint(first_stage)?
            .unwrap_or_default()
            .block_number;

        // Compare all other stages against the first
        for stage_id in all_stages {
            let stage_checkpoint = self
                .blockchain_db()
                .get_stage_checkpoint(stage_id)?
                .unwrap_or_default()
                .block_number;

            // If the checkpoint of any stage is less than the checkpoint of the first stage,
            // retrieve and return the block hash of the latest header and use it as the target.
            debug!(
                target: "consensus::engine",
                first_stage_id = %first_stage,
                first_stage_checkpoint,
                stage_id = %stage_id,
                stage_checkpoint = stage_checkpoint,
                "Checking stage against first stage",
            );
            if stage_checkpoint < first_stage_checkpoint {
                debug!(
                    target: "consensus::engine",
                    first_stage_id = %first_stage,
                    first_stage_checkpoint,
                    inconsistent_stage_id = %stage_id,
                    inconsistent_stage_checkpoint = stage_checkpoint,
                    "Pipeline sync progress is inconsistent"
                );
                return self.blockchain_db().block_hash(first_stage_checkpoint);
            }
        }

        self.ensure_chain_specific_db_checks()?;

        Ok(None)
    }

    /// Returns the metrics sender.
    pub fn sync_metrics_tx(&self) -> UnboundedSender<MetricEvent> {
        self.right().db_provider_container.metrics_sender.clone()
    }

    /// Returns the node adapter components.
    pub const fn components(&self) -> &CB::Components {
        &self.node_adapter().components
    }

    /// Creates consensus layer health events stream based on node configuration.
    ///
    /// Returns a stream that monitors consensus layer health if:
    /// - No debug tip is configured
    ///
    /// Otherwise returns an empty stream.
    pub fn consensus_layer_events(
        &self,
    ) -> impl Stream<Item = NodeEvent<PrimitivesTy<T::Types>>> + 'static
    where
        T::Provider: smartnet_provider::CanonChainTracker,
    {
        if self.node_config().debug.tip.is_none() {
            Either::Left(
                ConsensusLayerHealthEvents::new(Box::new(self.blockchain_db().clone()))
                    .map(Into::into),
            )
        } else {
            Either::Right(stream::empty())
        }
    }

}

/// Joins two attachments together, preserving access to both values.
///
/// This type enables the launch process to accumulate state while maintaining
/// access to all previously attached components. The `left` field holds the
/// previous state, while `right` holds the newly attached component.
#[derive(Clone, Copy, Debug)]
pub struct Attached<L, R> {
    left: L,
    right: R,
}

impl<L, R> Attached<L, R> {
    /// Creates a new `Attached` with the given values.
    pub const fn new(left: L, right: R) -> Self {
        Self { left, right }
    }

    /// Maps the left value to a new value.
    pub fn map_left<F, T>(self, f: F) -> Attached<T, R>
    where
        F: FnOnce(L) -> T,
    {
        Attached::new(f(self.left), self.right)
    }

    /// Maps the right value to a new value.
    pub fn map_right<F, T>(self, f: F) -> Attached<L, T>
    where
        F: FnOnce(R) -> T,
    {
        Attached::new(self.left, f(self.right))
    }

    /// Get a reference to the left value.
    pub const fn left(&self) -> &L {
        &self.left
    }

    /// Get a reference to the right value.
    pub const fn right(&self) -> &R {
        &self.right
    }

    /// Get a mutable reference to the left value.
    pub const fn left_mut(&mut self) -> &mut L {
        &mut self.left
    }

    /// Get a mutable reference to the right value.
    pub const fn right_mut(&mut self) -> &mut R {
        &mut self.right
    }
}

/// Helper container type to bundle the initial [`NodeConfig`] and the loaded settings from the
/// reth.toml config
#[derive(Debug)]
pub struct WithConfigs<ChainSpec> {
    /// The configured, usually derived from the CLI.
    pub config: NodeConfig<ChainSpec>,
    /// The loaded reth.toml config.
    pub toml_config: smartnet_config::Config,
}

impl<ChainSpec> Clone for WithConfigs<ChainSpec> {
    fn clone(&self) -> Self {
        Self { config: self.config.clone(), toml_config: self.toml_config.clone() }
    }
}

/// Helper container type to bundle the [`ProviderFactory`] and the metrics
/// sender.
#[derive(Debug, Clone)]
pub struct WithMeteredProvider<N: NodeTypesWithDB> {
    provider_factory: ProviderFactory<N>,
    metrics_sender: UnboundedSender<MetricEvent>,
}

/// Helper container to bundle the [`ProviderFactory`], [`FullNodeTypes::Provider`]
/// and a metrics sender.
#[expect(missing_debug_implementations)]
pub struct WithMeteredProviders<T>
where
    T: FullNodeTypes,
{
    db_provider_container: WithMeteredProvider<NodeTypesWithDBAdapter<T::Types, T::DB>>,
    blockchain_db: T::Provider,
}

/// Helper container to bundle the metered providers container and [`NodeAdapter`].
#[expect(missing_debug_implementations)]
pub struct WithComponents<T, CB>
where
    T: FullNodeTypes,
    CB: NodeComponentsBuilder<T>,
{
    db_provider_container: WithMeteredProvider<NodeTypesWithDBAdapter<T::Types, T::DB>>,
    node_adapter: NodeAdapter<T, CB::Components>,
    head: Head,
}

#[cfg(test)]
mod tests {
    use super::{LaunchContext, NodeConfig};
    use smartnet_config::Config;
    use smartnet_node_core::args::PruningArgs;

    const EXTENSION: &str = "toml";

    fn with_tempdir(filename: &str, proc: fn(&std::path::Path)) {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join(filename).with_extension(EXTENSION);
        proc(&config_path);
        temp_dir.close().unwrap()
    }

    #[test]
    fn test_save_prune_config() {
        with_tempdir("prune-store-test", |config_path| {
            let mut smartnet_config = Config::default();
            let node_config = NodeConfig {
                pruning: PruningArgs {
                    full: true,
                    minimal: false,
                    block_interval: None,
                    sender_recovery_full: false,
                    sender_recovery_distance: None,
                    sender_recovery_before: None,
                    transaction_lookup_full: false,
                    transaction_lookup_distance: None,
                    transaction_lookup_before: None,
                    receipts_full: false,
                    receipts_pre_merge: false,
                    receipts_distance: None,
                    receipts_before: None,
                    account_history_full: false,
                    account_history_distance: None,
                    account_history_before: None,
                    storage_history_full: false,
                    storage_history_distance: None,
                    storage_history_before: None,
                    bodies_pre_merge: false,
                    bodies_distance: None,
                    receipts_log_filter: None,
                    bodies_before: None,
                    minimum_distance: None,
                    changeset_window: None,
                },
                ..NodeConfig::test()
            };
            LaunchContext::save_pruning_config(&mut smartnet_config, &node_config, config_path)
                .unwrap();

            let loaded_config = Config::from_path(config_path).unwrap();

            assert_eq!(smartnet_config, loaded_config);
        })
    }
}
