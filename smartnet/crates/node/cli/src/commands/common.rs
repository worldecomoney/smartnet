//! Contains common node arguments

pub use smartnet_primitives_traits::header::HeaderMut;

use crate::cli_core::chainspec::ChainSpecParser;
use eyre::WrapErr;
use smartnet_chainspec::EthChainSpec;
use smartnet_config::{config::EtlConfig, load_config_file, Config};
use smartnet_db::{init_db, open_db_read_only, DatabaseBackend, DatabaseEnv};
use smartnet_eth_wire_types::NetPrimitivesFor;
use smartnet_evm::ConfigureEvm;
use smartnet_network_api::NetworkEventListenerProvider;
use smartnet_node_api::FullNodeTypesAdapter;
use smartnet_node_builder::{
    Node, NodeComponents, NodeComponentsBuilder, NodeTypes, NodeTypesWithDBAdapter,
};
use smartnet_node_core::{
    args::{DatabaseArgs, DatadirArgs},
    dirs::{ChainPath, DataDirPath},
};
use smartnet_provider::{
    init::init_genesis_with_settings,
    providers::{AuxDbProvider, BlockchainProvider, NodeTypesForProvider},
    ProviderFactory, StorageSettings,
};
use std::{path::PathBuf, sync::Arc};
use tracing::{debug, info};

/// Struct to hold config and chain selection (`datadir` is passed separately to
/// [`EnvironmentArgs::init`]).
#[derive(Debug)]
pub struct EnvironmentArgs<C: ChainSpecParser> {
    /// Path to the config file (`--config`), if given on the command line.
    pub config: Option<PathBuf>,
    /// Parsed chain rulebook this environment runs against.
    pub chain: Arc<C::ChainSpec>,
    /// Database arguments.
    pub db: DatabaseArgs,
    /// Verbatim bytes of the genesis file this environment was initialized from (the file's
    /// bytes, or the inline JSON exactly as typed). Stored byte-for-byte in the `GENESIS_SPEC`
    /// row at genesis init — the database keeps the network's self-declared record intact,
    /// including any key the parser does not read. `None` for programmatic specs.
    pub genesis_raw: Option<Vec<u8>>,
}

