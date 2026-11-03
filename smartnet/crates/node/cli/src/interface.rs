//! CLI definition and entrypoint to executable

use crate::{
    app::CliApp,
    chainspec::EthereumChainSpecParser,
    cli_core::chainspec::ChainSpecParser,
    commands::{
        common::{CliComponentsBuilder, CliNodeTypes, HeaderMut},
        init_cmd::InitCommand,
        launcher::FnLauncher,
        node::NoArgs,
    },
    runner::CliRunner,
};
use clap::{Parser, Subcommand};
use smartnet_chainspec::{ChainSpec, EthChainSpec, Hardforks};
use smartnet_db::DatabaseEnv;
use smartnet_node_api::NodePrimitives;
use smartnet_node_builder::{NodeBuilder, WithLaunchContext};
use smartnet_node_core::{args::LogArgs, version::version_metadata};
use smartnet_rpc_server_types::{DefaultRpcModuleValidator, RpcModuleValidator};
use smartnet_tracing::{FileWorkerGuard, Layers};
use std::{ffi::OsString, fmt, future::Future, marker::PhantomData, path::PathBuf, sync::Arc};
use tracing::info;

/// Default node config file path: `$HOME/.smartnet/config.toml`.
///
/// `HOME` unset is an error — there is no fallback to the current working directory and no
/// built-in config location. Pass `--config <path>` explicitly instead.
pub fn default_config_path() -> eyre::Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| {
        eyre::eyre!(
            "HOME is not set, so the default config path ~/.smartnet/config.toml cannot be \
             resolved — pass `--config /path/to/config.toml` (no built-in fallback path exists)"
        )
    })?;
    Ok(PathBuf::from(home).join(".smartnet").join("config.toml"))
}

/// The main `SmartNet` CLI interface.
///
/// This is the entrypoint to the executable.
#[derive(Debug, Parser)]
#[command(author, name = version_metadata().name_client.as_ref(), version = version_metadata().short_version.as_ref(), long_version = version_metadata().long_version.as_ref(), about = "SmartNet execution node", long_about = None)]
pub struct Cli<
    C: ChainSpecParser = EthereumChainSpecParser,
    Ext: clap::Args + fmt::Debug + Default = NoArgs,
    Rpc: RpcModuleValidator = DefaultRpcModuleValidator,
    SubCmd: Subcommand + fmt::Debug = NoSubCmd,
> {
    /// Operator configuration file. Datadir, chain identity (database), ports and RPC are all
    /// read from it; place this flag before subcommands, e.g.
    /// `smartnet --config /path/config.toml init ...`.
    /// Defaults to `~/.smartnet/node/config.toml` if not specified.
    #[arg(long = "config", value_name = "FILE")]
    pub config_path: Option<PathBuf>,

    /// Optional subcommand. When omitted, `smartnet` starts the node using `--config`.
    #[command(subcommand)]
    pub command: Option<Commands<C, SubCmd>>,

    /// The logging configuration for the CLI.
    #[command(flatten)]
    pub logs: LogArgs,

    /// Type marker for the RPC module validator
    #[arg(skip)]
    pub _phantom: PhantomData<Rpc>,
    /// Type marker for extension args.
    #[arg(skip)]
    pub _ext: PhantomData<Ext>,
}

impl Cli {
    /// Parsers only the default CLI arguments
    pub fn parse_args() -> Self {
        Self::parse()
    }

    /// Parsers only the default CLI arguments from the given iterator
    pub fn try_parse_args_from<I, T>(itr: I) -> Result<Self, clap::error::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        Self::try_parse_from(itr)
    }
}

impl<
        C: ChainSpecParser,
        Ext: clap::Args + fmt::Debug + Default,
        Rpc: RpcModuleValidator,
        SubCmd: crate::app::ExtendedCommand + Subcommand + fmt::Debug,
    > Cli<C, Ext, Rpc, SubCmd>
