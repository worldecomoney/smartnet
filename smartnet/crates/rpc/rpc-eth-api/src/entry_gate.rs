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
//! * **MARS transaction types** (operator directive, 2026-11-22): `0x0` legacy and `0x2`
//!   dynamic-fee are both first-class at every height — mixed types need no fork/timestamp gate.
//!   `0x1`/`0x3+` are warned about and refused here, before the pool.
//! * **`TXPOOL_MARGIN` is an independent admission filter** (operator directive, 2026-11-22):
//!   a fresh native coin tx may be filtered by its declared fee here at entry and nowhere else —
//!   never at block assembly, never at block acceptance. Absent/`0` = no filter. This is NOT the
//!   fixed native fee policy: `FIXED_GAS_PRICE_WEI` is the enforced price used by headers, EVM
//!   execution and wallet quotes; the margin only decides whether a rumored tx may enter the
//!   pool. What revm does with an admitted tx is revm's business.
//!
//! Both knobs are process globals set once at node bootstrap (operator keys
//! `MIN_TO_TXPOOL_ENTRY`, wei, and `TXPOOL_MARGIN`, integer 0..=5). They are deliberately NOT
//! consensus state — admission policy can change between node versions without changing the
//! chain.

use std::sync::OnceLock;

use alloy_consensus::Transaction;
use alloy_eips::BlockId;
use alloy_primitives::{Address, U256};

use smartnet_chainspec::fixed_gas_price_wei;
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

/// Admission filter multiplier (`TXPOOL_MARGIN`). `0` (the fallback) disables the filter; `1`
/// means exact; `n` means `fixed/n .. fixed*n`. It is a txpool-entry filter only — not the fixed
/// native fee policy, which lives in `FIXED_GAS_PRICE_WEI` / `fixed_gas_price_wei()`.
static TXPOOL_MARGIN: OnceLock<u8> = OnceLock::new();

/// Sets the admission multiplier at node bootstrap. Returns `false` when already set.
pub fn set_txpool_margin(margin: u8) -> bool {
    TXPOOL_MARGIN.set(margin).is_ok()
}

/// Configured admission multiplier (`0` = no fixed-fee check).
pub fn txpool_margin() -> u8 {
    *TXPOOL_MARGIN.get().unwrap_or(&0)
}

/// Inclusive wei/gas band for a fixed price and multiplier. `None` means no check.
fn fee_band(fixed: u128, margin: u8) -> Option<(u128, u128)> {
    match margin {
        0 => None,
        1 => Some((fixed, fixed)),
        n => Some((fixed / u128::from(n), fixed.saturating_mul(u128::from(n)))),
    }
}

/// Applies the admission band to one declared fee. Split out from the global-reading gate so the
/// multiplier math is testable without touching the one-shot process global.
fn check_fee(fixed: u128, margin: u8, got: u128) -> Result<(), EthApiError> {
    let Some((min, max)) = fee_band(fixed, margin) else {
        return Ok(());
    };
    if (min..=max).contains(&got) {
        return Ok(());
    }
    Err(EthApiError::InvalidTransaction(
        RpcInvalidTransactionError::FeeOutsideTxpoolMargin { got, min, max, margin },
    ))
}

/// `TXPOOL_MARGIN` admission filter for a fresh native coin tx. The tx is left byte-for-byte
/// intact; only its declared fee cap is inspected. Contract creation, calldata, blobs and
/// authorization lists are not native coin transfers and are left alone.
///
/// Called from the single `EthTransactions::send_transaction` submit seam (RPC and mesh rumor),
/// right after the funded-wallet floor. Never called from block assembly or block acceptance.
/// This does not enforce the fixed native price — that belongs to the header/EVM/quote path.
pub fn txpool_margin_entry_gate<T: Transaction>(tx: &T) -> Result<(), EthApiError> {
    if txpool_margin() == 0 {
        return Ok(());
    }
    if tx.is_create() ||
        !tx.input().is_empty() ||
        tx.blob_versioned_hashes().is_some_and(|hashes| !hashes.is_empty()) ||
        tx.authorization_list().is_some_and(|list| !list.is_empty())
    {
        return Ok(());
    }
    check_fee(u128::from(fixed_gas_price_wei()), txpool_margin(), tx.max_fee_per_gas())
}

/// `MARS` transaction-type policy: legacy (`0x0`) and dynamic-fee (`0x2`) co-exist at every
/// height — no fork or timestamp decides which one a wallet may use. Other types (`0x1` access
/// list, `0x3+` nonexistent here) are warned about and refused cleanly at admission; EVM/import
/// does not treat them as an error. Kept separate from the fee gates above.
pub fn mars_tx_type_entry_gate(tx_type: u8) -> Result<(), EthApiError> {
    match tx_type {
        0 | 2 => Ok(()),
        ty => {
            tracing::warn!(
                target: "smartnet::entry_gate",
                tx_type = ty,
                "MARS admits only legacy (0x0) and dynamic-fee (0x2) transactions; refusing type"
            );
            Err(EthApiError::InvalidTransaction(
                RpcInvalidTransactionError::TxTypeNotSupported,
            ))
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const FIXED: u128 = 238_095_238_095;

    #[test]
    fn mars_tx_types_allow_legacy_and_dynamic_fee_only() {
        // Mixed 0x0/0x2 at any height — no fork/timestamp input to this gate.
        mars_tx_type_entry_gate(0).expect("legacy type is first-class");
        mars_tx_type_entry_gate(2).expect("dynamic-fee type is first-class");
        for ty in [1u8, 3, 4, 0x7f] {
            assert!(matches!(
                mars_tx_type_entry_gate(ty),
                Err(EthApiError::InvalidTransaction(
                    RpcInvalidTransactionError::TxTypeNotSupported
                ))
            ));
        }
    }

    #[test]
    fn margin_zero_or_absent_never_checks_a_fee() {
        // Absent falls back to 0; explicit 0 and any wild fee both pass.
        assert_eq!(txpool_margin(), 0);
        for got in [0, 1, FIXED, u128::MAX] {
            check_fee(FIXED, 0, got).expect("margin 0 means no check");
        }
    }

    #[test]
    fn margin_one_requires_the_exact_fixed_fee() {
        check_fee(FIXED, 1, FIXED).expect("exact price admitted");
        for got in [FIXED - 1, FIXED + 1, 0, FIXED * 2] {
            let err = check_fee(FIXED, 1, got).expect_err("exact margin rejects drift");
            assert!(
                matches!(
                    err,
                    EthApiError::InvalidTransaction(
                        RpcInvalidTransactionError::FeeOutsideTxpoolMargin {
                            got: g, min, max, margin: 1
                        }
                    ) if g == got && min == FIXED && max == FIXED
                ),
                "unexpected rejection for {got}: {err:?}"
            );
        }
    }

    #[test]
    fn margin_two_through_five_are_symmetric_up_and_down() {
        for margin in 2u8..=5 {
            let m = u128::from(margin);
            let (min, max) = fee_band(FIXED, margin).expect("nonzero margin has a band");
            assert_eq!(min, FIXED / m);
            assert_eq!(max, FIXED * m);
            check_fee(FIXED, margin, min).expect("lower bound admitted");
            check_fee(FIXED, margin, max).expect("upper bound admitted");
            check_fee(FIXED, margin, FIXED).expect("the fixed price is always admitted");
            check_fee(FIXED, margin, min - 1).expect_err("below lower bound rejected");
            check_fee(FIXED, margin, max + 1).expect_err("above upper bound rejected");
        }
    }
}
