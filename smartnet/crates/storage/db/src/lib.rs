//! Fjall implementation for the database abstraction layer.
//!
//! This crate is an implementation of `smartnet-db-api` for Fjall, as well as a few other common
//! database types.
//!
//! # Overview
//!
//! An overview of the current data model can be found in the [`mod@tables`] module.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod implementation;
#[cfg(all(test, unix))]
mod kill9;
pub mod lockfile;
mod persistence;
mod utils;
pub mod version;

pub mod fjall;

pub use smartnet_storage_errors::db::{DatabaseError, DatabaseWriteOperation};
pub use utils::is_database_empty;

pub use fjall::{
    create_db, init_db, init_db_for, open_db, open_db_read_only, Cursor, DatabaseArguments,
    DatabaseEnv, DatabaseEnvKind, Tx,
};
pub use implementation::backend::{DatabaseBackend, FJALL_COMPILED};

pub use models::ClientVersion;
pub use smartnet_db_api::*;

/// Collection of database test utilities
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils {
    /// Backend-agnostic behavioral contract for [`Database`], instantiated per backend.
    pub mod conformance;

    use super::*;
    use crate::DatabaseArguments;
    use parking_lot::RwLock;
    use smartnet_db_api::{database::Database, database_metrics::DatabaseMetrics};
    use smartnet_fs_util;
    use std::{
        fmt::Formatter,
        path::{Path, PathBuf},
        sync::Arc,
    };

    /// Error during database open
    pub const ERROR_DB_OPEN: &str = "could not open the database file";
    /// Error during database creation
    pub const ERROR_DB_CREATION: &str = "could not create the database file";
    /// Error during table creation
    pub const ERROR_TABLE_CREATION: &str = "could not create tables in the database";
    /// Error during tempdir creation
    pub const ERROR_TEMPDIR: &str = "could not create a temporary directory";

    /// A database will delete the db dir when dropped.
    pub struct TempDatabase<DB> {
        db: Option<DB>,
        path: PathBuf,
        /// Executed right before a database transaction is created.
        pre_tx_hook: RwLock<Box<dyn Fn() + Send + Sync>>,
        /// Executed right after a database transaction is created.
        post_tx_hook: RwLock<Box<dyn Fn() + Send + Sync>>,
    }

    impl<DB: std::fmt::Debug> std::fmt::Debug for TempDatabase<DB> {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TempDatabase").field("db", &self.db).field("path", &self.path).finish()
        }
    }

    impl<DB> Drop for TempDatabase<DB> {
        fn drop(&mut self) {
            if let Some(db) = self.db.take() {
                drop(db);
                let _ = smartnet_fs_util::remove_dir_all(&self.path);
            }
        }
    }

    impl<DB> TempDatabase<DB> {
        /// Create new [`TempDatabase`] instance.
        pub fn new(db: DB, path: PathBuf) -> Self {
            Self {
                db: Some(db),
                path,
                pre_tx_hook: RwLock::new(Box::new(|| ())),
                post_tx_hook: RwLock::new(Box::new(|| ())),
            }
        }

        /// Returns the reference to inner db.
        pub const fn db(&self) -> &DB {
            self.db.as_ref().unwrap()
        }

        /// Returns the path to the database.
        pub fn path(&self) -> &Path {
            &self.path
        }

        /// Convert temp database into inner.
        pub fn into_inner_db(mut self) -> DB {
            self.db.take().unwrap() // take out db to avoid clean path in drop fn
        }

        /// Sets [`TempDatabase`] new pre transaction creation hook.
        pub fn set_pre_transaction_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
            let mut db_hook = self.pre_tx_hook.write();
            *db_hook = hook;
        }

        /// Sets [`TempDatabase`] new post transaction creation hook.
        pub fn set_post_transaction_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
            let mut db_hook = self.post_tx_hook.write();
            *db_hook = hook;
        }
    }

    impl<DB: Database> Database for TempDatabase<DB> {
        type TX = <DB as Database>::TX;
        type TXMut = <DB as Database>::TXMut;
        fn tx(&self) -> Result<Self::TX, DatabaseError> {
            self.pre_tx_hook.read()();
            let tx = self.db().tx()?;
            self.post_tx_hook.read()();
            Ok(tx)
        }

        fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
            self.db().tx_mut()
        }

        fn path(&self) -> std::path::PathBuf {
            self.db().path()
        }

        fn last_txnid(&self) -> Option<u64> {
            self.db().last_txnid()
        }
    }

    impl<DB: DatabaseMetrics> DatabaseMetrics for TempDatabase<DB> {
        fn report_metrics(&self) {
            self.db().report_metrics()
        }
    }

    /// Get a temporary directory path to use for the database
    pub fn tempdir_path() -> PathBuf {
        let builder = tempfile::Builder::new().prefix("smartnet-test-").rand_bytes(8).tempdir();
        builder.expect(ERROR_TEMPDIR).keep()
    }

    /// Create read/write database for testing
    #[track_caller]
    pub fn create_test_rw_db() -> Arc<TempDatabase<DatabaseEnv>> {
        let path = tempdir_path();
        let emsg = format!("{ERROR_DB_CREATION}: {path:?}");

        let db = init_db(&path, DatabaseArguments::test()).expect(&emsg);

        Arc::new(TempDatabase::new(db, path))
    }

    /// Create read/write database for testing
    #[track_caller]
    pub fn create_test_rw_db_with_path<P: AsRef<Path>>(path: P) -> Arc<TempDatabase<DatabaseEnv>> {
        let path = path.as_ref().to_path_buf();
        let emsg = format!("{ERROR_DB_CREATION}: {path:?}");
        let db = init_db(path.as_path(), DatabaseArguments::test()).expect(&emsg);
        Arc::new(TempDatabase::new(db, path))
    }

    /// Create read/write database for testing within a data directory.
    ///
    /// The database is created at `datadir/db`, and `TempDatabase` will clean up the entire
    /// `datadir` on drop.
    #[track_caller]
    pub fn create_test_rw_db_with_datadir<P: AsRef<Path>>(
        datadir: P,
    ) -> Arc<TempDatabase<DatabaseEnv>> {
        let datadir = datadir.as_ref().to_path_buf();
        let db_path = datadir.join("db");
        let emsg = format!("{ERROR_DB_CREATION}: {db_path:?}");
        let db = init_db(&db_path, DatabaseArguments::test()).expect(&emsg);
        Arc::new(TempDatabase::new(db, datadir))
    }

    /// Create read only database for testing
    #[track_caller]
    pub fn create_test_ro_db() -> Arc<TempDatabase<DatabaseEnv>> {
        let args = DatabaseArguments::test();

        let path = tempdir_path();
        let emsg = format!("{ERROR_DB_CREATION}: {path:?}");
        {
            init_db(path.as_path(), args.clone()).expect(&emsg);
        }
        let db = open_db_read_only(path.as_path(), args).expect(ERROR_DB_OPEN);
        Arc::new(TempDatabase::new(db, path))
    }
}
