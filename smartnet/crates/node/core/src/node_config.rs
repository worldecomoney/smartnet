//! Support for customizing the node

use crate::{
    args::{
        DatabaseArgs, DatadirArgs, DebugArgs, EngineArgs, PayloadBuilderArgs, PruningArgs,
        RpcServerArgs, TxPoolArgs,
    },
    dirs::{ChainPath, DataDirPath},
};
use alloy_consensus::BlockHeader;
use alloy_primitives::{BlockNumber, U256};
use eyre::eyre;
use smartnet_ethereum_forks::{EthereumHardforks, Head};
use smartnet_stages_types::StageId;
use serde::{de::DeserializeOwned, Serialize};
use smartnet_chainspec::{dev_chain_spec, ChainSpec, EthChainSpec, MAINNET};
use smartnet_config::config::PruneConfig;
use smartnet_storage_api::{
    BlockHashReader, DatabaseProviderFactory, HeaderProvider, StageCheckpointReader,
};
use smartnet_storage_errors::provider::ProviderResult;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

pub use smartnet_engine_primitives::{
    DEFAULT_MEMORY_BLOCK_BUFFER_TARGET, DEFAULT_PERSISTENCE_THRESHOLD, DEFAULT_RESERVED_CPU_CORES,
};

/// Default size of cross-block cache in megabytes.
pub const DEFAULT_CROSS_BLOCK_CACHE_SIZE_MB: usize = 4 * 1024;

/// This includes all necessary configuration to launch the node.
/// The individual configuration options can be overwritten before launching the node.
///
/// # Example
/// ```rust
/// # use smartnet_node_core::{
/// #     node_config::NodeConfig,
/// #     args::RpcServerArgs,
/// # };
/// # use smartnet_rpc_server_types::RpcModuleSelection;
/// # use tokio::runtime::Handle;
///
/// async fn t() {
///     // create the builder
///     let builder = NodeConfig::default();
///
///     // configure the rpc apis
///     let mut rpc = RpcServerArgs::default().with_http();
///     rpc.http_api = Some(RpcModuleSelection::All);
///     let builder = builder.with_rpc(rpc);
/// }
/// ```
///
/// This can also be used to launch a node with a temporary test database. This can be done with
/// the [`NodeConfig::test`] method.
///
/// # Example
/// ```rust
/// # use smartnet_node_core::{
/// #     node_config::NodeConfig,
/// #     args::RpcServerArgs,
/// # };
/// # use smartnet_rpc_server_types::RpcModuleSelection;
/// # use tokio::runtime::Handle;
///
/// async fn t() {
///     // create the builder with a test database, using the `test` method
///     let builder = NodeConfig::test();
///
///     // configure the rpc apis
///     let mut rpc = RpcServerArgs::default().with_http();
///     rpc.http_api = Some(RpcModuleSelection::All);
///     let builder = builder.with_rpc(rpc);
/// }
/// ```
#[derive(Debug)]
pub struct NodeConfig<ChainSpec> {
    /// All data directory related arguments
    pub datadir: DatadirArgs,

    /// The path to the configuration file to use.
    pub config: Option<PathBuf>,

    /// Whether this launch may arm the merged validator daemon (the heartbeat seal
    /// beats). Defaults to `true`: the operator config's `SEAL` key then decides as
    /// before. `smartnet import` clears it — importing never runs the validator (there
    /// is no case for it: import and the sync path both feed the engine directly).
    pub validator_daemon: bool,

    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    pub chain: Arc<ChainSpec>,

    /// Add a new instance of a node.
    ///
    /// Configures the ports of the node to avoid conflicts with the defaults.
    /// This is useful for running multiple nodes on the same machine.
    ///
    /// Max number of instances is 200. It is chosen in a way so that it's not possible to have
    /// port numbers that conflict with each other.
    ///
    /// Changes to the following port numbers:
    /// - `AUTH_PORT`: default + `instance` * 100 - 100
    /// - `HTTP_RPC_PORT`: default - `instance` + 1
    /// - `WS_RPC_PORT`: default + `instance` * 2 - 2
    /// - (removed) `IPC_PATH`: not used — HTTP/WS only
    pub instance: Option<u16>,

    /// All rpc related arguments
    pub rpc: RpcServerArgs,

    /// All txpool related arguments with --txpool prefix
    pub txpool: TxPoolArgs,

    /// All payload builder related arguments
    pub builder: PayloadBuilderArgs,

    /// All debug related arguments with --debug prefix
    pub debug: DebugArgs,

