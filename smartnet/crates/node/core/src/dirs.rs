//! `SmartNet` data directories.
//!
//! **Default datadir** (when `--datadir` is not passed): full path in [`DEFAULT_DATADIR_PATH`]
//! (see also [`default_datadir_path`]). Used by [`DataDirPath`](DataDirPath) and
//! [`DatadirArgs`](crate::args::DatadirArgs). **`--datadir`** overrides this for all chain-scoped
//! paths.

use crate::{args::DatadirArgs, utils::parse_path};
use smartnet_chainspec::Chain;
use smartnet_db::DatabaseBackend;
use std::{
    convert::Infallible,
    fmt::{Debug, Display, Formatter},
    path::{Path, PathBuf},
    str::FromStr,
    sync::LazyLock,
};

/// Scopes `path` under its storage engine's private subdirectory: the engine name is
/// appended to the configured datadir — the rule is **`DATADIR_PATH/DB_ENGINE`** (never a
/// hard-coded location: whatever `DATADIR_PATH` names, the engine directory is its child).
///
/// Every path the node derives from its datadir goes through this rule, which is what
/// keeps engine data out of the shared root: the engine directory (`fjall/`) is created
/// (fresh) on first use and is the only tree the node ever touches.
pub fn engine_scoped_datadir(path: &Path, engine: DatabaseBackend) -> PathBuf {
    path.join(engine.subdir())
}

/// Constructs a string to be used as a path for configuration and db paths.
pub fn config_path_prefix(chain: Chain) -> String {
    chain.to_string()
}

fn compute_default_datadir_path() -> Option<PathBuf> {
    dirs_next::data_dir().map(|root| root.join("smartnet"))
}

/// Full filesystem path to the default datadir when `--datadir` is not set (same directory as
/// [`default_datadir_path`] when the OS provides a data directory).
///
/// Panics if no default exists; use [`default_datadir_path`] for a fallible alternative or pass
/// `--datadir` explicitly.
pub static DEFAULT_DATADIR_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    compute_default_datadir_path().unwrap_or_else(|| {
        panic!("Could not resolve default datadir path. Set --datadir explicitly.")
    })
});

/// Returns the full default datadir path when `--datadir` is omitted (`None` if the OS has no data
/// directory).
pub fn default_datadir_path() -> Option<PathBuf> {
    compute_default_datadir_path()
}

/// Returns the path to the `SmartNet` data directory.
///
/// Same as [`default_datadir_path`].
pub fn data_dir() -> Option<PathBuf> {
    default_datadir_path()
}

/// Legacy per-chain config filename (read-only fallback when migrating datadirs).
pub const LEGACY_CHAIN_CONFIG_FILENAME: &str = "reth.toml";

/// Canonical per-chain config filename.
pub const CHAIN_CONFIG_FILENAME: &str = "smartnet.toml";

/// Prefer [`CHAIN_CONFIG_FILENAME`]; if missing, use legacy [`LEGACY_CHAIN_CONFIG_FILENAME`] in the
/// same directory.
pub fn resolve_config_file(primary: &Path) -> PathBuf {
    if primary.exists() {
        return primary.to_path_buf();
    }
    if let Some(parent) = primary.parent() {
        let legacy = parent.join(LEGACY_CHAIN_CONFIG_FILENAME);
        if legacy.exists() {
            return legacy;
        }
    }
    primary.to_path_buf()
}

/// Returns the path to the database.
///
/// Refer to [`dirs_next::data_dir`] for cross-platform behavior.
pub fn database_path() -> Option<PathBuf> {
    data_dir().map(|root| root.join("db"))
}

/// Returns the path to the configuration directory.
///
/// Refer to [`dirs_next::config_dir`] for cross-platform behavior.
pub fn config_dir() -> Option<PathBuf> {
    dirs_next::config_dir().map(|root| root.join("smartnet"))
}

/// Returns the path to the cache directory.
///
/// Refer to [`dirs_next::cache_dir`] for cross-platform behavior.
pub fn cache_dir() -> Option<PathBuf> {
    dirs_next::cache_dir().map(|root| root.join("smartnet"))
}

/// Returns the path to the `SmartNet` logs directory.
///
/// Logs live outside every datadir and engine tree (shared across engines), so switching
/// `DB_ENGINE` never moves them.
///
/// Refer to [`dirs_next::home_dir`] for cross-platform behavior.
pub fn logs_dir() -> Option<PathBuf> {
    dirs_next::home_dir().map(|root| root.join(".smartnet/node/logs"))
}

/// Returns the path to the data dir.
///
/// The data dir should contain a subdirectory for each chain, and those chain directories will
/// include all information for that chain, such as the p2p secret.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct DataDirPath;

