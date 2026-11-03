//! Extension subcommands carried over from the merged validator:
//! `smartnet keystore`, `smartnet seedTx`, `smartnet ping` — the exact same clap structs
//! (`smartnet_validator`) driven through the validator's own dispatch, so the node binary
//! keeps every old `smartnet-validator` command with identical flags and behavior.

use crate::{app::ExtendedCommand, runner::CliRunner};
use std::path::PathBuf;

/// The node CLI with the validator ops subcommands merged in.
///
/// `ValidatorCommands` is a *subcommand* extension (`Cli`'s `SubCmd` slot), not flattened
/// args: it must keep the `--config` flag of the node and appear beside `init`.
pub type SmartnetCli = crate::Cli<
    crate::chainspec::EthereumChainSpecParser,
    crate::commands::node::NoArgs,
    smartnet_rpc_server_types::DefaultRpcModuleValidator,
    ValidatorCommands,
>;

/// The old `smartnet-validator` command surface, reused verbatim.
#[derive(Debug, clap::Subcommand)]
pub enum ValidatorCommands {
    /// Import a plaintext private-key file into an encrypted keystore, point the config at
    /// it, then delete the source (the only key-change interface).
    Keystore(smartnet_validator::KeystoreCli),
    /// Send a seed transaction via the node.
    SeedTx,
    /// Read-only health probe; `--finalize` opts in to explicit fork-choice steering.
    Ping(smartnet_validator::PingCli),
}

impl ExtendedCommand for ValidatorCommands {
    fn execute(self, runner: CliRunner, cfg_path: PathBuf) -> eyre::Result<()> {
        let cli = smartnet_validator::Cli {
            command: Some(match self {
                Self::Keystore(k) => smartnet_validator::Commands::Keystore(k),
                Self::SeedTx => smartnet_validator::Commands::SeedTx,
                Self::Ping(p) => smartnet_validator::Commands::Ping(p),
            }),
            config: Some(cfg_path),
            dump_payload: None,
        };
        let code = runner.block_on(async move { smartnet_validator::run_validator_cli(cli).await });
        if code != 0 {
            std::process::exit(code);
        }
        Ok(())
    }
}
