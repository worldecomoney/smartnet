//! clap [Args](clap::Args) for datadir config

use crate::dirs::{ChainPath, DataDirPath, MaybePlatformPath};
use clap::Args;
use smartnet_chainspec::Chain;
use smartnet_db::DatabaseBackend;
use std::path::PathBuf;

/// Parameters for datadir configuration
#[derive(Debug, Args, PartialEq, Eq, Default, Clone)]
#[command(next_help_heading = "Datadir")]
pub struct DatadirArgs {
    /// The path to the data dir for all node files and subdirectories.
    ///
    /// When omitted, resolves to [`DEFAULT_DATADIR_PATH`](crate::dirs::DEFAULT_DATADIR_PATH)
    /// (Linux: `$XDG_DATA_HOME/smartnet/` or `$HOME/.local/share/smartnet/`; see
    /// [`default_datadir_path`](crate::dirs::default_datadir_path)). Per-chain files live
    /// under `<datadir>/<chain_id>/`.
    #[arg(skip)]
    pub datadir: MaybePlatformPath<DataDirPath>,

    /// The storage engine whose data lives under this datadir.
    ///
    /// Not a CLI flag: the value is stamped at the resolution points (`EnvironmentArgs::init`,
    /// `NodeCommand::execute`) from the configured engine — config key `DB_ENGINE`, default
    /// `fjall`, overridden by the `SMARTNET_DB_BACKEND` environment variable — via
    /// [`DatabaseBackend::resolve`], so every path derived from this datadir belongs to the
    /// exact engine the database open selects. All paths are scoped as `DATADIR_PATH/DB_ENGINE`
    /// (never a hard-coded location — the engine directory is a child of whatever the
    /// config's datadir names), which makes engine switching restart-agnostic: a
    /// different `DB_ENGINE` in config points the node at a different, independent tree.
    #[arg(skip)]
    pub engine: DatabaseBackend,

    /// The absolute path to store pprof dumps in.
    #[arg(skip)]
    pub pprof_dumps_path: Option<PathBuf>,
}

impl DatadirArgs {
    /// Resolves the final datadir path.
    pub fn resolve_datadir(self, chain: Chain) -> ChainPath<DataDirPath> {
        let datadir = self.datadir.clone();
        datadir.unwrap_or_chain_default(chain, self)
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

    #[test]
    fn test_parse_datadir_args() {
        let default_args = DatadirArgs::default();
        let args = CommandParser::<DatadirArgs>::parse_from(["smartnet"]).args;
        assert_eq!(args, default_args);
    }
}
