//! Revm utils and implementations specific to `SmartNet`.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

/// Cache database that reads from an underlying [`DatabaseRef`].
/// Database adapters for payload building.
pub mod cached;

/// A marker that can be used to cancel execution.
pub mod cancelled;

/// Contains glue code for integrating the node's database into revm's [Database].
pub mod database;

pub use revm::{database as db, inspector};

/// Common test helpers
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

// Convenience re-exports.
pub use revm::{self, database::State, *};

/// Helper types for execution witness generation.
#[cfg(feature = "witness")]
pub mod witness;