impl XdgPath for DataDirPath {
    fn resolve() -> Option<PathBuf> {
        default_datadir_path()
    }
}

/// Returns the node logs directory.
///
/// Refer to [`dirs_next::cache_dir`] for cross-platform behavior.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct LogsDir;

impl XdgPath for LogsDir {
    fn resolve() -> Option<PathBuf> {
        logs_dir()
    }
}

/// A small helper trait for unit structs that represent a standard path following the XDG
/// path specification.
pub trait XdgPath {
    /// Resolve the standard path.
    fn resolve() -> Option<PathBuf>;
}

/// A wrapper type that either parses a user-given path or defaults to an
/// OS-specific path.
///
/// The [`FromStr`] implementation parses a string into a path.
///
/// # Example
///
/// ```
/// use smartnet_node_core::dirs::{DataDirPath, PlatformPath};
/// use std::str::FromStr;
///
/// // Resolves to the platform-specific database path
/// let default: PlatformPath<DataDirPath> = PlatformPath::default();
/// // Resolves to `$(pwd)/my/path/to/datadir`
/// let custom: PlatformPath<DataDirPath> = PlatformPath::from_str("my/path/to/datadir").unwrap();
///
/// assert_ne!(default.as_ref(), custom.as_ref());
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct PlatformPath<D>(PathBuf, std::marker::PhantomData<D>);

impl<D> Display for PlatformPath<D> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl<D> Clone for PlatformPath<D> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), std::marker::PhantomData)
    }
}

impl<D: XdgPath> Default for PlatformPath<D> {
    fn default() -> Self {
        Self(
            D::resolve().expect("Could not resolve default path. Set one manually."),
            std::marker::PhantomData,
        )
    }
}

impl<D> FromStr for PlatformPath<D> {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(parse_path(s), std::marker::PhantomData))
    }
}

impl<D> AsRef<Path> for PlatformPath<D> {
    fn as_ref(&self) -> &Path {
        self.0.as_path()
    }
}

impl<D> From<PlatformPath<D>> for PathBuf {
    fn from(value: PlatformPath<D>) -> Self {
        value.0
    }
}

impl<D> PlatformPath<D> {
    /// Returns the path joined with another path
    pub fn join<P: AsRef<Path>>(&self, path: P) -> Self {
        Self(self.0.join(path), std::marker::PhantomData)
    }
}

impl<D> PlatformPath<D> {
    /// Converts the path to a `ChainPath` with the given `Chain`.
    pub fn with_chain(&self, chain: Chain, datadir_args: DatadirArgs) -> ChainPath<D> {
        // extract chain name
        let platform_path = self.platform_path_from_chain(chain);

        ChainPath::new(platform_path, chain, datadir_args)
    }

    fn platform_path_from_chain(&self, chain: Chain) -> Self {
        let chain_name = config_path_prefix(chain);
        let path = self.0.join(chain_name);
        Self(path, std::marker::PhantomData)
    }

    /// Map the inner path to a new type `T`.
    pub fn map_to<T>(&self) -> PlatformPath<T> {
        PlatformPath(self.0.clone(), std::marker::PhantomData)
    }
}

/// An Optional wrapper type around [`PlatformPath`].
///
/// This is useful for when a path is optional, such as the `--data-dir` flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaybePlatformPath<D>(Option<PlatformPath<D>>);

// === impl MaybePlatformPath ===

impl<D: XdgPath> MaybePlatformPath<D> {
    /// Returns the path if it is set, otherwise returns the default path for the given chain.
    pub fn unwrap_or_chain_default(&self, chain: Chain, datadir_args: DatadirArgs) -> ChainPath<D> {
        let path = self
            .0
            .clone()
            .unwrap_or_else(|| PlatformPath::default().platform_path_from_chain(chain));
        ChainPath::new(path, chain, datadir_args)
    }

    /// Returns the default platform path for the specified [Chain].
    pub fn chain_default(chain: Chain) -> ChainPath<D> {
        PlatformPath::default().with_chain(chain, DatadirArgs::default())
    }

    /// Returns true if a custom path is set
    pub const fn is_some(&self) -> bool {
        self.0.is_some()
    }

    /// Returns the path if it is set, otherwise returns `None`.
    pub fn as_ref(&self) -> Option<&Path> {
        self.0.as_ref().map(|p| p.as_ref())
    }

    /// Returns the path if it is set, otherwise returns the default path, without any chain
    /// directory.
    pub fn unwrap_or_default(&self) -> PlatformPath<D> {
        self.0.clone().unwrap_or_default()
    }
}

