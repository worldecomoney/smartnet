//! Validator account-state lookups — the **single encapsulation boundary** for "how much does
//! this account hold, and what is its next nonce?".
//!
//! The validator has no database and never builds a view of state itself; it asks the node
//! through the single in-process seam ([`crate::io::ValidatorIo`]). Everything state-related goes
//! through [`observe_sender_state`]: the run loop calls it once per pass, [`prefetch_balances`] /
//! [`prefetch_nonces`] are its only users of the I/O boundary, and the balance gate in
//! [`crate::mempool_policy::select_pending_for_block`] stays pure by reading the returned
//! snapshots. Keeping these entry points top-level and dedicated means no balance/nonce lookup is
//! allowed to exist anywhere else in the crate, the per-sender fan-out lives only here, and the
//! wall-clock accounting of the state read (`STATE_CHECKED`) is emitted *inside* the seam, so it
//! is never confused with the seal or import walls.
//!
//! # Bounded work
//!
//! A mempool is attacker-shaped: every distinct sender in it costs the validator a state lookup
//! once per block, *before* the pending txs are chosen from it.
//! That is bounded by **a ceiling** — at most [`MAX_LOOKUPS_PER_PASS`] of each kind per pass. Past
//! it the   remaining senders read as zero / stay absent, so their txs are dropped fail-closed and
//! sit   in the node's pool for a later block. It is a backstop against amplification, not a
//! policy:   it logs `WARN` / `STATE_FANOUT_CAPPED` with the counts rather than stalling silently.
//! The   split point is the node's own `BTreeMap` order (by address) because nothing cheaper than a
//!   state lookup says who should win — the ceiling is meant to be hit only under abuse.
//!
//! # Who is strict about what
//!
//! The **node's** mempool admission is deliberately lenient: it only checks that the sender has
//! *some* funds above the fee, so a tx sits in the pool and waits. The **validator** is the strict
//! gate before a tx enters a block — see [`upfront_native_cost`] and the ERC-20 check — because the
//! validator, not the node, is what seals. A tx the validator drops is left in the node's
//! txpool; the validator never removes anything from it.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use alloy_primitives::{Address, U256};
use alloy_rpc_types_eth::{transaction::TransactionTrait, Transaction};
use alloy_rpc_types_txpool::TxpoolContent;
use eyre::Result;
use tracing::{debug, warn};

use smartnet_timing::fmt_wall;

use crate::{
    io::ValidatorIo,
    mempool_policy::{classify, TxClass},
};

/// Hard ceiling on state lookups of each kind per selection pass. See the module docs: this is
/// an anti-amplification backstop, so it errs high (a node's default pending pool is an order
/// of magnitude above it in practice for a healthy chain) and it always logs when it bites.
const MAX_LOOKUPS_PER_PASS: usize = 4096;

/// Upfront native cost of `tx` at the worst case fee: `value + gas_limit * max_fee_per_gas`.
///
/// This is the conservative bound the gate uses — the most the sender can be charged if the block
/// seals at `max_fee_per_gas`. Using `value + gas_limit * min_fee` (or the effective fee) would
/// admit a tx the sender cannot pay for at the top of its own declared range.
pub(crate) fn upfront_native_cost(tx: &Transaction) -> U256 {
    let worst_fee = U256::from(tx.gas_limit()) * U256::from(tx.max_fee_per_gas());
    tx.value() + worst_fee
}