where
    C::ChainSpec: EthChainSpec,
{
    /// Configures the CLI and returns a [`CliApp`] instance.
    ///
    /// This method is used to prepare the CLI for execution by wrapping it in a
    /// [`CliApp`] that can be further configured before running.
    pub fn configure(self) -> CliApp<C, Ext, Rpc, SubCmd> {
        CliApp::new(self)
    }

    /// Execute the configured CLI command on the default runtime.
    pub fn run<L, Fut>(self, launcher: L) -> eyre::Result<()>
    where
        L: FnOnce(WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>, Ext) -> Fut,
        Fut: Future<Output = eyre::Result<()>>,
        C: ChainSpecParser<ChainSpec = ChainSpec>,
    {
        self.configure()
            .run(FnLauncher::new::<C, Ext>(async move |builder, ext| launcher(builder, ext).await))
    }

    /// Execute the configured CLI command with the provided [`CliComponentsBuilder`].
    pub fn run_with_components<N>(
        self,
        components: impl CliComponentsBuilder<N>,
        launcher: impl AsyncFnOnce(
            WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>,
            Ext,
        ) -> eyre::Result<()>,
    ) -> eyre::Result<()>
    where
        N: CliNodeTypes<Primitives: NodePrimitives<BlockHeader: HeaderMut>, ChainSpec: Hardforks>,
        C: ChainSpecParser<ChainSpec = N::ChainSpec>,
    {
        self.configure().run_with_components(components, launcher)
    }

    /// Execute the configured CLI command with the provided [`CliRunner`].
    pub fn with_runner<L, Fut>(self, runner: CliRunner, launcher: L) -> eyre::Result<()>
    where
        L: FnOnce(WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>, Ext) -> Fut,
        Fut: Future<Output = eyre::Result<()>>,
        C: ChainSpecParser<ChainSpec = ChainSpec>,
    {
        let mut app = self.configure();
        app.set_runner(runner);
        app.run(FnLauncher::new::<C, Ext>(async move |builder, ext| launcher(builder, ext).await))
    }

    /// Execute the configured cli command with the provided [`CliRunner`] and
    /// [`CliComponentsBuilder`].
    pub fn with_runner_and_components<N>(
        self,
        runner: CliRunner,
        components: impl CliComponentsBuilder<N>,
        launcher: impl AsyncFnOnce(
            WithLaunchContext<NodeBuilder<DatabaseEnv, C::ChainSpec>>,
            Ext,
        ) -> eyre::Result<()>,
    ) -> eyre::Result<()>
    where
        N: CliNodeTypes<Primitives: NodePrimitives<BlockHeader: HeaderMut>, ChainSpec: Hardforks>,
        C: ChainSpecParser<ChainSpec = N::ChainSpec>,
    {
        let mut app = self.configure();
        app.set_runner(runner);
        app.run_with_components(components, launcher)
    }

    /// Subdirectory name under the data dir's `logs/` for file logging (from init genesis;
    /// otherwise straight from the database — the single source of truth for the chain id).
    pub fn log_chain_directory_name(&self) -> eyre::Result<String> {
        match &self.command {
            Some(Commands::Init(cmd)) => {
                // `init` is the single genesis exception: the label follows the genesis chain id.
                let spec = cmd.chain_spec().expect("init has genesis");
                Ok(EthChainSpec::chain(spec.as_ref()).to_string())
            }
            // Node run and `import`: both boot the node against the configured datadir, so
            // the label follows the chain id the same way (database only, no fallback).
            None | Some(Commands::Import(_)) => {
                let cfg_path = match self.config_path.clone() {
                    Some(p) => p,
                    None => crate::interface::default_config_path()?,
                };
                // Node run: once bootstrapped, the chain id is the global accessor; before the
                // bootstrap (this label is chosen during CLI setup) it is read straight from the
                // database. No config fallback: `config.toml` is never a chain-identity source.
                let overlay = crate::app::load_operator_overlay(&cfg_path)?;
                if smartnet_file_tier::network_settings_initialised() {
                    return Ok(smartnet_file_tier::get_chain_id().to_string());
                }
                let db_chain_id =
                    crate::app::read_chain_identity_from_db(&overlay)?.unwrap_or_default().chain_id;
                db_chain_id.map(|id| id.to_string()).ok_or_else(|| {
                    eyre::eyre!(
                        "no chain id: the database carries none (initialise with `smartnet init \
                         <genesis>`); cannot label the log directory"
                    )
                })
            }
            Some(Commands::Ext(_)) => Ok("extension".to_string()),
        }
    }

    /// Initializes tracing with the configured options.
    ///
    /// If file logging is enabled, this function returns a guard that must be kept alive to ensure
    /// that all logs are flushed to disk.
    ///
    /// If an OTLP endpoint is specified, it will export traces and logs to the configured
    /// collector.
    pub fn init_tracing(
        &mut self,
        runner: &CliRunner,
        layers: Layers,
    ) -> eyre::Result<Option<FileWorkerGuard>> {
        let _ = runner;
        // Enable reload support if debug RPC namespace is available
        let enable_reload = self.command.as_ref().is_some_and(|c| c.debug_namespace_enabled());
        let file_guard = self.logs.init_tracing_with_layers(layers, enable_reload)?;
        info!(target: "smartnet::cli", "Initialized tracing, debug log directory: {}", self.logs.log_file_directory);

        Ok(file_guard)
    }
}