impl<D: XdgPath> Display for MaybePlatformPath<D> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.0 {
            path.fmt(f)
        } else {
            // NOTE: this is a workaround for making it work with clap's `default_value_t` which
            // computes the default value via `Default -> Display -> FromStr`
            write!(f, "default")
        }
    }
}

impl<D> Default for MaybePlatformPath<D> {
    fn default() -> Self {
        Self(None)
    }
}

impl<D> FromStr for MaybePlatformPath<D> {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let p = match s {
            "default" => {
                // NOTE: this is a workaround for making it work with clap's `default_value_t` which
                // computes the default value via `Default -> Display -> FromStr`
                None
            }
            _ => Some(PlatformPath::from_str(s)?),
        };
        Ok(Self(p))
    }
}

impl<D> From<PathBuf> for MaybePlatformPath<D> {
    fn from(path: PathBuf) -> Self {
        Self(Some(PlatformPath(path, std::marker::PhantomData)))
    }
}

/// Wrapper type around `PlatformPath` that includes a `Chain`, used for separating node data for
/// different networks.
///
/// If the chain is either mainnet, sepolia, or holesky, then the path will be:
///  * mainnet: `<DIR>/mainnet`
///  * sepolia: `<DIR>/sepolia`
///  * holesky: `<DIR>/holesky`
///
/// Otherwise, the path will be dependent on the chain ID:
///  * `<DIR>/<CHAIN_ID>`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainPath<D>(PlatformPath<D>, Chain, DatadirArgs);

impl<D> ChainPath<D> {
    /// Returns a new `ChainPath` given a `PlatformPath` and a `Chain`.
    ///
    /// The path is scoped under the datadir's storage engine here — the single place
    /// every [`ChainPath`] is built — so `data_dir()`, `db()`, `pprof_dumps()` and the
    /// fallback config path all live in the engine's private tree (see
    /// [`engine_scoped_datadir`]).
    pub fn new(path: PlatformPath<D>, chain: Chain, datadir_args: DatadirArgs) -> Self {
        let scoped = PlatformPath(
            engine_scoped_datadir(path.as_ref(), datadir_args.engine),
            std::marker::PhantomData,
        );
        Self(scoped, chain, datadir_args)
    }

    /// Returns the path to the data directory for this chain.
    ///
    /// `<DIR>/<CHAIN_ID>/<ENGINE>`
    pub fn data_dir(&self) -> &Path {
        self.0.as_ref()
    }

    /// Returns the path to the db directory for this chain.
    ///
    /// `<DIR>/<CHAIN_ID>/<ENGINE>/db`
    pub fn db(&self) -> PathBuf {
        self.data_dir().join("db")
    }

    /// Returns the path to the directory for storing pprof dumps for this chain.
    ///
    /// `<DIR>/<CHAIN_ID>/<ENGINE>/pprof`; an explicit `--datadir.pprof-dumps` override is
    /// engine-scoped by the same rule as the datadir itself.
    pub fn pprof_dumps(&self) -> PathBuf {
        let datadir_args = &self.2;
        if let Some(pprof_dumps_path) = &datadir_args.pprof_dumps_path {
            engine_scoped_datadir(pprof_dumps_path, datadir_args.engine)
        } else {
            self.data_dir().join("pprof")
        }
    }

    /// Returns the path to the blobstore directory for this chain where blobs of unfinalized
    /// transactions are stored.
    ///
    /// `<DIR>/<CHAIN_ID>/blobstore`
    pub fn blobstore(&self) -> PathBuf {
        self.data_dir().join("blobstore")
    }

    /// Returns the path to the local transactions backup file
    ///
    /// `<DIR>/<CHAIN_ID>/txpool-transactions-backup.rlp`
    pub fn txpool_transactions(&self) -> PathBuf {
        self.data_dir().join("txpool-transactions-backup.rlp")
    }

    /// Returns the path to the config file for this chain.
    ///
    /// `<DIR>/<CHAIN_ID>/smartnet.toml` (legacy `reth.toml` is read if present and primary is
    /// missing).
    pub fn config(&self) -> PathBuf {
        self.data_dir().join("smartnet.toml")
    }

    /// Resolves chain config path: prefers `smartnet.toml`, falls back to legacy `reth.toml`.
    pub fn resolve_config(&self) -> PathBuf {
        resolve_config_file(&self.config())
    }

    /// Returns the path to the ExEx WAL directory for this chain.
    pub fn exex_wal(&self) -> PathBuf {
        self.data_dir().join("exex/wal")
    }
}

