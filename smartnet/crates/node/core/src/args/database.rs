//! clap [Args](clap::Args) for database configuration

use crate::version::default_client_version;
use clap::Args;
use smartnet_db::{ClientVersion, DatabaseArguments, DatabaseBackend};
use smartnet_storage_errors::db::LogLevel;

/// Parameters for database configuration.
#[derive(Debug, Args, PartialEq, Eq, Default, Clone, Copy)]
#[command(next_help_heading = "Database")]
pub struct DatabaseArgs {
    /// Database logging level. Levels higher than "notice" require a debug build.
    #[arg(skip)]
    pub log_level: Option<LogLevel>,

    /// The storage engine selected by the operator config (`DB_ENGINE`, default `fjall`).
    ///
    /// Not a CLI flag: the config file dictates the engine, and
    /// [`DatabaseBackend::resolve`] applies the `SMARTNET_DB_BACKEND` environment
    /// override on top when the database or a datadir path is opened.
    #[arg(skip)]
    pub engine: DatabaseBackend,
}

impl DatabaseArgs {
    /// Returns default database arguments with configured log level and client version.
    pub fn database_args(&self) -> DatabaseArguments {
        self.get_database_args(default_client_version())
    }

    /// Returns the database arguments with configured log level and client version.
    pub const fn get_database_args(&self, client_version: ClientVersion) -> DatabaseArguments {
        DatabaseArguments::smartnet(client_version)
            .with_log_level(self.log_level)
            .with_backend(self.engine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    /// The operator CLI is config-file driven: these per-flag options were removed
    /// (all struct fields are `#[arg(skip)]`). The legacy flags must stay rejected.
    #[test]
    fn removed_cli_flags_are_rejected() {
        const REMOVED: &[&str] = &[
            "--db.log-level",
        ];
        for flag in REMOVED {
            assert!(
                CommandParser::<DatabaseArgs>::try_parse_from(["smartnet", flag, "x"]).is_err(),
                "{flag} must stay rejected (config-file-driven CLI)"
            );
        }
    }

    #[test]
    fn test_default_database_args() {
        let default_args = DatabaseArgs::default();
        let args = CommandParser::<DatabaseArgs>::parse_from(["smartnet"]).args;
        assert_eq!(args, default_args);
    }

    #[test]
    fn test_command_parser_without_log_level() {
        let cmd = CommandParser::<DatabaseArgs>::try_parse_from(["smartnet"]).unwrap();
        assert_eq!(cmd.args.log_level, None);
    }
}
