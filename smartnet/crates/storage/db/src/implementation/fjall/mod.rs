//! Fjall implementation for `SmartNet` database abstraction layer.

use crate::{
    implementation::backend::{check_engine_separation, DatabaseBackend},
    lockfile::StorageLock,
    tables,
    tables::Tables,
    DatabaseError, TableSet,
};
use fjall::{Database as FjallDatabase, Keyspace, KeyspaceCreateOptions, PersistMode};
use smartnet_db_api::{
    cursor::{DbCursorRO, DbCursorRW},
    database::Database,
    database_metrics::DatabaseMetrics,
    models::ClientVersion,
    table::{Compress, Decode, Decompress, Table},
    tables::TableGroup,
    transaction::{DbTx, DbTxMut},
};
use smartnet_storage_errors::db::{
    DatabaseErrorInfo, DatabaseWriteError, DatabaseWriteOperation, LogLevel,
};
use smartnet_tracing::tracing::info;
use std::{
    collections::HashMap,
    fmt::{self, Debug},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub mod cursor;
pub mod tx;
pub(crate) mod view;

use tx::{Tx, WriteLock};

/// Builds a [`DatabaseErrorInfo`] from any displayable error.
pub(crate) fn err_info(error: impl std::fmt::Display) -> DatabaseErrorInfo {
    DatabaseErrorInfo { message: error.to_string().into(), code: 0 }
}

/// Compresses `value` into the bytes stored in the database.
///
/// This keeps the `smartnet-codecs` compression layer (zstd for compressible values) intact.
pub(crate) fn encode_value<V: Compress>(value: &V) -> Vec<u8> {
    let mut encoded = Vec::new();
    value.compress_to_buf(&mut encoded);
    encoded
}

/// Wraps an error as a [`DatabaseError::Read`].
pub(crate) fn read_error(error: impl std::fmt::Display) -> DatabaseError {
    DatabaseError::Read(err_info(error))
}

/// Wraps an error as a [`DatabaseError::Write`] for table `T`.
pub(crate) fn write_error<T: Table>(
    operation: DatabaseWriteOperation,
    key: Vec<u8>,
    error: impl std::fmt::Display,
) -> DatabaseError {
    DatabaseWriteError { info: err_info(error), operation, table_name: T::NAME, key }.into()
}

/// Returns the physical key under which a `(key, value)` entry of table `T` is stored.
///
/// `DupSort` tables hold one entry per value, so the compressed value is appended to the encoded
/// key to give every duplicate a distinct, ordered physical key, so duplicates of a key
/// duplicates of a key are ordered by their value bytes.
pub(crate) fn physical_key<T: Table>(key: &[u8], value: &[u8]) -> Vec<u8> {
    if T::DUPSORT {
        let mut physical = Vec::with_capacity(key.len() + value.len());
        physical.extend_from_slice(key);
        physical.extend_from_slice(value);
        physical
    } else {
        key.to_vec()
    }
}

/// Splits a physical key back into the encoded table key.
///
/// For `DupSort` tables the physical key is `encoded key ++ encoded value`, and the physical
/// value repeats the encoded value, so the key length is `physical - physical value`.
pub(crate) fn table_key<'a, T: Table>(key: &'a [u8], value: &[u8]) -> &'a [u8] {
    if T::DUPSORT {
        let key_len = key.len().saturating_sub(value.len());
        &key[..key_len]
    } else {
        key
    }
}

/// Decodes a stored `(physical key, physical value)` pair into a table row.
pub(crate) fn decode_pair<T: Table>(
    key: &[u8],
    value: &[u8],
) -> Result<(T::Key, T::Value), DatabaseError> {
    Ok((T::Key::decode(table_key::<T>(key, value))?, T::Value::decompress(value)?))
}

/// Returns the smallest key that sorts after every key having `key` as a prefix.
pub(crate) fn successor(key: &[u8]) -> Vec<u8> {
    let mut successor = key.to_vec();
    successor.push(0);
    successor
}

