//! `SmartNet` node builder — launch, components, and RPC add-ons (derived from Reth 2.1.0).

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
// eyre::bail!/ensure! expansions end in a `;` we do not control (third-party macro).
#![allow(semicolon_in_expressions_from_macros)]

/// Node event handlers (was the standalone `smartnet-node-events` crate).
pub mod events;


/// Engine API stream adapters for test orchestration (was `smartnet-engine-util`).
pub mod engine_util;


/// Node event hooks.
pub mod hooks;

/// Support for configuring the higher level node types.
pub mod node;
pub use node::*;

/// Support for accessing the EngineApi outside the RPC server context.
mod engine_api_ext;
pub use engine_api_ext::EngineApiExt;

/// Support for configuring the components of a node.
pub mod components;
pub use components::{NodeComponents, NodeComponentsBuilder};

mod builder;
pub use builder::{add_ons::AddOns, *};

mod launch;
pub use launch::{engine::EngineNodeLauncher, *};

mod handle;
pub use handle::NodeHandle;

pub mod rpc;

pub mod setup;

/// Type aliases for traits that are often used together
pub mod aliases;
pub use aliases::*;

/// Re-export the core configuration traits.
pub use smartnet_node_core::cli::config::{PayloadBuilderConfig, SmartnetTransactionPoolConfig};

// re-export the core config for convenience
pub use smartnet_node_core::node_config::NodeConfig;

// re-export API types for convenience
pub use smartnet_node_api::*;

use aquamarine as _;

use smartnet_rpc as _;
