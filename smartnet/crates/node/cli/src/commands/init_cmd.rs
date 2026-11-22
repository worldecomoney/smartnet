//! Genesis-backed storage bootstrap (`smartnet init`).
//!
//! The command’s job is to create and populate on-disk state for the chain: database, static files,
//! and genesis execution according to the given chainspec (e.g. `genesis.json`). The operator's
//! keystore is checked first (`smartnet keystore` must already have run); only then does this
//! command read `DATADIR_PATH` from `--config`. A preliminary check requires that chain datadir
//! to be missing or empty so existing data is never overwritten.

use crate::{
    app::ensure_keystore_ready,
    cli_core::chainspec::{ChainSpecParser, GenesisWithRaw},
    commands::{
        common::{AccessRights, CliNodeTypes, Environment, EnvironmentArgs},
        node::operator_datadir_args,
    },
};
use clap::Parser;
use smartnet_chainspec::{
    validate_calibri_lite_genesis, ChainSpecProvider, EthChainSpec, EthereumHardforks,
};
use smartnet_config::load_config_file;
use smartnet_db::DatabaseBackend;
use smartnet_fs_util::runtime_paths::{home_dir_from_config, resolve_runtime_path};
use smartnet_node_core::args::DatabaseArgs;
use smartnet_primitives_traits::AlloyBlockHeader;
use smartnet_provider::{
    keys, BlockHashReader, DBProvider, DatabaseProviderFactory, MetadataWriter,
};
use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::info;

/// Precondition before writing genesis-backed storage: the resolved chain datadir from config
/// (`datadir` / `DATADIR_PATH`) must be **missing** or an **empty directory** (no entries). We do
/// not inspect parents, children like `db/` alone, or the genesis file path — only that directory.
fn ensure_chain_datadir_empty_for_init(
    datadir: &smartnet_node_core::args::DatadirArgs,
    chain: smartnet_chainspec::Chain,
) -> eyre::Result<()> {
    let chain_path = datadir.clone().resolve_datadir(chain);
    let data_dir = chain_path.data_dir();
    if data_dir.exists() {
        let mut entries = std::fs::read_dir(data_dir)?;
        if entries.next().is_some() {
            eyre::bail!(
                "chain datadir {} is not empty; must be empty for `init` (use an empty directory or a new path)",
                data_dir.display()
            );
        }
    }

    // Static payload lives beside the engine dir. Refuse to init over an existing file tree:
    // appending genesis after already-written blocks would corrupt the height order.
    if let Some(base) = data_dir.parent() {
        for name in ["blocks", "bytecodes"] {
            let dir = base.join(name);
            if dir.exists() && std::fs::read_dir(&dir)?.next().is_some() {
                eyre::bail!(
                    "file-tier directory {} is not empty; must be empty for `init` (use an empty directory or a new path)",
                    dir.display()
                );
            }
        }
    }
    Ok(())
}

/// Bootstrap chain storage from a genesis chainspec: opens/creates DB and static files under the
/// configured datadir, then writes genesis state.
///
/// Use: `smartnet init <GENESIS_FILE_OR_JSON> [--config /path/to/smartnet.toml]`. The global
/// config rule applies: an explicit `--config` is used verbatim; otherwise search
/// `./config.toml` then `~/.smartnet/config.toml`.
#[derive(Debug, Parser)]
pub struct InitCommand<C: ChainSpecParser> {
    /// Genesis / chainspec: path to a JSON file, or inline JSON. The operator's verbatim
    /// bytes (file content or the typed JSON) are kept beside the parsed spec and stored
    /// byte-for-byte in the database at init — nothing the genesis carried is dropped.
    #[arg(value_name = "GENESIS_FILE_OR_JSON", value_parser = C::parser_raw())]
    genesis: Arc<GenesisWithRaw<C>>,
}