impl<D> AsRef<Path> for ChainPath<D> {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

impl<D> Display for ChainPath<D> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<D> From<ChainPath<D>> for PathBuf {
    fn from(value: ChainPath<D>) -> Self {
        value.0.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_maybe_data_dir_path() {
        let path = MaybePlatformPath::<DataDirPath>::default();
        let path = path.unwrap_or_chain_default(Chain::mainnet(), DatadirArgs::default());
        // Default engine is Fjall: the engine directory is the datadir's child —
        // `<DIR>/<CHAIN>/<ENGINE>`.
        assert!(path.as_ref().ends_with("smartnet/mainnet/fjall"), "{path:?}");

        let db_path = path.db();
        assert!(db_path.ends_with("smartnet/mainnet/fjall/db"), "{db_path:?}");

        let path = MaybePlatformPath::<DataDirPath>::from_str("my/path/to/datadir").unwrap();
        let path = path.unwrap_or_chain_default(Chain::mainnet(), DatadirArgs::default());
        assert!(path.as_ref().ends_with("my/path/to/datadir/fjall"), "{path:?}");
    }

    #[test]
    fn test_maybe_testnet_datadir_path() {
        let path = MaybePlatformPath::<DataDirPath>::default();
        let path = path.unwrap_or_chain_default(Chain::holesky(), DatadirArgs::default());
        assert!(path.as_ref().ends_with("smartnet/holesky/fjall"), "{path:?}");

        let path = MaybePlatformPath::<DataDirPath>::default();
        let path = path.unwrap_or_chain_default(Chain::sepolia(), DatadirArgs::default());
        assert!(path.as_ref().ends_with("smartnet/sepolia/fjall"), "{path:?}");
    }

    /// The engine directory is a child of whatever `DATADIR_PATH` names —
    /// `DATADIR_PATH/DB_ENGINE` — the layout the operator sees.
    #[test]
    fn engine_scoped_datadir_appends_engine_to_the_datadir() {
        assert_eq!(
            engine_scoped_datadir(Path::new("/srv/datadir"), DatabaseBackend::Fjall),
            PathBuf::from("/srv/datadir/fjall")
        );
        assert_eq!(
            engine_scoped_datadir(Path::new("/mnt/ssd/mydata"), DatabaseBackend::Fjall),
            PathBuf::from("/mnt/ssd/mydata/fjall")
        );
        // Relative single-component and nested datadirs follow the same rule.
        assert_eq!(
            engine_scoped_datadir(Path::new("datadir"), DatabaseBackend::Fjall),
            PathBuf::from("datadir/fjall")
        );
        assert_eq!(
            engine_scoped_datadir(Path::new("a/b/datadir"), DatabaseBackend::Fjall),
            PathBuf::from("a/b/datadir/fjall")
        );
    }

    /// The engine tree is always a strict child of the configured datadir — the scoping
    /// that keeps engine files out of the shared root (and a stale tree of a removed
    /// engine, if one is ever present on disk, inert).
    #[test]
    fn engine_scoped_datadir_is_a_strict_child_of_the_datadir() {
        let base = Path::new("/srv/datadir");
        let fjall = engine_scoped_datadir(base, DatabaseBackend::Fjall);
        assert_ne!(fjall, base);
        assert!(fjall.starts_with(base));
        assert!(!base.starts_with(&fjall));
    }

    /// A full `ChainPath` resolves every derived path into the datadir's engine tree —
    /// including an explicit `--datadir.pprof-dumps` override, which must not bypass the
    /// engine scoping.
    #[test]
    fn chain_path_derives_all_paths_inside_the_engine_tree() {
        let datadir_args = DatadirArgs {
            engine: DatabaseBackend::Fjall,
            pprof_dumps_path: Some(PathBuf::from("/custom/pprof")),
            ..Default::default()
        };
        let path = MaybePlatformPath::<DataDirPath>::from_str("/srv/datadir")
            .unwrap()
            .unwrap_or_chain_default(Chain::mainnet(), datadir_args);
        assert_eq!(path.data_dir(), Path::new("/srv/datadir/fjall"));
        assert_eq!(path.db(), PathBuf::from("/srv/datadir/fjall/db"));
        assert_eq!(path.pprof_dumps(), PathBuf::from("/custom/pprof/fjall"));
    }

    /// Engine scoping is driven by the datadir's engine field alone (the resolution
    /// points stamp it from config + environment), so path derivation never depends on
    /// ambient state and stays stable across restarts.
    #[test]
    fn engine_scoping_is_pure_over_the_engine_field() {
        let args = DatadirArgs { engine: DatabaseBackend::Fjall, ..Default::default() };
        let scoped = engine_scoped_datadir(Path::new("/srv/datadir"), args.engine);
        assert_eq!(scoped, PathBuf::from("/srv/datadir/fjall"));
        assert_eq!(DatadirArgs::default().engine, DatabaseBackend::Fjall);
    }
}
