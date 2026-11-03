//! High level error types for the node in general.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![no_std]

extern crate alloc;

mod error;
pub use error::{SmartnetError, SmartnetResult};

pub use smartnet_execution_errors::{BlockExecutionError, BlockValidationError};
pub use smartnet_consensus::ConsensusError;
pub use smartnet_storage_errors::{
    db::DatabaseError,
    provider::{ProviderError, ProviderResult},
};