impl<C: ChainSpecParser<ChainSpec: EthChainSpec + EthereumHardforks>> InitCommand<C> {
    /// Execute `init`. `global_config` is the mandatory `--config` path from the root CLI.
    pub async fn execute<N: CliNodeTypes<ChainSpec = C::ChainSpec>>(
        self,
        global_config: &Path,
        runtime: smartnet_tasks::Runtime,
    ) -> eyre::Result<()> {
        info!(target: "smartnet::cli", "smartnet init starting");

        // Exact order: keystore first. `smartnet init` must not even inspect DATADIR_PATH
        // until the operator's key has been created by `smartnet keystore` and verified here.
        ensure_keystore_ready(global_config)?;

        // Config only, no built-in default: DATADIR_PATH is relative to HOME_DIR_PATH, the one
        // absolute root in the operator config. A missing config, root, or key is a hard error —
        // `init` must never guess where chain data lives.
        let cfg = load_config_file(global_config).map_err(|e| {
            eyre::eyre!("operator config `{}` cannot be loaded: {e}", global_config.display())
        })?;
        let raw = std::fs::read_to_string(global_config).map_err(|e| {
            eyre::eyre!("operator config `{}` cannot be read: {e}", global_config.display())
        })?;
        let home_dir = home_dir_from_config(&raw).map_err(|e| {
            eyre::eyre!("operator config `{}` HOME_DIR_PATH: {e}", global_config.display())
        })?;
        let datadir = cfg.datadir.ok_or_else(|| {
            eyre::eyre!(
                "operator config `{}` must set DATADIR_PATH — there is no built-in default data \
                 directory (template: smartnet/example.config.toml)",
                global_config.display()
            )
        })?;
        let datadir_root = resolve_runtime_path(&home_dir, &datadir.to_string_lossy())?;
        // The engine comes from config (`DB_ENGINE`, default `fjall`) and is validated here:
        // an unknown engine name must fail `init` loudly instead of silently defaulting —
        // config dictates the engine, and the tree this init creates belongs to that engine.
        let db_args = DatabaseArgs {
            engine: match cfg.db_engine.as_deref() {
                Some(name) => name.parse().map_err(|err| {
                    eyre::eyre!("operator config `{}` DB_ENGINE: {err}", global_config.display())
                })?,
                None => DatabaseBackend::default(),
            },
            ..Default::default()
        };
        let mut datadir_args = operator_datadir_args(datadir_root);
        // Same resolution as `EnvironmentArgs::init` and the database open, so the
        // emptiness precondition below checks the tree the init will actually write.
        datadir_args.engine = DatabaseBackend::resolve(&db_args.database_args());

        ensure_chain_datadir_empty_for_init(&datadir_args, self.genesis.spec.chain())?;

        validate_calibri_lite_genesis(self.genesis.spec.genesis())
            .map_err(|err| eyre::eyre!("invalid genesis seal config: {err}"))?;

        let env = EnvironmentArgs::<C> {
            config: Some(global_config.to_path_buf()),
            chain: self.genesis.spec.clone(),
            genesis_raw: Some(self.genesis.raw.clone()),
            db: db_args,
        };

        let Environment { provider_factory, .. } =
            env.init::<N>(&datadir_args, AccessRights::RW, runtime)?;

        let genesis_block_number = provider_factory.chain_spec().genesis_header().number();
        let hash = provider_factory
            .block_hash(genesis_block_number)?
            .ok_or_else(|| eyre::eyre!("Genesis hash not found."))?;

        info!(target: "smartnet::cli", hash = ?hash, "Genesis block written");

        // A freshly initialised datadir is clean by definition: record `SHUTDOWN_CLEAN` so the
        // first boot skips crash recovery (an unnecessary truncate/re-sync on a datadir that
        // only holds genesis) and clears the marker as usual. Absence still means "run
        // recovery" — that path is unchanged.
        let timestamp =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|e| e.as_secs()).unwrap_or_default();
        let provider_rw = provider_factory.database_provider_rw()?;
        provider_rw.write_metadata(keys::SHUTDOWN_CLEAN, timestamp.to_be_bytes().to_vec())?;
        provider_rw.commit()?;
        info!(
            target: "smartnet::cli",
            timestamp,
            "recorded clean shutdown (SHUTDOWN_CLEAN) for the initialised datadir"
        );

        Ok(())
    }
}

impl<C: ChainSpecParser> InitCommand<C> {
    /// Returns the chainspec used for this init run.
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.genesis.spec)
    }
}
