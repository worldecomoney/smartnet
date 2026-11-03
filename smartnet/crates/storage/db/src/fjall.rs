//! Helper functions for initializing and opening a database (Fjall backend).

use crate::{is_database_empty, TableSet, Tables};
use eyre::Context;
use smartnet_tracing::tracing::info;
use std::path::Path;

pub use crate::implementation::fjall::{cursor::Cursor, tx::Tx, *};

/// Tables that have been removed from the schema but may still exist on disk from previous
/// versions. Fjall only opens the keyspaces of the current table set, so these are never present.
const ORPHAN_TABLES: &[&str] = &["AccountsTrieChangeSets", "StoragesTrieChangeSets"];

/// Creates a new database at the specified path if it doesn't exist. Does NOT create keyspaces.
/// Check [`init_db`].
pub fn create_db<P: AsRef<Path>>(path: P, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
    use crate::implementation::backend::{check_engine_separation, DatabaseBackend};

    let rpath = path.as_ref();
    // Engine isolation: resolve the selected backend once and validate the layout
    // inside that backend's private directory, never in the shared one
    // (`implementation::backend`).
    let engine_dir = check_engine_separation(rpath, DatabaseBackend::resolve(&args))?;
    let fresh = is_database_empty(&engine_dir);
    if fresh {
        smartnet_fs_util::create_dir_all(&engine_dir).wrap_err_with(|| {
            format!("Could not create database directory {}", engine_dir.display())
        })?;
    }

    let db = DatabaseEnv::open(rpath, DatabaseEnvKind::RW, args)?;
    Ok(db)
}

/// Opens up an existing database or creates a new one at the specified path. Creates keyspaces
/// defined in [`Tables`] if necessary. Read/Write mode.
pub fn init_db<P: AsRef<Path>>(path: P, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
    init_db_for::<P, Tables>(path, args)
}

/// Opens up an existing database or creates a new one at the specified path. Creates keyspaces
/// defined in the given [`TableSet`] if necessary. Read/Write mode.
///
/// Enforces the schema version recorded in the `SchemaInfo` row
/// ([`crate::version::check_schema_version`]): writes [`crate::version::DB_VERSION`] on
/// fresh init, refuses a missing row or a mismatch on an existing database. Fresh-init-
/// only: a mismatch is refused, never migrated.
pub fn init_db_for<P: AsRef<Path>, TS: TableSet>(
    path: P,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    use crate::{
        implementation::backend::{check_engine_separation, DatabaseBackend},
        version::check_schema_version,
    };

    let client_version = args.client_version().clone();
    // Captured before the open, which creates the engine directory if it is absent.
    let engine_dir = check_engine_separation(path.as_ref(), DatabaseBackend::resolve(&args))?;
    let fresh = is_database_empty(&engine_dir);
    let mut db = create_db(path, args)?;
    // The `SchemaInfo` keyspace exists only after table tracking, so the version gate
    // runs here rather than inside `create_db` — before any orphan drop.
    db.create_and_track_tables_for::<TS>()?;
    check_schema_version(&db, fresh)?;
    db.record_client_version(client_version)?;
    drop_orphan_tables(&db);
    Ok(db)
}

/// Drops orphaned keyspaces that are no longer part of the schema.
fn drop_orphan_tables(db: &DatabaseEnv) {
    for table_name in ORPHAN_TABLES {
        match db.drop_orphan_table(table_name) {
            Ok(true) => {
                info!(target: "smartnet::db", table = %table_name, "Dropped orphaned database table");
            }
            Ok(false) => {}
            Err(e) => {
                smartnet_tracing::tracing::warn!(
                    target: "smartnet::db",
                    table = %table_name,
                    %e,
                    "Failed to drop orphaned database table"
                );
            }
        }
    }
}

/// Opens up an existing database. Read only mode. It doesn't create it or create keyspaces if
/// missing.
pub fn open_db_read_only(
    path: impl AsRef<Path>,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    let path = path.as_ref();
    DatabaseEnv::open(path, DatabaseEnvKind::RO, args)
        .with_context(|| format!("Could not open database at path: {}", path.display()))
}

/// Opens up an existing database. Read/Write mode. It doesn't create it or create keyspaces if
/// missing.
pub fn open_db(path: impl AsRef<Path>, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
    fn open(path: &Path, args: DatabaseArguments) -> eyre::Result<DatabaseEnv> {
        let client_version = args.client_version().clone();
        let db = DatabaseEnv::open(path, DatabaseEnvKind::RW, args)
            .with_context(|| format!("Could not open database at path: {}", path.display()))?;
        db.record_client_version(client_version)?;
        Ok(db)
    }
    open(path.as_ref(), args)
}