/// Returns the exclusive upper bound of every key having `prefix` as a prefix, or `None` when
/// the prefix is all `0xff` and therefore has no upper bound.
///
/// [`successor`] cannot be used for this: appending a zero byte sorts *below* every longer key
/// with the same prefix, so `successor("ab")` would bound the range at `ab\0` and cut off
/// `abc`. The last byte has to be incremented instead, carrying into the earlier bytes.
pub(crate) fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        match last.checked_add(1) {
            Some(next) => {
                *last = next;
                return Some(end);
            }
            // `0xff` cannot be incremented, so drop it and carry into the previous byte.
            None => {
                end.pop();
            }
        }
    }
    None
}

/// Arguments for database initialization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseArguments {
    /// Client version that accesses the database.
    client_version: ClientVersion,
    /// Database log level. If `None`, the default value is used.
    log_level: Option<LogLevel>,
    /// Storage backend the environment should open.
    backend: DatabaseBackend,
}

impl Default for DatabaseArguments {
    fn default() -> Self {
        Self::smartnet(ClientVersion::default())
    }
}

impl DatabaseArguments {
    /// `SmartNet` fork default database arguments.
    pub const fn smartnet(client_version: ClientVersion) -> Self {
        Self { client_version, log_level: None, backend: DatabaseBackend::Fjall }
    }

    /// Creates database arguments for the given client version (`SmartNet` defaults).
    pub const fn new(client_version: ClientVersion) -> Self {
        Self::smartnet(client_version)
    }

    /// Creates database arguments suitable for testing.
    pub fn test() -> Self {
        Self::new(ClientVersion::default())
    }

    /// Sets the database log level.
    pub const fn with_log_level(mut self, log_level: Option<LogLevel>) -> Self {
        self.log_level = log_level;
        self
    }

    /// Configured database log level.
    pub const fn log_level(&self) -> Option<LogLevel> {
        self.log_level
    }

    /// Sets the storage backend to open.
    ///
    /// The `SMARTNET_DB_BACKEND` environment variable overrides this when it names a
    /// known backend.
    pub const fn with_backend(mut self, backend: DatabaseBackend) -> Self {
        self.backend = backend;
        self
    }

    /// Configured storage backend.
    pub const fn backend(&self) -> DatabaseBackend {
        self.backend
    }

    /// Returns the client version that accesses the database.
    pub const fn client_version(&self) -> &ClientVersion {
        &self.client_version
    }
}

/// Fjall database environment.
#[derive(Clone)]
pub struct DatabaseEnv {
    db: FjallDatabase,
    keyspaces: Arc<HashMap<&'static str, Keyspace>>,
    path: PathBuf,
    writable: bool,
    /// Serializes write transactions on this environment (MDBX single-writer parity).
    write_lock: Arc<WriteLock>,
    _lock_file: Option<StorageLock>,
}

impl Debug for DatabaseEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseEnv")
            .field("path", &self.path)
            .field("writable", &self.writable)
            .field("keyspaces", &self.keyspaces.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Database for DatabaseEnv {
    type TX = Tx;
    type TXMut = Tx;

    fn tx(&self) -> Result<Self::TX, DatabaseError> {
        // Pinned so that every read in the transaction observes the same data for as long as
        // it lives, even if a writer commits meanwhile (see `view::View`).
        let snapshot = self.db.snapshot();
        Ok(Tx::new(self.db.clone(), self.keyspaces.clone(), false, None, Some(snapshot)))
    }

    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
        if !self.writable {
            return Err(DatabaseError::InitTx(err_info("database was opened in read-only mode")));
        }

        let write_guard = self.write_lock.acquire()?;
        Ok(Tx::new(self.db.clone(), self.keyspaces.clone(), true, Some(write_guard), None))
    }

    fn path(&self) -> PathBuf {
        self.path.clone()
    }

    fn last_txnid(&self) -> Option<u64> {
        // The watermark's `commit_sequence`: monotonic, incremented inside the same
        // transaction as the data it describes (SQLITE.md §4.6). An absent row — a
        // database that predates the watermark — reads as "unknown", which callers
        // treat as `0`.
        let tx = self.tx().ok()?;
        let progress = tx
            .get::<tables::PersistenceProgress>(tables::PersistenceProgressKey::Singleton)
            .ok()
            .flatten()?;
        Some(progress.commit_sequence)
    }
}

