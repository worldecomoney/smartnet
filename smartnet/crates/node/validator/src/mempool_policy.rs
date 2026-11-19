//! Pre-daemon mempool selection — **validator-owned** policy only.
//!
//! Deterministic stable ordering, all-executable-one-pass selection, independent signature checks,
//! a per-class balance gate, **per-class caps** ([`SelectionCaps`]; native is packed FIFO by
//! arrival, every other class is fee-priority only when its cap bites) and a hard **block gas
//! limit** cap (the sum of the selected body's *declared* tx gas limits never exceeds the block
//! gas limit; any skip closes the rest of that sender's nonce chain so a block never carries a
//! nonce gap).
//! Not node txpool policy, not FCU, not a tuning surface (no CLI flags in this phase).

use std::{
    cmp::Ordering,
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    ops::Deref,
    time::Instant,
};

use alloy_consensus::transaction::SignerRecoverable;
use alloy_network::TransactionResponse;
use alloy_primitives::{Address, TxHash, U256};
use alloy_rpc_types_eth::{transaction::TransactionTrait, Transaction};
use alloy_rpc_types_txpool::TxpoolContent;
use tracing::debug;

use crate::balances::{upfront_native_cost, BalanceBook};

const SEL_ERC20_TRANSFER: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];
const SEL_ERC20_TRANSFER_FROM: [u8; 4] = [0x23, 0xb8, 0x72, 0xdd];
const SEL_ERC20_APPROVE: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];

// ERC-721 / ERC-1155. `0x23b872dd` is shared with ERC-20 `transferFrom` and is deliberately
// left in the ERC-20 bucket — it cannot be told apart without a token registry.
const SEL_NFT_721_SAFE_TRANSFER_FROM: [u8; 4] = [0x42, 0x84, 0x2e, 0x0e];
const SEL_NFT_721_SAFE_TRANSFER_FROM_BYTES: [u8; 4] = [0xb8, 0x8d, 0x4f, 0xde];
const SEL_NFT_1155_SAFE_TRANSFER_FROM: [u8; 4] = [0xf2, 0x42, 0x43, 0x2a];
const SEL_NFT_1155_SAFE_BATCH_TRANSFER_FROM: [u8; 4] = [0x2e, 0xb2, 0xc2, 0xd6];

/// Per-class block caps (`VAL.md` Issue 2, selection protocol). Optional config keys
/// (`MAX_NATIVE_PER_BLOCK`, `MAX_ERC_20_PER_BLOCK`, `MAX_CONTRACTS_PER_BLOCK`,
/// `MAX_NFT_PER_BLOCK`); blank or absent keeps these defaults.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionCaps {
    /// Cap on plain value-transfer txs sealed per block (`MAX_NATIVE_PER_BLOCK`).
    pub max_native_per_block: u64,
    /// Cap on ERC-20 transfer txs sealed per block (`MAX_ERC_20_PER_BLOCK`).
    pub max_erc20_per_block: u64,
    /// Cap on contract-creation txs sealed per block (`MAX_CONTRACTS_PER_BLOCK`);
    /// also caps the shared `Other` bucket (blobs / 7702 auth / unknown calldata).
    pub max_contracts_per_block: u64,
    /// Cap on ERC-721/1155 transfer txs sealed per block (`MAX_NFT_PER_BLOCK`).
    pub max_nft_per_block: u64,
}

