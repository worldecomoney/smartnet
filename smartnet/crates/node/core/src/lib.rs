//! The core of the Ethereum node. Collection of utilities and libraries that are used by the node.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// CLI argument types.
pub mod args;
/// Node-owned transaction-pool lifecycle policy.
pub mod txpool_policy;
/// Hand-rolled value parsers.
pub mod parsers;
/// CLI command dispatch.
pub mod cli;
/// Data-directory resolution.
pub mod dirs;
/// Node exit handling.
pub mod exit;
/// Node configuration.
pub mod node_config;
/// Small helpers.
pub mod utils;
/// Version metadata.
pub mod version;

/// Re-exported primitive types
pub mod primitives {
    pub use smartnet_ethereum_forks::*;
    pub use smartnet_primitives_traits::*;
}

/// Re-export of `smartnet_rpc_*` crates.
pub mod rpc {
    /// Re-exported from `smartnet_rpc_server_types::result`.
    pub mod result {
        pub use smartnet_rpc_server_types::result::*;
    }

    /// Re-exported from `smartnet_rpc_convert`.
    pub mod compat {
        pub use smartnet_rpc_convert::*;
    }
}