    /// All database related arguments
    pub db: DatabaseArgs,

    /// All pruning related arguments
    pub pruning: PruningArgs,

    /// All engine related arguments
    pub engine: EngineArgs,
}

impl NodeConfig<ChainSpec> {
    /// Creates a testing [`NodeConfig`], causing the database to be launched ephemerally.
    ///
    /// Uses the dev chain spec rather than [`MAINNET`]: the node bootstraps its chain id and
    /// genesis signer list at boot and exits if either is missing, and only the dev genesis
    /// carries a signer in its `extraData`.
    pub fn test() -> Self {
        Self::new(dev_chain_spec())
            // set all ports to zero by default for test instances
            .with_unused_ports()
    }
}

impl<ChainSpec> NodeConfig<ChainSpec> {
    /// Creates a new config with given chain spec, setting all fields to default values.
    pub fn new(chain: Arc<ChainSpec>) -> Self {
        Self {
            config: None,
            validator_daemon: true,
            chain,
            instance: None,
            rpc: RpcServerArgs::default(),
            txpool: TxPoolArgs::default(),
            builder: PayloadBuilderArgs::default(),
            debug: DebugArgs::default(),
            db: DatabaseArgs::default(),
            pruning: PruningArgs::default(),
            datadir: DatadirArgs::default(),
            engine: EngineArgs::default(),
        }
    }

    /// Apply a function to the config.
    pub fn apply<F>(self, f: F) -> Self
    where
        F: FnOnce(Self) -> Self,
    {
        f(self)
    }

    /// Applies a fallible function to the config.
    pub fn try_apply<F, R>(self, f: F) -> Result<Self, R>
    where
        F: FnOnce(Self) -> Result<Self, R>,
    {
        f(self)
    }

    /// Set the data directory args for the node
    pub fn with_datadir_args(mut self, datadir_args: DatadirArgs) -> Self {
        self.datadir = datadir_args;
        self
    }

    /// Set the config file for the node
    pub fn with_config(mut self, config: impl Into<PathBuf>) -> Self {
        self.config = Some(config.into());
        self
    }

    /// Set the [`ChainSpec`] for the node
    pub fn with_chain(mut self, chain: impl Into<Arc<ChainSpec>>) -> Self {
        self.chain = chain.into();
        self
    }

    /// Set the [`ChainSpec`] for the node and converts the type to that chainid.
    pub fn map_chain<C>(self, chain: impl Into<Arc<C>>) -> NodeConfig<C> {
        let Self {
            datadir,
            config,
            validator_daemon,
            instance,
            rpc,
            txpool,
            builder,
            debug,
            db,
            pruning,
            engine,
            ..
        } = self;
        NodeConfig {
            datadir,
            config,
            validator_daemon,
            chain: chain.into(),
            instance,
            rpc,
            txpool,
            builder,
            debug,
            db,
            pruning,
            engine,
        }
    }

    /// Set the instance for the node
    pub const fn with_instance(mut self, instance: u16) -> Self {
        self.instance = Some(instance);
        self
    }

    /// Returns the instance value, defaulting to 1 if not set.
    pub fn get_instance(&self) -> u16 {
        self.instance.unwrap_or(1)
    }

    /// Set the rpc args for the node
    pub fn with_rpc(mut self, rpc: RpcServerArgs) -> Self {
        self.rpc = rpc;
        self
    }

    /// Set the txpool args for the node
    pub fn with_txpool(mut self, txpool: TxPoolArgs) -> Self {
        self.txpool = txpool;
        self
    }

    /// Set the builder args for the node
    pub fn with_payload_builder(mut self, builder: PayloadBuilderArgs) -> Self {
        self.builder = builder;
        self
    }

    /// Set the debug args for the node
    pub fn with_debug(mut self, debug: DebugArgs) -> Self {
        self.debug = debug;
        self
    }

    /// Set the database args for the node
    pub const fn with_db(mut self, db: DatabaseArgs) -> Self {
        self.db = db;
        self
    }

    /// Set the pruning args for the node
    pub fn with_pruning(mut self, pruning: PruningArgs) -> Self {
        self.pruning = pruning;
        self
    }

    /// Returns pruning configuration.
    pub fn prune_config(&self) -> Option<PruneConfig>
    where
        ChainSpec: EthereumHardforks,
    {
        self.pruning.prune_config(&self.chain)
    }

