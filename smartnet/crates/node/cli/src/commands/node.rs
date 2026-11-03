//! Main node command for launching a node

use crate::{
    cli_core::chainspec::ChainSpecParser, commands::launcher::Launcher, runner::CliContext,
};
use clap::Args;
use smartnet_chainspec::{EthChainSpec, EthereumHardforks};
use smartnet_db::{init_db, DatabaseBackend};
use smartnet_node_builder::NodeBuilder;
use smartnet_node_core::{
    args::{
        DatabaseArgs, DatadirArgs, DebugArgs, EngineArgs, PayloadBuilderArgs, PruningArgs,
        RpcServerArgs, TxPoolArgs,
    },
    dirs::MaybePlatformPath,
    node_config::NodeConfig,
    version,
};
use std::{fmt, path::PathBuf, sync::Arc};

/// Start the node — built by `operator_default_node_command`, not parsed from CLI.
#[derive(Debug)]
pub struct NodeCommand<C: ChainSpecParser, Ext: clap::Args + fmt::Debug = NoArgs> {
    /// Path to the config file (`--config`), if given on the command line.
    pub config: Option<PathBuf>,
    /// Whether this launch may arm the merged validator daemon (see
    /// [`NodeConfig::validator_daemon`](smartnet_node_core::NodeConfig::validator_daemon));
    /// `smartnet import` sets it to `false` — import never runs the validator.
    pub validator_daemon: bool,
    /// Parsed chain rulebook this command runs against.
    pub chain: Arc<C::ChainSpec>,
    /// Node instance number, if `--instance` was given.
    pub instance: Option<u16>,
    /// Assign unused ports instead of the configured ones.
    pub with_unused_ports: bool,
    /// RPC server arguments.
    pub rpc: RpcServerArgs,
    /// Transaction pool arguments.
    pub txpool: TxPoolArgs,
    /// Payload builder arguments.
    pub builder: PayloadBuilderArgs,
    /// Debug and tracing arguments.
    pub debug: DebugArgs,
    /// Database arguments.
    pub db: DatabaseArgs,
    /// Pruning arguments.
    pub pruning: PruningArgs,
    /// Engine API arguments.
    pub engine: EngineArgs,
    /// Command-specific extra arguments.
    pub ext: Ext,
}

impl<C, Ext> NodeCommand<C, Ext>
where
    C: ChainSpecParser,
    C::ChainSpec: EthChainSpec + EthereumHardforks,
    Ext: clap::Args + fmt::Debug,
{
    /// Launches the node
    ///
    /// This transforms the node command into a node config and launches the node using the given
    /// launcher.
    pub async fn execute<L>(
        self,
        ctx: CliContext,
        mut datadir: DatadirArgs,
        launcher: L,
    ) -> eyre::Result<()>
    where
        L: Launcher<C, Ext>,
    {
        tracing::info!(target: "smartnet::cli", version = ?version::version_metadata().short_version, "Starting {}",  version::version_metadata().name_client);

        let Self {
            config,
            validator_daemon,
            chain,
            instance,
            with_unused_ports,
            rpc,
            txpool,
            builder,
            debug,
            db,
            pruning,
            engine,
            ext,
        } = self;

        // Stamp the resolved engine (config `DB_ENGINE` in `db.engine`, plus the
        // `SMARTNET_DB_BACKEND` override) onto the datadir before any path is checked or
        // derived — the same resolution the init_db below performs, so the ensure check,
        // the node config paths and the database open always agree on the engine.
        datadir.engine = DatabaseBackend::resolve(&db.database_args());

        engine.validate()?;

        ensure_chain_datadir_initialized(&datadir, chain.chain())?;

        // set up node config
        let mut node_config = NodeConfig {
            datadir,
            config,
            validator_daemon,
            chain,
            instance,
            rpc,
            txpool,
            builder,
            debug,
            db,
            pruning,
            engine,
        };

        let data_dir = node_config.datadir();
        let db_path = data_dir.db();

        tracing::info!(target: "smartnet::cli", path = ?db_path, "Opening database");
        let database = init_db(db_path.clone(), db.database_args())?.with_metrics();

        if with_unused_ports {
            node_config = node_config.with_unused_ports();
        }

        let builder = NodeBuilder::new(node_config)
            .with_database(database)
            .with_launch_context(ctx.task_executor);

        launcher.entrypoint(builder, ext).await
    }
}

impl<C: ChainSpecParser, Ext: clap::Args + fmt::Debug> NodeCommand<C, Ext> {
    /// Returns the underlying chain being used to run this command
    pub const fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.chain)
    }
}

/// No Additional arguments
#[derive(Debug, Clone, Copy, Default, Args)]
#[non_exhaustive]
pub struct NoArgs;

