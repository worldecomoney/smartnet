//! Extension subcommands carried over from the merged validator:
//! `smartnet keystore`, `smartnet seedTx`, `smartnet ping` — the exact same clap structs
//! (`smartnet_validator`) driven through the validator's own dispatch.
//!
//! `--config` is global on the node CLI, so both orders work:
//! `smartnet keystore RAW_KEY --config CFG` and `smartnet --config CFG keystore RAW_KEY`.
//! With no `--config`, the global rule searches `./config.toml` then `~/.smartnet/config.toml`;
//! with `--config`, that exact file is used and no search happens.

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

/// The old `smartnet-validator` command surface.
#[derive(Debug, clap::Subcommand)]
pub enum ValidatorCommands {
    /// Write an encrypted keystore from an optional plaintext private-key file. With no file,
    /// a fresh random key is generated. `KEY_PATH` and `PASSWORD_FILE` come from `--config`.
    Keystore(smartnet_validator::KeystoreCli),
    /// Send a seed transaction via the node.
    SeedTx,
    /// Read-only health probe; `--finalize` opts in to explicit fork-choice steering.
    Ping(smartnet_validator::PingCli),
}

impl ExtendedCommand for ValidatorCommands {
    fn execute(self, runner: CliRunner, cfg_path: Option<PathBuf>) -> eyre::Result<()> {
        // Pass the explicit `--config` through unchanged. The validator CLI applies the same
        // global rule as the node: no search when it is present, cwd → ~/.smartnet otherwise.
        let command = match self {
            Self::Keystore(k) => smartnet_validator::Commands::Keystore(k),
            Self::SeedTx => smartnet_validator::Commands::SeedTx,
            Self::Ping(p) => smartnet_validator::Commands::Ping(p),
        };
        let cli = smartnet_validator::Cli {
            command: Some(command),
            config: cfg_path,
            dump_payload: None,
        };
        let code = runner.block_on(async move { smartnet_validator::run_validator_cli(cli).await });
        if code != 0 {
            std::process::exit(code);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::Commands;
    use clap::Parser;

    /// `--config` is global: both operator spellings parse to the same command.
    #[test]
    fn keystore_parses_config_before_and_after_the_subcommand() {
        for argv in [
            vec![
                "smartnet",
                "keystore",
                "/tmp/raw.key",
                "--config",
                "/home/u/smartnet/config.toml",
            ],
            vec![
                "smartnet",
                "--config",
                "/home/u/smartnet/config.toml",
                "keystore",
                "/tmp/raw.key",
            ],
        ] {
            let cli = SmartnetCli::try_parse_from(argv).expect("both orders must parse");
            assert_eq!(cli.config_path, Some(PathBuf::from("/home/u/smartnet/config.toml")));
            match cli.command {
                Some(Commands::Ext(ValidatorCommands::Keystore(k))) => {
                    assert_eq!(k.private_key_file, Some(PathBuf::from("/tmp/raw.key")));
                }
                other => panic!("expected keystore, got {other:?}"),
            }
        }
    }

    /// No positional key is legal (random key), but the old flag spellings are not.
    #[test]
    fn keystore_has_no_key_or_password_flags() {
        let cli = SmartnetCli::try_parse_from([
            "smartnet",
            "--config",
            "/home/u/smartnet/config.toml",
            "keystore",
        ])
        .expect("a generated key needs no positional file");
        match cli.command {
            Some(Commands::Ext(ValidatorCommands::Keystore(k))) => {
                assert_eq!(k.private_key_file, None);
            }
            other => panic!("expected keystore, got {other:?}"),
        }
        assert!(
            SmartnetCli::try_parse_from([
                "smartnet",
                "keystore",
                "--key",
                "/tmp/k",
                "--config",
                "/home/u/smartnet/config.toml",
            ])
            .is_err(),
            "--key must be rejected"
        );
        assert!(
            SmartnetCli::try_parse_from([
                "smartnet",
                "keystore",
                "--password",
                "/tmp/pw",
                "--config",
                "/home/u/smartnet/config.toml",
            ])
            .is_err(),
            "--password must be rejected"
        );
    }
}
