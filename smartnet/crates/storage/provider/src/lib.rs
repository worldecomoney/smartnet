//! Collection of traits and trait implementations for common database operations.
//!
//! ## Feature Flags
//!
//! - `test-utils`: Export utilities for testing

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
// eyre::bail!/ensure! expansions end in a `;` we do not control (third-party macro).
#![allow(semicolon_in_expressions_from_macros)]

// Used only by the cfg(test / feature = "test-utils") helpers — marked as
// intentionally present so the default build does not flag the dependency.
use smartnet_ethereum_primitives as _;

/// Utility functions for initializing the database.
pub mod init;

/// ETL data collector (bulk sorted loading).
pub mod etl;

/// Interactive DB inspection helpers (`DbTool`).
pub mod db_tool;

mod storage_profile_log;
pub use storage_profile_log::log_calibri_lite_storage_profile;

/// Various provider traits.
mod traits;
pub use traits::*;

/// The encapsulated storage engine (both stores, one write/recovery protocol).
pub mod storage_engine;
pub use storage_engine::{SaveBlocksOutcome, StorageEngine, StorageError};

/// Provider trait implementations.
pub mod providers;
pub use providers::{
    DatabaseProvider, DatabaseProviderRO, DatabaseProviderRW, HistoricalStateProvider,
    HistoricalStateProviderRef, LatestStateProvider, LatestStateProviderRef, ProviderFactory,
    PrunedIndices, SaveBlocksMode,
};

pub mod changesets_utils;

#[cfg(any(test, feature = "test-utils"))]
/// Common test helpers for mocking the Provider.
pub mod test_utils;

pub mod either_writer;
pub use either_writer::*;

/// Re-export `OriginalValuesKnown`
pub use revm_database::states::OriginalValuesKnown;
pub use smartnet_chain_state::{
    CanonStateNotification, CanonStateNotificationSender, CanonStateNotificationStream,
    CanonStateNotifications, CanonStateSubscriptions,
};
pub use smartnet_execution_types::*;
pub use smartnet_storage_api::{
    metadata::keys, HistoryWriter, MetadataProvider, MetadataWriter, StateWriteConfig, StatsReader,
    StorageSettings, StorageSettingsCache,
};
/// Re-export provider error.
pub use smartnet_storage_errors::provider::{ProviderError, ProviderResult};

/// Converts a [`RangeBounds`](std::ops::RangeBounds) into a concrete [`Range`](std::ops::Range)
pub fn to_range<R: std::ops::RangeBounds<u64>>(bounds: R) -> std::ops::Range<u64> {
    let start = match bounds.start_bound() {
        std::ops::Bound::Included(&v) => v,
        std::ops::Bound::Excluded(&v) => v + 1,
        std::ops::Bound::Unbounded => 0,
    };

    let end = match bounds.end_bound() {
        std::ops::Bound::Included(&v) => v + 1,
        std::ops::Bound::Excluded(&v) => v,
        std::ops::Bound::Unbounded => u64::MAX,
    };

    start..end
}
