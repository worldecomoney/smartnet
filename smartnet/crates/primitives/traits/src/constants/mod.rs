//! Ethereum protocol-related constants

/// Gas units, for example [`GIGAGAS`].
pub mod gas_units;
pub use gas_units::{GIGAGAS, KILOGAS, MEGAGAS};

/// Minimum gas limit allowed for transactions.
pub const MINIMUM_GAS_LIMIT: u64 = 5000;

/// Maximum gas limit allowed for block.
/// In hex this number is `0x7fffffffffffffff`
pub const MAXIMUM_GAS_LIMIT_BLOCK: u64 = 2u64.pow(63) - 1;

/// The bound divisor of the gas limit, used in update calculations.
pub const GAS_LIMIT_BOUND_DIVISOR: u64 = 1024;

/// Maximum transaction gas limit as defined by [EIP-7825](https://eips.ethereum.org/EIPS/eip-7825) activated in `Osaka` hardfork.
pub const MAX_TX_GAS_LIMIT_OSAKA: u64 = 2u64.pow(24);

/// The maximum RLP length of a block, defined in [EIP-7934](https://eips.ethereum.org/EIPS/eip-7934).
///
/// Calculated as `MAX_BLOCK_SIZE` - `SAFETY_MARGIN` where
/// `MAX_BLOCK_SIZE` = `10_485_760`
/// `SAFETY_MARGIN` = `2_097_152`
pub const MAX_RLP_BLOCK_SIZE: usize = 8_388_608;
