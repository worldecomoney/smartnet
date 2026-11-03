//! Database schema-version utils.
//!
//! The schema version lives inside the database, in the singleton `SchemaInfo` row
//! (SQLITE.md §11 A `schema_info`, §13.5 3B.4). It replaces the former
//! `database.version` sidecar file: the version is stored in the same store it
//! describes, so a datadir can never carry a version that disagrees with its
//! keyspaces. Fresh-init-only policy: a mismatch is refused, never migrated.

use crate::{
    implementation::fjall::DatabaseEnv,
    tables::{Migrations, SchemaInfo, SchemaInfoKey},
};
use smartnet_db_api::{
    database::Database,
    models::{StoredMigration, StoredSchemaInfo},
    transaction::{DbTx, DbTxMut},
    DatabaseError,
};

/// The version of the database recorded in the [`SchemaInfo`] table on fresh init.
///
/// v3 (F2): state/history keyspace-group classification — the schema is physically
/// unchanged (fresh-init-only; no v2 migration — the schema-version check refuses the
/// mismatch, so the policy needs no migration code, SQLITE.md §14.4).
pub const DB_VERSION: u64 = 3;

/// Error when checking a database schema version.
#[derive(thiserror::Error, Debug)]
pub enum DatabaseVersionError {
    /// The database has no [`SchemaInfo`] row; it predates in-DB version tracking (or is
    /// not a database). Fresh-init-only: refused, never healed by writing the row.
    #[error("unable to determine the version of the database, schema_info row is missing")]
    MissingSchemaInfo,
    /// Breaking database change detected.
    ///
    /// Your database version is incompatible with the latest database version.
    #[error(
        "breaking database change detected: your database version (v{version}) \
         is incompatible with the latest database version (v{DB_VERSION})"
    )]
    VersionMismatch {
        /// The detected version in the database.
        version: u64,
    },
    /// IO error occurred while reading or writing the schema version row.
    #[error("database error while checking schema version: {0}")]
    Database(#[from] DatabaseError),
}

/// Enforces the schema version recorded in the [`SchemaInfo`] row of an open database.
///
/// * `fresh == true` (the database directory was empty before it was opened): writes [`DB_VERSION`]
///   into the row if it is absent. The row is the single source of truth going forward — the former
///   `database.version` sidecar file is gone.
/// * `fresh == false`: refuses a missing row (a pre-`schema_info` datadir, or not a database at
///   all) and a row whose version differs from [`DB_VERSION`]. Fresh-init- only: no migration is
///   attempted, so no migration code is needed for the policy.
pub fn check_schema_version(db: &DatabaseEnv, fresh: bool) -> Result<(), DatabaseVersionError> {
    let row = db.tx()?.get::<SchemaInfo>(SchemaInfoKey::Singleton)?;
    match row {
        Some(StoredSchemaInfo { schema_version }) if schema_version == DB_VERSION => Ok(()),
        Some(StoredSchemaInfo { schema_version }) => {
            Err(DatabaseVersionError::VersionMismatch { version: schema_version })
        }
        None if fresh => {
            let tx_mut = db.tx_mut()?;
            tx_mut.put::<SchemaInfo>(
                SchemaInfoKey::Singleton,
                StoredSchemaInfo { schema_version: DB_VERSION },
            )?;
            tx_mut.commit()?;
            Ok(())
        }
        None => Err(DatabaseVersionError::MissingSchemaInfo),
    }
}

/// Records an applied schema migration in the [`Migrations`] log table.
///
/// Empty under the fresh-init-only policy — present so a future migration framework has
/// its log. Returns whether a row was written (`false` if it already existed).
pub fn record_migration(
    db: &DatabaseEnv,
    schema_version: u64,
    description: impl Into<alloy_primitives::Bytes>,
) -> Result<bool, DatabaseError> {
    let tx_mut = db.tx_mut()?;
    if tx_mut.get::<Migrations>(schema_version)?.is_some() {
        return Ok(false);
    }
    tx_mut.put::<Migrations>(
        schema_version,
        StoredMigration { schema_version, description: description.into() },
    )?;
    tx_mut.commit()?;
    Ok(true)
}
