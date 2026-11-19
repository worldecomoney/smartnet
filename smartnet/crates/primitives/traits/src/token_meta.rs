//! ERC-20 contract metadata captured at ingest, written beside the runtime bytecode.
//!
//! A token's metadata is not in its bytecode — it lives in contract state and is read
//! back through views (`name()`, `symbol()`, `decimals()`, `totalSupply()`, `owner()`).
//! The import's range executor and the engine's block executor probe those views once
//! per new contract (operator, 2026-11-19: "token meta must be captured from
//! import/sync same way as bytecode"), and the record rides the executed block to the
//! commit, where it is written as `{address}.meta` beside the runtime-bytecode file.

use alloc::string::String;
use alloy_primitives::{Address, B256, U256};

/// The metadata of a deployed contract.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenMeta {
    /// `name()` — empty when the contract does not return one.
    pub name: String,
    /// `symbol()` — empty when the contract does not return one.
    pub symbol: String,
    /// `decimals()` — the token's smallest-unit exponent.
    pub decimals: u8,
    /// `totalSupply()` — raw base units, as the contract reports it.
    pub total_supply: U256,
    /// `owner()` — `None` when the contract has no owner view.
    pub owner: Option<Address>,
    /// The address that ran the create frame deploying this contract — `None` when
    /// unknown (captured before creator tracking, or a pre-genesis deployment).
    pub creator: Option<Address>,
    /// `keccak256` of the runtime bytecode — the contract's code hash.
    pub code_hash: B256,
    /// The solc version from the trailing CBOR metadata (`"0.7.6"`), empty when the
    /// code carries no compiler stamp.
    pub compiler: String,
}
