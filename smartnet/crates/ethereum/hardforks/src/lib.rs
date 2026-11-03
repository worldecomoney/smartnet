//! Ethereum fork types used by `SmartNet`.
//!
//! This crate contains Ethereum fork types and helper functions.
//!
//! ## Feature Flags
//!
//! - `arbitrary`: Adds `arbitrary` support for primitive types.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

/// Re-exported [EIP-2124](https://eips.ethereum.org/EIPS/eip-2124) forkid types.
pub use alloy_eip2124::*;

mod display;
mod hardforks;

pub use alloy_hardforks::*;

pub use display::DisplayHardforks;
pub use hardforks::*;

#[cfg(any(test, feature = "arbitrary"))]
pub use arbitrary;
