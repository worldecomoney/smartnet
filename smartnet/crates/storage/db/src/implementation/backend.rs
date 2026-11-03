//! Engine selection and engine-directory isolation for the Fjall backend.
//!
//! Fjall is the only storage engine; it is always compiled in (the `fjall` feature is
//! the crate default and the shipped node binary's only engine). The historical
//! `SQLite` and `redb` engines were removed (F5); their names are still *parsed* by
//! [`DatabaseBackend::from_str`] so an old config fails with a clear "removed" error
//! instead of an unknown-key error.
//!
//! [`DatabaseBackend`] still drives the on-disk layout: Fjall owns exactly one private
//! subdirectory of the configured database path (`fjall/`), so the layout a node
//! operator sees (`DATADIR_PATH/DB_ENGINE`) stays stable and a pre-F5 `sqlite/` or
//! `redb/` tree is never mistaken for the live database.
//!
//! # Engine isolation
//!
//! [`check_engine_separation`] runs on every open, read-only alike: it fails the open
//! when the shared directory still holds a pre-separation layout (where engines wrote
//! directly into the directory they were given) or when the private Fjall directory
//! contains artifacts of a removed engine, instead of silently mixing the two. Sibling
//! engine directories under the shared root (a stale `sqlite/` next to `fjall/`) are
//! fine: they are inert.

use crate::DatabaseError;
use smartnet_storage_errors::db::DatabaseErrorInfo;
use std::{
    fmt::{self, Debug},
    path::{Path, PathBuf},
};

/// Builds a [`DatabaseErrorInfo`] from any displayable error.
pub(crate) fn err_info(error: impl fmt::Display) -> DatabaseErrorInfo {
    DatabaseErrorInfo { message: error.to_string().into(), code: 0 }
}

/// Storage backend of a database environment.
///
/// Fjall is the only engine; the enum stays because the datadir layout and the
/// `DB_ENGINE` config key are part of the operator-facing surface
/// (`DATADIR_PATH/<engine>`), and so that a removed engine name can be rejected with a
/// dedicated message rather than falling out of the type system.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DatabaseBackend {
    /// The Fjall LSM-tree backend (the only engine).
    #[default]
    Fjall,
}

/// Whether this build compiles the Fjall backend. Always `true`: Fjall is not optional
/// in the shipped node binary.
pub const FJALL_COMPILED: bool = cfg!(feature = "fjall");

impl DatabaseBackend {
    /// The single source of truth for engine selection: the `SMARTNET_DB_BACKEND`
    /// environment variable when it names a known backend, otherwise
    /// [`DatabaseArguments::backend`](crate::DatabaseArguments::backend). Every open
    /// resolves the backend through this function, so no code path can end up selecting
    /// a different engine than another.
    ///
    /// It is also the single source of truth for datadir paths: the CLI resolves the
    /// configured engine through this function and scopes every path as
    /// `DATADIR_PATH/DB_ENGINE`, so the paths a command touches always belong to the
    /// engine the database open selects — regardless of whether the engine came from
    /// config (`DB_ENGINE`) or from the environment override.
    pub fn resolve(args: &crate::DatabaseArguments) -> Self {
        match std::env::var("SMARTNET_DB_BACKEND") {
            Ok(value) => value.parse().unwrap_or_else(|_| args.backend()),
            Err(_) => args.backend(),
        }
    }

    /// The name of this backend's private subdirectory under the configured database
    /// path: `fjall/`.
    ///
    /// The name is part of the on-disk layout: an engine never reads or writes a
    /// sibling engine's directory, which is what keeps engines separated.
    #[must_use]
    pub const fn subdir(&self) -> &'static str {
        match self {
            Self::Fjall => "fjall",
        }
    }

    /// The private directory of this backend under the shared database directory `base`
    /// (`base/<subdir>`). This is the only directory the backend may touch.
    #[must_use]
    pub fn dir_under(&self, base: &Path) -> PathBuf {
        base.join(self.subdir())
    }
}

impl fmt::Display for DatabaseBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.subdir())
    }
}

impl std::str::FromStr for DatabaseBackend {
    type Err = String;

