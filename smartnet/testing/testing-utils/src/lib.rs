//! Testing utilities.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod generators;
pub mod genesis_allocator;

pub use genesis_allocator::GenesisAllocator;
