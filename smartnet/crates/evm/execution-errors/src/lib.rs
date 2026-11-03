//! Commonly used error types used when doing block execution.
//!
//! The trie-domain error taxonomy (state root, storage root, sparse trie, proofs) lives in
//! `smartnet-trie-common` — the trie family owns its own errors.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

pub use alloy_evm::block::{
    BlockExecutionError, BlockValidationError, InternalBlockExecutionError,
};