impl<C: ChainSpecParser> EnvironmentArgs<C> {
    /// Initializes environment according to [`AccessRights`] and returns an instance of
    /// [`Environment`].
    ///
    /// The provided `runtime` is used for parallel storage I/O.
    pub fn init<N: CliNodeTypes>(
        &self,
        datadir: &DatadirArgs,
        access: AccessRights,
        runtime: smartnet_tasks::Runtime,
    ) -> eyre::Result<Environment<N>>
    where
        C: ChainSpecParser<ChainSpec = N::ChainSpec>,
    {
        // Stamp the resolved engine onto the datadir before any path is derived from it:
        // config `DB_ENGINE` (carried in `self.db.engine`) with the `SMARTNET_DB_BACKEND`
        // environment override applied on top — the exact same resolution the database open
        // performs below — so every path here belongs to the engine that is actually opened.
        let mut datadir = datadir.clone();
        datadir.engine = DatabaseBackend::resolve(&self.db.database_args());
        let data_dir = datadir.resolve_datadir(self.chain.chain());
        let db_path = data_dir.db();

        // Legacy cleanup: static files (MDBX-era store) are retired — remove any leftover
        // `static_files/` directory so it never confuses operators or wastes disk. The node
        // never recreates it.
        let legacy_static_files = data_dir.data_dir().join("static_files");
        if access.is_read_write() && legacy_static_files.exists() {
            match std::fs::remove_dir_all(&legacy_static_files) {
                Ok(()) => info!(
                    target: "smartnet::cli",
                    path = %legacy_static_files.display(),
                    "removed legacy static_files directory (subsystem retired)"
                ),
                Err(err) => debug!(
                    target: "smartnet::cli",
                    path = %legacy_static_files.display(),
                    %err, "failed to remove legacy static_files directory — ignoring"
                ),
            }
        }

        // Static payload — block headers/bodies, transactions, receipts, logs, runtime
        // bytecodes — lives only in the file tree beside the engine dir (the chain datadir),
        // never in Fjall. Set the process-global base before the database is opened so every
        // write in this environment follows one rule, genesis included.
        let file_tier_base = data_dir.data_dir().parent().ok_or_else(|| {
            eyre::eyre!(
                "chain datadir {} has no parent to use as file-tier base",
                data_dir.data_dir().display()
            )
        })?;
        if !smartnet_file_tier::set_base(file_tier_base) {
            let existing = smartnet_file_tier::base();
            if existing != Some(file_tier_base) {
                return Err(eyre::eyre!(
                    "file-tier base already set to {existing:?}; refusing to use {}",
                    file_tier_base.display()
                ));
            }
        }

        if access.is_read_write() {
            smartnet_fs_util::create_dir_all(&db_path)?;
        }

        let config_path = self.config.clone().unwrap_or_else(|| data_dir.config());

        // Config only, no silent fallback: an absent or unreadable config is an error. The
        // previous `Config::from_path(..).unwrap_or_default()` both created a default file and
        // continued with it — a hidden default that a typo would never surface.
        let mut config = match &self.config {
            Some(p) => load_config_file(p).wrap_err_with(|| format!("{}", p.display()))?,
            None => load_config_file(&config_path).wrap_err_with(|| {
                format!(
                    "no `--config` was given and `{}` cannot be loaded — there is no built-in \
                     default config",
                    config_path.display()
                )
            })?,
        };

        // Make sure ETL uses the configured datadir rather than a fixed system temp location.
        if config.stages.etl.dir.is_none() {
            config.stages.etl.dir = Some(EtlConfig::from_datadir(data_dir.data_dir()));
        }

        info!(target: "smartnet::cli", ?db_path, "Opening storage");
        let db = match access {
            AccessRights::RW => init_db(db_path, self.db.database_args())?,
            AccessRights::RO | AccessRights::RoInconsistent => {
                open_db_read_only(&db_path, self.db.database_args())?
            }
        };
        let aux_db_provider = None;

        let provider_factory =
            self.create_provider_factory(&config, db, aux_db_provider, runtime)?;
        if access.is_read_write() {
            debug!(target: "smartnet::cli", chain=%self.chain.chain(), genesis=?self.chain.genesis_hash(), "Initializing genesis");
            // The database keeps the genesis VERBATIM (every key it carried, including ones
            // the parser does not read, e.g. a `calibri-lite { period, epoch, quorum }` object):
            // the file's bytes, byte-for-byte. Programmatic specs (tests, engine bootstrap)
            // re-serialize the built object instead.
            init_genesis_with_settings(
                &provider_factory,
                StorageSettings::base(),
                self.genesis_raw.clone(),
            )?;
        }

        Ok(Environment { config, provider_factory, data_dir })
    }

    /// Returns a [`ProviderFactory`].
    ///
    /// Static files are retired: no static file state exists to compare against the
    /// database, so no consistency unwind is performed here.
    fn create_provider_factory<N: CliNodeTypes>(
        &self,
        config: &Config,
        db: DatabaseEnv,
        aux_db_provider: Option<AuxDbProvider>,
        runtime: smartnet_tasks::Runtime,
    ) -> eyre::Result<ProviderFactory<NodeTypesWithDBAdapter<N, DatabaseEnv>>>
    where
        C: ChainSpecParser<ChainSpec = N::ChainSpec>,
    {
        let factory = ProviderFactory::<NodeTypesWithDBAdapter<N, DatabaseEnv>>::new(
            db,
            self.chain.clone(),
            aux_db_provider,
            runtime,
        )?
        .with_prune_modes(config.prune.segments.clone())
        .with_minimum_pruning_distance(config.prune.minimum_pruning_distance);

        Ok(factory)
    }
}