    /// Returns the max block that the node should run to.
    ///
    /// This is a pure config lookup: the `--debug.max-block` / `--debug.tip` flags are the
    /// only sources. The former network-tip fetch (p2p `HeadersClient`) is gone — there is
    /// no p2p fetch path in `SmartNet`; the tip must already be in the database.
    pub const fn max_block(&self) -> Option<BlockNumber> {
        self.debug.max_block
    }

    /// Fetches the head block from the database.
    ///
    /// If the database is empty, returns the genesis block.
    pub fn lookup_head<Factory>(&self, factory: &Factory) -> ProviderResult<Head>
    where
        Factory: DatabaseProviderFactory<
            Provider: HeaderProvider + StageCheckpointReader + BlockHashReader,
        >,
    {
        let provider = factory.database_provider_ro()?;

        let head = provider.get_stage_checkpoint(StageId::Finish)?.unwrap_or_default().block_number;

        let header = provider
            .header_by_number(head)?
            .expect("the header for the latest block is missing, database is corrupt");

        let hash = provider
            .block_hash(head)?
            .expect("the hash for the latest block is missing, database is corrupt");

        Ok(Head {
            number: head,
            hash,
            difficulty: header.difficulty(),
            total_difficulty: U256::ZERO,
            timestamp: header.timestamp(),
        })
    }

    /// Change rpc port numbers based on the instance number, using the inner
    /// [`RpcServerArgs::adjust_instance_ports`] method.
    pub fn adjust_instance_ports(&mut self) {
        self.rpc.adjust_instance_ports(self.instance);
    }

    /// Sets networking and RPC ports to zero, causing the OS to choose random unused ports when
    /// sockets are bound.
    pub fn with_unused_ports(mut self) -> Self {
        self.rpc = self.rpc.with_unused_ports();
        self
    }

    /// Effectively disables the RPC state cache by setting the cache sizes to `0`.
    ///
    /// By setting the cache sizes to 0, caching of newly executed or fetched blocks will be
    /// effectively disabled.
    pub const fn with_disabled_rpc_cache(mut self) -> Self {
        self.rpc.rpc_state_cache.set_zero_lengths();
        self
    }

    /// Resolve the final datadir path.
    pub fn datadir(&self) -> ChainPath<DataDirPath>
    where
        ChainSpec: EthChainSpec,
    {
        self.datadir.clone().resolve_datadir(self.chain.chain())
    }

    /// Load an application configuration from a specified path.
    ///
    /// A new configuration file is created with default values if none
    /// exists.
    pub fn load_path<T: Serialize + DeserializeOwned + Default>(
        path: impl AsRef<Path>,
    ) -> eyre::Result<T> {
        let path = path.as_ref();
        match fs::read_to_string(path) {
            Ok(cfg_string) => {
                toml::from_str(&cfg_string).map_err(|e| eyre!("Failed to parse TOML: {e}"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| eyre!("Failed to create directory: {e}"))?;
                }
                let cfg = T::default();
                let s = toml::to_string_pretty(&cfg)
                    .map_err(|e| eyre!("Failed to serialize to TOML: {e}"))?;
                fs::write(path, s).map_err(|e| eyre!("Failed to write configuration file: {e}"))?;
                Ok(cfg)
            }
            Err(e) => Err(eyre!("Failed to load configuration: {e}")),
        }
    }

    /// Modifies the [`ChainSpec`] generic of the config using the provided closure.
    pub fn map_chainspec<F, C>(self, f: F) -> NodeConfig<C>
    where
        F: FnOnce(Arc<ChainSpec>) -> C,
    {
        let chain = Arc::new(f(self.chain));
        NodeConfig {
            chain,
            datadir: self.datadir,
            config: self.config,
            validator_daemon: self.validator_daemon,
            instance: self.instance,
            rpc: self.rpc,
            txpool: self.txpool,
            builder: self.builder,
            debug: self.debug,
            db: self.db,
            pruning: self.pruning,
            engine: self.engine,
        }
    }
}

impl Default for NodeConfig<ChainSpec> {
    fn default() -> Self {
        Self::new(MAINNET.clone())
    }
}

impl<ChainSpec> Clone for NodeConfig<ChainSpec> {
    fn clone(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            config: self.config.clone(),
            validator_daemon: self.validator_daemon,
            instance: self.instance,
            rpc: self.rpc.clone(),
            txpool: self.txpool.clone(),
            builder: self.builder.clone(),
            debug: self.debug.clone(),
            db: self.db,
            pruning: self.pruning.clone(),
            datadir: self.datadir.clone(),
            engine: self.engine.clone(),
        }
    }
}