impl DatabaseMetrics for DatabaseEnv {
    fn report_metrics(&self) {}
}

impl Drop for DatabaseEnv {
    fn drop(&mut self) {
        // Journal writes that skipped their fsync (regenerable-only commits) reach
        // the disk on a clean shutdown, so only an unclean exit can lose them.
        if self.writable {
            let _ = self.db.persist(PersistMode::SyncData);
        }
    }
}

/// Group- and workload-classified create options for a table's keyspace (F1+F2).
///
/// Applied when a keyspace is first created; Fjall persists the options and reuses the
/// stored ones on every later open, so tuning never rewrites an existing datadir —
/// a fresh init gets the tuned layout, an old one keeps the layout it was created with.
///
/// The base comes from the table's [`TableGroup`] (F2: one database, two keyspace
/// groups, PROP5 classification):
///
/// * [`History`](TableGroup::History) keyspaces are append-mostly/range-read by default (payload,
///   index and history-index tables) — skipping last-level bloom filters cuts filter bytes by ~90%
///   on the largest trees;
/// * [`State`](TableGroup::State) keyspaces keep Fjall's point-read-tuned defaults (bloom on every
///   level).
///
/// Two per-table override classes adjust the base, both justified by access shape:
///
/// * unwind-ranged tables inside the State group (`AccountChangeSets`, `StorageChangeSets`) get the
///   range-read base too — unwind walks and deletes them by block-number range, never by point
///   lookup;
/// * large-payload tables (bytecode, header/transaction/receipt RLP) route values above 1 KiB into
///   blob files, out of the LSM levels, so compaction never rewrites multi-kilobyte values.
fn keyspace_options(group: TableGroup, name: &str) -> KeyspaceCreateOptions {
    // History-group point-lookup maps that must NOT get the range-read base: they
    // are number/hash-keyed lookups (canonical hash by height, tx hash by number),
    // not range scans.
    const HISTORY_POINT_LOOKUPS: &[&str] =
        &[<tables::CanonicalChain as Table>::NAME, <tables::TransactionHashNumbers as Table>::NAME];
    // State-group tables read by block-number range on unwind.
    const STATE_RANGE_READ: &[&str] =
        &[<tables::AccountChangeSets as Table>::NAME, <tables::StorageChangeSets as Table>::NAME];

    let range_read_base = (group == TableGroup::History && !HISTORY_POINT_LOOKUPS.contains(&name)) ||
        STATE_RANGE_READ.contains(&name);

    let large_payload = matches!(name, <tables::Bytecodes as Table>::NAME);

    let options = KeyspaceCreateOptions::default();
    let options = if range_read_base { options.expect_point_read_hits(true) } else { options };
    if large_payload {
        options.with_kv_separation(Some(fjall::KvSeparationOptions::default()))
    } else {
        options
    }
}

/// Opens the keyspace for every table of the given table set.
fn open_keyspaces<TS: TableSet>(
    db: &FjallDatabase,
) -> Result<HashMap<&'static str, Keyspace>, DatabaseError> {
    let mut keyspaces = HashMap::new();

    for table in TS::tables() {
        let name = table.name();
        let options = keyspace_options(table.group(), name);
        let keyspace = db
            .keyspace(name, || options)
            .map_err(|error| DatabaseError::CreateTable(err_info(error)))?;
        keyspaces.insert(name, keyspace);
    }

    Ok(keyspaces)
}

