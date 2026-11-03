//! `SmartNet` RPC interface definitions
//!
//! Provides all RPC interfaces.
//!
//! ## Feature Flags
//!
//! - `client`: Enables JSON-RPC client support.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod admin;
mod anvil;
mod debug;
mod engine;
mod hardhat;
mod mev;
mod miner;
mod net;
mod otterscan;
mod rpc;
mod smartnet;
mod smartnet_engine;
mod trace;
mod txpool;
mod validation;
mod web3;
mod wem;
mod wem_engine;

pub use admin::{MeshNodeInfo, MeshPeerEntry};
pub use smartnet_engine::{SmartNetNewPayloadInput, SmartNetPayloadStatus};

/// re-export of all server traits
pub use servers::*;

/// Aggregates all server traits.
pub mod servers {
    pub use crate::{
        admin::AdminApiServer,
        debug::{DebugApiServer, DebugExecutionWitnessApiServer},
        engine::{EngineApiServer, EngineEthApiServer, IntoEngineApiRpcModule},
        mev::{MevFullApiServer, MevSimApiServer},
        miner::MinerApiServer,
        net::NetApiServer,
        otterscan::OtterscanServer,
        rpc::RpcApiServer,
        smartnet::SmartnetApiServer,
        smartnet_engine::SmartnetEngineApiServer,
        trace::TraceApiServer,
        txpool::TxPoolApiServer,
        validation::BlockSubmissionValidationApiServer,
        web3::Web3ApiServer,
        wem::WemApiServer,
        wem_engine::WemEngineApiServer,
    };
    pub use smartnet_rpc_eth_api::{
        self as eth, EthApiServer, EthBundleApiServer, EthCallBundleApiServer, EthConfigApiServer,
        EthFilterApiServer, EthPubSubApiServer, L2EthApiExtServer,
    };
}

/// re-export of all client traits
#[cfg(feature = "client")]
pub use clients::*;

/// Aggregates all client traits.
#[cfg(feature = "client")]
pub mod clients {
    pub use crate::{
        admin::AdminApiClient,
        anvil::AnvilApiClient,
        debug::{DebugApiClient, DebugExecutionWitnessApiClient},
        engine::{EngineApiClient, EngineEthApiClient},
        hardhat::HardhatApiClient,
        mev::{MevFullApiClient, MevSimApiClient},
        miner::MinerApiClient,
        net::NetApiClient,
        otterscan::OtterscanClient,
        rpc::RpcApiClient,
        smartnet::SmartnetApiClient,
        smartnet_engine::SmartnetEngineApiClient,
        trace::TraceApiClient,
        txpool::TxPoolApiClient,
        validation::BlockSubmissionValidationApiClient,
        web3::Web3ApiClient,
        wem::WemApiClient,
        wem_engine::WemEngineApiClient,
    };
    pub use smartnet_rpc_eth_api::{
        EthApiClient, EthBundleApiClient, EthCallBundleApiClient, EthConfigApiClient,
        EthFilterApiClient, L2EthApiExtClient,
    };
}