/// `smartnet import` — stream a chain RLP file into this node's engine, one block at a
/// time, through the exact validated `smartnet_newPayload` path (IMPORT-EXPORT.md). The
/// file is read in place and never modified; re-running the command resumes at the local
/// tip and skips the blocks already stored.
#[derive(Debug, Parser)]
pub struct ImportCommand {
    /// Chain RLP file: concatenated block RLP items (the chaindata format), read in
    /// place — never copied, never rewritten.
    #[arg(value_name = "CHAIN_RLP_FILE")]
    pub file: PathBuf,
}

/// Commands to be executed
#[derive(Debug, Subcommand)]
pub enum Commands<C: ChainSpecParser, SubCmd: Subcommand + fmt::Debug = NoSubCmd> {
    /// Initialize chain storage from a genesis/chainspec file (requires an empty datadir).
    #[command(name = "init")]
    Init(Box<InitCommand<C>>),
    /// Import a chain RLP file into this node's engine (offline, validator not required).
    #[command(name = "import")]
    Import(ImportCommand),
    /// Extension subcommands provided by consumers.
    #[command(flatten)]
    Ext(SubCmd),
}

/// A no-op subcommand type for when no extension subcommands are needed.
///
/// This is the default type parameter for `Commands` when consumers don't need
/// to add custom subcommands.
#[derive(Debug, Subcommand)]
pub enum NoSubCmd {}

impl crate::app::ExtendedCommand for NoSubCmd {
    fn execute(self, _runner: CliRunner, _cfg_path: std::path::PathBuf) -> eyre::Result<()> {
        match self {}
    }
}

impl<C: ChainSpecParser, SubCmd: Subcommand + fmt::Debug> Commands<C, SubCmd> {
    /// Returns the underlying chain being used for commands
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        match self {
            Self::Init(cmd) => cmd.chain_spec(),
            Self::Import(_) | Self::Ext(_) => None,
        }
    }

    /// Returns whether debug RPC namespace is enabled.
    pub const fn debug_namespace_enabled(&self) -> bool {
        match self {
            Self::Init(_) | Self::Import(_) | Self::Ext(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chainspec::EthereumChainSpecParser, commands::node::NoArgs};

    #[test]
    fn test_parse_help() {
        let err =
            Cli::<EthereumChainSpecParser, NoArgs>::try_parse_args_from(["smartnet", "--help"])
                .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn default_config_path() {
        let path = crate::interface::default_config_path().unwrap();
        let home = std::env::var("HOME").unwrap();
        assert_eq!(path, std::path::PathBuf::from(home).join(".smartnet/config.toml"));
    }

    #[test]
    fn parse_rejects_node_subcommand() {
        let err = Cli::try_parse_args_from(["smartnet", "node"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}
