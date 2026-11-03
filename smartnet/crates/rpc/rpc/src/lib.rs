//! `SmartNet` RPC implementation
//!
//! Provides the implementation of all RPC interfaces.
//!
//!
//! ## Note on blocking behaviour
//!
//! All async RPC handlers must non-blocking, see also [What is blocking](https://ryhl.io/blog/async-what-is-blocking/).
//!
//! A lot of the RPC are using a mix of async and direct calls to the database, which are blocking
//! and can reduce overall performance of all concurrent requests handled via the jsonrpsee server.
//!
//! To avoid this, all blocking or CPU intensive handlers must be spawned to a separate task. See
//! the [`EthApi`] handler implementations for examples. The rpc-api traits make no use of the
//! available jsonrpsee `blocking` attribute to give implementers more freedom because the
//! `blocking` attribute and async handlers are mutually exclusive. However, as mentioned above, a
//! lot of handlers make use of async functions, caching for example, but are also using blocking
//! disk-io, hence these calls are spawned as futures to a blocking task manually.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod admin;
mod aliases;
mod debug;
mod engine;
pub mod eth;
mod miner;
mod net;
mod otterscan;
mod rpc;
mod smartnet;
mod trace;
mod txpool;
mod validation;
mod web3;
mod wem;

pub use admin::{mesh_node_info, AdminApi};
pub use aliases::*;
pub use debug::DebugApi;
pub use engine::{EngineApi, EngineEthApi};
pub use eth::{helpers::SyncListener, EthApi, EthApiBuilder, EthBundle, EthFilter, EthPubSub};
pub use miner::MinerApi;
pub use net::NetApi;
pub use otterscan::OtterscanApi;
pub use rpc::RPCApi;
pub use smartnet_rpc_convert::RpcTypes;
pub use trace::TraceApi;
pub use txpool::TxPoolApi;
pub use validation::{ValidationApi, ValidationApiConfig};
pub use web3::Web3Api;
/// Operator extension RPC backend (`wem_*` + `smartnet_*` namespaces on one type).
pub use wem::OperatorExtensionApi;
