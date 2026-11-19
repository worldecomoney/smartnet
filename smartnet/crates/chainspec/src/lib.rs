//! The spec of an Ethereum network

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

/// Chain specific constants
mod constants;
pub use constants::*;

/// SmartNet fixed-fee policy constants (single source for the protocol gas price).
mod fixed_fee;
pub use fixed_fee::{SMARTNET_FIXED_GAS_PRICE_WEI, SMARTNET_FIXED_TRANSFER_FEE_WEI};

mod calibri_lite_genesis;
pub use calibri_lite_genesis::{
    extra_data_validators, normalize_calibri_genesis_raw, parse_extra_data_validators,
    validate_calibri_lite_genesis, CALIBRI_LITE_MAX_EXTRA_DATA_SIZE,
};

mod api;
/// The chain info module.
mod info;
/// The chain spec module.
mod spec;

pub use alloy_chains::{Chain, ChainKind, NamedChain};
/// Re-export for convenience
pub use smartnet_ethereum_forks::*;

pub use alloy_evm::EvmLimitParams;
pub use api::EthChainSpec;
pub use info::ChainInfo;
#[cfg(any(test, feature = "test-utils"))]
pub use spec::test_fork_ids;
pub use spec::{
    dev_chain_spec, make_genesis_header, BaseFeeParams, BaseFeeParamsKind, ChainSpec,
    ChainSpecBuilder, ChainSpecProvider, DepositContract, ForkBaseFeeParams, MAINNET,
};
