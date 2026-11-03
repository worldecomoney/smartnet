//! Commonly used error types used when interacting with storage.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

/// Database error
pub mod db;

/// Lockfile error
pub mod lockfile;

/// Provider error
pub mod provider;
pub use provider::{ProviderError, ProviderResult};

/// Any error
pub mod any;
