//! The implementation of Engine API.
//! [Read more](https://github.com/ethereum/execution-apis/tree/main/src/engine).

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// The Engine API implementation.
mod engine_api;

/// `wem_*` + canonical `smartnet_*` engine extensions (dual-publish).
mod engine_extensions;
mod wem_engine_api;

/// Engine API capabilities.
pub mod capabilities;
pub use capabilities::EngineCapabilities;

/// Engine API error.
mod error;

/// Engine API metrics.
mod metrics;

pub use engine_api::{EngineApi, EngineApiSender};
pub use engine_extensions::{merge_engine_extension_rpc, EngineExtensionsApi};
pub use error::*;
pub use wem_engine_api::WemEngineApi;

// re-export server trait for convenience
pub use smartnet_rpc_api::EngineApiServer;

#[cfg(test)]
mod tests {
    // silence unused import warning
    use alloy_rlp as _;
}
