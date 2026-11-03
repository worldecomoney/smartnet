//! `SmartNet` execution node binary.
//!
//! ## Feature Flags
//!
//! ### Default Features
//!
//! - `otlp`: Enables [OpenTelemetry](https://opentelemetry.io/) metrics export to a configured OTLP
//!   collector endpoint.
//! - `keccak-cache-global`: Enables global caching for Keccak256 hashes to improve performance.
//! ### Log Level Features
//!
//! - `min-error-logs`: Disables all logs below `error` level.
//! - `min-warn-logs`: Disables all logs below `warn` level.
//! - `min-info-logs`: Disables all logs below `info` level. This can speed up the node, since fewer
//!   calls to the logging component are made.
//! - `min-debug-logs`: Disables all logs below `debug` level.
//! - `min-trace-logs`: Disables all logs below `trace` level.
//!
//! ### Development Features
//!
//! - `dev`: Enables development mode features, including test vector generation commands.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

// Used in feature flags only (`keccak-cache-global`)
use alloy_primitives as _;

// Used by the binary entrypoint (`main.rs`); referenced here so the lib target satisfies
// `unused_crate_dependencies`.
use smartnet_tls_provider as _;

pub mod cli;

/// Re-exported utils.
pub mod utils {
    pub use smartnet_db::open_db_read_only;

    /// Re-exported from `smartnet_node_core`, also to prevent a breaking change. See the
    /// comment on the `smartnet_node_core::args` re-export for more details.
    pub use smartnet_node_core::utils::*;
}

/// Re-exported payload related types
pub mod payload {
    pub use smartnet_ethereum_payload_builder::EthereumExecutionPayloadValidator;
    pub use smartnet_payload_builder::*;
    pub use smartnet_payload_primitives::*;
}

/// Re-exported from `smartnet_node_api`.
pub mod api {
    pub use smartnet_node_api::*;
}

/// Re-exported from `smartnet_node_core`.
pub mod core {
    pub use smartnet_node_core::*;
}

/// Re-export of the `smartnet_node_core` types specifically in the `args` module.
///
/// This is re-exported because the types in `smartnet_node_core::args` were moved from the
/// legacy CLI crate; this re-export avoids a breaking change.
pub mod args {
    pub use smartnet_node_core::args::*;
}

/// Re-exported from `smartnet_node_core`, also to prevent a breaking change. See the comment on
/// the `smartnet_node_core::args` re-export for more details.
pub mod version {
    pub use smartnet_node_core::version::*;
}

/// Re-exported from `smartnet_node_builder`
pub mod builder {
    pub use smartnet_node_builder::*;
}

/// Re-exported from `smartnet_node_core`, also to prevent a breaking change. See the comment on
/// the `smartnet_node_core::args` re-export for more details.
pub mod dirs {
    pub use smartnet_node_core::dirs::*;
}

/// Re-exported from `smartnet_chainspec`
pub mod chainspec {
    pub use smartnet_chainspec::*;
    pub use smartnet_cli::chainspec::*;
}

/// Re-exported from `smartnet_provider`.
pub mod providers {
    pub use smartnet_provider::*;
}

/// Re-exported primitives.
#[allow(ambiguous_glob_reexports)]
pub mod primitives {
    pub use smartnet_ethereum_primitives::*;
    pub use smartnet_primitives_traits::*;
}

/// Re-exported from `smartnet_ethereum_consensus`.
pub mod beacon_consensus {
    pub use smartnet_node_ethereum::consensus::*;
}

/// Re-exported from `smartnet_consensus`.
pub mod consensus {
    pub use smartnet_consensus::*;
}

/// Re-exported from `smartnet_ethereum_consensus` (was `smartnet_consensus_common`).
pub mod consensus_common {
    pub use smartnet_ethereum_consensus::*;
}

/// Re-exported from `smartnet_revm`.
pub mod revm {
    pub use smartnet_revm::*;
}

/// Re-exported from `smartnet_tasks`.
pub mod tasks {
    pub use smartnet_tasks::*;
}

/// Re-exported from `smartnet_network_api`.
pub mod network {
    pub use smartnet_network_api::{
        noop, test_utils::PeersHandleProvider, NetworkInfo, Peers, PeersInfo,
    };
}

/// Re-exported from `smartnet_transaction_pool`.
pub mod transaction_pool {
    pub use smartnet_transaction_pool::*;
}

/// Re-export of `smartnet_rpc_*` crates.
pub mod rpc {
    /// Re-exported from `smartnet_rpc_builder`.
    pub mod builder {
        pub use smartnet_rpc_builder::*;
    }

    /// Re-exported from `alloy_rpc_types`.
    pub mod types {
        pub use alloy_rpc_types::*;
    }

    /// Re-exported from `smartnet_rpc_server_types`.
    pub mod server_types {
        pub use smartnet_rpc_server_types::*;
        /// Re-exported from `smartnet_rpc_eth_types`.
        pub mod eth {
            pub use smartnet_rpc_eth_types::*;
        }
    }

    /// Re-exported from `smartnet_rpc_api`.
    pub mod api {
        pub use smartnet_rpc_api::*;
    }
    /// Re-exported from `smartnet_rpc::eth`.
    pub mod eth {
        pub use smartnet_rpc::eth::*;
    }

    /// Re-exported from `smartnet_rpc_server_types::result`.
    pub mod result {
        pub use smartnet_rpc_server_types::result::*;
    }

    /// Re-exported from `smartnet_rpc_convert`.
    pub mod compat {
        pub use smartnet_rpc_convert::*;
    }
}

// re-export for convenience
#[doc(inline)]
pub use smartnet_cli::{CliContext, CliRunner};

/// Custom allocator (merged from `smartnet-cli-util`).
pub mod allocator;

/// Signal handler to extract a backtrace from stack overflow.
#[cfg(all(unix, target_env = "gnu"))]
pub mod sigsegv_handler;

/// Signal handler to extract a backtrace from stack overflow.
///
/// This is a no-op because this platform doesn't support our signal handler's requirements.
#[cfg(not(all(unix, target_env = "gnu")))]
pub mod sigsegv_handler {
    /// No-op function.
    pub fn install() {}
}

// for rendering diagrams
use aquamarine as _;

// used in main
use clap as _;
use tracing as _;