/// Environment built from [`EnvironmentArgs`].
#[derive(Debug)]
pub struct Environment<N: NodeTypes> {
    /// Configuration for the node
    pub config: Config,
    /// Provider factory.
    pub provider_factory: ProviderFactory<NodeTypesWithDBAdapter<N, DatabaseEnv>>,
    /// Datadir path.
    pub data_dir: ChainPath<DataDirPath>,
}

/// Environment access rights.
#[derive(Debug, Copy, Clone)]
pub enum AccessRights {
    /// Read-write access
    RW,
    /// Read-only access
    RO,
    /// Read-only access with possibly inconsistent data
    RoInconsistent,
}

impl AccessRights {
    /// Returns `true` if it requires read-write access to the environment.
    pub const fn is_read_write(&self) -> bool {
        matches!(self, Self::RW)
    }

    /// Returns `true` if it requires read-only access to the environment with possibly inconsistent
    /// data.
    pub const fn is_read_only_inconsistent(&self) -> bool {
        matches!(self, Self::RoInconsistent)
    }
}

/// Helper alias to satisfy `FullNodeTypes` bound on [`Node`] trait generic.
type FullTypesAdapter<T> = FullNodeTypesAdapter<
    T,
    DatabaseEnv,
    BlockchainProvider<NodeTypesWithDBAdapter<T, DatabaseEnv>>,
>;

/// Helper trait with a common set of requirements for the
/// [`NodeTypes`] in CLI.
pub trait CliNodeTypes: Node<FullTypesAdapter<Self>> + NodeTypesForProvider {
    /// EVM configuration for this node type.
    type Evm: ConfigureEvm<Primitives = Self::Primitives>;
    /// Network-level primitive types matching [`Self::Primitives`].
    type NetworkPrimitives: NetPrimitivesFor<Self::Primitives>;
}

impl<N> CliNodeTypes for N
where
    N: Node<FullTypesAdapter<Self>> + NodeTypesForProvider,
{
    type Evm = <<N::ComponentsBuilder as NodeComponentsBuilder<FullTypesAdapter<Self>>>::Components as NodeComponents<FullTypesAdapter<Self>>>::Evm;
    type NetworkPrimitives = <<<N::ComponentsBuilder as NodeComponentsBuilder<FullTypesAdapter<Self>>>::Components as NodeComponents<FullTypesAdapter<Self>>>::Network as NetworkEventListenerProvider>::Primitives;
}

type EvmFor<N> = <<<N as Node<FullTypesAdapter<N>>>::ComponentsBuilder as NodeComponentsBuilder<
    FullTypesAdapter<N>,
>>::Components as NodeComponents<FullTypesAdapter<N>>>::Evm;

type ConsensusFor<N> =
    <<<N as Node<FullTypesAdapter<N>>>::ComponentsBuilder as NodeComponentsBuilder<
        FullTypesAdapter<N>,
    >>::Components as NodeComponents<FullTypesAdapter<N>>>::Consensus;

/// Helper trait aggregating components required for the CLI.
pub trait CliNodeComponents<N: CliNodeTypes>: Send + Sync + 'static {
    /// Returns the configured EVM.
    fn evm_config(&self) -> &EvmFor<N>;
    /// Returns the consensus implementation.
    fn consensus(&self) -> &ConsensusFor<N>;
}

impl<N: CliNodeTypes> CliNodeComponents<N> for (EvmFor<N>, ConsensusFor<N>) {
    fn evm_config(&self) -> &EvmFor<N> {
        &self.0
    }

    fn consensus(&self) -> &ConsensusFor<N> {
        &self.1
    }
}

/// Helper trait alias for an [`FnOnce`] producing [`CliNodeComponents`].
pub trait CliComponentsBuilder<N: CliNodeTypes>:
    FnOnce(Arc<N::ChainSpec>) -> Self::Components + Send + Sync + 'static
{
    /// Concrete components produced by this builder.
    type Components: CliNodeComponents<N>;
}

impl<N: CliNodeTypes, F, Comp> CliComponentsBuilder<N> for F
where
    F: FnOnce(Arc<N::ChainSpec>) -> Comp + Send + Sync + 'static,
    Comp: CliNodeComponents<N>,
{
    type Components = Comp;
}
