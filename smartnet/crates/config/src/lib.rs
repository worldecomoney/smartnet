//! Standalone crate for `SmartNet` configuration types.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod config;
#[cfg(feature = "serde")]
pub use config::load_config_file;
pub use config::{BodiesConfig, Config, PruneConfig};