/// Build [`DatadirArgs`] for operator flows (datadir only from config; no CLI overrides).
///
/// `engine` is left at its default here and stamped from the resolved engine inside
/// [`NodeCommand::execute`] /
/// [`EnvironmentArgs::init`](crate::commands::common::EnvironmentArgs::init) before any path is
/// derived, so this constructor cannot disagree with the database open.
pub fn operator_datadir_args(datadir: PathBuf) -> DatadirArgs {
    DatadirArgs {
        datadir: MaybePlatformPath::from(datadir),
        engine: DatabaseBackend::default(),
        pprof_dumps_path: None,
    }
}

/// Expand a leading `~` in a path to the user's home directory.
///
/// `HOME` unset is an error: the literal `~/…` must never be kept as-is (it would be resolved
/// relative to the current working directory at first use) and there is no fallback path.
pub fn expand_tilde(path: PathBuf) -> eyre::Result<PathBuf> {
    let Some(s) = path.to_str() else { return Ok(path) };
    let Some(rest) = s.strip_prefix('~') else { return Ok(path) };
    let home = std::env::var("HOME").map_err(|_| {
        eyre::eyre!(
            "path {path:?} starts with `~` but HOME is not set — use an absolute path instead \
             (no built-in fallback path exists)"
        )
    })?;
    Ok(PathBuf::from(home).join(rest.trim_start_matches('/')))
}

/// Require a prior successful `smartnet init`: the resolved chain datadir must exist and be
/// non-empty (e.g. contains `db/`, …). We do not treat “only `db/` populated” as a
/// separate rule — the datadir root must have at least one entry.
fn ensure_chain_datadir_initialized(
    datadir: &DatadirArgs,
    chain: smartnet_chainspec::Chain,
) -> eyre::Result<()> {
    let chain_path = datadir.clone().resolve_datadir(chain);
    let data_dir = chain_path.data_dir();
    if !data_dir.exists() {
        eprintln!(
            "Data directory does not exist: {}\n\
             \n\
             Run `smartnet init <GENESIS>` first, for example:\n\
             \n\
             smartnet init /path/to/genesis.json",
            data_dir.display()
        );
        std::process::exit(1);
    }
    let mut entries = std::fs::read_dir(data_dir)?;
    if entries.next().is_none() {
        eprintln!(
            "Data directory is empty: {}\n\
             \n\
             Run `smartnet init <GENESIS>` first, for example:\n\
             \n\
             smartnet init /path/to/genesis.json",
            data_dir.display()
        );
        std::process::exit(1);
    }
    Ok(())
}

/// Default [`NodeCommand`] for `smartnet --config …` (no per-flag tuning on the operator CLI).
pub fn operator_default_node_command<C: ChainSpecParser, Ext: clap::Args + fmt::Debug + Default>(
    config_path: PathBuf,
    chain: Arc<C::ChainSpec>,
) -> NodeCommand<C, Ext> {
    NodeCommand {
        config: Some(config_path),
        validator_daemon: true,
        chain,
        instance: None,
        with_unused_ports: false,
        rpc: RpcServerArgs::default(),
        txpool: TxPoolArgs::default(),
        builder: PayloadBuilderArgs::default(),
        debug: DebugArgs::default(),
        db: DatabaseArgs::default(),
        pruning: PruningArgs::default(),
        engine: EngineArgs::default(),
        ext: Ext::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smartnet_chainspec::Chain;

    /// The operator datadir resolves every path into the engine's private tree
    /// (`DATADIR_PATH/DB_ENGINE`), and the Fjall tree is created (fresh) on first use.
    #[test]
    fn operator_datadir_resolves_into_the_engine_tree() {
        let tmp = tempfile::tempdir().expect("temp datadir");
        let datadir = tmp.path().join("datadir");

        let args = operator_datadir_args(datadir);
        let path = args.clone().resolve_datadir(Chain::mainnet()).data_dir().to_path_buf();
        assert_eq!(path, tmp.path().join("datadir/fjall"));
        assert!(!path.exists(), "the tree must start uninitialised");
        std::fs::create_dir_all(&path).expect("create engine tree");
        std::fs::write(path.join("marker"), "fjall data").expect("write engine data");
        assert!(args.resolve_datadir(Chain::mainnet()).db().ends_with("datadir/fjall/db"));
        assert!(path.join("marker").exists());
    }

    /// The removed engines are rejected where configuration names them: a `DB_ENGINE`
    /// naming `sqlite`/`redb` fails `DatabaseBackend` parsing with a dedicated message
    /// instead of silently pointing the node at a stale tree.
    #[test]
    fn removed_engine_names_are_a_hard_config_error() {
        for name in ["sqlite", "redb"] {
            let error = name.parse::<DatabaseBackend>().expect_err(name);
            assert!(error.contains("was removed"), "{name}: {error}");
        }
        assert_eq!(
            "fjall".parse::<DatabaseBackend>().expect("fjall parses"),
            DatabaseBackend::Fjall
        );
    }
}
