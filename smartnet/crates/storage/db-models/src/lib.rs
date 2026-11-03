//! Models used in storage module.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

/// Accounts
pub mod accounts;
pub use accounts::AccountBeforeTx;

/// Blocks
pub mod blocks;
pub use blocks::{StoredBlockBodyIndices, StoredBlockWithdrawals};

/// Persistence watermark
pub mod persistence;
pub use persistence::PersistenceWatermark;

pub mod schema;
pub use schema::{StoredMigration, StoredSchemaInfo};

/// Transactions
pub mod transactions;
pub use transactions::StoredTransactionLocation;

/// Storage
pub mod storage;
pub use storage::StorageBeforeTx;

/// Client Version
pub mod client_version;
pub use client_version::ClientVersion;