/// Applies the [`MAX_LOOKUPS_PER_PASS`] ceiling, logging `WARN` / `STATE_FANOUT_CAPPED` when it
/// bites. Everything a selection pass looks up passes through here, so there is exactly one
/// place where "we stopped asking" can happen — and exactly one place that has to say so.
fn apply_ceiling<T>(mut items: Vec<T>, kind: &'static str) -> Vec<T> {
    let total = items.len();
    if total > MAX_LOOKUPS_PER_PASS {
        items.truncate(MAX_LOOKUPS_PER_PASS);
        warn!(
            target: "smartnet_validator",
            event = "STATE_FANOUT_CAPPED",
            kind,
            limit = MAX_LOOKUPS_PER_PASS,
            requested = total,
            skipped = total - MAX_LOOKUPS_PER_PASS,
            "state lookups capped at {MAX_LOOKUPS_PER_PASS} this pass ({} skipped); those txs \
             are dropped from this block and stay queued in the node's pool",
            total - MAX_LOOKUPS_PER_PASS,
        );
    }
    items
}

/// The senders worth asking about this pass, capped at [`MAX_LOOKUPS_PER_PASS`].
///
/// Ordering is the node's own (address) order — deterministic, and not chosen to favour anyone.
/// A sender left out has no entry at all: the nonce gate skips it and the balance gate reads
/// zero for it, so both fail closed.
fn capped_senders(pending: &TxpoolContent<Transaction>, kind: &'static str) -> Vec<Address> {
    apply_ceiling(pending.pending.keys().copied().collect(), kind)
}

/// Every `(token, sender)` pair the balance gate will ask about, in deterministic order, capped
/// at [`MAX_LOOKUPS_PER_PASS`]. One pair per ERC-20 tx, deduplicated on the way past.
fn capped_erc20_pairs(
    pending: &TxpoolContent<Transaction>,
    senders: &[Address],
) -> Vec<(Address, Address)> {
    let mut seen: BTreeSet<(Address, Address)> = BTreeSet::new();
    let mut pairs = Vec::new();
    for sender in senders {
        let Some(by_nonce) = pending.pending.get(sender) else {
            continue;
        };
        for tx in by_nonce.values() {
            if classify(tx) != TxClass::Erc20 {
                continue;
            }
            let Some(token) = tx.to() else { continue };
            if seen.insert((token, *sender)) {
                pairs.push((token, *sender));
            }
        }
    }
    apply_ceiling(pairs, "erc20")
}

/// Prefetched balances for one selection pass.
///
/// Filled by [`prefetch_balances`] over the transport of the day. Selection itself stays pure and
/// synchronous: it only reads this book, so the balance gate never issues an RPC mid-loop.
#[derive(Debug, Default)]
pub(crate) struct BalanceBook {
    native: BTreeMap<Address, U256>,
    /// `(token, owner) → balance`. A missing entry reads as zero, which drops the tx.
    erc20: BTreeMap<(Address, Address), U256>,
}

impl BalanceBook {
    /// Native balance of `owner`. A sender whose balance could not be looked up reads as zero and
    /// has every tx dropped — "not deemed safe" is the correct outcome for an unknown balance.
    pub(crate) fn native_of(&self, owner: Address) -> U256 {
        self.native.get(&owner).copied().unwrap_or(U256::ZERO)
    }

    /// ERC-20 balance of `owner` for `token`, or zero when never fetched.
    pub(crate) fn erc20_of(&self, token: Address, owner: Address) -> U256 {
        self.erc20.get(&(token, owner)).copied().unwrap_or(U256::ZERO)
    }

    /// How many ERC-20 balances were actually looked up this cycle (diagnostics).
    pub(crate) fn erc20_lookups(&self) -> usize {
        self.erc20.len()
    }

    /// Test-only: force `owner`'s native balance.
    #[cfg(test)]
    pub(crate) fn set_native(&mut self, owner: Address, balance: U256) {
        self.native.insert(owner, balance);
    }

    /// Test-only: force `owner`'s `token` balance.
    #[cfg(test)]
    pub(crate) fn set_erc20(&mut self, token: Address, owner: Address, balance: U256) {
        self.erc20.insert((token, owner), balance);
    }

