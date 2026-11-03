//! Pruning implementation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![allow(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod builder;
mod db_ext;
mod error;
mod limiter;
mod metrics;
mod pruner;
pub mod segments;

use crate::metrics::Metrics;
pub use builder::PrunerBuilder;
pub use error::PrunerError;
pub use limiter::PruneLimiter;
pub use pruner::{Pruner, PrunerResult, PrunerWithFactory, PrunerWithResult};

// Re-export prune types
#[doc(inline)]
pub use smartnet_prune_types::*;
