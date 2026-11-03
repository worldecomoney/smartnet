//! Cli abstraction for SmartNet based nodes.

use crate::runner::CliRunner;
use clap::{Error, Parser};
use smartnet_db::ClientVersion;
use std::{borrow::Cow, ffi::OsString};

/// The chainspec module defines the different chainspecs that can be used by the node.
pub mod chainspec;
use self::chainspec::ChainSpecParser;

/// `SmartNet` based node cli.
///
/// This trait is supposed to be implemented by the main struct of the CLI.
///
/// It provides commonly used functionality for running commands and information about the CLI, such
/// as the name and version.
pub trait NodeCli: Sized {
    /// The associated `ChainSpecParser` type
    type ChainSpecParser: ChainSpecParser;

    /// The name of the implementation, eg. `SmartNet`.
    fn name(&self) -> Cow<'static, str>;

    /// The version of the node, such as `smartnet/v1.0.0`
    fn version(&self) -> Cow<'static, str>;

    /// Parse args from iterator from [`std::env::args_os()`].
    fn parse_args() -> Result<Self, Error>
    where
        Self: Parser,
    {
        <Self as NodeCli>::try_parse_from(std::env::args_os())
    }

    /// Parse args from the given iterator.
    fn try_parse_from<I, T>(itr: I) -> Result<Self, Error>
    where
        Self: Parser,
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        <Self as Parser>::try_parse_from(itr)
    }

    /// Executes a command.
    fn with_runner<F, R>(self, f: F, runner: CliRunner) -> R
    where
        F: FnOnce(Self, CliRunner) -> R,
    {
        f(self, runner)
    }

    /// Parses and executes a command.
    fn execute<F, R>(f: F) -> Result<R, Error>
    where
        Self: Parser,
        F: FnOnce(Self, CliRunner) -> R,
    {
        let cli = Self::parse_args()?;
        let runner = CliRunner::try_default_runtime()
            .map_err(|e| Error::raw(clap::error::ErrorKind::Io, e))?;
        Ok(cli.with_runner(f, runner))
    }

    /// The client version of the node.
    fn client_version() -> ClientVersion;
}
