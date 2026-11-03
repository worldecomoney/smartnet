//! `SmartNet` CLI implementation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// A configurable App on top of the cli parser.
pub mod app;
/// Chain specification parser.
pub mod chainspec;
/// Validator subcommands merged into the node binary (`keystore` / `seedTx` / `ping`).
pub mod ext;
pub mod interface;

/// CLI abstraction (ChainSpecParser trait, NodeCli) — merged from `smartnet-cli-core`.
pub mod cli_core;
/// Operator-facing commands (`init`, default node launch) — merged from `smartnet-cli-commands`.
pub mod commands;
/// Tokio based CLI runner — merged from `smartnet-cli-runner`.
pub mod runner;

pub use app::{CliApp, ExtendedCommand};
pub use ext::ValidatorCommands;
pub use interface::{Cli, Commands, NoSubCmd};
pub use runner::{CliContext, CliRunner};

/// Code-built canonical chain rulebook — test/dev-only (`test-utils`), runtime-unused.
#[cfg(any(test, feature = "test-utils"))]
pub mod canonical;

#[cfg(test)]
mod test {
    use crate::{chainspec::EthereumChainSpecParser, commands::node::NoArgs, interface::Cli};

    #[test]
    fn parse_rejects_dev_flag() {
        let dir = tempfile::tempdir().unwrap();
        let genesis = dir.path().join("genesis.json");
        std::fs::write(
            &genesis,
            r#"{"config":{"chainId":315,"clique":{"period":1,"epoch":30000}},"difficulty":"0x1","gasLimit":"0x1","timestamp":"0x0"}"#,
        )
        .unwrap();
        let err = Cli::<EthereumChainSpecParser, NoArgs>::try_parse_args_from([
            "smartnet",
            "init",
            genesis.to_str().unwrap(),
            "--dev",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