impl Default for SelectionCaps {
    fn default() -> Self {
        Self {
            max_native_per_block: 64,
            max_erc20_per_block: 32,
            max_contracts_per_block: 1,
            max_nft_per_block: 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum TxClass {
    /// Simple value transfer path: no calldata, not a create.
    Native,
    /// Heuristic ERC-20 function selector allowlist.
    Erc20,
    /// ERC-721 / ERC-1155 `safeTransferFrom` family.
    Nft,
    /// Contract creation.
    Contract,
    /// Blobs, EIP-7702 auth, or unknown calldata.
    Other,
}

#[derive(Debug, Default)]
pub(crate) struct SelectionStats {
    pub pending_total: usize,
    pub candidate_count: usize,
    pub selected_count: usize,
    pub rejected_invalid_signature: usize,
    pub rejected_signer_mismatch: usize,
    /// `tx.chain_id()` is absent (a legacy tx that declares no chain) or differs from the
    /// configured `CHAINID`. The validator's config is the single source of truth: a tx that
    /// is not provably for this chain is never sealed.
    pub rejected_chain_id_mismatch: usize,
    /// Outside the per-class cap (`SelectionCaps`). The sender's remaining nonces are dropped
    /// for this block, because the cap member set is always a nonce prefix per sender.
    pub rejected_native_cap: usize,
    pub rejected_erc20_cap: usize,
    pub rejected_contract_cap: usize,
    pub rejected_nft_cap: usize,
    /// Sender's native balance cannot cover `value + gas_limit * max_fee_per_gas`.
    pub rejected_insufficient_native: usize,
    /// ERC-20 tx whose decoded token amount exceeds the sender's balance of that token.
    pub rejected_insufficient_erc20: usize,
    /// ERC-20 tx whose calldata did not decode to a token amount — never guessed at.
    pub rejected_undecodable_erc20: usize,
    /// First tx of a sender whose declared gas limit would push Σ over the block gas limit
    /// (or a single tx whose declared limit already exceeds it).
    pub rejected_gas_overflow: usize,
    /// Later nonces of a sender skipped because an earlier nonce of that sender was skipped
    /// (gas, signature, or a class cap) — dropping nonce N while keeping N+1 would seal a
    /// nonce-gap block the node rejects at execution.
    pub dropped_after_sender_drop: usize,
    pub included_native: usize,
    pub included_erc20: usize,
    pub included_nft: usize,
    pub included_contract: usize,
    pub included_other: usize,
    /// Σ declared gas limits of the selected body — always ≤ `block_gas_limit`.
    pub selected_gas_limit_sum: u64,
}

/// FIFO order key between two senders' earliest pool arrivals: the older sender comes
/// first. A sender with no arrival data (`None`) sorts after one with known times; two
/// unknowns compare equal and fall through to the caller's address tie-break — a pool
/// with no arrival data at all therefore orders exactly as before, by address.
fn cmp_arrival(a: Option<Instant>, b: Option<Instant>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// The earliest pool arrival among these txs, or `None` when none of them carries arrival
/// data (not in the map).
fn min_arrival(
    arrival: &BTreeMap<TxHash, Instant>,
    hashes: impl IntoIterator<Item = TxHash>,
) -> Option<Instant> {
    hashes.into_iter().filter_map(|h| arrival.get(&h)).min().copied()
}

/// Deterministic FIFO ordering over `txpool_content.pending`: senders in pool-arrival
/// order (oldest first), each sender's txs in ascending nonce — so the candidate walk is
/// "oldest eligible tx first" (the `SmartNet` rule: native picks by time, never by fee) and
/// a sender's nonce chain is never reordered against itself. A sender's key is its
/// earliest tx's arrival; missing arrival data falls back to address order (the previous
/// behaviour), never to map-iteration luck.
fn flatten_pending_sorted(
    content: &TxpoolContent<Transaction>,
    arrival: &BTreeMap<TxHash, Instant>,
) -> Vec<(Address, u64, Transaction)> {
    let mut by_sender: Vec<(Address, Vec<(u64, Transaction)>)> = Vec::new();
    for (sender, by_nonce) in &content.pending {
        let mut txs: Vec<(u64, Transaction)> = by_nonce
            .iter()
            .filter_map(|(nonce_str, tx)| nonce_str.parse::<u64>().ok().map(|n| (n, tx.clone())))
            .collect();
        txs.sort_by_key(|(nonce, _)| *nonce);
        by_sender.push((*sender, txs));
    }
    by_sender.sort_by(|(a_sender, a_txs), (b_sender, b_txs)| {
        let a = min_arrival(arrival, a_txs.iter().map(|(_, tx)| tx.tx_hash()));
        let b = min_arrival(arrival, b_txs.iter().map(|(_, tx)| tx.tx_hash()));
        cmp_arrival(a, b).then_with(|| a_sender.cmp(b_sender))
    });
    by_sender
        .into_iter()
        .flat_map(|(sender, txs)| txs.into_iter().map(move |(nonce, tx)| (sender, nonce, tx)))
        .collect()
}

/// Keeps only the `max_txs` oldest pending txs (`TXPOOL_VAL_MAX_TXS`) in the pool snapshot,
/// dropping the newest — or, with no arrival data, everything past address/nonce order.
/// Returns how many pending txs were dropped. Call after
/// [`fold_queued_into_candidates`] so the cap covers the combined candidate set.
///
/// The cap is applied **once, at the source of the seal pass**: the state prefetch
/// ([`crate::balances`]) and the selection both see the same capped snapshot, so a large
/// pool can never fan out more work than the budget allows. Truncation is oldest-first —
/// the dropped txs are the newest, which is exactly what a FIFO rule wants under pressure.
pub(crate) fn truncate_pool_to_max_txs(
    content: &mut TxpoolContent<Transaction>,
    arrival: &BTreeMap<TxHash, Instant>,
    max_txs: usize,
) -> usize {
    let total: usize = content.pending.values().map(BTreeMap::len).sum();
    if total <= max_txs {
        return 0;
    }
    let (pending, dropped) = {
        // Borrow the pool (no tx clones until after the keep/drop decision), order by
        // arrival like `flatten_pending_sorted`, then keep only the oldest `max_txs`.
        let mut txs: Vec<(Address, u64, Option<Instant>, &Transaction)> = Vec::with_capacity(total);
        for (sender, by_nonce) in &content.pending {
            for (nonce_str, tx) in by_nonce {
                let Ok(nonce) = nonce_str.parse::<u64>() else {
                    continue;
                };
                let at = arrival.get(&tx.tx_hash()).copied();
                txs.push((*sender, nonce, at, tx));
            }
        }
        txs.sort_by(|a, b| {
            cmp_arrival(a.2, b.2).then_with(|| a.0.cmp(&b.0)).then_with(|| a.1.cmp(&b.1))
        });
        let dropped = txs.len().saturating_sub(max_txs);
        txs.truncate(max_txs);
        let mut pending: BTreeMap<Address, BTreeMap<String, Transaction>> = BTreeMap::new();
        for (sender, nonce, _, tx) in &txs {
            pending.entry(*sender).or_default().insert(nonce.to_string(), (*tx).clone());
        }
        (pending, dropped)
    };
    content.pending = pending;
    dropped
}

/// Folds the pool's `queued` subpool into `pending` so block selection treats both subpools
/// as seal candidates, and returns how many txs were folded in.
///
/// The pool's subpool label is a hint, not a verdict. A tx can land in `queued` merely
/// because the pool cached a `state_nonce` one behind the true head at validation time
/// (`NO_NONCE_GAPS` not set at insert); the only path back to `pending` is a
/// `CanonStateNotification` for a committed block, so a sealer that reads only `pending`
/// can deadlock on a misclassified contiguous run (`MPP-ISSUE.md`: `pending=0 queued=15`,
/// head stalls forever).
///
/// Folding is safe because every executability verdict is re-decided downstream against
/// canonical state, never against the pool's label: [`gate_executable_nonces`] re-derives
/// the contiguous nonce run from the on-chain nonce (canonical, via `io.nonces()`), the
/// balance gate reads canonical balances, and `eth_simulateV1` re-validates the block. A
/// genuinely future-nonce tx (a real gap) is still excluded by the nonce gate; an
/// under-funded or stale-nonce tx is still excluded by balance / simulate. No consensus
/// validation is weakened.
pub(crate) fn fold_queued_into_candidates(content: &mut TxpoolContent<Transaction>) -> usize {
    if content.queued.is_empty() {
        return 0;
    }
    let queued = std::mem::take(&mut content.queued);
    let mut folded = 0usize;
    for (sender, by_nonce) in queued {
        let slot = content.pending.entry(sender).or_default();
        for (nonce_key, tx) in by_nonce {
            // A same-(sender, nonce) collision cannot happen (the pool rejects duplicates);
            // if it somehow did, keep the existing `pending` entry (do not overwrite it)
            // and do not count it as folded.
            if let Entry::Vacant(vacant) = slot.entry(nonce_key) {
                vacant.insert(tx);
                folded += 1;
            }
        }
    }
    folded
}

/// Keeps only the contiguous executable nonce chain for each sender, starting at the account's
/// current on-chain nonce.
///
/// ```text
/// on-chain nonce = 5
/// pool: {5, 6, 7, 9}
///   → selected {5, 6, 7}; nonce 9 stays queued (gap)
/// ```
///
/// A gap (a future-nonce tx whose predecessor is absent) stops the chain: later txs remain queued
/// and never enter the block. There is no `max_gap` policy — nonce gaps inside a block are never
/// permitted. Senders absent from `on_chain_nonces` are skipped; the caller must populate the map
/// for every sender it expects to include.
fn gate_executable_nonces(
    sorted: Vec<(Address, u64, Transaction)>,
    on_chain_nonces: &BTreeMap<Address, u64>,
) -> Vec<(Address, u64, Transaction)> {
    // Group by sender, but keep the input's first-seen sender order (arrival order from
    // `flatten_pending_sorted`): walking the `BTreeMap` directly would silently re-sort
    // senders by address and undo the FIFO ordering upstream.
    let mut by_sender: BTreeMap<Address, Vec<(u64, Transaction)>> = BTreeMap::new();
    let mut order: Vec<Address> = Vec::new();
    for (sender, nonce, tx) in sorted {
        if !by_sender.contains_key(&sender) {
            order.push(sender);
        }
        by_sender.entry(sender).or_default().push((nonce, tx));
    }
    let mut out = Vec::new();
    for sender in order {
        let Some(slot) = by_sender.get_mut(&sender) else { continue };
        let Some(&expected) = on_chain_nonces.get(&sender) else {
            continue;
        };
        let mut txs = std::mem::take(slot);
        txs.sort_by_key(|(nonce, _)| *nonce);
        let mut next = expected;
        for (nonce, tx) in txs {
            if nonce < next {
                continue; // stale: already mined or superseded
            }
            if nonce > next {
                break; // gap: rest of the chain is queued
            }
            out.push((sender, nonce, tx));
            // `checked`, never `next += 1`: a tx claiming nonce `u64::MAX` has no successor,
            // and a release-mode wrap would restart the chain at 0 — re-admitting already-mined
            // nonces, or worse, admitting the same `u64::MAX` nonce twice.
            next = match next.checked_add(1) {
                Some(n) => n,
                None => break,
            };
        }
    }
    out
}

/// Revises a final block body after simulation reports a rejected transaction at `rejected`
/// indices. A rejected tx leaves the sender's on-chain nonce unadvanced, so every later nonce of
/// that sender is also non-executable and is removed. Order of the remaining txs is preserved.
pub(crate) fn revise_final_body_for_rejected(
    body: &[Transaction],
    rejected: &[usize],
) -> Vec<Transaction> {
    let mut drop_min_nonce: BTreeMap<Address, u64> = BTreeMap::new();
    for &idx in rejected {
        if let Some(tx) = body.get(idx) {
            let sender = tx.inner.signer();
            let nonce = tx.inner.nonce();
            drop_min_nonce
                .entry(sender)
                .and_modify(|min| *min = (*min).min(nonce))
                .or_insert(nonce);
        }
    }
    body.iter()
        .filter(|tx| {
            drop_min_nonce.get(&tx.inner.signer()).is_none_or(|&min| tx.inner.nonce() < min)
        })
        .cloned()
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignatureReject {
    InvalidSignature,
    SignerMismatch,
}

fn verify_transaction_signature(tx: &Transaction) -> Result<(), SignatureReject> {
    let recovered = SignerRecoverable::recover_signer(tx.inner.deref())
        .map_err(|_| SignatureReject::InvalidSignature)?;
    if recovered != tx.inner.signer() {
        return Err(SignatureReject::SignerMismatch);
    }
    Ok(())
}

pub(crate) fn classify(tx: &Transaction) -> TxClass {
    if tx.is_create() {
        return TxClass::Contract;
    }
    if tx.blob_versioned_hashes().is_some_and(|b| !b.is_empty()) {
        return TxClass::Other;
    }
    if tx.authorization_list().is_some_and(|a| !a.is_empty()) {
        return TxClass::Other;
    }
    let input = tx.input().as_ref();
    if input.is_empty() {
        return TxClass::Native;
    }
    if input.len() >= 4 {
        let sel = &input[..4];
        if sel == SEL_NFT_721_SAFE_TRANSFER_FROM ||
            sel == SEL_NFT_721_SAFE_TRANSFER_FROM_BYTES ||
            sel == SEL_NFT_1155_SAFE_TRANSFER_FROM ||
            sel == SEL_NFT_1155_SAFE_BATCH_TRANSFER_FROM
        {
            return TxClass::Nft;
        }
        if sel == SEL_ERC20_TRANSFER || sel == SEL_ERC20_TRANSFER_FROM || sel == SEL_ERC20_APPROVE {
            return TxClass::Erc20;
        }
    }
    TxClass::Other
}

/// The tip a tx bids — `max_priority_fee_per_gas`. Legacy txs report `None` and bid zero.
fn fee_bid(tx: &Transaction) -> u128 {
    tx.max_priority_fee_per_gas().unwrap_or(0)
}

/// Decides which candidates survive the per-class caps, **before** any per-tx gate runs.
///
/// Native is packed in FIFO order: the validator picks native coins by time, never by fee.
/// Every other class is fee-priority — when its candidates exceed the cap, the sender runs
/// with the highest `max_priority_fee_per_gas` win (`VAL.md` Issue 2 selection protocol).
///
/// Candidates grouped as class -> sender -> (nonce, tx) in nonce order.
type ByClass<'a> = BTreeMap<TxClass, BTreeMap<Address, Vec<(u64, &'a Transaction)>>>;
/// One sender's contiguous nonce run, tagged with the fee it bids.
type SenderRun<'a> = (u128, Address, Vec<(u64, &'a Transaction)>);

/// A sender's txs are always taken as a **prefix** in nonce order, so the surviving set never
/// contains an internal nonce gap. Reordering is only ever between senders, never within one.
fn cap_members(
    candidates: &[(Address, u64, Transaction)],
    caps: &SelectionCaps,
    arrival: &BTreeMap<TxHash, Instant>,
) -> BTreeSet<(Address, u64)> {
    let mut by_class: ByClass<'_> = BTreeMap::new();
    for (sender, nonce, tx) in candidates {
        by_class.entry(classify(tx)).or_default().entry(*sender).or_default().push((*nonce, tx));
    }

    let mut keep: BTreeSet<(Address, u64)> = BTreeSet::new();
    for (class, senders) in by_class {
        let cap = match class {
            TxClass::Native => caps.max_native_per_block,
            TxClass::Erc20 => caps.max_erc20_per_block,
            // `Other` (blobs, 7702 auth, unknown calldata) shares the contract cap: it is the
            // "not a plain transfer" bucket and must stay rare.
            TxClass::Contract | TxClass::Other => caps.max_contracts_per_block,
            TxClass::Nft => caps.max_nft_per_block,
        } as usize;

        // Each sender is one contiguous nonce run; the run's bid is its entry tx's tip.
        let mut runs: Vec<SenderRun<'_>> =
            senders.into_iter().map(|(sender, txs)| (fee_bid(txs[0].1), sender, txs)).collect();

        if class == TxClass::Native {
            // FIFO — the SmartNet rule: native coins are picked by pool arrival (the
            // sender whose oldest tx arrived first wins), never by fee. Address is only
            // the deterministic tie-break, and the whole key when no arrival data exists.
            runs.sort_by(|a, b| {
                let arr_a = min_arrival(arrival, a.2.iter().map(|(_, tx)| tx.tx_hash()));
                let arr_b = min_arrival(arrival, b.2.iter().map(|(_, tx)| tx.tx_hash()));
                cmp_arrival(arr_a, arr_b).then_with(|| a.1.cmp(&b.1))
            });
        } else {
            // Fee economy: highest tip first, address as the deterministic tie-break.
            runs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        }

        let mut remaining = cap;
        for (_bid, sender, txs) in runs {
            if remaining == 0 {
                break;
            }
            let take = remaining.min(txs.len());
            for (nonce, _) in &txs[..take] {
                keep.insert((sender, *nonce));
            }
            remaining -= take;
        }
    }
    keep
}

/// Token amount an [`TxClass::Erc20`] tx moves, decoded from its calldata.
///
/// ```text
/// transfer(to, amt)      selector(4) + to(32)   + amt(32)
/// approve(spender, amt)  selector(4) + sp(32)   + amt(32)
/// transferFrom(f, t, amt) selector(4) + from(32) + to(32) + amt(32)
/// ```
///
/// `None` means the calldata did not decode — the tx is not "deemed safe" and is dropped rather
/// than guessed at.
pub(crate) fn erc20_amount(tx: &Transaction) -> Option<U256> {
    let input = tx.input().as_ref();
    if input.len() < 4 {
        return None;
    }
    let sel = &input[..4];
    let amount_at = if sel == SEL_ERC20_TRANSFER || sel == SEL_ERC20_APPROVE {
        4 + 32
    } else if sel == SEL_ERC20_TRANSFER_FROM {
        4 + 32 + 32
    } else {
        return None;
    };
    let bytes = input.get(amount_at..amount_at + 32)?;
    Some(U256::from_be_slice(bytes))
}

/// Owner whose tokens a `transferFrom` moves — the `from` argument, decoded from calldata
/// (`transferFrom(from, to, amt)`: selector(4) + from(32) + to(32) + amt(32); the address
/// is the low 20 bytes of the first word). `None` when the calldata is not a decodable
/// `transferFrom`.
pub(crate) fn erc20_transfer_from_owner(tx: &Transaction) -> Option<Address> {
    let input = tx.input().as_ref();
    if input.len() < 4 + 32 || input[..4] != SEL_ERC20_TRANSFER_FROM {
        return None;
    }
    let word = input.get(4..36)?;
    Some(Address::from_slice(&word[12..32]))
}

/// Applies pre-daemon mempool policy: per-sender executable nonce-chain gating (from the account's
/// on-chain nonce), a **balance gate**, signature checks, **per-class caps** ([`SelectionCaps`]),
/// and a hard block gas limit cap: the Σ of selected *declared* tx gas limits never exceeds
/// `block_gas_limit` (same conservative rule the node's own payload builders use). A future-nonce
/// tx whose predecessor is missing stays queued and never enters the block. Any skipped tx closes
/// the rest of that sender's chain for this block. The only total bound is the pool snapshot
/// itself (already truncated to `TXPOOL_VAL_MAX_TXS` upstream): every executable pending tx is
/// considered, one loop, no candidate window.
///
/// # Per-class caps
///
/// [`cap_members`] runs first and decides which candidates are *eligible* for each class
/// ([`TxClass::Native`], [`TxClass::Erc20`], [`TxClass::Nft`], [`TxClass::Contract`], plus
/// [`TxClass::Other`] sharing the contract cap). Native is packed in FIFO order — the validator
/// picks native coins by pool arrival (oldest first, from
/// [`crate::io::ValidatorIo::txpool_arrival`]), never by fee. Every other class is fee-priority
/// *only when its cap bites*: the highest-bidder senders win, and each sender is always taken as a
/// nonce prefix so the surviving set never holds an internal gap. The walk below runs senders in
/// arrival order (nonce-ascending within each sender), so a cap-dropped nonce 0 latches the sender
/// and its later nonces go with it even when those later txs are a different class that still has
/// room.
///
/// # Balance gate
///
/// A tx is only included if the validator has verified the sender can pay for it:
///
/// - **every class** must satisfy `native_balance >= value + gas_limit * max_fee_per_gas`
///   ([`upfront_native_cost`]), and
/// - an [`TxClass::Erc20`] tx must additionally have a decodable token amount and —
///   per selector — the right owner's token balance:
///   `transfer` → `erc20_balance(token, sender) >= amount`;
///   `transferFrom` → `erc20_balance(token, from) >= amount` (the `from` owner's
///   tokens, not the caller's);
///   `approve` → no token-balance gate (the infinite-approval pattern legitimately
///   approves more than the current balance; the native/fee gates still apply).
///
/// A tx that fails either is dropped and left in the **node's** txpool (the validator never removes
/// anything from it); the drop reason is logged at `VERB = 1`. Balances come from the prefetched
/// [`BalanceBook`], so selection itself never issues an RPC — the transport lives entirely behind
/// [`crate::balances::val_get_curr_bal`] / [`crate::balances::val_get_curr_bal_erc`].
///
/// The check is **per-tx against the current on-chain balance, not cumulative**: it does not model
/// "this block already spends X of this sender". That keeps the rule the simple one the protocol
/// asks for; `eth_simulateV1` is what catches a cumulative shortfall, and
/// [`revise_final_body_for_rejected`] then drops the failing tx and the rest of that sender's
/// chain from the final body before sealing.
pub(crate) fn select_pending_for_block(
    content: &TxpoolContent<Transaction>,
    on_chain_nonces: &BTreeMap<Address, u64>,
    balances: &BalanceBook,
    caps: SelectionCaps,
    chain_id: u64,
    block_gas_limit: u64,
    arrival: &BTreeMap<TxHash, Instant>,
) -> (Vec<Transaction>, SelectionStats) {
    let sorted = flatten_pending_sorted(content, arrival);
    let pending_total = sorted.len();
    let candidates: Vec<(Address, u64, Transaction)> =
        gate_executable_nonces(sorted, on_chain_nonces);
    let candidate_count = candidates.len();

    // Per-class caps decide *membership* first, so a fee-priority rule can look at the whole
    // class before anything is packed. Everything below still walks per-sender nonce runs in
    // arrival order, so a cap-dropped nonce 0 latches the sender and its nonce 1 goes with it.
    let members = cap_members(&candidates, &caps, arrival);

    let mut stats = SelectionStats { pending_total, candidate_count, ..Default::default() };

    let mut selected = Vec::new();
    let mut gas_limit_sum: u64 = 0;
    let mut dropped_sender: Option<Address> = None;

    for (sender, nonce, tx) in candidates {
        // Nonce-contiguity latch: once any tx of this sender is skipped, every later nonce of
        // the same sender is skipped too (candidates are grouped per sender in arrival order,
        // nonces ascending).
        if dropped_sender == Some(sender) {
            stats.dropped_after_sender_drop += 1;
            continue;
        }

        // Outside this class's cap. `cap_members` only ever keeps a nonce prefix per sender,
        // so every later nonce of this sender is also outside — latch and move on.
        if !members.contains(&(sender, nonce)) {
            match classify(&tx) {
                TxClass::Native => stats.rejected_native_cap += 1,
                TxClass::Erc20 => stats.rejected_erc20_cap += 1,
                TxClass::Contract | TxClass::Other => stats.rejected_contract_cap += 1,
                TxClass::Nft => stats.rejected_nft_cap += 1,
            }
            dropped_sender = Some(sender);
            continue;
        }

        // Chain id — the config is the single source of truth (`CHAINID`). A tx that declares
        // no chain at all, or one that differs, is not provably for this chain and is never
        // sealed; without this the node would reject the block at execution anyway.
        // Cheapest and most decisive check, so it runs before signature recovery.
        match tx.chain_id() {
            Some(tx_chain) if tx_chain == chain_id => {}
            other => {
                stats.rejected_chain_id_mismatch += 1;
                log_tx_dropped(
                    &tx,
                    sender,
                    "chain id mismatch",
                    &format!(
                        "tx declares {}, config CHAINID is {chain_id}",
                        other.map_or_else(|| "none".to_string(), |c| c.to_string())
                    ),
                );
                dropped_sender = Some(sender);
                continue;
            }
        }

        match verify_transaction_signature(&tx) {
            Ok(()) => {}
            Err(SignatureReject::InvalidSignature) => {
                stats.rejected_invalid_signature += 1;
                dropped_sender = Some(sender);
                continue;
            }
            Err(SignatureReject::SignerMismatch) => {
                stats.rejected_signer_mismatch += 1;
                dropped_sender = Some(sender);
                continue;
            }
        }

        // Classify before the balance gate: the ERC-20 leg only applies to ERC-20 txs.
        let class = classify(&tx);

        // Balance gate — the validator only seals a tx it has verified the sender can pay for.
        let native_have = balances.native_of(sender);
        let native_need = upfront_native_cost(&tx);
        if native_have < native_need {
            stats.rejected_insufficient_native += 1;
            log_tx_dropped(
                &tx,
                sender,
                "insufficient funds",
                &format!("need {native_need} wei, have {native_have}"),
            );
            dropped_sender = Some(sender);
            continue;
        }

        if class == TxClass::Erc20 {
            let Some(token) = tx.to() else {
                stats.rejected_undecodable_erc20 += 1;
                log_tx_dropped(&tx, sender, "erc-20 tx has no recipient token", "no token");
                dropped_sender = Some(sender);
                continue;
            };
            let Some(amount) = erc20_amount(&tx) else {
                stats.rejected_undecodable_erc20 += 1;
                log_tx_dropped(
                    &tx,
                    sender,
                    "erc-20 calldata did not decode",
                    "amount not readable",
                );
                dropped_sender = Some(sender);
                continue;
            };
            // Token-balance owner per selector: `transfer` moves the sender's tokens;
            // `transferFrom` moves the `from` owner's tokens (not the caller's);
            // `approve` moves nothing — the infinite-approval pattern legitimately
            // approves more than the current balance, so no token-balance gate applies.
            let sel = tx.input().as_ref().get(..4);
            let gate_owner = if sel == Some(&SEL_ERC20_TRANSFER[..]) {
                Some(sender)
            } else if sel == Some(&SEL_ERC20_TRANSFER_FROM[..]) {
                erc20_transfer_from_owner(&tx)
            } else {
                None // approve
            };
            if let Some(owner) = gate_owner {
                let erc_have = balances.erc20_of(token, owner);
                if erc_have < amount {
                    stats.rejected_insufficient_erc20 += 1;
                    log_tx_dropped(
                        &tx,
                        sender,
                        "insufficient token balance",
                        &format!("token {token}: need {amount}, have {erc_have}"),
                    );
                    dropped_sender = Some(sender);
                    continue;
                }
            } else if sel != Some(&SEL_ERC20_APPROVE[..]) {
                // transferFrom whose `from` word did not decode — malformed, never seal.
                stats.rejected_undecodable_erc20 += 1;
                log_tx_dropped(
                    &tx,
                    sender,
                    "erc-20 transferFrom owner did not decode",
                    "from argument not readable",
                );
                dropped_sender = Some(sender);
                continue;
            }
        }

        // Hard stop at the block gas limit. Declared limits (not simulated gas) are summed:
        // selection runs before any simulation, so declared limits are the only sound bound.
        let tx_gas = tx.gas_limit();
        if tx_gas > block_gas_limit || gas_limit_sum.saturating_add(tx_gas) > block_gas_limit {
            stats.rejected_gas_overflow += 1;
            dropped_sender = Some(sender);
            continue;
        }

        match class {
            TxClass::Native => stats.included_native += 1,
            TxClass::Erc20 => stats.included_erc20 += 1,
            TxClass::Nft => stats.included_nft += 1,
            TxClass::Contract => stats.included_contract += 1,
            TxClass::Other => stats.included_other += 1,
        }
        gas_limit_sum += tx_gas;
        selected.push(tx);
    }

    stats.selected_count = selected.len();
    stats.selected_gas_limit_sum = gas_limit_sum;
    (selected, stats)
}

/// Logs one balance-gate (or decode) drop at `VERB = 1`, so the operator sees *why* a tx that is
/// sitting in the node's txpool did not make it into the block. Deliberately carries no RPC,
/// transport or node detail — just the tx, the sender and the reason.
fn log_tx_dropped(tx: &Transaction, sender: Address, reason: &str, detail: &str) {
    debug!(
        target: "smartnet_validator",
        event = "TX_DROPPED",
        tx_hash = %tx.tx_hash(),
        sender = %sender,
        reason,
        detail,
        "tx {} dropped: {reason} ({detail}) — left in the node's txpool",
        tx.tx_hash()
    );
}

pub(crate) fn log_mempool_selection(event: &'static str, stats: &SelectionStats) {
    debug!(
        target: "smartnet_validator",
        event,
        pending_total = stats.pending_total,
        candidate_count = stats.candidate_count,
        selected_count = stats.selected_count,
        rejected_invalid_signature = stats.rejected_invalid_signature,
        rejected_signer_mismatch = stats.rejected_signer_mismatch,
        rejected_chain_id_mismatch = stats.rejected_chain_id_mismatch,
        rejected_insufficient_native = stats.rejected_insufficient_native,
        rejected_insufficient_erc20 = stats.rejected_insufficient_erc20,
        rejected_undecodable_erc20 = stats.rejected_undecodable_erc20,
        rejected_native_cap = stats.rejected_native_cap,
        rejected_erc20_cap = stats.rejected_erc20_cap,
        rejected_contract_cap = stats.rejected_contract_cap,
        rejected_nft_cap = stats.rejected_nft_cap,
        rejected_gas_overflow = stats.rejected_gas_overflow,
        dropped_after_sender_drop = stats.dropped_after_sender_drop,
        selected_gas_limit_sum = stats.selected_gas_limit_sum,
        included_native = stats.included_native,
        included_erc20 = stats.included_erc20,
        included_nft = stats.included_nft,
        included_contract = stats.included_contract,
        included_other = stats.included_other,
        policy = "pre_daemon_fifo_all_executable_chainid_sig_verify_balance_gate_per_class_caps",
        "mempool selection (validator-owned pre-daemon policy)"
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloy_network::{EthereumWallet, NetworkTransactionBuilder, TransactionBuilder};
    use alloy_primitives::{address, B256, U256};
    use alloy_rpc_types_eth::TransactionRequest;
    use alloy_signer_local::PrivateKeySigner;
    use k256::ecdsa::SigningKey;
    use std::{collections::BTreeMap, time::Duration};

    const CHAIN_ID: u64 = 315;
    /// Tests that are not about gas pass an effectively unlimited block gas limit.
    const NO_GAS_CAP: u64 = u64::MAX;

    /// Every existing selection test only cares about nonce / signature / gas / class, so it gets
    /// a balance book rich enough that the balance gate never fires.
    pub(crate) fn call_select(
        pool: &TxpoolContent<Transaction>,
        nonces: &BTreeMap<Address, u64>,
        block_gas_limit: u64,
    ) -> (Vec<Transaction>, SelectionStats) {
        call_select_with(
            pool,
            nonces,
            &BalanceBook::rich_for(pool),
            SelectionCaps::default(),
            CHAIN_ID,
            block_gas_limit,
        )
    }

    /// Same, but with an explicit balance book and explicit caps — for the balance-gate and
    /// per-class-cap tests. No arrival data: ordering falls back to address order.
    pub(crate) fn call_select_with(
        pool: &TxpoolContent<Transaction>,
        nonces: &BTreeMap<Address, u64>,
        balances: &BalanceBook,
        caps: SelectionCaps,
        chain_id: u64,
        block_gas_limit: u64,
    ) -> (Vec<Transaction>, SelectionStats) {
        select_pending_for_block(
            pool,
            nonces,
            balances,
            caps,
            chain_id,
            block_gas_limit,
            &BTreeMap::new(),
        )
    }

    /// Selection with an explicit pool-arrival map — the FIFO-ordering tests, where the
    /// oldest sender must win regardless of address order.
    pub(crate) fn call_select_arrival(
        pool: &TxpoolContent<Transaction>,
        nonces: &BTreeMap<Address, u64>,
        caps: SelectionCaps,
        block_gas_limit: u64,
        arrival: &BTreeMap<TxHash, Instant>,
    ) -> (Vec<Transaction>, SelectionStats) {
        select_pending_for_block(
            pool,
            nonces,
            &BalanceBook::rich_for(pool),
            caps,
            CHAIN_ID,
            block_gas_limit,
            arrival,
        )
    }

    /// A signed tx with fully explicit fields — the general form the other builders share.
    pub(crate) fn signed_tx_with_input(
        nonce: u64,
        to: Option<Address>,
        input: Vec<u8>,
        value: U256,
        gas_limit: u64,
        max_fee_per_gas: u128,
    ) -> Transaction {
        let secret = signer(7);
        let from = secret_key_address(&secret);
        let signer = PrivateKeySigner::from_bytes(&B256::from_slice(&secret.to_bytes())).unwrap();
        let wallet = EthereumWallet::from(signer);
        let mut req = TransactionRequest::default()
            .with_from(from)
            .with_nonce(nonce)
            .with_gas_limit(gas_limit)
            .with_chain_id(CHAIN_ID)
            .with_max_fee_per_gas(max_fee_per_gas)
            .with_max_priority_fee_per_gas(1_000_000_000u128)
            .with_value(value);
        if let Some(to) = to {
            req = req.with_to(to);
        } else {
            req = req.create();
        }
        if !input.is_empty() {
            req = req.with_input(input);
        }
        rpc_tx_from(rt_block_on(req, &wallet), from)
    }

    /// A signed native (empty-calldata) tx with an explicit value, gas limit and max fee —
    /// shared with `balances.rs` tests.
    pub(crate) fn signed_native_tx(
        nonce: u64,
        value: U256,
        gas_limit: u64,
        max_fee_per_gas: u128,
    ) -> Transaction {
        signed_tx_with_input(
            nonce,
            Some(Address::repeat_byte(0xaa)),
            Vec::new(),
            value,
            gas_limit,
            max_fee_per_gas,
        )
    }

    /// A signed ERC-20 tx: `selector` + a 32-byte dummy arg + a 32-byte `amount`, sent to
    /// `token`. Shared with `balances.rs` tests.
    pub(crate) fn signed_erc20_tx(
        nonce: u64,
        gas_limit: u64,
        max_fee_per_gas: u128,
        selector: &[u8; 4],
        token: Address,
        amount: U256,
    ) -> Transaction {
        let mut input = selector.to_vec();
        input.extend_from_slice(&[0u8; 32]);
        input.extend_from_slice(&amount.to_be_bytes::<32>());
        signed_tx_with_input(nonce, Some(token), input, U256::ZERO, gas_limit, max_fee_per_gas)
    }

    fn signer(key_byte: u8) -> SigningKey {
        SigningKey::from_slice(&[key_byte; 32]).expect("valid secret")
    }

    fn build_tx(
        secret: &SigningKey,
        nonce: u64,
        to: Option<Address>,
        input: Vec<u8>,
    ) -> Transaction {
        build_tx_with_gas(secret, nonce, to, input, 21_000)
    }

    fn build_tx_with_gas(
        secret: &SigningKey,
        nonce: u64,
        to: Option<Address>,
        input: Vec<u8>,
        gas_limit: u64,
    ) -> Transaction {
        let from = secret_key_address(secret);
        let signer = PrivateKeySigner::from_bytes(&B256::from_slice(&secret.to_bytes())).unwrap();
        let wallet = EthereumWallet::from(signer);
        let mut req = TransactionRequest::default()
            .with_from(from)
            .with_nonce(nonce)
            .with_gas_limit(gas_limit)
            .with_chain_id(CHAIN_ID)
            .with_max_fee_per_gas(2_000_000_000u128)
            .with_max_priority_fee_per_gas(1_000_000_000u128)
            .with_value(U256::from(1u64));
        if let Some(to) = to {
            req = req.with_to(to);
        } else {
            req = req.create();
        }
        if !input.is_empty() {
            req = req.with_input(input);
        }
        let envelope = rt_block_on(req, &wallet);
        rpc_tx_from(envelope, from)
    }

    /// Same as [`build_tx`] but with an explicit tip, so a test can make one sender outbid
    /// another — only the fee-priority path (`cap_members`) reads this.
    fn build_tx_with_tip(
        secret: &SigningKey,
        nonce: u64,
        to: Option<Address>,
        input: Vec<u8>,
        tip: u128,
    ) -> Transaction {
        let from = secret_key_address(secret);
        let signer = PrivateKeySigner::from_bytes(&B256::from_slice(&secret.to_bytes())).unwrap();
        let wallet = EthereumWallet::from(signer);
        let mut req = TransactionRequest::default()
            .with_from(from)
            .with_nonce(nonce)
            .with_gas_limit(21_000u64)
            .with_chain_id(CHAIN_ID)
            .with_max_fee_per_gas(2_000_000_000u128 + tip)
            .with_max_priority_fee_per_gas(tip)
            .with_value(U256::from(1u64));
        if let Some(to) = to {
            req = req.with_to(to);
        } else {
            req = req.create();
        }
        if !input.is_empty() {
            req = req.with_input(input);
        }
        let envelope = rt_block_on(req, &wallet);
        rpc_tx_from(envelope, from)
    }

    fn rt_block_on(
        req: TransactionRequest,
        wallet: &EthereumWallet,
    ) -> alloy_consensus::TxEnvelope {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(req.build(wallet)).unwrap()
    }

    fn rpc_tx_from(envelope: alloy_consensus::TxEnvelope, from: Address) -> Transaction {
        Transaction {
            inner: alloy_consensus::transaction::Recovered::new_unchecked(envelope, from),
            block_hash: None,
            block_number: None,
            transaction_index: None,
            effective_gas_price: None,
            block_timestamp: None,
        }
    }

    fn secret_key_address(secret: &SigningKey) -> Address {
        let uncompressed = secret.verifying_key().to_encoded_point(false);
        let digest = alloy_primitives::keccak256(&uncompressed.as_bytes()[1..]);
        Address::from_slice(&digest.as_slice()[12..])
    }

    fn pool_of(txs: Vec<Transaction>) -> TxpoolContent<Transaction> {
        let mut pending: BTreeMap<Address, BTreeMap<String, Transaction>> = BTreeMap::new();
        for tx in txs {
            let from = tx.inner.signer();
            let nonce = tx.inner.nonce();
            pending.entry(from).or_default().insert(nonce.to_string(), tx);
        }
        TxpoolContent { pending, queued: BTreeMap::new() }
    }

    /// Moves every tx from `pending` to `queued` — the snapshot shape the seal deadlock needs
    /// (`pending=0`, `queued=N`), where the pool labelled an executable contiguous run as queued.
    fn queued_only(mut content: TxpoolContent<Transaction>) -> TxpoolContent<Transaction> {
        content.queued = std::mem::take(&mut content.pending);
        content
    }

    /// Fresh-sender on-chain nonces (0) for every sender present in the pool.
    fn all_zero_nonces(content: &TxpoolContent<Transaction>) -> BTreeMap<Address, u64> {
        content.pending.keys().map(|sender| (*sender, 0u64)).collect()
    }

    #[test]
    fn classify_native_erc20_and_other() {
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");

        let native = build_tx(&secret, 0, Some(to), vec![]);
        assert_eq!(classify(&native), TxClass::Native);

        let erc20_selector = [0xa9, 0x05, 0x9c, 0xbb];
        let erc20 = build_tx(&secret, 1, Some(to), [&erc20_selector[..], &[0u8; 64]].concat());
        assert_eq!(classify(&erc20), TxClass::Erc20);

        let create = build_tx(&secret, 2, None, vec![0x60, 0x00]);
        assert_eq!(classify(&create), TxClass::Contract);

        // ERC-721 `safeTransferFrom(address,address,uint256)` — unambiguous NFT selector.
        let nft =
            build_tx(&secret, 4, Some(to), [&[0x42, 0x84, 0x2e, 0x0e][..], &[0u8; 96]].concat());
        assert_eq!(classify(&nft), TxClass::Nft);

        let unknown = build_tx(&secret, 3, Some(to), vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(classify(&unknown), TxClass::Other);
    }

    #[test]
    fn selection_includes_native_and_erc20_and_caps_other_at_one() {
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let erc20_selector = [0xa9, 0x05, 0x9c, 0xbb];

        let native1 = build_tx(&secret, 0, Some(to), vec![]);
        let native2 = build_tx(&secret, 1, Some(to), vec![]);
        let erc20 = build_tx(&secret, 2, Some(to), [&erc20_selector[..], &[0u8; 64]].concat());
        let other1 = build_tx(&secret, 3, None, vec![0x60, 0x00]);
        let other2 = build_tx(&secret, 4, None, vec![0x60, 0x01]);

        let pool = pool_of(vec![native1, native2, erc20, other1, other2]);
        let (selected, stats) = call_select(&pool, &all_zero_nonces(&pool), NO_GAS_CAP);

        assert_eq!(stats.pending_total, 5);
        assert_eq!(stats.selected_count, 4);
        assert_eq!(stats.included_native, 2);
        assert_eq!(stats.included_erc20, 1);
        // `to: None` makes these contract creates — the contract cap (1) now governs them.
        assert_eq!(stats.included_other, 0);
        assert_eq!(stats.included_contract, 1);
        assert_eq!(stats.rejected_contract_cap, 1);
        // Nonce 4 *is* the rejected create tx itself (it is the last tx of this single-sender
        // pool), so the nonce-contiguity latch has nothing further to drop here — see
        // `other_cap_reject_latches_rest_of_sender_chain` for the mid-chain case.
        assert_eq!(stats.dropped_after_sender_drop, 0);
        assert_eq!(selected.len(), 4);

        // FIFO (sender, nonce) ordering preserved.
        let nonces: Vec<u64> = selected.iter().map(|tx| tx.inner.nonce()).collect();
        assert_eq!(nonces, vec![0, 1, 2, 3]);
    }

    #[test]
    fn selection_sorts_across_senders_by_address_then_nonce() {
        let secret_a = signer(1);
        let secret_b = signer(2);
        let to = address!("0x1111111111111111111111111111111111111111");

        let a0 = build_tx(&secret_a, 0, Some(to), vec![]);
        let b0 = build_tx(&secret_b, 0, Some(to), vec![]);
        let a1 = build_tx(&secret_a, 1, Some(to), vec![]);

        let pool = pool_of(vec![a0, b0, a1]);
        let (selected, stats) = call_select(&pool, &all_zero_nonces(&pool), NO_GAS_CAP);
        assert_eq!(stats.pending_total, 3);
        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].inner.nonce(), 0);
        assert_eq!(selected[1].inner.nonce(), 1);
        assert_eq!(selected[2].inner.nonce(), 0);
        assert_eq!(selected[0].inner.signer(), selected[1].inner.signer());
    }

    /// The FIFO rule uses real pool-arrival times, not address order: with the native cap
    /// at 1 the sender whose tx arrived first wins — and the earlier arrival is
    /// deliberately assigned to the *higher* address, so plain address ordering would
    /// pick the loser.
    #[test]
    fn native_cap_packs_oldest_arrival_not_address() {
        let to = address!("0x1111111111111111111111111111111111111111");
        let tx_a = build_tx(&signer(1), 0, Some(to), vec![]);
        let tx_b = build_tx(&signer(2), 0, Some(to), vec![]);
        let (addr_a, addr_b) = (tx_a.inner.signer(), tx_b.inner.signer());
        let (early_hash, early_addr, late_hash) = if addr_a < addr_b {
            (tx_b.tx_hash(), addr_b, tx_a.tx_hash())
        } else {
            (tx_a.tx_hash(), addr_a, tx_b.tx_hash())
        };
        let base = Instant::now();
        let mut arrival = BTreeMap::new();
        arrival.insert(early_hash, base);
        arrival.insert(late_hash, base + Duration::from_secs(1));

        let pool = pool_of(vec![tx_a, tx_b]);
        let caps = SelectionCaps { max_native_per_block: 1, ..SelectionCaps::default() };
        let (selected, _) =
            call_select_arrival(&pool, &all_zero_nonces(&pool), caps, NO_GAS_CAP, &arrival);

        assert_eq!(selected.len(), 1, "cap 1 keeps exactly one native tx");
        assert_eq!(
            selected[0].inner.signer(),
            early_addr,
            "the oldest arrival must win — not the lower address"
        );
    }

    /// Without any cap biting, the sealed body lists senders in pool-arrival order: the
    /// sender whose tx arrived first is first in the block (the earlier arrival is assigned
    /// to the higher address, so address order cannot explain the result).
    #[test]
    fn body_orders_senders_by_arrival_not_address() {
        let to = address!("0x1111111111111111111111111111111111111111");
        let tx_a = build_tx(&signer(1), 0, Some(to), vec![]);
        let tx_b = build_tx(&signer(2), 0, Some(to), vec![]);
        let (addr_a, addr_b) = (tx_a.inner.signer(), tx_b.inner.signer());
        let (early_hash, early_addr, late_hash, late_addr) = if addr_a < addr_b {
            (tx_b.tx_hash(), addr_b, tx_a.tx_hash(), addr_a)
        } else {
            (tx_a.tx_hash(), addr_a, tx_b.tx_hash(), addr_b)
        };
        let base = Instant::now();
        let mut arrival = BTreeMap::new();
        arrival.insert(early_hash, base);
        arrival.insert(late_hash, base + Duration::from_secs(1));

        let pool = pool_of(vec![tx_a, tx_b]);
        let (selected, stats) = call_select_arrival(
            &pool,
            &all_zero_nonces(&pool),
            SelectionCaps::default(),
            NO_GAS_CAP,
            &arrival,
        );

        assert_eq!(stats.selected_count, 2);
        assert_eq!(selected[0].inner.signer(), early_addr, "oldest sender first");
        assert_eq!(selected[1].inner.signer(), late_addr, "newest sender second");
    }

    /// `TXPOOL_VAL_MAX_TXS` caps the pass at the oldest N pending txs: the newest are
    /// dropped from the snapshot (state prefetch and selection see only the kept set), and
    /// a pool at or below the cap is left untouched.
    #[test]
    fn truncate_pool_keeps_oldest_max_txs() {
        let to = address!("0x1111111111111111111111111111111111111111");
        let txs: Vec<Transaction> =
            (0..4).map(|nonce| build_tx(&signer(1), nonce, Some(to), vec![])).collect();
        let base = Instant::now();
        let mut arrival = BTreeMap::new();
        for (i, tx) in txs.iter().enumerate() {
            arrival.insert(tx.tx_hash(), base + Duration::from_secs(i as u64));
        }

        let mut pool = pool_of(txs);
        let dropped = truncate_pool_to_max_txs(&mut pool, &arrival, 2);
        assert_eq!(dropped, 2, "nonces 2 and 3 are the newest, they go");
        let kept: Vec<u64> = pool
            .pending
            .values()
            .next()
            .expect("one sender")
            .keys()
            .map(|k| k.parse().expect("numeric nonce key"))
            .collect();
        assert_eq!(kept, vec![0, 1], "the two oldest nonces survive");

        assert_eq!(truncate_pool_to_max_txs(&mut pool, &arrival, 2), 0, "at the cap: noop");
        assert_eq!(truncate_pool_to_max_txs(&mut pool, &arrival, 16), 0, "below the cap: noop");
    }

    #[test]
    fn selection_covers_all_executable_pending() {
        // No candidate window: with the per-class cap lifted, every executable pending tx is
        // selected in one pass (the only total bound is the upstream TXPOOL_VAL_MAX_TXS snapshot).
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let many: Vec<Transaction> = (0..300u64)
            .map(|nonce| build_tx(&secret, nonce, Some(to), vec![]))
            .collect();
        let pool = pool_of(many);
        let caps = SelectionCaps { max_native_per_block: u64::MAX, ..SelectionCaps::default() };
        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &BalanceBook::rich_for(&pool),
            caps,
            CHAIN_ID,
            NO_GAS_CAP,
        );
        assert_eq!(stats.pending_total, 300);
        assert_eq!(stats.candidate_count, 300);
        assert_eq!(selected.len(), 300);
    }

    #[test]
    fn signature_rejection_counts() {
        // A tx recovered from a different key than the reported signer must be rejected.
        let real = signer(1);
        let fake = signer(2);
        let to = address!("0x1111111111111111111111111111111111111111");
        let tx = build_tx(&real, 0, Some(to), vec![]);

        // Build a tx actually signed by the fake key but claim `from` is the real address,
        // so the recovered signer mismatches the claimed sender.
        let signer = PrivateKeySigner::from_bytes(&B256::from_slice(&fake.to_bytes())).unwrap();
        let wallet = EthereumWallet::from(signer);
        let req = TransactionRequest::default()
            .with_from(secret_key_address(&fake))
            .with_nonce(1)
            .with_gas_limit(21_000)
            .with_chain_id(CHAIN_ID)
            .with_max_fee_per_gas(2_000_000_000u128)
            .with_max_priority_fee_per_gas(1_000_000_000u128)
            .with_value(U256::from(1u64))
            .with_to(to);
        let mismatched: Transaction =
            rpc_tx_from(rt_block_on(req, &wallet), secret_key_address(&real));

        assert_eq!(verify_transaction_signature(&tx), Ok(()));
        assert_eq!(verify_transaction_signature(&mismatched), Err(SignatureReject::SignerMismatch));

        let pool = pool_of(vec![tx, mismatched]);
        let (selected, stats) = call_select(&pool, &all_zero_nonces(&pool), NO_GAS_CAP);
        assert_eq!(selected.len(), 1);
        assert_eq!(stats.rejected_signer_mismatch, 1);
    }

    #[test]
    fn nonce_gap_keeps_future_nonce_queued() {
        // on-chain nonce 5; pool has 5,6,7,9. 9 must stay queued (nonce 8 missing).
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let sender = secret_key_address(&secret);
        let pool = pool_of(vec![
            build_tx(&secret, 5, Some(to), vec![]),
            build_tx(&secret, 6, Some(to), vec![]),
            build_tx(&secret, 7, Some(to), vec![]),
            build_tx(&secret, 9, Some(to), vec![]),
        ]);
        let mut nonces = all_zero_nonces(&pool);
        nonces.insert(sender, 5);

        let (selected, stats) = call_select(&pool, &nonces, NO_GAS_CAP);
        let sel: Vec<u64> = selected.iter().map(|tx| tx.inner.nonce()).collect();
        assert_eq!(sel, vec![5, 6, 7]);
        assert_eq!(stats.pending_total, 4);
        assert_eq!(selected.len(), 3);
    }

    #[test]
    fn contiguous_chain_all_executable() {
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let sender = secret_key_address(&secret);
        let pool = pool_of(vec![
            build_tx(&secret, 1, Some(to), vec![]),
            build_tx(&secret, 2, Some(to), vec![]),
            build_tx(&secret, 3, Some(to), vec![]),
        ]);
        let mut nonces = all_zero_nonces(&pool);
        nonces.insert(sender, 1);

        let (selected, _) = call_select(&pool, &nonces, NO_GAS_CAP);
        let sel: Vec<u64> = selected.iter().map(|tx| tx.inner.nonce()).collect();
        assert_eq!(sel, vec![1, 2, 3]);
    }

    /// A nonce at the `u64` maximum ends the sender's chain — it must not overflow `next += 1`.
    ///
    /// In debug the old `next += 1` panicked; in release it wrapped to 0, which re-opens the
    /// chain on already-mined nonces (or, with a second claimant of the same terminal nonce,
    /// admits `u64::MAX` twice — a duplicate-nonce block the node rejects at execution).
    #[test]
    fn nonce_at_u64_max_ends_the_chain_without_overflowing() {
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let sender = secret_key_address(&secret);

        // The gate itself, with two claimants of the same terminal nonce: the first ends the
        // chain, the second must not be admitted and `next` must never wrap.
        let gated = gate_executable_nonces(
            vec![
                (sender, u64::MAX, build_tx(&secret, u64::MAX, Some(to), vec![])),
                (sender, u64::MAX, build_tx(&secret, u64::MAX, Some(to), vec![])),
            ],
            &BTreeMap::from([(sender, u64::MAX)]),
        );
        assert_eq!(gated.len(), 1, "nonce u64::MAX has no successor nonce");
        assert_eq!(gated[0].1, u64::MAX);

        // End to end: one tx at the terminal nonce still selects normally.
        let pool = pool_of(vec![build_tx(&secret, u64::MAX, Some(to), vec![])]);
        let mut nonces = all_zero_nonces(&pool);
        nonces.insert(sender, u64::MAX);
        let (selected, _) = call_select(&pool, &nonces, NO_GAS_CAP);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].inner.nonce(), u64::MAX);
    }

    #[test]
    fn below_on_chain_nonce_is_stale_and_dropped() {
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let sender = secret_key_address(&secret);
        let pool = pool_of(vec![
            build_tx(&secret, 0, Some(to), vec![]),
            build_tx(&secret, 1, Some(to), vec![]),
        ]);
        let mut nonces = all_zero_nonces(&pool);
        nonces.insert(sender, 1);

        let (selected, _) = call_select(&pool, &nonces, NO_GAS_CAP);
        let sel: Vec<u64> = selected.iter().map(|tx| tx.inner.nonce()).collect();
        assert_eq!(sel, vec![1]);
    }

    #[test]
    fn fold_queued_into_candidates_moves_and_counts() {
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let sender = secret_key_address(&secret);

        // pending already holds nonce 0; queued holds a duplicate nonce 0 (a collision that
        // must keep the pending entry) plus a new nonce 1 (which must be folded in).
        let mut content = pool_of(vec![build_tx(&secret, 0, Some(to), vec![])]);
        let pending_nonce0 = content.pending[&sender]["0"].clone();
        content
            .queued
            .entry(sender)
            .or_default()
            .insert("0".to_string(), pending_nonce0.clone());
        content
            .queued
            .entry(sender)
            .or_default()
            .insert("1".to_string(), build_tx(&secret, 1, Some(to), vec![]));

        let folded = fold_queued_into_candidates(&mut content);
        assert_eq!(folded, 1, "only the genuinely-new nonce counts as folded");
        assert!(content.queued.is_empty(), "queued is emptied");
        let by_nonce = &content.pending[&sender];
        assert_eq!(by_nonce.len(), 2, "both nonces now live in pending");
        assert_eq!(
            by_nonce["0"].tx_hash(),
            pending_nonce0.tx_hash(),
            "on a collision the existing pending entry is kept, not overwritten"
        );
    }

    /// The seal deadlock regression: the pool labelled an *executable* contiguous run as
    /// `queued` (`pending=0`, `queued=N`) because it cached a state nonce behind the true
    /// head. A sealer that read only `pending` would find nothing, seal nothing, and wait
    /// forever for a promotion that only a committed block can deliver. Folding the queued
    /// bucket into the candidates must make the run selectable.
    #[test]
    fn seal_selects_misclassified_queued_run() {
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let sender = secret_key_address(&secret);

        // A clean contiguous run 5,6,7 sitting entirely in `queued`; pending is empty.
        let mut pool = queued_only(pool_of(vec![
            build_tx(&secret, 5, Some(to), vec![]),
            build_tx(&secret, 6, Some(to), vec![]),
            build_tx(&secret, 7, Some(to), vec![]),
        ]));
        assert!(pool.pending.is_empty(), "precondition: nothing in pending");
        assert_eq!(pool.queued[&sender].len(), 3);

        // Canonical on-chain nonce matches the run's start (the stale read that mislabelled it).
        let mut nonces = BTreeMap::new();
        nonces.insert(sender, 5);

        // Mirror production: fold first, then the balance book and selection see `pending`.
        let folded = fold_queued_into_candidates(&mut pool);
        assert_eq!(folded, 3);
        let (selected, stats) = call_select(&pool, &nonces, NO_GAS_CAP);
        let sel: Vec<u64> = selected.iter().map(|tx| tx.inner.nonce()).collect();
        assert_eq!(sel, vec![5, 6, 7], "misclassified run must seal");
        assert_eq!(stats.pending_total, 3);
    }

    /// Folding must not weaken the nonce gate: a *genuine* gap in the queued bucket is still
    /// excluded even though the tx is now a candidate.
    #[test]
    fn seal_still_excludes_real_gap_in_queued() {
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let sender = secret_key_address(&secret);

        // on-chain nonce 5, pool has 5,6,7,9 — nonce 8 is genuinely missing.
        let mut pool = queued_only(pool_of(vec![
            build_tx(&secret, 5, Some(to), vec![]),
            build_tx(&secret, 6, Some(to), vec![]),
            build_tx(&secret, 7, Some(to), vec![]),
            build_tx(&secret, 9, Some(to), vec![]),
        ]));
        let mut nonces = BTreeMap::new();
        nonces.insert(sender, 5);

        fold_queued_into_candidates(&mut pool);
        let (selected, stats) = call_select(&pool, &nonces, NO_GAS_CAP);
        let sel: Vec<u64> = selected.iter().map(|tx| tx.inner.nonce()).collect();
        assert_eq!(sel, vec![5, 6, 7], "the gap at nonce 9 still stops the chain");
        assert_eq!(stats.pending_total, 4);
    }

    /// Folding must not weaken the stale-nonce rule: a queued tx below the on-chain nonce is
    /// still dropped, not resurrected.
    #[test]
    fn seal_still_excludes_stale_queued_below_on_chain() {
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let sender = secret_key_address(&secret);

        let mut pool = queued_only(pool_of(vec![
            build_tx(&secret, 0, Some(to), vec![]),
            build_tx(&secret, 1, Some(to), vec![]),
        ]));
        let mut nonces = BTreeMap::new();
        nonces.insert(sender, 1);

        fold_queued_into_candidates(&mut pool);
        let (selected, _) = call_select(&pool, &nonces, NO_GAS_CAP);
        let sel: Vec<u64> = selected.iter().map(|tx| tx.inner.nonce()).collect();
        assert_eq!(sel, vec![1], "stale nonce 0 stays dropped; nonce 1 seals");
    }

    #[test]
    fn revise_final_body_truncates_sender_chain_on_reject() {
        let secret_a = signer(1);
        let secret_b = signer(2);
        let to = address!("0x1111111111111111111111111111111111111111");
        let sender_a = secret_key_address(&secret_a);
        let sender_b = secret_key_address(&secret_b);

        let a1 = build_tx(&secret_a, 1, Some(to), vec![]);
        let a2 = build_tx(&secret_a, 2, Some(to), vec![]);
        let b0 = build_tx(&secret_b, 0, Some(to), vec![]);
        let body = vec![a1, a2, b0];

        // Rejecting A(nonce 1) also removes A(nonce 2); B stays.
        let revised = revise_final_body_for_rejected(&body, &[0]);
        let signers: Vec<Address> = revised.iter().map(|tx| tx.inner.signer()).collect();
        let nonces: Vec<u64> = revised.iter().map(|tx| tx.inner.nonce()).collect();
        assert_eq!(signers, vec![sender_b]);
        assert_eq!(nonces, vec![0]);

        // Rejecting A(nonce 2) removes only it; A(nonce 1) and B remain.
        let revised2 = revise_final_body_for_rejected(&body, &[1]);
        let signers2: Vec<Address> = revised2.iter().map(|tx| tx.inner.signer()).collect();
        let nonces2: Vec<u64> = revised2.iter().map(|tx| tx.inner.nonce()).collect();
        assert_eq!(signers2, vec![sender_a, sender_b]);
        assert_eq!(nonces2, vec![1, 0]);

        // No-op when nothing is rejected.
        let revised3 = revise_final_body_for_rejected(&body, &[]);
        assert_eq!(revised3.len(), body.len());
    }

    #[test]
    fn selection_stops_at_block_gas_limit_and_keeps_sender_contiguous() {
        let secret_a = signer(1);
        let secret_b = signer(2);
        let to = address!("0x1111111111111111111111111111111111111111");

        // A: nonce0 (60k) + nonce1 (60k) cannot both fit a 100k block; B: one 30k tx.
        let a0 = build_tx_with_gas(&secret_a, 0, Some(to), vec![], 60_000);
        let a1 = build_tx_with_gas(&secret_a, 1, Some(to), vec![], 60_000);
        let b0 = build_tx_with_gas(&secret_b, 0, Some(to), vec![], 30_000);

        let pool = pool_of(vec![a0, a1, b0]);
        let (selected, stats) = call_select(&pool, &all_zero_nonces(&pool), 100_000);

        // Σ declared gas never exceeds the limit, whichever sender is addressed first.
        assert_eq!(stats.selected_count, 2);
        assert_eq!(stats.selected_gas_limit_sum, 90_000);
        assert_eq!(stats.rejected_gas_overflow, 1);
        assert_eq!(stats.dropped_after_sender_drop, 0);

        // Per sender: selected nonces are contiguous from the on-chain nonce (0) — never a gap.
        let mut per_sender: BTreeMap<Address, Vec<u64>> = BTreeMap::new();
        for tx in &selected {
            per_sender.entry(tx.inner.signer()).or_default().push(tx.inner.nonce());
        }
        for nonces in per_sender.values() {
            let expected: Vec<u64> = (0..nonces.len() as u64).collect();
            assert_eq!(*nonces, expected);
        }
        // A's chain was cut at the overflow: only one of A's two 60k txs fits a 100k block.
        let a_sender = secret_key_address(&secret_a);
        assert_eq!(per_sender.get(&a_sender).map(|v| v.len()), Some(1));
    }

    #[test]
    fn selection_exact_fit_at_block_gas_limit_is_allowed() {
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let txs: Vec<Transaction> = (0u64..3)
            .map(|nonce| build_tx_with_gas(&secret, nonce, Some(to), vec![], 21_000))
            .collect();
        let pool = pool_of(txs);

        let (selected, stats) = call_select(&pool, &all_zero_nonces(&pool), 42_000);

        // Σ == limit exactly is allowed; the third tx would push Σ over the limit.
        assert_eq!(stats.selected_count, 2);
        assert_eq!(stats.selected_gas_limit_sum, 42_000);
        assert_eq!(stats.rejected_gas_overflow, 1);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn oversized_tx_is_skipped_and_next_sender_still_packs() {
        let secret_a = signer(1);
        let secret_b = signer(2);
        let to = address!("0x1111111111111111111111111111111111111111");

        // A nonce0 declares 60k > the 50k block limit; A nonce1 must be dropped with it;
        // B still packs. Selection never returns an empty body when a fitting tx exists.
        let a0 = build_tx_with_gas(&secret_a, 0, Some(to), vec![], 60_000);
        let a1 = build_tx_with_gas(&secret_a, 1, Some(to), vec![], 21_000);
        let b0 = build_tx_with_gas(&secret_b, 0, Some(to), vec![], 21_000);

        let pool = pool_of(vec![a0, a1, b0]);
        let (selected, stats) = call_select(&pool, &all_zero_nonces(&pool), 50_000);

        assert_eq!(stats.rejected_gas_overflow, 1);
        assert_eq!(stats.dropped_after_sender_drop, 1);
        assert_eq!(stats.selected_count, 1);
        assert!(stats.selected_gas_limit_sum <= 50_000);
        assert_eq!(selected[0].inner.signer(), secret_key_address(&secret_b));
    }

    #[test]
    fn other_cap_reject_latches_rest_of_sender_chain() {
        // One sender: nonce0 Other fills the single Other slot, nonce1 Other is rejected by the
        // cap, and nonce2 (native) must be dropped with it — otherwise the block would carry
        // [0, 2] and the node would reject it on the nonce gap.
        let secret = signer(1);
        let to = address!("0x1111111111111111111111111111111111111111");
        let other0 = build_tx(&secret, 0, None, vec![0x60, 0x00]);
        let other1 = build_tx(&secret, 1, None, vec![0x60, 0x01]);
        let native2 = build_tx(&secret, 2, Some(to), vec![]);

        let pool = pool_of(vec![other0, other1, native2]);
        let (selected, stats) = call_select(&pool, &all_zero_nonces(&pool), NO_GAS_CAP);

        assert_eq!(stats.included_other, 0);
        assert_eq!(stats.included_contract, 1);
        assert_eq!(stats.rejected_contract_cap, 1);
        assert_eq!(stats.dropped_after_sender_drop, 1);
        assert_eq!(stats.selected_count, 1);
        assert_eq!(selected[0].inner.nonce(), 0);
    }

    /// Same as [`build_tx`] but signed for an explicit chain id — for the chain-id gate test.
    fn build_tx_with_chain(
        secret: &SigningKey,
        nonce: u64,
        to: Option<Address>,
        input: Vec<u8>,
        chain_id: u64,
    ) -> Transaction {
        let from = secret_key_address(secret);
        let signer = PrivateKeySigner::from_bytes(&B256::from_slice(&secret.to_bytes())).unwrap();
        let wallet = EthereumWallet::from(signer);
        let mut req = TransactionRequest::default()
            .with_from(from)
            .with_nonce(nonce)
            .with_gas_limit(21_000u64)
            .with_chain_id(chain_id)
            .with_max_fee_per_gas(2_000_000_000u128)
            .with_max_priority_fee_per_gas(1_000_000_000u128)
            .with_value(U256::from(1u64));
        if let Some(to) = to {
            req = req.with_to(to);
        } else {
            req = req.create();
        }
        if !input.is_empty() {
            req = req.with_input(input);
        }
        rpc_tx_from(rt_block_on(req, &wallet), from)
    }

    /// `CHAINID` in the validator config is the single source of truth. A tx signed for another
    /// chain must never be sealed — the node would reject the whole block at execution.
    #[test]
    fn chain_id_mismatch_is_rejected_and_latches_the_sender() {
        let to = address!("0x1111111111111111111111111111111111111111");
        let secret = signer(1);
        let wrong_chain = build_tx_with_chain(&secret, 0, Some(to), vec![], CHAIN_ID + 1);
        // Nonce 1 is correctly chained — it must still go, because nonce 0 was dropped.
        let good_chain = build_tx(&secret, 1, Some(to), vec![]);

        let pool = pool_of(vec![wrong_chain, good_chain]);
        let (selected, stats) = call_select(&pool, &all_zero_nonces(&pool), NO_GAS_CAP);

        assert_eq!(selected.len(), 0, "a wrong-chain tx must not be sealed");
        assert_eq!(stats.rejected_chain_id_mismatch, 1);
        assert_eq!(stats.dropped_after_sender_drop, 1, "nonce 1 went with nonce 0");
        assert_eq!(stats.rejected_signer_mismatch, 0, "the chain id failed first, not the signer");
    }

    /// The happy path: a tx for exactly the configured chain id passes the gate untouched.
    #[test]
    fn chain_id_matching_config_is_admitted() {
        let to = address!("0x1111111111111111111111111111111111111111");
        let pool = pool_of(vec![build_tx(&signer(1), 0, Some(to), vec![])]);
        let (selected, stats) = call_select(&pool, &all_zero_nonces(&pool), NO_GAS_CAP);
        assert_eq!(selected.len(), 1);
        assert_eq!(stats.rejected_chain_id_mismatch, 0);
    }

    // ---- per-class caps (`VAL.md` Issue 2, selection protocol) ----

    /// Senders sorted by address; native never looks at the tip, so the low-fee sender still
    /// wins its slot. This is the "native is picked by time, not fee" half of the rule.
    #[test]
    fn native_cap_is_fifo_not_fee() {
        let to = address!("0x1111111111111111111111111111111111111111");
        // signer(1) < signer(2) < signer(3) by address is not guaranteed, so collect and sort.
        let mut senders: Vec<SigningKey> = vec![signer(1), signer(2), signer(3)];
        senders.sort_by_key(secret_key_address);
        let low_fee_first = build_tx_with_tip(&senders[0], 0, Some(to), vec![], 1);
        let mid_fee = build_tx_with_tip(&senders[1], 0, Some(to), vec![], 1_000);
        let high_fee_last = build_tx_with_tip(&senders[2], 0, Some(to), vec![], 1_000_000);

        let pool = pool_of(vec![low_fee_first, mid_fee, high_fee_last]);
        let caps = SelectionCaps { max_native_per_block: 2, ..SelectionCaps::default() };
        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &BalanceBook::rich_for(&pool),
            caps,
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert_eq!(selected.len(), 2, "native cap 2 must keep exactly two");
        assert_eq!(stats.rejected_native_cap, 1);
        // The two kept are the two lowest-address senders, regardless of tip.
        let kept: std::collections::BTreeSet<Address> =
            selected.iter().map(|tx| tx.inner.signer()).collect();
        assert_eq!(
            kept,
            [secret_key_address(&senders[0]), secret_key_address(&senders[1])]
                .into_iter()
                .collect()
        );
    }

    /// The mirror image: an ERC-20 class over its cap keeps the highest tip, not the FIFO
    /// order — this is the "bid higher fees" half, and only ever bites when a cap bites.
    #[test]
    fn erc20_cap_prefers_higher_fee() {
        let to = address!("0x1111111111111111111111111111111111111111");
        let erc20_selector = [0xa9, 0x05, 0x9c, 0xbb];
        let calldata = [&erc20_selector[..], &[0u8; 64]].concat();

        let mut senders: Vec<SigningKey> = vec![signer(1), signer(2), signer(3)];
        senders.sort_by_key(secret_key_address);
        let low = build_tx_with_tip(&senders[0], 0, Some(to), calldata.clone(), 1);
        let mid = build_tx_with_tip(&senders[1], 0, Some(to), calldata.clone(), 1_000);
        let high = build_tx_with_tip(&senders[2], 0, Some(to), calldata, 1_000_000);

        let pool = pool_of(vec![low, mid, high]);
        let caps = SelectionCaps { max_erc20_per_block: 2, ..SelectionCaps::default() };
        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &BalanceBook::rich_for(&pool),
            caps,
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert_eq!(selected.len(), 2, "erc-20 cap 2 must keep exactly two");
        assert_eq!(stats.rejected_erc20_cap, 1);
        let kept: std::collections::BTreeSet<Address> =
            selected.iter().map(|tx| tx.inner.signer()).collect();
        assert_eq!(
            kept,
            [secret_key_address(&senders[1]), secret_key_address(&senders[2])]
                .into_iter()
                .collect(),
            "the two highest tips win, not the two lowest addresses"
        );
    }

    /// The four caps are independent: a native flood cannot crowd out an ERC-20 tx, and a
    /// contract cannot eat the NFT slots.
    #[test]
    fn per_class_caps_are_independent() {
        let to = address!("0x1111111111111111111111111111111111111111");
        let erc20_selector = [0xa9, 0x05, 0x9c, 0xbb];
        let nft_selector = [0x42, 0x84, 0x2e, 0x0e];

        // Four distinct senders so no latch couples the classes.
        let n1 = build_tx(&signer(1), 0, Some(to), vec![]);
        let n2 = build_tx(&signer(2), 0, Some(to), vec![]);
        let n3 = build_tx(&signer(3), 0, Some(to), vec![]);
        let e1 = build_tx(&signer(4), 0, Some(to), [&erc20_selector[..], &[0u8; 64]].concat());
        let e2 = build_tx(&signer(5), 0, Some(to), [&erc20_selector[..], &[0u8; 64]].concat());
        let c1 = build_tx(&signer(6), 0, None, vec![0x60, 0x00]);
        let m1 = build_tx(&signer(7), 0, Some(to), [&nft_selector[..], &[0u8; 96]].concat());

        let pool = pool_of(vec![n1, n2, n3, e1, e2, c1, m1]);
        let caps = SelectionCaps {
            max_native_per_block: 1,
            max_erc20_per_block: 1,
            max_contracts_per_block: 1,
            max_nft_per_block: 1,
        };
        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &BalanceBook::rich_for(&pool),
            caps,
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert_eq!(stats.included_native, 1);
        assert_eq!(stats.included_erc20, 1);
        assert_eq!(stats.included_contract, 1);
        assert_eq!(stats.included_nft, 1);
        assert_eq!(selected.len(), 4, "one of each class, four senders");
        assert_eq!(stats.rejected_native_cap, 2);
        assert_eq!(stats.rejected_erc20_cap, 1);
        assert_eq!(stats.rejected_nft_cap, 0);
        assert_eq!(stats.rejected_contract_cap, 0);
    }

    /// The correctness property the whole design rests on: a cap drop at nonce 0 must take the
    /// sender's later nonces with it, even when those later txs are a *different class* that
    /// still has room under its own cap. Otherwise the block carries [1] alone and the node
    /// rejects it on the nonce gap.
    #[test]
    fn cap_drop_latches_cross_class_nonce_gap() {
        let to = address!("0x1111111111111111111111111111111111111111");
        let secret = signer(1);
        // Nonce 0 is a contract create (capped to 0 here); nonce 1 is native with room to spare.
        let create0 = build_tx(&secret, 0, None, vec![0x60, 0x00]);
        let native1 = build_tx(&secret, 1, Some(to), vec![]);

        let pool = pool_of(vec![create0, native1]);
        let caps = SelectionCaps { max_contracts_per_block: 0, ..SelectionCaps::default() };
        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &BalanceBook::rich_for(&pool),
            caps,
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert_eq!(selected.len(), 0, "nonce 1 cannot be sealed without nonce 0");
        assert_eq!(stats.rejected_contract_cap, 1);
        assert_eq!(stats.dropped_after_sender_drop, 1, "the native nonce went with the create");
        assert_eq!(stats.included_native, 0);
    }

    #[test]
    fn signature_reject_latches_rest_of_sender_chain() {
        // Nonce 1 fails signature verification; nonce 2 of the same sender must be dropped too —
        // including it would seal [nonce 0, nonce 2] and the node would reject the block.
        let real = signer(1);
        let fake = signer(2);
        let to = address!("0x1111111111111111111111111111111111111111");

        let ok0 = build_tx(&real, 0, Some(to), vec![]);
        let ok2 = build_tx(&real, 2, Some(to), vec![]);

        let fake_signer =
            PrivateKeySigner::from_bytes(&B256::from_slice(&fake.to_bytes())).unwrap();
        let wallet = EthereumWallet::from(fake_signer);
        let req = TransactionRequest::default()
            .with_from(secret_key_address(&fake))
            .with_nonce(1)
            .with_gas_limit(21_000)
            .with_chain_id(CHAIN_ID)
            .with_max_fee_per_gas(2_000_000_000u128)
            .with_max_priority_fee_per_gas(1_000_000_000u128)
            .with_value(U256::from(1u64))
            .with_to(to);
        let mismatched = rpc_tx_from(rt_block_on(req, &wallet), secret_key_address(&real));

        let pool = pool_of(vec![ok0, mismatched, ok2]);
        let (selected, stats) = call_select(&pool, &all_zero_nonces(&pool), NO_GAS_CAP);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].inner.nonce(), 0);
        assert_eq!(stats.rejected_signer_mismatch, 1);
        assert_eq!(stats.dropped_after_sender_drop, 1);
    }

    // ---- balance gate ----------------------------------------------------

    const ERC20_TRANSFER: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];
    const ERC20_TRANSFER_FROM: [u8; 4] = [0x23, 0xb8, 0x72, 0xdd];
    const ERC20_APPROVE: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];
    const TOKEN: Address = Address::repeat_byte(0xbb);

    /// `value=100, gas=21_000, max_fee=10` needs exactly `210_100` wei.
    fn book_with_native(sender: Address, wei: u64) -> BalanceBook {
        let mut book = BalanceBook::default();
        book.set_native(sender, U256::from(wei));
        book
    }

    #[test]
    fn balance_gate_drops_tx_the_sender_cannot_pay_for() {
        let tx = signed_native_tx(0, U256::from(100u64), 21_000, 10);
        let sender = tx.inner.signer();
        let pool = pool_of(vec![tx]);
        let book = book_with_native(sender, 210_099); // one wei short

        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &book,
            SelectionCaps::default(),
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert!(selected.is_empty(), "an unaffordable tx must not be sealed");
        assert_eq!(stats.rejected_insufficient_native, 1);
        assert_eq!(stats.selected_count, 0);
    }

    #[test]
    fn balance_gate_admits_exactly_what_the_sender_can_pay() {
        let tx = signed_native_tx(0, U256::from(100u64), 21_000, 10);
        let sender = tx.inner.signer();
        let pool = pool_of(vec![tx]);
        let book = book_with_native(sender, 210_100); // exactly enough

        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &book,
            SelectionCaps::default(),
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert_eq!(selected.len(), 1);
        assert_eq!(stats.rejected_insufficient_native, 0);
    }

    /// The gate is deliberately **per-tx against the current on-chain balance**, not cumulative:
    /// it never models "this block already spends X of this sender". Simulation
    /// (`eth_simulateV1`) is what catches a cumulative shortfall, and
    /// `revise_final_body_for_rejected` drops the failing tx and the rest of that sender's chain
    /// from the final body. This test therefore gives the second tx a *larger* value so it fails
    /// on its own terms.
    #[test]
    fn balance_gate_shortfall_latches_the_rest_of_the_sender_chain() {
        let tx0 = signed_native_tx(0, U256::from(100u64), 21_000, 10); // needs 210_100
        let tx1 = signed_native_tx(1, U256::from(500u64), 21_000, 10); // needs 210_500 — too much
        let tx2 = signed_native_tx(2, U256::from(100u64), 21_000, 10); // fine on its own, but
                                                                       // latched
        let sender = tx0.inner.signer();
        let pool = pool_of(vec![tx0, tx1, tx2]);
        let book = book_with_native(sender, 210_150); // covers tx0 only

        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &book,
            SelectionCaps::default(),
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].inner.nonce(), 0);
        assert_eq!(stats.rejected_insufficient_native, 1);
        assert_eq!(stats.dropped_after_sender_drop, 1);
    }

    #[test]
    fn erc20_gate_requires_the_token_balance_not_just_native() {
        let tx = signed_erc20_tx(0, 21_000, 10, &ERC20_TRANSFER, TOKEN, U256::from(7u64));
        let sender = tx.inner.signer();
        let pool = pool_of(vec![tx]);

        // Native is rich, token balance is short → must still drop.
        let mut book = BalanceBook::default();
        book.set_native(sender, U256::from(10u64).pow(U256::from(30u64)));
        book.set_erc20(TOKEN, sender, U256::from(6u64));

        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &book,
            SelectionCaps::default(),
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert!(selected.is_empty());
        assert_eq!(stats.rejected_insufficient_erc20, 1);
    }

    #[test]
    fn erc20_gate_admits_when_the_token_balance_covers_the_amount() {
        let tx = signed_erc20_tx(0, 21_000, 10, &ERC20_TRANSFER, TOKEN, U256::from(7u64));
        let sender = tx.inner.signer();
        let pool = pool_of(vec![tx]);

        let mut book = BalanceBook::default();
        book.set_native(sender, U256::from(10u64).pow(U256::from(30u64)));
        book.set_erc20(TOKEN, sender, U256::from(7u64));

        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &book,
            SelectionCaps::default(),
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert_eq!(selected.len(), 1);
        assert_eq!(stats.included_erc20, 1);
        assert_eq!(stats.rejected_insufficient_erc20, 0);
    }

    #[test]
    fn erc20_gate_drops_calldata_that_does_not_decode() {
        // Correct ERC-20 selector but no arguments — classified as ERC-20, amount unreadable.
        let mut input = ERC20_TRANSFER.to_vec();
        input.extend_from_slice(&[0u8; 32]); // one dummy word, amount missing
        let tx = signed_tx_with_input(0, Some(TOKEN), input, U256::ZERO, 21_000, 10);
        let sender = tx.inner.signer();
        let pool = pool_of(vec![tx]);

        let mut book = BalanceBook::default();
        book.set_native(sender, U256::from(10u64).pow(U256::from(30u64)));
        book.set_erc20(TOKEN, sender, U256::MAX);

        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &book,
            SelectionCaps::default(),
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert!(selected.is_empty(), "an undecodable erc-20 tx is never sealed");
        assert_eq!(stats.rejected_undecodable_erc20, 1);
    }

    /// `approve` legitimately approves more than the sender's current token balance
    /// (infinite-approval pattern) — no token-balance gate applies; only the native
    /// gate decides.
    #[test]
    fn erc20_gate_skips_token_balance_for_approve() {
        let tx = signed_erc20_tx(0, 21_000, 10, &ERC20_APPROVE, TOKEN, U256::MAX);
        let sender = tx.inner.signer();
        let pool = pool_of(vec![tx]);

        let mut book = BalanceBook::default();
        book.set_native(sender, U256::from(10u64).pow(U256::from(30u64)));
        book.set_erc20(TOKEN, sender, U256::ZERO); // no tokens at all — approve still passes

        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &book,
            SelectionCaps::default(),
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert_eq!(selected.len(), 1, "an over-balance approve must be admitted");
        assert_eq!(stats.included_erc20, 1);
        assert_eq!(stats.rejected_insufficient_erc20, 0);
    }

    /// `transferFrom` moves the `from` owner's tokens — the gate checks the owner's
    /// balance, not the caller's.
    #[test]
    fn erc20_gate_transfer_from_checks_the_owner_not_the_caller() {
        let owner = Address::repeat_byte(0x0c);
        let mut input = ERC20_TRANSFER_FROM.to_vec();
        let mut from_word = [0u8; 32];
        from_word[12..32].copy_from_slice(owner.as_slice());
        input.extend_from_slice(&from_word);
        input.extend_from_slice(&[0u8; 32]); // to (dummy)
        input.extend_from_slice(&U256::from(7u64).to_be_bytes::<32>()); // amount
        let tx = signed_tx_with_input(0, Some(TOKEN), input, U256::ZERO, 21_000, 10);
        let sender = tx.inner.signer();
        let pool = pool_of(vec![tx]);

        // Caller is token-poor; the owner is rich → must pass (the owner's tokens move).
        let mut book = BalanceBook::default();
        book.set_native(sender, U256::from(10u64).pow(U256::from(30u64)));
        book.set_erc20(TOKEN, sender, U256::ZERO);
        book.set_erc20(TOKEN, owner, U256::from(7u64));

        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &book,
            SelectionCaps::default(),
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert_eq!(selected.len(), 1, "owner-covered transferFrom must be admitted");
        assert_eq!(stats.included_erc20, 1);
        assert_eq!(stats.rejected_insufficient_erc20, 0);
    }

    /// `transferFrom` whose *owner* cannot cover the amount is still dropped.
    #[test]
    fn erc20_gate_transfer_from_drops_when_the_owner_is_short() {
        let owner = Address::repeat_byte(0x0c);
        let mut input = ERC20_TRANSFER_FROM.to_vec();
        let mut from_word = [0u8; 32];
        from_word[12..32].copy_from_slice(owner.as_slice());
        input.extend_from_slice(&from_word);
        input.extend_from_slice(&[0u8; 32]); // to (dummy)
        input.extend_from_slice(&U256::from(7u64).to_be_bytes::<32>()); // amount
        let tx = signed_tx_with_input(0, Some(TOKEN), input, U256::ZERO, 21_000, 10);
        let sender = tx.inner.signer();
        let pool = pool_of(vec![tx]);

        // Caller is rich in tokens, the owner is short → must drop (owner's tokens move).
        let mut book = BalanceBook::default();
        book.set_native(sender, U256::from(10u64).pow(U256::from(30u64)));
        book.set_erc20(TOKEN, sender, U256::MAX);
        book.set_erc20(TOKEN, owner, U256::from(6u64));

        let (selected, stats) = call_select_with(
            &pool,
            &all_zero_nonces(&pool),
            &book,
            SelectionCaps::default(),
            CHAIN_ID,
            NO_GAS_CAP,
        );

        assert!(selected.is_empty(), "owner-short transferFrom must be dropped");
        assert_eq!(stats.rejected_insufficient_erc20, 1);
    }
}
