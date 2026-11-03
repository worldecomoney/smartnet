//! Commonly used types for staged sync usage.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod id;
use alloy_primitives::BlockHash;
pub use id::StageId;

mod checkpoints;
pub use checkpoints::{
    AccountHashingCheckpoint, CheckpointBlockRange, EntitiesCheckpoint, ExecutionCheckpoint,
    HeadersCheckpoint, IndexHistoryCheckpoint, MerkleCheckpoint, StageCheckpoint,
    StageUnitCheckpoint, StorageHashingCheckpoint, StorageRootMerkleCheckpoint,
};

mod execution;
pub use execution::*;

/// Direction and target block for pipeline operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineTarget {
    /// Target for forward synchronization, indicating a block hash to sync to.
    Sync(BlockHash),
}

impl PipelineTarget {
    /// Returns the target block hash for forward synchronization, if applicable.
    ///
    /// # Returns
    ///
    /// - `Some(BlockHash)`: The target block hash for forward synchronization.
    pub const fn sync_target(self) -> Option<BlockHash> {
        match self {
            Self::Sync(hash) => Some(hash),
        }
    }
}

impl From<BlockHash> for PipelineTarget {
    fn from(hash: BlockHash) -> Self {
        Self::Sync(hash)
    }
}

impl core::fmt::Display for PipelineTarget {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sync(block) => {
                write!(f, "Sync({block})")
            }
        }
    }
}
