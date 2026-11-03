//! Compatibility and conversion utils for RPC types
//!
//! This crate various helper functions to convert between node primitive types and rpc types.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod rpc;
pub mod transaction;
mod traits;

pub use rpc::*;
pub use transaction::{RpcConvert, RpcConverter, TransactionConversionError};

pub use alloy_evm::rpc::{CallFees, CallFeesError, EthTxEnvError, TryIntoTxEnv};

// Conversion traits (module `traits`, merged from the former `smartnet-rpc-traits` crate)
pub use traits::{
    FromConsensusHeader, FromConsensusTx, SignTxRequestError, SignableTxRequest, TryIntoSimTx,
    TxInfoMapper,
};
