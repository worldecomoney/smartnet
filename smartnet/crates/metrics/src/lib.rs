//! Collection of metrics utilities.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Metrics derive macro.
pub use metrics_derive::Metrics;

/// Re-export core metrics crate.
pub use metrics;
