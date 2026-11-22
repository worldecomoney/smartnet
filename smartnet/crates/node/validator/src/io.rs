//! The validator's **single I/O boundary**, and it is never RPC.
//!
//! The merged validator runs inside the node it proposes for. [`ValidatorIo`] is how the seal
//! pipeline ([`crate::run_once`]) and the state seam ([`crate::balances`]) reach "the node":
//! read the head, read the pool, read account state, preflight a body, and submit a sealed block.
//! The node supplies the one implementation, and every call is an in-process method call — the
//! pipeline has no URL, no client, and no transport to configure.
//!
//! Observation (`eth_*`, `txpool_*`) and submission share these methods; submission is a whole
//! sealed block handed to the node's engine, never a JSON-RPC round trip. This module also holds
//! the pure wire encoders the node's implementation needs, so the validator and the node cannot
//! disagree about how a `balanceOf` call looks.

use alloy_primitives::{Address, Bytes, TxHash, B256, U256};
use alloy_rpc_types_eth::{
    simulate::{SimulatePayload, SimulatedBlock},
    Block, BlockNumberOrTag, Transaction, TransactionRequest,
};
use alloy_rpc_types_txpool::TxpoolContent;
use eyre::{bail, Result};
use serde_json::Value;
use std::{collections::BTreeMap, time::Instant};

/// `balanceOf(address)` selector.
const ERC20_BALANCE_OF: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];

/// Everything one seal pass needs to ask the node.
///
/// Methods return concrete RPC-shaped types (`alloy_rpc_types_eth`) rather than consensus types, so
/// the node can answer them straight from its `EthApi`. Balance/nonce lookups are batch methods:
/// one result per requested item, in order.
// Owned end-to-end by this crate and the node crate; the async-in-trait form is the clearest
// here, and callers only ever hold a concrete implementor (never a boxed `dyn`).
#[allow(async_fn_in_trait)]
pub trait ValidatorIo: Send + Sync {
    /// Human-readable endpoint, for logs only (e.g. `in-proc`).
    fn label(&self) -> &str;

    /// The node's chain id, cross-checked against the validator config.
    async fn chain_id(&self) -> Result<u64>;

    /// Enabled validator addresses from the node-owned registry (`get_val_all` reduced to
    /// `status: true`). The validator never opens `validators.jsonl`; the node answers here.
    fn enabled_validators(&self) -> Vec<Address>;

    /// The chair for `(block_hash, ts, purpose)` from the node-owned registry. The validator
    /// uses this to know whether it is the chair for the slot it is about to seal; plain RPC
    /// nodes expose the same node-side query without running a validator.
    fn get_val_chair(&self, block_hash: B256, ts: u64, purpose: &str) -> Option<Address>;

    /// Fetch a block by number/tag.
    async fn block_by_number(&self, tag: BlockNumberOrTag) -> Result<Option<Block<Transaction>>>;

    /// Fetch a block by hash.
    async fn block_by_hash(
        &self,
        hash: alloy_primitives::B256,
    ) -> Result<Option<Block<Transaction>>>;

    /// Full pool content (pending + queued).
    async fn txpool_content(&self) -> Result<TxpoolContent<Transaction>>;

    /// Pool arrival time of every currently pooled tx, keyed by tx hash — when the tx
    /// entered the node's pool. The validator's native selection packs strictly oldest-
    /// first from this map (the `SmartNet` FIFO rule). A missing hash (or an empty map)
    /// falls back to the deterministic address order, so a node without arrival data
    /// keeps the previous behaviour instead of misordering.
    async fn txpool_arrival(&self) -> Result<BTreeMap<TxHash, Instant>>;

    /// Pool status summary (counts).
    async fn txpool_status(&self) -> Result<Value>;

    /// Native balance of each owner, in order.
    async fn balances(&self, owners: &[Address]) -> Vec<Result<U256>>;

    /// ERC-20 balance of each `(token, owner)` pair, in order.
    async fn erc20_balances(&self, pairs: &[(Address, Address)]) -> Vec<Result<U256>>;

    /// On-chain executable nonce of each owner, in order.
    async fn nonces(&self, owners: &[Address]) -> Vec<Result<u64>>;

    /// `eth_simulateV1` preflight over the final block body against the head state.
    async fn simulate_v1(
        &self,
        payload: SimulatePayload,
    ) -> Result<Vec<SimulatedBlock<Block<Transaction>>>>;

    /// Submit a complete sealed block as RLP. Returns the node's raw status object
    /// (`status` / `latestValidHash` / `validationError` at top level). In the merged node this is
    /// the engine import; there is no JSON-RPC submission path.
    async fn submit_block_rlp(&self, rlp: Bytes) -> Result<Value>;
}

/// The exact `eth_call` body for one `balanceOf`, shared by the node's [`ValidatorIo`]
/// implementation so its ERC-20 reads match what the validator expects.
pub fn erc20_balance_of_call(token: Address, owner: Address) -> TransactionRequest {
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&ERC20_BALANCE_OF);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(owner.as_slice());
    TransactionRequest::default().to(token).input(data.into())
}

/// Decode a 32-byte `balanceOf` return word.
pub fn erc20_decode_balance(raw: &Bytes, token: Address, owner: Address) -> Result<U256> {
    if raw.len() < 32 {
        bail!(
            "balanceOf(token={token}, owner={owner}): return data is {} bytes, need 32",
            raw.len()
        );
    }
    Ok(U256::from_be_slice(&raw[..32]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::TxKind;

    /// The `balanceOf(address)` body is the exact ABI encoding: 4-byte selector + 12 zero bytes
    /// + the 20-byte owner. The node's implementation builds its `eth_call` from this, so the
    ///   validator and the node can never disagree about the calldata.
    #[test]
    fn erc20_balance_of_call_encodes_selector_and_owner() {
        let token = Address::repeat_byte(0xbb);
        let owner = Address::repeat_byte(0x42);
        let call = erc20_balance_of_call(token, owner);
        assert_eq!(call.to, Some(TxKind::Call(token)));
        let data = call.input.input.expect("calldata present");
        assert!(data.starts_with(&ERC20_BALANCE_OF), "selector in {data}");
        // 4-byte selector + 32-byte ABI head = 36 bytes.
        assert_eq!(data.len(), 36, "selector + abi head: {data}");
        assert!(data.ends_with(owner.as_slice()), "owner word in {data}");
    }

    /// A short `balanceOf` return is rejected rather than read as a truncated number.
    #[test]
    fn erc20_decode_balance_rejects_short_return() {
        let token = Address::repeat_byte(0xbb);
        let owner = Address::repeat_byte(0x42);
        assert!(erc20_decode_balance(&Bytes::from(vec![0u8; 31]), token, owner).is_err());
        assert_eq!(
            erc20_decode_balance(&Bytes::from(vec![0u8; 32]), token, owner).unwrap(),
            U256::ZERO
        );
    }
}