    /// Parses an engine name (`fjall`; case-insensitive). The removed engines `sqlite`
    /// and `redb` are rejected with a dedicated message so an old config file names
    /// the problem, and every other name is rejected as unknown.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("fjall") {
            return Ok(Self::Fjall);
        }
        if s.eq_ignore_ascii_case("sqlite") || s.eq_ignore_ascii_case("redb") {
            return Err(format!(
                "database engine {s:?} was removed: Fjall is the only engine — set \
                 `DB_ENGINE = \"fjall\"` (or remove the key) to keep this database path"
            ));
        }
        Err(format!("unknown database engine {s:?} (valid engines: fjall)"))
    }
}

/// Entries in the shared database directory that identify the pre-separation layout,
/// where every engine wrote directly into the directory it was given.
const LEGACY_ROOT_MARKERS: &[&str] = &[
    // Written by `create_db` for every backend.
    "database.version",
    // Directory lock of `StorageLock` and of Fjall itself (same name, engine-local).
    "lock",
    // Fjall's own layout: keyspaces folder, journal and format version marker.
    "keyspaces",
    "0.jnl",
    "version",
    // SQLite's own layout: database file and its WAL sidecars.
    "sqlite.db",
    "sqlite.db-wal",
    "sqlite.db-shm",
    // redb's own layout: its single database file.
    "redb.db",
];

/// Artifacts of the removed engines that must never appear inside the Fjall engine
/// directory (they would mean the directory was once another engine's).
const FOREIGN_TO_FJALL_MARKERS: &[&str] =
    &["sqlite.db", "sqlite.db-wal", "sqlite.db-shm", "redb.db"];

/// Returns the markers of `names` present as direct entries of `dir`, sorted for
/// deterministic error messages. A missing directory contains no markers.
fn find_markers(dir: &Path, names: &[&'static str]) -> Vec<&'static str> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };

    let mut found: Vec<&'static str> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            names.iter().copied().find(|marker| name == **marker)
        })
        .collect();
    found.sort_unstable();
    found
}

/// Verifies that `base` and the private Fjall directory under it hold no data of a
/// pre-separation layout and no artifacts of a removed engine, returning the engine
/// directory to use.
///
/// This runs on every open, read-only alike, so the Fjall engine can never read — or
/// silently ignore — another engine's files:
///
/// * entries of the shared directory that belong to the old shared layout (any engine) fail the
///   open with migration instructions, instead of forking a fresh empty database next to the old
///   data;
/// * artifacts of a removed engine inside the private directory fail the open instead of being
///   mixed in.
///
/// Sibling engine directories under `base` (e.g. a stale `sqlite/` next to `fjall/`)
/// are fine: they are private to their own engine and Fjall never looks inside them.
pub(crate) fn check_engine_separation(
    base: &Path,
    backend: DatabaseBackend,
) -> Result<PathBuf, DatabaseError> {
    let dir = backend.dir_under(base);

    let legacy = find_markers(base, LEGACY_ROOT_MARKERS);
    if !legacy.is_empty() {
        return Err(DatabaseError::Open(err_info(format!(
            "database directory {} holds pre-separation data ({}); engine data now lives \
             in {} — move those entries into {} to keep engines separated, or remove \
             them to start fresh",
            base.display(),
            legacy.join(", "),
            dir.display(),
            dir.display(),
        ))));
    }

    let foreign = find_markers(&dir, FOREIGN_TO_FJALL_MARKERS);
    if !foreign.is_empty() {
        return Err(DatabaseError::Open(err_info(format!(
            "engine directory {} contains artifacts of a removed engine ({}) — refusing \
             to open {} on top of foreign data",
            dir.display(),
            foreign.join(", "),
            backend.subdir(),
        ))));
    }

    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{init_db, DatabaseArguments};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn init_db_rejects_pre_separation_layout_end_to_end() {
        let base = tempdir().unwrap();
        fs::write(base.path().join("database.version"), "2").unwrap();
        fs::write(base.path().join("sqlite.db"), b"legacy").unwrap();

        let error = init_db(base.path(), DatabaseArguments::test()).unwrap_err();
        let message = format!("{:?}", error.downcast::<DatabaseError>().unwrap());
        assert!(message.contains("database.version"), "{message}");
        assert!(message.contains("sqlite.db"), "{message}");
    }
}
