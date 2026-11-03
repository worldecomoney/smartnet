//! Mempool entry gate — the knownWallet / min-balance abuse floor.
//!
//! Division of responsibility (operator directive, 2026-10-18):
//! * **The node checks knownWallets/minBalances ONLY at tx entry** — `eth_sendRawTransaction`,
//!   `eth_sendTransaction`, and the mesh rumor path, all of which funnel through the single submit
//!   seam (`EthTransactions::send_transaction`). A sender below the floor is rejected there, before
//!   the pool, with no state effects.
//! * **The validator checks the real amount** — on-chain balance compared against the tx value (+
//!   upfront fee) at inclusion (`validator/src/mempool_policy.rs` over the `observe_sender_state`
//!   seam).
//! * **The node checks nothing in a signed block** — a received block is validated for its seal,
//!   never for sender balances.
//!
//! The floor is a process global set once at node bootstrap (operator key
//! `MIN_TO_TXPOOL_ENTRY`, wei; `0` disables the gate). It is deliberately NOT consensus state —
//! it is an admission-policy knob that can change between node versions without changing the
//! chain.

use std::sync::OnceLock;

use alloy_eips::BlockId;
use alloy_primitives::{Address, U256};

use smartnet_rpc_eth_types::error::{EthApiError, FromEthApiError, RpcInvalidTransactionError};
use smartnet_storage_api::StateProviderFactory;

/// 0.02 WEM in wei — the default entry floor (`MIN_TO_TXPOOL_ENTRY` unset).
pub const DEFAULT_MIN_TO_TXPOOL_ENTRY_WEI: u128 = 20_000_000_000_000_000;

static MIN_TO_TXPOOL_ENTRY: OnceLock<u128> = OnceLock::new();

/// Sets the entry floor (wei) at node bootstrap. Returns `false` when already set.
pub fn set_min_to_txpool_entry_wei(wei: u128) -> bool {
    MIN_TO_TXPOOL_ENTRY.set(wei).is_ok()
}

/// The entry floor in wei (`0` = gate disabled).
pub fn min_to_txpool_entry_wei() -> u128 {
    *MIN_TO_TXPOOL_ENTRY.get().unwrap_or(&DEFAULT_MIN_TO_TXPOOL_ENTRY_WEI)
}

/// The tx-entry gate: `Ok(())` iff the sender is a known wallet — balance at the head state
/// at or above the [`min_to_txpool_entry_wei`] floor. An absent account reads as zero balance.
///
/// Entry-only by design: never called on the signed-block import path.
pub fn known_wallet_entry_gate<P>(provider: &P, sender: Address) -> Result<(), EthApiError>
where
    P: StateProviderFactory,
{
    let min = min_to_txpool_entry_wei();
    if min == 0 {
        return Ok(());
    }

    let state = provider.state_by_block_id(BlockId::latest()).map_err(EthApiError::from_eth_err)?;
    let balance =
        state.account_balance(&sender).map_err(EthApiError::from_eth_err)?.unwrap_or_default();

    if balance < U256::from(min) {
        return Err(EthApiError::InvalidTransaction(
            RpcInvalidTransactionError::InsufficientFunds { cost: U256::from(min), balance },
        ));
    }
    Ok(())
}