    /// Test-only: a book rich enough that no balance gate ever fires, for every sender in `pool`.
    #[cfg(test)]
    pub(crate) fn rich_for(pool: &TxpoolContent<Transaction>) -> Self {
        let mut book = Self::default();
        let rich = U256::from(10u64).pow(U256::from(36u64)); // 1e36 wei
        for sender in pool.pending.keys() {
            book.set_native(*sender, rich);
        }
        book
    }
}

/// Fetches every balance the next selection pass will need, in one place.
///
/// One native lookup per unique pending sender (not per tx), one ERC-20 lookup per unique
/// `(token, sender)` pair whose queued tx classifies as [`TxClass::Erc20`] — batched, and both
/// sets capped at [`MAX_LOOKUPS_PER_PASS`] (see the module docs).
///
/// A failed lookup is **not** reported as a validator error: it reads as zero, the balance gate
/// drops that sender's txs, and the per-tx drop reason is logged at `VERB = 1`. The validator
/// never surfaces an RPC or transport failure to the operator — it just does not include the tx.
pub(crate) async fn prefetch_balances<I: ValidatorIo>(
    io: &I,
    pending: &TxpoolContent<Transaction>,
) -> BalanceBook {
    let mut book = BalanceBook::default();

    let senders = capped_senders(pending, "balances");
    let native = io.balances(&senders).await;
    for (sender, result) in senders.iter().zip(native) {
        let balance = match result {
            Ok(b) => b,
            Err(e) => {
                debug!(
                    target: "smartnet_validator",
                    event = "BALANCE_LOOKUP_FAILED",
                    kind = "native",
                    owner = %sender,
                    error = %e,
                    "native balance lookup failed; treating as 0 so this sender's txs are dropped"
                );
                U256::ZERO
            }
        };
        book.native.insert(*sender, balance);
    }

    let pairs = capped_erc20_pairs(pending, &senders);
    let erc = io.erc20_balances(&pairs).await;
    for ((token, sender), result) in pairs.iter().zip(erc) {
        let balance = match result {
            Ok(b) => b,
            Err(e) => {
                debug!(
                    target: "smartnet_validator",
                    event = "BALANCE_LOOKUP_FAILED",
                    kind = "erc20",
                    token = %token,
                    owner = %sender,
                    error = %e,
                    "erc-20 balance lookup failed; treating as 0 so this sender's txs are dropped"
                );
                U256::ZERO
            }
        };
        book.erc20.insert((*token, *sender), balance);
    }

    book
}

/// On-chain executable nonce of every pending sender, at the current head.
///
/// Same fan-out, batching and ceiling as [`prefetch_balances`]. Unlike a balance, a sender that
/// is **absent** from the returned map is skipped by the nonce gate rather than read as zero —
/// so a capped or failed lookup still fails closed.
///
/// A transport failure here **is** fatal: unlike balances (whose failure only drops txs we would
/// otherwise have to prove safe), an unusable nonce view means the validator cannot know what a
/// block is allowed to contain, so it must not seal one. That matches the loop this replaces,
/// which propagated the error with `?`.
pub(crate) async fn prefetch_nonces<I: ValidatorIo>(
    io: &I,
    pending: &TxpoolContent<Transaction>,
) -> Result<BTreeMap<Address, u64>> {
    let senders = capped_senders(pending, "nonces");
    let results = io.nonces(&senders).await;

    let mut out = BTreeMap::new();
    for (sender, result) in senders.iter().zip(results) {
        out.insert(*sender, result?);
    }
    Ok(out)
}

/// What one selection pass needs to know about the candidate senders' on-chain state.
///
/// Built by [`observe_sender_state`] and then read-only: [`crate::mempool_policy`] gates on it
/// without ever issuing a lookup of its own.
#[derive(Debug, Default)]
pub(crate) struct SenderState {
    /// `(sender -> on-chain executable nonce)` at the head; an abstaining (capped/failed) sender
    /// is absent and its nonce chain is skipped fail-closed.
    pub(crate) nonces: BTreeMap<Address, u64>,
    /// `(sender[, token] -> balance)`; missing entries read as zero, same fail-closed rule.
    pub(crate) balances: BalanceBook,
}