impl DatabaseEnv {
    /// Opens a Fjall database at `path`.
    ///
    /// `path` is the shared database directory; the environment is redirected into the
    /// engine's private subdirectory (`path/fjall`) and validated by
    /// [`check_engine_separation`], so only Fjall's directory is ever touched.
    pub fn open(
        path: &Path,
        kind: DatabaseEnvKind,
        args: DatabaseArguments,
    ) -> Result<Self, DatabaseError> {
        let backend = DatabaseBackend::resolve(&args);
        let path = check_engine_separation(path, backend)?;
        if kind.is_rw() {
            std::fs::create_dir_all(&path).map_err(|error| DatabaseError::Open(err_info(error)))?;
        }

        let writable = kind.is_rw();
        let _lock_file = if writable {
            Some(
                StorageLock::try_acquire(&path)
                    .map_err(|error| DatabaseError::Other(error.to_string()))?,
            )
        } else {
            None
        };

        // `manual_journal_persist(true)`: the adapter owns persistence entirely —
        // `Tx::commit` decides per commit whether the journal is fsynced (durability
        // classification), instead of Fjall flushing on every batch by itself.
        let db = FjallDatabase::builder(&path)
            .manual_journal_persist(true)
            .open()
            .map_err(|error| DatabaseError::Open(err_info(error)))?;
        let keyspaces = open_keyspaces::<Tables>(&db)?;

        info!(
            target: "storage::db::fjall",
            path = %path.display(),
            ?kind,
            tables = keyspaces.len(),
            "Fjall database opened"
        );

        Ok(Self {
            db,
            keyspaces: Arc::new(keyspaces),
            path,
            writable,
            write_lock: Arc::new(WriteLock::new()),
            _lock_file,
        })
    }

    /// Creates all the tables defined in [`Tables`], if necessary.
    pub fn create_tables(&mut self) -> Result<(), DatabaseError> {
        self.create_and_track_tables_for::<Tables>()
    }

    /// Creates all the tables defined in the given [`TableSet`], if necessary.
    pub fn create_and_track_tables_for<TS: TableSet>(&mut self) -> Result<(), DatabaseError> {
        let mut keyspaces = (*self.keyspaces).clone();

        for table in TS::tables() {
            let name = table.name();
            if !keyspaces.contains_key(name) {
                let options = keyspace_options(table.group(), name);
                let keyspace = self
                    .db
                    .keyspace(name, || options)
                    .map_err(|error| DatabaseError::CreateTable(err_info(error)))?;
                keyspaces.insert(name, keyspace);
            }
        }

        self.keyspaces = Arc::new(keyspaces);

        Ok(())
    }

    /// Attaches metrics reporting to the environment.
    ///
    /// Fjall performs its own background maintenance, so there are no environment gauges to
    /// report. The method exists for parity with the previous backend's startup path.
    pub const fn with_metrics(self) -> Self {
        self
    }

    /// Returns `true` if this environment was opened in read-only mode.
    pub const fn is_read_only(&self) -> Result<bool, DatabaseError> {
        Ok(!self.writable)
    }

    /// Drops an orphaned keyspace that is no longer part of the schema.
    ///
    /// Fjall only ever opens the keyspaces of the current table set, so there are no orphaned
    /// tables on disk and this always reports `Ok(false)`.
    pub const fn drop_orphan_table(&self, _name: &str) -> Result<bool, DatabaseError> {
        Ok(false)
    }

    /// Records the version that accesses the database with write privileges.
    pub fn record_client_version(&self, version: ClientVersion) -> Result<(), DatabaseError> {
        if version.is_empty() {
            return Ok(());
        }

        let tx = self.tx_mut()?;
        let mut version_cursor = tx.cursor_write::<tables::ClientVersions>()?;

        let last_version = version_cursor.last()?.map(|(_, value)| value);
        if Some(&version) != last_version.as_ref() {
            version_cursor.upsert(
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
                &version,
            )?;
            tx.commit()?;
        }

        Ok(())
    }
}

/// Fjall database environment kind: read-only or read-write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseEnvKind {
    /// Read-only environment. Transactions cannot modify the database.
    RO,
    /// Read-write environment.
    RW,
}

impl DatabaseEnvKind {
    /// Returns `true` if the environment is read-write.
    pub const fn is_rw(&self) -> bool {
        matches!(self, Self::RW)
    }
}
