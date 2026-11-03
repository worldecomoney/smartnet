//! Staged syncing primitives for `SmartNet`.
//!
//! ## Feature Flags
//!
//! - `test-utils`: Utilities for testing

#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod error;
mod metrics;
mod pipeline;
mod stage;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
mod util;

pub use crate::metrics::*;
pub use error::*;
pub use pipeline::*;
pub use stage::*;

use aquamarine as _;

// re-export the stages types for convenience
pub use smartnet_stages_types::*;