/// The **one seam call** the run loop makes against sender state, once per selection pass.
///
/// Fetches executable nonces ([`prefetch_nonces`]) and balances ([`prefetch_balances`]) over the
/// transport of the day and accounts the cost as its own bucket — the `STATE_CHECKED` event with
/// its own `wall` — so the state read (potentially many RPC round trips for a wide pool) is never
/// charged to, or mixed with, the seal (`PROPOSAL_SEALED`) or import (`NEW_PAYLOAD_RTT`) walls.
/// When the validator moves in-node, only this function changes; its callers and everything
/// downstream stay untouched.
///
/// Nonce fetch failures stay fatal (see [`prefetch_nonces`]); balance fetch failures degrade to
/// zeros inside [`prefetch_balances`].
pub(crate) async fn observe_sender_state<I: ValidatorIo>(
    io: &I,
    pending: &TxpoolContent<Transaction>,
    node_label: &str,
) -> Result<SenderState> {
    let state_start = Instant::now();
    let nonces = prefetch_nonces(io, pending).await?;
    let balances = prefetch_balances(io, pending).await;
    let checked_addresses = nonces.len();
    debug!(
        target: "smartnet_validator",
        event = "STATE_CHECKED",
        addresses = checked_addresses,
        erc20_lookups = balances.erc20_lookups(),
        node = node_label,
        wall = %fmt_wall(state_start.elapsed()),
        "balances and nonces checked for {checked_addresses} addresses at {node_label} in {}",
        fmt_wall(state_start.elapsed())
    );
    Ok(SenderState { nonces, balances })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mempool_policy::{
        erc20_amount,
        tests::{signed_erc20_tx, signed_native_tx},
    };

    const TOKEN: Address = Address::repeat_byte(0xbb);

    /// `value + gas_limit * max_fee_per_gas` — the worst case, not the effective fee.
    #[test]
    fn upfront_cost_is_value_plus_worst_case_fee() {
        let tx = signed_native_tx(1, U256::from(100u64), 21_000, 10);
        // 100 + 21_000 * 10 = 210_100
        assert_eq!(upfront_native_cost(&tx), U256::from(210_100u64));
    }

    #[test]
    fn zero_value_zero_fee_costs_nothing() {
        let tx = signed_native_tx(1, U256::ZERO, 21_000, 0);
        assert_eq!(upfront_native_cost(&tx), U256::ZERO);
    }

    #[test]
    fn empty_book_reads_everything_as_zero() {
        let book = BalanceBook::default();
        assert_eq!(book.native_of(Address::repeat_byte(0x11)), U256::ZERO);
        assert_eq!(book.erc20_of(TOKEN, Address::repeat_byte(0x11)), U256::ZERO);
        assert_eq!(book.erc20_lookups(), 0);
    }

    #[test]
    fn erc20_amount_decodes_transfer_and_approve() {
        // transfer(to, 7) — amount is the second 32-byte word.
        let tx = signed_erc20_tx(1, 21_000, 10, &[0xa9, 0x05, 0x9c, 0xbb], TOKEN, U256::from(7u64));
        assert_eq!(erc20_amount(&tx), Some(U256::from(7u64)));
        // approve(spender, 9) — same shape.
        let tx = signed_erc20_tx(1, 21_000, 10, &[0x09, 0x5e, 0xa7, 0xb3], TOKEN, U256::from(9u64));
        assert_eq!(erc20_amount(&tx), Some(U256::from(9u64)));
    }

    #[test]
    fn erc20_amount_is_none_for_unrecognized_calldata() {
        let tx = signed_erc20_tx(1, 21_000, 10, &[0xde, 0xad, 0xbe, 0xef], TOKEN, U256::from(1u64));
        assert_eq!(erc20_amount(&tx), None);
    }

    #[test]
    fn rich_for_covers_every_pending_sender() {
        let tx = signed_native_tx(1, U256::from(1u64), 21_000, 10);
        let sender = tx.inner.signer();
        let mut pending: BTreeMap<Address, BTreeMap<String, Transaction>> = BTreeMap::new();
        pending.entry(sender).or_default().insert("1".to_string(), tx);
        let pool = TxpoolContent { pending, queued: BTreeMap::new() };

        let book = BalanceBook::rich_for(&pool);
        assert!(book.native_of(sender) > U256::ZERO);
        assert_eq!(book.erc20_lookups(), 0);
    }

    // ---- bounded fan-out (batching shape + the ceiling) ----

    /// A pool of `n` distinct senders. The cap only reads the sender keys, so empty nonce maps
    /// say exactly as much as real ones and keep the test free of thousands of signatures.
    fn senders_only(n: usize) -> TxpoolContent<Transaction> {
        let mut pending: BTreeMap<Address, BTreeMap<String, Transaction>> = BTreeMap::new();
        for i in 0..n {
            let mut addr = Address::ZERO;
            addr[0..8].copy_from_slice(&(i as u64).to_be_bytes());
            pending.insert(addr, BTreeMap::new());
        }
        TxpoolContent { pending, queued: BTreeMap::new() }
    }

    /// The ceiling truncates at exactly [`MAX_LOOKUPS_PER_PASS`], and leaves shorter lists alone.
    #[test]
    fn the_ceiling_truncates_only_when_it_bites() {
        assert_eq!(
            apply_ceiling(vec![(); MAX_LOOKUPS_PER_PASS], "balances").len(),
            MAX_LOOKUPS_PER_PASS,
            "at the limit is not past it"
        );
        assert_eq!(
            apply_ceiling(vec![(); MAX_LOOKUPS_PER_PASS + 7], "balances").len(),
            MAX_LOOKUPS_PER_PASS
        );
        assert_eq!(apply_ceiling(vec![(); 10], "balances").len(), 10);
        assert_eq!(apply_ceiling(Vec::<()>::new(), "balances").len(), 0);
    }

    /// The cap is what makes the fan-out bounded at all, and it is deterministic between passes.
    #[test]
    fn state_lookups_are_capped_per_pass() {
        let big = senders_only(MAX_LOOKUPS_PER_PASS + 10);
        let senders = capped_senders(&big, "balances");
        assert_eq!(senders.len(), MAX_LOOKUPS_PER_PASS, "must cap");
        assert_eq!(senders, capped_senders(&big, "balances"), "same set every pass");

        let small = senders_only(10);
        assert_eq!(capped_senders(&small, "balances").len(), 10, "small pool is not capped");
    }

    /// Two ERC-20 txs from one sender to one token are one lookup, not two.
    #[test]
    fn erc20_pairs_are_deduplicated() {
        let selector = [0xa9u8, 0x05, 0x9c, 0xbb];
        let tx1 = signed_erc20_tx(0, 21_000, 10, &selector, TOKEN, U256::from(1u64));
        let tx2 = signed_erc20_tx(1, 21_000, 10, &selector, TOKEN, U256::from(2u64));
        let sender = tx1.inner.signer();
        assert_eq!(tx2.inner.signer(), sender, "same key, same sender");

        let mut by_nonce: BTreeMap<String, Transaction> = BTreeMap::new();
        by_nonce.insert("0".into(), tx1);
        by_nonce.insert("1".into(), tx2);
        let pool = TxpoolContent {
            pending: BTreeMap::from([(sender, by_nonce)]),
            queued: BTreeMap::new(),
        };
        assert_eq!(capped_erc20_pairs(&pool, &[sender]).len(), 1);
        // A sender it was not asked about contributes nothing.
        assert!(capped_erc20_pairs(&pool, &[]).is_empty());
    }
}
