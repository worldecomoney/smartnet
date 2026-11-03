//! `SmartNet` RPC `eth_` API implementation
//!
//! ## Feature Flags
//!
//! - `client`: Enables JSON-RPC client support.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod bundle;
pub mod core;
pub mod entry_gate;
pub mod ext;
pub mod filter;
pub mod helpers;
pub mod node;
pub mod pubsub;
pub mod types;

pub use bundle::{EthBundleApiServer, EthCallBundleApiServer};
pub use core::{EthApiServer, FullEthApiServer};
pub use ext::L2EthApiExtServer;
pub use filter::{EngineEthFilter, EthFilterApiServer, QueryLimits};
pub use helpers::config::EthConfigApiServer;
pub use node::{RpcNodeCore, RpcNodeCoreExt};
pub use pubsub::EthPubSubApiServer;
pub use smartnet_rpc_convert::*;
pub use smartnet_rpc_eth_types::error::{
    AsEthApiError, FromEthApiError, FromEvmError, IntoEthApiError,
};
pub use types::{EthApiTypes, FullEthApiTypes, RpcBlock, RpcHeader, RpcReceipt, RpcTransaction};

#[cfg(feature = "client")]
pub use bundle::{EthBundleApiClient, EthCallBundleApiClient};
#[cfg(feature = "client")]
pub use core::EthApiClient;
#[cfg(feature = "client")]
pub use ext::L2EthApiExtClient;
#[cfg(feature = "client")]
pub use filter::EthFilterApiClient;
#[cfg(feature = "client")]
pub use helpers::config::EthConfigApiClient;

use smartnet_trie_common as _;
