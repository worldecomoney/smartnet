use crate::spec::DepositContract;
use alloy_eips::eip6110::MAINNET_DEPOSIT_CONTRACT_ADDRESS;
use alloy_primitives::b256;

/// Gas per transaction not creating a contract.
pub const MIN_TRANSACTION_GAS: u64 = 21_000u64;

/// Mainnet prune delete limit.
pub const MAINNET_PRUNE_DELETE_LIMIT: usize = 20000;

/// RTBC deployment admission: maximum creation-input (initcode) length in bytes
/// (`SC-FILTER.md` §2.1). EIP-3860's value adopted as a `SmartNet` rule — size check
/// only, no metering; revm does not enforce it under the MARS `SpecId::LONDON`
/// execution spec (`SC-FILTER.md` §11 `W0.1b`). Authoritative source for
/// `smartnet_evm::rtbc::RtbcPolicy` (which sits below this crate's consumers and
/// mirrors the value for tests).
pub const MAX_INITCODE_SIZE: usize = 49_152;

/// RTBC deployment admission: maximum deployed runtime bytecode length in bytes
/// (`SC-FILTER.md` §2.2). EIP-170's value — revm already enforces it under the MARS
/// spec (belt-only in the RTBC scanner, `SC-FILTER.md` §11 `W0.1`).
pub const MAX_RUNTIME_SIZE: usize = 24_576;

/// Deposit contract address: `0x00000000219ab540356cbb839cbe05303d7705fa`
pub(crate) const MAINNET_DEPOSIT_CONTRACT: DepositContract = DepositContract::new(
    MAINNET_DEPOSIT_CONTRACT_ADDRESS,
    11052984,
    b256!("0x649bbc62d0e31342afea4e5cd82d4049e7e1ee912fc0889aa790803be39038c5"),
);
