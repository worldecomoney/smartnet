//! One-shot `SmartNet` block production (**pre-daemon** architecture testing).
//!
//! # Strategic bridge (normative)
//!
//! **`smartnet_newPayload` + RLP-encoded full block (`BlockRlp`)** is the **only** submission path
//! in `run-once`: the validator hands the node a **complete sealed block**; the node **validates
//! and may import**. There is **no** `wem_forkchoiceUpdated`, **no** fork-choice steering, and
//! **no** `payloadAttributes` / Engine orchestration in this flow.
//!
//! # Block-body / execution-roots invariant (normative)
//!
//! The block body and the execution that produces the block roots always describe the **exact
//! same transaction sequence**. Selection happens first (`[``crate::mempool_policy``]`): per-sender
//! executable nonce chains starting at the on-chain nonce, signature checks.
//! The result is the one immutable `final_body`.
//!
//! **`eth_simulateV1`** is used only as **preflight/verification**: it receives exactly
//! `final_body`, and if it reports a call that cannot execute, the body is **revised before root
//! derivation** (the rejected tx and later nonces of its sender are dropped, then re-simulated)
//! until the simulated roots describe exactly the body that will be sealed. The node's canonical
//! execution of the sealed body therefore reproduces the same `stateRoot` / `receiptsRoot` /
//! `logsBloom` / `gasUsed`. This is transitional: the long-term direction is a validator-embedded
//! local EVM + state provider, with `smartnet_newPayload` remaining the only submission path.
//!
//! **Observation RPC** (`eth_*`, `txpool_*`) for head / pool visibility remains acceptable
//! (informational only). **Validator-owned mempool policy** (nonce-chain gating,
//! ordering, signature checks, temporary “other” cap) lives in [`crate::mempool_policy`] — not the
//! node.
//!
//! # Canonical admission (no FCU)
//!
//! After **`smartnet_newPayload`** returns **`VALID`**, we **probe** `eth_getBlockByNumber(latest)`
//! until the head matches the imported block, for at most `CANONICAL_PROBE_TIMEOUT_MS` (config),
//! sleeping `CANONICAL_PROBE_INTERVAL_MS` between probes and doubling that delay up to
//! `CANONICAL_PROBE_MAX_INTERVAL_MS` (both config keys):
//! - Head hash **and** number match → log **`CANONICAL_ADVANCED_WITHOUT_FCU`** (expected normal
//!   path when native admission is wired) and report success.
//! - Deadline expires → **one diagnosis round classifies the outcome** (A5 · M21) instead of a
//!   blind exit 1, consulting `eth_getBlockByHash(imported)` and `eth_getBlockByNumber(number)`:
//!   - persisted and canonical at its height, head not promoted yet →
//!     **`IMPORT_CANONICAL_STALLED`**, `Idle`, **exit 0** (slow-but-healthy node is not a crash;
//!     the next cycle retries),
//!   - the node no longer has the block → **`IMPORT_LOST`**, failure, **exit 1**,
//!   - a different block occupies the height → **`IMPORT_SUPERSEDED`**, failure, **exit 1**.
//!     No silent FCU rescue: the classification is purely observational.

use crate::{
    balances::{observe_sender_state, SenderState},
    block_rlp_wire::warn_block_rlp_wire_envelope,
    io::ValidatorIo,
    keystore_util::KeystoreMaterial,
    mempool_policy::{
        fold_queued_into_candidates, log_mempool_selection, revise_final_body_for_rejected,
        select_pending_for_block, truncate_pool_to_max_txs,
    },
    outcome::{format_rpc_error, idle, is_node_unreachable, ValidatorStatus},
    payload_checkpoints::{log_post_seal, log_pre_seal, validate_final_payload},
};
use alloy_consensus::{proofs, Block, BlockHeader, Header, TxEnvelope};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rpc_types_eth::{
    simulate::{SimBlock, SimulatePayload, SimulatedBlock},
    BlockNumberOrTag, BlockOverrides, Transaction,
};
use alloy_rpc_types_txpool::TxpoolContent;
use calibri_lite_consensus::pre_consensus::{
    build_vanity_with_meta, calibri_lite_seal_hash,
    validate_smartnet_proposal, SmartNetProposalMeta, SmartNetRuntimeProfile,
};
use eyre::{Context, Result};
use smartnet_timing::{fmt_wall, fmt_wall_opt};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::time::sleep;
use tracing::{debug, info, warn};

const SEAL_SIGNATURE_LEN: usize = 65;

/// What one `run_once_inner` cycle produced, before mapping to [`ValidatorStatus`].
#[derive(Debug)]
enum InnerOutcome {
    /// A block was sealed, submitted and admitted as canonical.
    Sealed,
    /// Idle skip: empty txpool with `ALLOW_EMPTY` false.
    NothingToSeal,
    /// `smartnet_newPayload` said VALID, the block is persisted and canonical at its height,
    /// but the head was not promoted within `CANONICAL_PROBE_TIMEOUT_MS` — benign.
    CanonicalStalled { message: String },
}

/// Drop guard for the single-flight latch: clears `in_flight` on every exit — normal
/// return, early `?`, or panic unwind. The primitive must never stay latched.
struct PassLane<'a>(&'a AtomicBool);

impl Drop for PassLane<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub(crate) async fn run_once<I: ValidatorIo>(io: &I, args: &RunOnceArgs) -> ValidatorStatus {
    // Single-flight (fork prevention): exactly one pass may run per primitive. A kick
    // that arrives while a pass is in flight is coalesced here — inside the primitive —
    // so no caller (heartbeat, daemon loop, tests) can fork the seal by kicking twice.
    if args.in_flight.swap(true, Ordering::AcqRel) {
        return idle(
            "VAL_BUSY",
            "seal pass already in flight — this kick was coalesced".to_string(),
        );
    }
    let _lane = PassLane(&args.in_flight);

    let label = io.label().to_string();
    match run_once_inner(io, args).await {
        Ok(InnerOutcome::Sealed) => ValidatorStatus::Ok,
        Ok(InnerOutcome::NothingToSeal) => idle(
            "NOTHING_TO_SEAL",
            "txpool empty — no block sealed (set ALLOW_EMPTY = true in config to seal empty blocks)".to_string(),
        ),
        Ok(InnerOutcome::CanonicalStalled { message }) => idle("IMPORT_CANONICAL_STALLED", message),
        Err(e) if is_node_unreachable(&e) => {
            idle("NODE_UNREACHABLE", format_rpc_error(&label, "node I/O", &e))
        }
        Err(e) => ValidatorStatus::Failed {
            message: e.to_string(),
        },
    }
}

// No `Debug`: this struct holds the encrypted keystore + password buffers. The address is public;
// key bytes and password bytes must never be formatted into a log (PRIVATE-KEY.md).
pub(crate) struct RunOnceArgs {
    /// Validator identity anchor; logging and operator correlation.
    pub coinbase: Address,
    /// On-chain fee recipient (`header.beneficiary`).
    pub fee_receiver: Address,
    /// Encrypted keystore + password in RAM. No decrypted `SigningKey` is retained.
    pub key: KeystoreMaterial,
    /// If set, write final block RLP bytes here (for `smartnet-test-util` probes).
    pub dump_payload: Option<PathBuf>,
    /// Seal when txpool is empty (`ALLOW_EMPTY = true` in config).
    pub allow_empty: bool,
    /// Chain ID from config (`CHAINID`). Single source of truth.
    pub chain_id: u64,
    /// `CANONICAL_PROBE_TIMEOUT_MS` — how long a VALID import may take to become canonical
    /// before the outcome is classified (A5 · M21).
    pub canonical_probe_timeout_ms: u64,
    /// `CANONICAL_PROBE_INTERVAL_MS` — base delay between canonical probes; doubles up to
    /// `canonical_probe_max_interval_ms`.
    pub canonical_probe_interval_ms: u64,
    /// `CANONICAL_PROBE_MAX_INTERVAL_MS` — cap on the probe backoff (config, not code).
    pub canonical_probe_max_interval_ms: u64,
    /// Per-class block caps (`MAX_*_PER_BLOCK`, config with built-in defaults).
    pub selection_caps: crate::mempool_policy::SelectionCaps,
    /// Total txs one pass may process (`TXPOOL_VAL_MAX_TXS`, default 512): the pool
    /// snapshot is truncated to the oldest that many before the state prefetch and the
    /// selection both read it. Per-class narrowing (native / heavy / …) comes later.
    pub txpool_val_max_txs: usize,
    /// Equivocation-guard state shared with the daemon beat (XCURR D-2): the height of
    /// the last successfully submitted seal, `0` = none. Written the moment
    /// `smartnet_newPayload` is accepted on the wire — before response parsing — so
    /// every post-submission outcome (canonical, stalled, diagnosis error) is covered.
    /// The beat refuses to seal again while the canonical head hasn't caught up to it.
    pub sealed_height: Arc<AtomicU64>,
    /// Single-flight latch (fork prevention): one pass per primitive, ever. Set on entry
    /// to [`run_once`], cleared by a Drop guard on every exit — a second kick while a
    /// pass is in flight is coalesced (Idle `VAL_BUSY`), never queued and never run
    /// concurrently. Lives *inside* the primitive so the guarantee survives any caller
    /// (heartbeat loop, DAEMON2 daemon, tests) — the pass cannot fork itself.
    pub in_flight: Arc<AtomicBool>,
}

/// Derive the validator address from the signing key — centralized in
/// [`calibri_lite_consensus::secret_key_to_address`]; this is a local alias so the
/// sealing path reads uniformly.
pub use calibri_lite_consensus::secret_key_to_address as signer_address;

/// Current Unix time in whole seconds, for block timestamps.
///
/// Returns 0 if the clock is before the Unix epoch or otherwise unavailable; the caller
/// clamps against the parent timestamp, so a bad clock can never produce a block that
/// fails the `timestamp >= parent_timestamp` check.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Runtime sealed header: `extra_data = vanity || signature`. `SmartNet` reuses the
/// 65-byte ECDSA seal **wire layout** (genesis blocks are seal-compatible),
/// but post-genesis blocks are NOT seal consensus: no turn ordering, epoch checkpoint,
/// or recent-signer rule. Expected layout here is **32 + 65 = 97** bytes only (no embedded
/// signer list in `extra_data`; signer set lives in genesis only).
fn log_smartnet_proposal_extra_data(header: &Header, validator_set_len: usize) {
    let ed = header.extra_data.as_ref();
    let total = ed.len();
    if total < SEAL_SIGNATURE_LEN {
        warn!(
            target: "smartnet_validator",
            event = "CALIBRI_LITE_EXTRA_DATA_LAYOUT",
            extra_data_len = total,
            "extra_data shorter than seal (65 bytes)"
        );
        return;
    }
    let signature_len = SEAL_SIGNATURE_LEN;
    let prefix_len = total.saturating_sub(signature_len);
    // Geth-style: up to 32 bytes vanity, then optional vote/signer payload, then 65-byte seal.
    let vanity_len = prefix_len.min(32);
    let embedded_signer_or_vote_bytes = prefix_len.saturating_sub(32);
    let embedded_signer_address_slots = embedded_signer_or_vote_bytes / 20;
    let seal_hash = calibri_lite_seal_hash(header)
        .map(|h| format!("{h:#x}"))
        .unwrap_or_else(|e| format!("(calibri_lite_seal_hash error: {e})"));
    if total != 32 + SEAL_SIGNATURE_LEN || embedded_signer_or_vote_bytes != 0 {
        warn!(
            target: "smartnet_validator",
            event = "CALIBRI_LITE_EXTRA_DATA_NONSTANDARD",
            extra_data_len = total,
            embedded_after_32b_vanity_bytes = embedded_signer_or_vote_bytes,
            expected_extra_data_len = 32 + SEAL_SIGNATURE_LEN,
            "runtime extraData should be exactly 32-byte vanity + 65-byte seal (no signer list / vote payload between them)"
        );
    }
    debug!(
        target: "smartnet_validator",
        event = "CALIBRI_LITE_PROPOSAL_EXTRA_DATA_LAYOUT",
        extra_data_len = total,
        vanity_len,
        seal_signature_len = signature_len,
        prefix_before_signature_len = prefix_len,
        embedded_after_32b_vanity_bytes = embedded_signer_or_vote_bytes,
        embedded_address_slots_if_20b_aligned = embedded_signer_address_slots,
        validator_set_len = validator_set_len,
        difficulty = %header.difficulty,
        seal_hash,
        proposal_meta = ?SmartNetProposalMeta::decode_from_extra_data(&header.extra_data),
        "SmartNet proposal extraData (32-byte vanity with SmartNetProposalMeta + 65-byte seal)"
    );
}

fn sign_calibri_lite_header(mut header: Header, key: &KeystoreMaterial) -> Result<Header> {
    let seal_hash =
        calibri_lite_seal_hash(&header).map_err(|e| eyre::eyre!("calibri_lite seal hash: {e}"))?;
    let mut digest = [0u8; 32];
    digest.copy_from_slice(seal_hash.as_slice());

    // Decrypt only for this signature; the key is dropped when `with_key` returns.
    key.with_key(|secret| {
        let (sig, rec_id) = secret
            .sign_prehash_recoverable(&digest)
            .map_err(|_| eyre::eyre!("seal signature recovery failed"))?;
        let compact = sig.to_bytes();

        let mut extra = header.extra_data.to_vec();
        let sig_start = extra.len().saturating_sub(SEAL_SIGNATURE_LEN);
        if extra.len() < SEAL_SIGNATURE_LEN {
            eyre::bail!("header extra_data has no 65-byte seal: len {}", extra.len());
        }
        let tail = &mut extra[sig_start..];
        tail[..64].copy_from_slice(&compact);
        tail[64] = rec_id.to_byte();
        header.extra_data = extra.into();
        Ok(header)
    })?
}

/// Returns true if canonical `latest` reports `want_hash` at `want_number`.
async fn canonical_head_at<I: ValidatorIo>(
    io: &I,
    want_hash: B256,
    want_number: u64,
) -> Result<bool> {
    let Some(b) = io
        .block_by_number(BlockNumberOrTag::Latest)
        .await
        .wrap_err("latest head during canonical probe")?
    else {
        return Ok(false);
    };
    Ok(b.header.number == want_number && b.header.hash == want_hash)
}

/// Outcome of the post-import canonicalization diagnosis (A5 · M21).
///
/// The three conditions the old probe collapsed into one blind `bail!`:
/// * [`Self::Stalled`] — the node still has our block and it is still the canonical block at its
///   height; the head simply has not been promoted yet (slow-but-healthy → exit 0).
/// * [`Self::Lost`] — the node accepted VALID but no longer has the block (real failure).
/// * [`Self::Superseded`] — a competing block occupies the height (real failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CanonicalImport {
    Stalled,
    Lost,
    Superseded,
}

/// Pure diagnosis of a timed-out canonical probe — no I/O, so the mapping from RPC answers
/// to exit codes is unit-testable.
///
/// Inputs are the raw answers of one diagnosis round:
/// * `by_hash_number` — `eth_getBlockByHash(imported_hash)?.header.number`
/// * `canonical_hash_at_height` — `eth_getBlockByNumber(want_number)?.header.hash`
/// * `latest_number` — `eth_getBlockByNumber(latest)?.header.number`
fn classify_canonical_import(
    by_hash_number: Option<u64>,
    canonical_hash_at_height: Option<B256>,
    latest_number: Option<u64>,
    want_hash: B256,
    want_number: u64,
) -> CanonicalImport {
    // The node does not have our block at all, or answers with a mismatched height (a hash
    // determines its block — any other number means the answer is unusable).
    let Some(number) = by_hash_number else {
        return CanonicalImport::Lost;
    };
    if number != want_number {
        return CanonicalImport::Lost;
    }
    match canonical_hash_at_height {
        // Our block still holds its height on the canonical chain: only the head lags.
        Some(hash) if hash == want_hash => CanonicalImport::Stalled,
        // A different block occupies the height — a competing branch won.
        Some(_) => CanonicalImport::Superseded,
        // Canonical chain has no block at that height yet (head still below ours): that is
        // exactly the pending-promotion case, unless `latest` already claims to be past it
        // (inconsistent answers → conservative read: superseded).
        None => match latest_number {
            Some(latest) if latest >= want_number => CanonicalImport::Superseded,
            _ => CanonicalImport::Stalled,
        },
    }
}

/// Height of the block about to be proposed.
///
/// `checked`, never `+ 1`: a head at the `u64` maximum has no next height, and a release-mode
/// wrap would silently re-propose height 0 (a block every node already has).
fn next_block_number(head: u64) -> Result<u64> {
    head.checked_add(1).ok_or_else(|| {
        eyre::eyre!("block number {head} is the u64 maximum — there is no next height to propose")
    })
}

/// The hash the canonical probe watches after a `VALID` import.
///
/// `status == "VALID"` already says the node accepted *this* block, so `latestValidHash` has no
/// business naming a different head. The old code read it unconditionally (falling back to the
/// sealed hash only when the field was absent), so a node reporting someone else's hash sent
/// the probe chasing a block that was never imported: it waited out the full canonical timeout
/// and then misclassified an import the node had already accepted.
///
/// * field absent → the hash we sealed (there is nothing better to probe for);
/// * present and equal → that hash;
/// * present but a different hash, a non-string, or not a 32-byte hash → **error**, never a guessed
///   fallback.
fn imported_head_hash(payload: &serde_json::Value, sealed_hash: B256) -> Result<B256> {
    let Some(raw) = payload.get("latestValidHash") else {
        return Ok(sealed_hash);
    };
    let Some(text) = raw.as_str() else {
        eyre::bail!(
            "smartnet_newPayload latestValidHash is not a string: {raw} (payload: {payload})"
        );
    };
    let hash = text.parse::<B256>().map_err(|e| {
        eyre::eyre!("smartnet_newPayload latestValidHash is not a hash: {text}: {e}")
    })?;
    if hash != sealed_hash {
        eyre::bail!(
            "smartnet_newPayload latestValidHash {hash} != the sealed block hash {sealed_hash} \
             — the node reports VALID for a different head; refusing to probe for a hash it \
             never imported (payload: {payload})"
        );
    }
    Ok(hash)
}

/// `imported_hash` → the block's number, if the node still has it.
async fn import_block_number<I: ValidatorIo>(io: &I, hash: B256) -> Result<Option<u64>> {
    let block =
        io.block_by_hash(hash).await.wrap_err("block-by-hash during canonical diagnosis")?;
    Ok(block.map(|b| b.header.number))
}

/// Block `number` → the canonical hash at that height, if any.
async fn canonical_hash_at<I: ValidatorIo>(io: &I, number: u64) -> Result<Option<B256>> {
    let block = io
        .block_by_number(BlockNumberOrTag::Number(number))
        .await
        .wrap_err_with(|| format!("block-by-number({number}) during canonical diagnosis"))?;
    Ok(block.map(|b| b.header.hash))
}

/// Result of the canonical-head probe loop.
///
/// Separates the two things that make up post-confirmation time: how often the node was
/// asked (`probes`) and how much of the loop was pure backoff sleep (`slept`) — if a
/// VALID import "takes 500ms", the sleep field is what shows whether that was network
/// or the configured `CANONICAL_PROBE_INTERVAL_MS`.
struct CanonicalPoll {
    /// The head reached `want_hash` before the deadline.
    advanced: bool,
    /// `eth_getBlockByNumber(latest)` probes issued.
    probes: u32,
    /// Cumulative time spent in backoff `sleep` (excludes RPC round trips).
    slept: Duration,
}

/// Probe until `latest` matches the imported block or `timeout_ms` elapses.
///
/// The delay between probes starts at `interval_ms` and doubles up to `max_interval_ms`
/// (both from config — no hard-coded millisecond constant), so a slow node gets many cheap
/// early probes and still keeps checking near the deadline.
async fn poll_canonical<I: ValidatorIo>(
    io: &I,
    want_hash: B256,
    want_number: u64,
    timeout_ms: u64,
    interval_ms: u64,
    max_interval_ms: u64,
) -> Result<CanonicalPoll> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let base = Duration::from_millis(interval_ms);
    let mut interval = base;
    let mut probes = 0u32;
    let mut slept = Duration::ZERO;
    loop {
        probes += 1;
        if canonical_head_at(io, want_hash, want_number).await? {
            return Ok(CanonicalPoll { advanced: true, probes, slept });
        }
        if Instant::now() >= deadline {
            return Ok(CanonicalPoll { advanced: false, probes, slept });
        }
        sleep(interval).await;
        slept += interval;
        interval = next_probe_interval(interval, max_interval_ms);
    }
}

/// Backoff step: double the current delay, never past `max_interval_ms` (config key
/// `CANONICAL_PROBE_MAX_INTERVAL_MS`; the base delay is `CANONICAL_PROBE_INTERVAL_MS`).
fn next_probe_interval(current: Duration, max_interval_ms: u64) -> Duration {
    current.saturating_mul(2).min(Duration::from_millis(max_interval_ms))
}

// ── Seal-pass steps (HUGE-FN Tier-B split; DAEMON2 `seal_pass` sequencer) ────────
//
// One numbered step per phase. [`stage`] wraps each async step so a failure is logged
// with its step id (`VAL_STAGE_FAILED`) before bubbling — the pass never resumes
// mid-way against stale state; the next kick starts fresh at step 10.

/// Runs one numbered seal step; on failure logs `VAL_STAGE_FAILED` with the step id
/// and bubbles the error unchanged.
async fn stage<T>(
    step: &'static str,
    fut: impl core::future::Future<Output = Result<T>>,
) -> Result<T> {
    match fut.await {
        Ok(value) => Ok(value),
        Err(err) => {
            tracing::error!(
                target: "smartnet_validator",
                event = "VAL_STAGE_FAILED",
                step,
                %err,
                "seal pass failed at step"
            );
            Err(err)
        }
    }
}

/// Seal step 10 — prepare: chain-id cross-check, head + genesis fetch, signer-set gate.
async fn val_seq_10_prepare<I: ValidatorIo>(
    io: &I,
    args: &RunOnceArgs,
) -> Result<(alloy_rpc_types_eth::Block<Transaction>, Vec<Address>, Address)> {
    let chain_id = args.chain_id;

    // Chain-id cross-check (4.4). The node is the authority for the chain id (it reads it
    // from the database, written at `smartnet init`); the validator only has its own
    // config. Refuse to seal for a node on a different chain — a mismatch would otherwise
    // produce blocks the node rejects (EIP-155 replay protection + seal/state-root checks).
    let node_chain_id = io.chain_id().await?;
    if node_chain_id != chain_id {
        eyre::bail!(
            "node chain id {node_chain_id} != validator config CHAINID {chain_id}: \
             the validator is attached to a different chain"
        );
    }

    let latest = io
        .block_by_number(BlockNumberOrTag::Latest)
        .await
        .wrap_err("latest head")?
        .ok_or_else(|| eyre::eyre!("no latest block from node"))?;

    // The authorized set is node-owned registry data, returned through `ValidatorIo`.
    // The validator never opens `validators.jsonl`. The genesis `extraData` list is LEGACY
    // and is not consulted at all. Fail closed: an empty registry means no validator.
    let signers: Vec<Address> = io.enabled_validators();
    if signers.is_empty() {
        eyre::bail!(
            "validators.jsonl holds no `status: true` validator (fail closed) — the \
             genesis signer list is legacy and is not used"
        );
    }

    let signer_addr = args.key.address();
    if !signers.contains(&signer_addr) {
        eyre::bail!("secret key address {signer_addr} is not `status: true` in validators.jsonl");
    }

    Ok((latest, signers, signer_addr))
}

/// Seal step 20 — fetch the txpool snapshot, fold queued candidates, cap to
/// `TXPOOL_VAL_MAX_TXS`.
async fn val_seq_20_fetch_txpool<I: ValidatorIo>(
    io: &I,
    args: &RunOnceArgs,
) -> Result<(TxpoolContent<Transaction>, BTreeMap<B256, Instant>, serde_json::Value)> {
    let node_label = io.label();
    let txpool_start = Instant::now();
    let mut txpool: TxpoolContent<Transaction> =
        io.txpool_content().await.wrap_err("txpool content")?;
    let txpool_arrival: BTreeMap<B256, Instant> =
        io.txpool_arrival().await.wrap_err("txpool arrival")?;

    let txpool_status: serde_json::Value = io.txpool_status().await.wrap_err("txpool status")?;

    let pending_total = txpool.pending.values().map(BTreeMap::len).sum::<usize>();
    debug!(
        target: "smartnet_validator",
        event = "TXPOOL_FETCHED",
        txs = pending_total,
        node = node_label,
        wall = %fmt_wall(txpool_start.elapsed()),
        "fetched {pending_total} txs from the txpool at {node_label} in {}",
        fmt_wall(txpool_start.elapsed())
    );

    let queued_total = txpool.queued.values().map(BTreeMap::len).sum::<usize>();
    if pending_total + queued_total > 0 {
        info!(
            target: "smartnet_validator",
            event = "TXPOOL_TOTAL",
            pending = pending_total,
            queued = queued_total,
            total = pending_total + queued_total,
            "txpool total={}",
            pending_total + queued_total
        );
    }

    // Fold the pool's `queued` bucket into the seal candidates. The subpool label is a hint,
    // not a verdict: a tx can be parked `Queued` because the pool cached a state nonce one
    // behind the true head at insert time, and only a committed block promotes it — so a
    // sealer that reads only `pending` can deadlock on a misclassified contiguous run
    // (`MPP-ISSUE.md`). Every queued reason is re-checked downstream against canonical state
    // by the nonce gate, the balance gate, and `eth_simulateV1`, so folding is safe.
    let folded_queued = fold_queued_into_candidates(&mut txpool);
    let candidates_total = txpool.pending.values().map(BTreeMap::len).sum::<usize>();
    if folded_queued > 0 {
        info!(
            target: "smartnet_validator",
            event = "TXPOOL_QUEUED_FOLDED",
            node = node_label,
            folded = folded_queued,
            pending_before = pending_total,
            queued_before = queued_total,
            candidates = candidates_total,
            "folded {folded_queued} queued txs into the seal candidates (pool label is advisory; canonical nonce/balance/simulate gates decide executability)"
        );
    }

    // `TXPOOL_VAL_MAX_TXS` — bound what this pass processes: keep the oldest N candidate
    // txs (pool arrival) and drop the rest from the snapshot. Applied once, at the source,
    // so the state prefetch below and the selection see the same capped set — a huge pool
    // can never fan out more work than the budget. Oldest-first is exactly what the FIFO
    // rule wants under pressure; the dropped txs stay in the node's pool for a later block.
    let val_dropped =
        truncate_pool_to_max_txs(&mut txpool, &txpool_arrival, args.txpool_val_max_txs);
    if val_dropped > 0 {
        debug!(
            target: "smartnet_validator",
            event = "TXPOOL_VAL_CAPPED",
            node = node_label,
            pool_pending = candidates_total,
            max = args.txpool_val_max_txs,
            dropped = val_dropped,
            "TXPOOL_VAL_MAX_TXS = {}: kept the {} oldest pending txs, dropped {} newest",
            args.txpool_val_max_txs,
            candidates_total - val_dropped,
            val_dropped,
        );
    }

    Ok((txpool, txpool_arrival, txpool_status))
}

/// Seal step 30 — observe sender state (nonces + balances) from the head state via the
/// single state seam.
async fn val_seq_30_observe_state<I: ValidatorIo>(
    io: &I,
    txpool: &TxpoolContent<Transaction>,
) -> Result<SenderState> {
    let node_label = io.label();
    // On-chain executable nonce per sender, from the head state. The validator owns the
    // nonce-chain gate: it never builds [nonce 5, nonce 7] because nonce 6 is missing.
    //
    // Nonces *and* balances come from the single state seam — `balances::observe_sender_state`,
    // the one call the run loop makes against sender state. Its per-sender fan-out, batching and
    // ceiling live inside that seam, and its cost is accounted there as its own bucket
    // (`STATE_CHECKED` with its own `wall` — never mixed with this seal or import walls). When
    // the validator moves in-node, only the seam's insides change.
    let SenderState { nonces: on_chain_nonces, balances } =
        observe_sender_state(io, txpool, node_label).await?;

    Ok(SenderState { nonces: on_chain_nonces, balances })
}

/// Seal step 40 — plan the child header fields from the parent via the `SmartNet`
/// runtime profile (real wall-clock timestamp, parent-copied gas limit).
fn val_seq_40_plan_header(
    latest: &alloy_rpc_types_eth::Block<Transaction>,
    chain_id: u64,
) -> Result<(u64, u64, SmartNetRuntimeProfile, calibri_lite_consensus::pre_consensus::ChildHeaderFields)> {
    let parent_ts = latest.header.inner.timestamp;
    // Real wall-clock timestamp — NOT parent+1. Tier-0 validation is
    // `timestamp >= parent_timestamp` (calibri-lite timestamp.rs), so equal timestamps are
    // legal and two blocks closed inside the same second both carry the true time.
    // `.max(parent_ts)` only matters when the local clock lags the parent: a timestamp below
    // the parent would be rejected outright, so clamp instead of minting parent+1.
    let next_ts = now_unix_secs().max(parent_ts);
    let next_number = next_block_number(latest.header.number)?;

    let runtime = SmartNetRuntimeProfile::for_sealed_chain(chain_id);
    runtime.log_active();

    let parent_header = &latest.header.inner;
    let child_hdr = runtime.child_header_from_parent(parent_header);

    debug!(
        target: "smartnet_validator",
        event = "CALIBRI_LITE_HEADER_POLICY",
        parent_number = latest.header.number,
        next_number,
        parent_gas_limit = parent_header.gas_limit,
        next_gas_limit = child_hdr.gas_limit,
        parent_base_fee = ?parent_header.base_fee_per_gas,
        next_base_fee = child_hdr.base_fee_per_gas,
        wire_difficulty = %child_hdr.difficulty,
        "header fields from parent via SmartNetRuntimeProfile (uniform for all block heights)"
    );

    Ok((next_ts, next_number, runtime, child_hdr))
}

/// Seal step 50 — mempool selection against the observed sender state. `None` = nothing
/// to seal (empty selection with `ALLOW_EMPTY` false).
#[allow(clippy::too_many_arguments)] // the pass's selection inputs, kept explicit
fn val_seq_50_select<I: ValidatorIo>(
    _io: &I,
    args: &RunOnceArgs,
    txpool: &TxpoolContent<Transaction>,
    txpool_arrival: &BTreeMap<B256, Instant>,
    sender_state: &SenderState,
    child_hdr: &calibri_lite_consensus::pre_consensus::ChildHeaderFields,
    latest: &alloy_rpc_types_eth::Block<Transaction>,
    txpool_status: &serde_json::Value,
) -> Result<Option<Vec<Transaction>>> {
    let chain_id = args.chain_id;
    let SenderState { nonces: on_chain_nonces, balances } = sender_state;
    // Block gas limit for selection: the child header's limit (SmartNet copies the parent's,
    // constant for the chain) — never a hard-coded constant in the policy itself.
    let (pending_txs, mempool_stats) = select_pending_for_block(
        txpool,
        on_chain_nonces,
        balances,
        args.selection_caps,
        args.chain_id,
        child_hdr.gas_limit,
        txpool_arrival,
    );
    log_mempool_selection("MEMPOOL_POLICY", &mempool_stats);

    if pending_txs.is_empty() && !args.allow_empty {
        debug!(
            target: "smartnet_validator",
            event = "OBSERVE",
            chain_id,
            validator_coinbase = %args.coinbase,
            head = %latest.header.hash,
            number = latest.header.number,
            pending_total = mempool_stats.pending_total,
            block_gas_limit = child_hdr.gas_limit,
            selected_gas_limit_sum = mempool_stats.selected_gas_limit_sum,
            ?txpool_status,
            "txpool empty — skipping seal"
        );
        return Ok(None);
    }

    // THE final block body. After this point it is the only transaction list: it is never rebuilt
    // from the pool, and it is used verbatim for simulation, execution-derived roots and the sealed
    // block body. `eth_simulateV1` input and `block.body.transactions` are always the same set.
    let final_body: Vec<Transaction> = pending_txs;

    debug!(
        target: "smartnet_validator",
        event = "OBSERVE",
        chain_id,
        validator_coinbase = %args.coinbase,
        fee_receiver = %args.fee_receiver,
        head = %latest.header.hash,
        number = latest.header.number,
        pending_txs = final_body.len(),
        candidate_count = mempool_stats.candidate_count,
        pending_total = mempool_stats.pending_total,
        block_gas_limit = child_hdr.gas_limit,
        selected_gas_limit_sum = mempool_stats.selected_gas_limit_sum,
        ?txpool_status,
        "fetched head and txpool (validator-owned nonce-chain selection; no FCU in run-once)"
    );
    Ok(Some(final_body))
}

/// Seal step 60 — simulate the final body (`eth_simulateV1`) with revision until the
/// simulated roots describe exactly the body that will be sealed. `None` = the revision
/// loop emptied the body (nothing to seal).
async fn val_seq_60_simulate<I: ValidatorIo>(
    io: &I,
    args: &RunOnceArgs,
    final_body: &mut Vec<Transaction>,
    overrides: &BlockOverrides,
) -> Result<Option<alloy_rpc_types_eth::Block<Transaction>>> {
    /// Simulates exactly `body` against the head state with `SmartNet` block overrides.
    fn sim_payload_for(body: &[Transaction], overrides: &BlockOverrides) -> SimulatePayload {
        let mut sim_block = SimBlock::default().with_block_overrides(overrides.clone());
        for tx in body {
            sim_block = sim_block.call(tx.clone().into());
        }
        let mut payload = SimulatePayload::default().with_full_transactions().extend(sim_block);
        if !body.is_empty() {
            payload = payload.with_validation();
        }
        payload
    }

    // Preflight/revision loop: `eth_simulateV1` receives exactly `final_body` and returns the
    // execution-derived roots. If it reports a call that cannot execute, the final body is revised
    // (the rejected tx and any later nonce of the same sender are dropped) and re-simulated, until
    // the simulated roots describe exactly the body that will be sealed. Roots are taken only from
    // the final, converged simulation — never from a different transaction list.
    let rpc_block = loop {
        if final_body.is_empty() && !args.allow_empty {
            return Ok(None);
        }
        let sim: Vec<SimulatedBlock<alloy_rpc_types_eth::Block<Transaction>>> = io
            .simulate_v1(sim_payload_for(final_body, overrides))
            .await
            .wrap_err("simulate preflight over the final block body")?;

        let sim_block_out =
            sim.into_iter().next().ok_or_else(|| eyre::eyre!("empty eth_simulateV1"))?;

        let rejected: Vec<usize> = sim_block_out
            .calls
            .iter()
            .enumerate()
            .filter(|(_, call)| !call.status)
            .map(|(i, _)| i)
            .collect();

        if rejected.is_empty() {
            break sim_block_out.inner;
        }

        let revised = revise_final_body_for_rejected(final_body, &rejected);
        warn!(
            target: "smartnet_validator",
            rejected = rejected.len(),
            previous_len = final_body.len(),
            revised_len = revised.len(),
            "eth_simulateV1 reported a non-executable tx — revising final block body before root derivation"
        );
        if revised.len() == final_body.len() {
            eyre::bail!("simulation revision made no progress; refusing to seal");
        }
        *final_body = revised;
    };

    debug!(
        target: "smartnet_validator",
        event = "POST_SIMULATE_HEADER_ROOTS",
        state_root = %rpc_block.header.inner.state_root(),
        simulate_transactions_root = %rpc_block.header.inner.transactions_root,
        receipts_root = %rpc_block.header.inner.receipts_root,
        final_tx_count = final_body.len(),
        "state/receipts roots from eth_simulateV1 over the final body (state_root must be non-zero before seal); simulate_transactions_root is the node's trie over its rebuilt zero-signature call txs — the sealed transactions_root is recomputed below from the signed EIP-2718 bytes (BLOCK_BODY_FROM_SELECTED_FINAL)"
    );

    if rpc_block.header.inner.state_root() == B256::ZERO {
        eyre::bail!(
            "eth_simulateV1 returned zero state_root — refusing to seal a block with no execution state"
        );
    }

    Ok(Some(rpc_block))
}

/// Seal step 70 — build, validate, and sign the block from the converged simulation
/// (sync: no I/O). Returns the sealed block and its wire RLP.
#[allow(clippy::too_many_arguments)] // the pass's seal inputs, kept explicit
fn val_seq_70_seal(
    args: &RunOnceArgs,
    latest: &alloy_rpc_types_eth::Block<Transaction>,
    next_number: u64,
    next_ts: u64,
    runtime: &SmartNetRuntimeProfile,
    child_hdr: &calibri_lite_consensus::pre_consensus::ChildHeaderFields,
    signers: &[Address],
    signer_addr: Address,
    rpc_block: alloy_rpc_types_eth::Block<Transaction>,
    final_body: Vec<Transaction>,
) -> Result<(Block<TxEnvelope>, Vec<u8>)> {
    let parent_header = &latest.header.inner;
    let final_body = final_body;
    // `into_consensus()` yields `Block<alloy_rpc_types_eth::Transaction>`; that wrapper's
    // `Encodable` is **not** the execution block envelope (it flattens like a header). Strip to
    // `Block<alloy_consensus::TxEnvelope>` so RLP matches `debug_getRawBlock` /
    // `smartnet_ethereum_primitives::Block`.
    let mut block = rpc_block.into_consensus().map_transactions(Transaction::into_inner);

    // The block body is exactly `final_body` — the same list the converged simulation executed.
    // No second independently constructed transaction list exists between selection and sealing.
    if !final_body.is_empty() {
        let final_body_env: Vec<TxEnvelope> =
            final_body.iter().map(|tx| tx.clone().into_inner()).collect();
        let encoded: Vec<Bytes> =
            final_body_env.iter().map(|tx| Bytes::from(tx.encoded_2718())).collect();
        block.body.transactions = final_body_env;
        block.header.transactions_root = proofs::ordered_trie_root_encoded(&encoded);
        debug!(
            target: "smartnet_validator",
            event = "BLOCK_BODY_FROM_SELECTED_FINAL",
            tx_count = block.body.transactions.len(),
            transactions_root = %block.header.transactions_root,
            "block body = final selected txs (identical to eth_simulateV1 input) — preserves EIP-2718 bytes for execution"
        );
    }

    let mut header = std::mem::take(&mut block.header);
    let sim_gas_limit = header.gas_limit;
    header.parent_hash = latest.header.hash;
    header.number = next_number;
    header.timestamp = next_ts;
    header.beneficiary = args.fee_receiver;
    header.gas_limit = child_hdr.gas_limit;
    if sim_gas_limit != child_hdr.gas_limit {
        warn!(
            target: "smartnet_validator",
            event = "HEADER_GAS_LIMIT_OVERRIDE",
            sim_gas_limit,
            next_gas_limit = child_hdr.gas_limit,
            parent_gas_limit = parent_header.gas_limit,
            "eth_simulateV1 gasLimit ignored — child limit from SmartNetRuntimeProfile"
        );
    }

    header.difficulty = child_hdr.difficulty;

    let vanity = build_vanity_with_meta(&SmartNetProposalMeta::normal_v1());
    let mut extra = vanity.to_vec();
    extra.extend_from_slice(&[0u8; SEAL_SIGNATURE_LEN]);
    header.extra_data = extra.into();

    // Wallet-visible London fields only; consensus transition rules disabled on node (SmartNet
    // profile).
    header.base_fee_per_gas = Some(child_hdr.base_fee_per_gas);
    header.withdrawals_root = None;
    header.blob_gas_used = None;
    header.excess_blob_gas = None;
    header.parent_beacon_block_root = None;
    header.requests_hash = None;
    header.block_access_list_hash = None;
    header.slot_number = None;

    let seal_start = Instant::now();
    validate_smartnet_proposal(runtime, &header, signer_addr, signers)
        .map_err(|e| eyre::eyre!("SmartNet proposal: {e}"))?;

    let pre_seal_hash = log_pre_seal(&header)?;

    header =
        sign_calibri_lite_header(header, &args.key).map_err(|e| eyre::eyre!("seal sign: {e}"))?;

    let (post_seal_hash, recovered) = log_post_seal(&header, signer_addr)
        .map_err(|e| eyre::eyre!("post-seal checkpoint: {e}"))?;

    log_smartnet_proposal_extra_data(&header, signers.len());

    let body = std::mem::take(&mut block.body);
    // Full block for `wem_newPayload` `BlockRlp`:
    // `alloy_consensus::Block<alloy_consensus::TxEnvelope>`
    // after `map_transactions(Transaction::into_inner)` — same wire family as the node's block RLP.
    let sealed = Block::new(header, body);

    let mut rlp = Vec::new();
    alloy_rlp::Encodable::encode(&sealed, &mut rlp);
    warn_block_rlp_wire_envelope(&rlp);

    validate_final_payload(&sealed, &rlp, pre_seal_hash, post_seal_hash, recovered)
        .wrap_err("final payload hash checkpoints / RLP roundtrip")?;

    let sealed_tx_count = sealed.body.transactions.len();
    info!(
        target: "smartnet_validator",
        event = "PROPOSAL_SEALED",
        txs = sealed_tx_count,
        block_hash = %sealed.header.hash_slow(),
        block_number = sealed.header.number,
        wall = %fmt_wall(seal_start.elapsed()),
        "proposed block with {sealed_tx_count} txs validated and signed in {}",
        fmt_wall(seal_start.elapsed())
    );

    if let Some(path) = &args.dump_payload {
        std::fs::write(path, &rlp)
            .wrap_err_with(|| format!("write --dump-payload {}", path.display()))?;
        info!(
            target: "smartnet_validator",
            event = "DUMP_PAYLOAD",
            path = %path.display(),
            rlp_bytes = rlp.len(),
            block_hash = %sealed.header.hash_slow(),
            "wrote final sealed block RLP (use as smartnet-test-util N.dat)"
        );
    }

    Ok((sealed, rlp))
}

/// Seal step 80 — submit the sealed RLP (`smartnet_newPayload`), record the sealed
/// height for the equivocation guard (XCURR D-2), and parse the imported hash.
async fn val_seq_80_submit<I: ValidatorIo>(
    io: &I,
    args: &RunOnceArgs,
    sealed: &Block<TxEnvelope>,
    rlp: Vec<u8>,
    next_number: u64,
) -> Result<(serde_json::Value, B256)> {
    let node_label = io.label();
    let sealed_tx_count = sealed.body.transactions.len();
    let request_start = Instant::now();
    let new_payload: serde_json::Value = io
        .submit_block_rlp(Bytes::from(rlp))
        .await
        .wrap_err("submit sealed block (sole submission path in run-once)")?;
    // Equivocation guard (XCURR D-2): the seal is on the wire — record its height for the
    // daemon beat *now*, before response parsing, so a stall/diagnosis-error exit still
    // marks the height as ours. The beat will not seal again until the head catches up.
    args.sealed_height.store(next_number, Ordering::Release);
    // Server-reported timing breakdown, when the transport provides one (the merged in-process
    // path returns the engine's own timings next to the payload status).
    let server_us = |key: &str| new_payload.get(key).and_then(|v| v.as_u64());
    debug!(
        target: "smartnet_validator",
        event = "NEW_PAYLOAD_RTT",
        rtt_wall = %fmt_wall(request_start.elapsed()),
        server_latency_wall = %fmt_wall_opt(server_us("latency_us").map(Duration::from_micros)),
        server_queue_wall = %fmt_wall_opt(server_us("queue_wait_us").map(Duration::from_micros)),
        server_persistence_wall = %fmt_wall_opt(server_us("persistence_wait_us").map(Duration::from_micros)),
        "block submission (client wall vs server breakdown)"
    );

    // `SmartNetPayloadStatus` JSON: `PayloadStatus` is `#[serde(flatten)]` so `status` /
    // `latestValidHash` / `validationError` sit at the **top level** next to `latency_us`, etc.
    // — not `status.status`.
    let status = new_payload.get("status").and_then(|v| v.as_str()).ok_or_else(|| {
        eyre::eyre!(
            "block submit: missing top-level string field `status`; response was: {new_payload}"
        )
    })?;
    if status != "VALID" {
        let validation_error = new_payload
            .get("validationError")
            .and_then(|v| v.as_str())
            .unwrap_or("(no validationError)");
        eyre::bail!(
            "block submit status={status} validationError={validation_error} full={new_payload}"
        );
    }
    debug!(
        target: "smartnet_validator",
        event = "PROPOSAL_SENT",
        block_number = next_number,
        txs = sealed_tx_count,
        node = node_label,
        wall = %fmt_wall(request_start.elapsed()),
        "block proposal sent to {node_label} in {}",
        fmt_wall(request_start.elapsed())
    );

    let imported_hash = imported_head_hash(&new_payload, sealed.header.hash_slow())?;

    Ok((new_payload, imported_hash))
}

/// Seal step 90 — probe the canonical head for the imported block; on timeout, one
/// diagnosis round classifies the outcome (stalled / lost / superseded) instead of a
/// blind exit 1 (A5 · M21).
async fn val_seq_90_probe_and_diagnose<I: ValidatorIo>(
    io: &I,
    args: &RunOnceArgs,
    imported_hash: B256,
    next_number: u64,
) -> Result<InnerOutcome> {
    let probe_started = Instant::now();
    let poll = poll_canonical(
        io,
        imported_hash,
        next_number,
        args.canonical_probe_timeout_ms,
        args.canonical_probe_interval_ms,
        args.canonical_probe_max_interval_ms,
    )
    .await?;

    if poll.advanced {
        debug!(
            target: "smartnet_validator",
            event = "CANONICAL_ADVANCED_WITHOUT_FCU",
            imported_hash = %imported_hash,
            imported_number = next_number,
            probe_wall = %fmt_wall(probe_started.elapsed()),
            probes = poll.probes,
            probe_sleep_wall = %fmt_wall(poll.slept),
            "canonical head matches imported block after smartnet_newPayload (no forkchoice orchestration)"
        );
        return Ok(InnerOutcome::Sealed);
    }

    // Deadline expired: one diagnosis round classifies the outcome instead of a blind exit 1
    // (A5 · M21). Every message below carries imported_hash, the expected number, the current
    // head, elapsed time and the classification, so a post-mortem is possible from logs alone.
    // Deadline expired: one diagnosis round classifies the outcome instead of a blind exit 1
    // (A5 · M21). Every message below carries imported_hash, the expected number, the current
    // head, elapsed time and the classification, so a post-mortem is possible from logs alone.
    let by_hash_number = import_block_number(io, imported_hash).await?;
    let canonical_at_height = canonical_hash_at(io, next_number).await?;
    let still_latest = io
        .block_by_number(BlockNumberOrTag::Latest)
        .await
        .wrap_err("latest head during canonical diagnosis")?;
    let latest_hash = still_latest.as_ref().map(|b| b.header.hash).unwrap_or(B256::ZERO);
    let latest_number_opt = still_latest.as_ref().map(|b| b.header.number);
    let classification = classify_canonical_import(
        by_hash_number,
        canonical_at_height,
        latest_number_opt,
        imported_hash,
        next_number,
    );
    let latest_number = latest_number_opt.unwrap_or(0);
    let elapsed_ms = probe_started.elapsed().as_millis() as u64;
    let timeout_ms = args.canonical_probe_timeout_ms;

    match classification {
        CanonicalImport::Stalled => {
            // Benign: the node kept our block and it still holds its height; only the head
            // promotion is late. Exit 0 so cron/systemd do not raise a crash alarm, with a
            // distinct event so a strict operator can still alert on it.
            warn!(
                target: "smartnet_validator",
                event = "IMPORT_CANONICAL_STALLED",
                imported_hash = %imported_hash,
                expected_number = next_number,
                latest_hash = %latest_hash,
                latest_number,
                elapsed_ms,
                timeout_ms,
                "VALID import is persisted and canonical at its height but the head was not promoted within CANONICAL_PROBE_TIMEOUT_MS — benign, next cycle retries; no FCU applied"
            );
            Ok(InnerOutcome::CanonicalStalled {
                message: format!(
                    "IMPORT_CANONICAL_STALLED: imported_hash={imported_hash} number={next_number} \
                     is persisted and canonical at its height, but latest={latest_hash} \
                     number={latest_number} did not reach it within {elapsed_ms} ms \
                     (CANONICAL_PROBE_TIMEOUT_MS={timeout_ms}) — slow promotion on a healthy node, \
                     exit 0"
                ),
            })
        }
        CanonicalImport::Lost => {
            warn!(
                target: "smartnet_validator",
                event = "IMPORT_LOST",
                imported_hash = %imported_hash,
                expected_number = next_number,
                latest_hash = %latest_hash,
                latest_number,
                elapsed_ms,
                timeout_ms,
                "smartnet_newPayload reported VALID but the node no longer has the block — real failure, exit 1"
            );
            eyre::bail!(
                "IMPORT_LOST: smartnet_newPayload VALID but eth_getBlockByHash({imported_hash}) no \
                 longer returns the block (expected number={next_number}, latest={latest_hash} \
                 number={latest_number}) after {elapsed_ms} ms (CANONICAL_PROBE_TIMEOUT_MS={timeout_ms}) \
                 — classification=Lost, exit 1"
            );
        }
        CanonicalImport::Superseded => {
            warn!(
                target: "smartnet_validator",
                event = "IMPORT_SUPERSEDED",
                imported_hash = %imported_hash,
                expected_number = next_number,
                canonical_at_height = ?canonical_at_height,
                latest_hash = %latest_hash,
                latest_number,
                elapsed_ms,
                timeout_ms,
                "a competing block occupies the imported height — real failure, exit 1"
            );
            eyre::bail!(
                "IMPORT_SUPERSEDED: a competing block occupies height {next_number} \
                 (canonical_at_height={canonical_at_height:?}, imported_hash={imported_hash}, \
                 latest={latest_hash} number={latest_number}) after {elapsed_ms} ms \
                 (CANONICAL_PROBE_TIMEOUT_MS={timeout_ms}) — classification=Superseded, exit 1"
            );
        }
    }

}

/// One seal pass — the sequencer (HUGE-FN Tier-B split; the DAEMON2 `seal_pass`
/// primitive, still named for its one-shot CLI origin).
///
/// Steps run in order through [`stage`], which logs `VAL_STAGE_FAILED` with the step
/// id on error. A failed pass never resumes mid-way against stale state — the next
/// kick starts fresh at step 10 (pass-level retry; the no-fork layers — single-flight
/// latch, busy lane, sealed-height guard — live above this function and are untouched
/// by this split).
///
/// Outcomes: [`InnerOutcome::Sealed`] = sealed and canonical; [`InnerOutcome::NothingToSeal`]
/// = idle skip (empty txpool, `ALLOW_EMPTY` false); [`InnerOutcome::CanonicalStalled`] =
/// VALID import that did not get promoted to head within the configured timeout but is
/// still persisted.
async fn run_once_inner<I: ValidatorIo>(io: &I, args: &RunOnceArgs) -> Result<InnerOutcome> {
    let (latest, signers, signer_addr) =
        stage("val_seq_10", val_seq_10_prepare(io, args)).await?;
    let (txpool, txpool_arrival, txpool_status) =
        stage("val_seq_20", val_seq_20_fetch_txpool(io, args)).await?;
    let sender_state = stage("val_seq_30", val_seq_30_observe_state(io, &txpool)).await?;
    let (next_ts, next_number, runtime, child_hdr) =
        stage("val_seq_40", async { val_seq_40_plan_header(&latest, args.chain_id) }).await?;

    // The node owns the chair query too. With the exact `(parent_hash, timestamp)` this pass
    // will seal, an idle beat is the right answer when this signer is not the chair — the
    // node's seal gate would reject a finished block from anyone else.
    let chair = io.get_val_chair(latest.header.hash, next_ts, "");
    if chair != Some(signer_addr) {
        debug!(
            target: "smartnet_validator",
            signer = %signer_addr,
            ?chair,
            "not the chair for this slot — idle beat"
        );
        return Ok(InnerOutcome::NothingToSeal);
    }

    let final_body = stage("val_seq_50", async {
        val_seq_50_select(
            io,
            args,
            &txpool,
            &txpool_arrival,
            &sender_state,
            &child_hdr,
            &latest,
            &txpool_status,
        )
    })
    .await?;
    let Some(final_body) = final_body else {
        return Ok(InnerOutcome::NothingToSeal);
    };
    let overrides = BlockOverrides {
        number: Some(U256::from(next_number)),
        time: Some(next_ts),
        coinbase: Some(args.fee_receiver),
        difficulty: Some(child_hdr.difficulty),
        base_fee: Some(U256::from(child_hdr.base_fee_per_gas)),
        gas_limit: Some(child_hdr.gas_limit),
        ..Default::default()
    };
    let mut final_body = final_body;
    let rpc_block =
        stage("val_seq_60", val_seq_60_simulate(io, args, &mut final_body, &overrides)).await?;
    let Some(rpc_block) = rpc_block else {
        return Ok(InnerOutcome::NothingToSeal);
    };
    let (sealed, rlp) = stage("val_seq_70", async {
        val_seq_70_seal(
            args,
            &latest,
            next_number,
            next_ts,
            &runtime,
            &child_hdr,
            &signers,
            signer_addr,
            rpc_block,
            final_body,
        )
    })
    .await?;
    let (_new_payload, imported_hash) =
        stage("val_seq_80", val_seq_80_submit(io, args, &sealed, rlp, next_number)).await?;
    stage(
        "val_seq_90",
        val_seq_90_probe_and_diagnose(io, args, imported_hash, next_number),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use calibri_lite_consensus::pre_consensus::{
        build_vanity_with_meta, calibri_lite_seal_hash, recover_calibri_lite_signer,
        SmartNetProposalMeta,
    };
    use k256::ecdsa::SigningKey;

    fn secret(key_byte: u8) -> SigningKey {
        SigningKey::from_slice(&[key_byte; 32]).expect("valid secret")
    }

    fn proposal_header() -> Header {
        let mut extra = build_vanity_with_meta(&SmartNetProposalMeta::normal_v1()).to_vec();
        extra.extend_from_slice(&[0u8; SEAL_SIGNATURE_LEN]);
        Header { extra_data: extra.into(), number: 1, ..Default::default() }
    }

    #[test]
    fn seal_sign_and_recover_roundtrip() {
        let key = secret(7);
        let addr = signer_address(&key);

        let header = proposal_header();
        let pre_hash = calibri_lite_seal_hash(&header).unwrap();

        let material = crate::keystore_util::test_material(&key);
        let sealed = sign_calibri_lite_header(header, &material).unwrap();

        // Seal digest excludes the trailing 65-byte signature, so it must be unchanged.
        let post_hash = calibri_lite_seal_hash(&sealed).unwrap();
        assert_eq!(pre_hash, post_hash);

        let recovered = recover_calibri_lite_signer(&sealed).unwrap();
        assert_eq!(recovered, addr);
        assert_eq!(sealed.extra_data.len(), 32 + SEAL_SIGNATURE_LEN);
    }

    #[test]
    fn sign_calibri_lite_header_requires_seal_bytes() {
        let key = secret(7);
        let material = crate::keystore_util::test_material(&key);
        let header = Header { extra_data: vec![0u8; 31].into(), ..Header::default() };
        assert!(sign_calibri_lite_header(header, &material).is_err());
    }

    // ---- A5 · M21: canonical-probe classification ----

    const WANT_NUMBER: u64 = 42;
    fn want_hash() -> B256 {
        B256::repeat_byte(0x11)
    }
    fn other_hash() -> B256 {
        B256::repeat_byte(0x22)
    }

    /// Node no longer has the block → real failure (exit 1).
    #[test]
    fn classification_missing_block_is_lost() {
        let class = classify_canonical_import(None, None, Some(41), want_hash(), WANT_NUMBER);
        assert_eq!(class, CanonicalImport::Lost);
    }

    /// A hash determines its block: any answer with a different number is unusable → Lost.
    #[test]
    fn classification_height_mismatch_is_lost() {
        let class = classify_canonical_import(
            Some(7),
            Some(want_hash()),
            Some(41),
            want_hash(),
            WANT_NUMBER,
        );
        assert_eq!(class, CanonicalImport::Lost);
    }

    /// Persisted + canonical at its height, head below → slow promotion, benign (exit 0).
    #[test]
    fn classification_promotion_pending_is_stalled() {
        let class = classify_canonical_import(
            Some(WANT_NUMBER),
            Some(want_hash()),
            Some(WANT_NUMBER - 1),
            want_hash(),
            WANT_NUMBER,
        );
        assert_eq!(class, CanonicalImport::Stalled);
    }

    /// Persisted, canonical at its height, head not yet at the height at all (`latest` unknown).
    #[test]
    fn classification_unknown_latest_is_stalled() {
        let class = classify_canonical_import(
            Some(WANT_NUMBER),
            Some(want_hash()),
            None,
            want_hash(),
            WANT_NUMBER,
        );
        assert_eq!(class, CanonicalImport::Stalled);
    }

    /// A competing block occupies the height → real failure (exit 1).
    #[test]
    fn classification_competing_block_is_superseded() {
        let class = classify_canonical_import(
            Some(WANT_NUMBER),
            Some(other_hash()),
            Some(WANT_NUMBER),
            want_hash(),
            WANT_NUMBER,
        );
        assert_eq!(class, CanonicalImport::Superseded);
    }

    /// Head claims to be past the height while the canonical hash at that height is missing:
    /// inconsistent answers → conservative Superseded (exit 1).
    #[test]
    fn classification_inconsistent_answers_are_superseded() {
        let class = classify_canonical_import(
            Some(WANT_NUMBER),
            None,
            Some(WANT_NUMBER + 5),
            want_hash(),
            WANT_NUMBER,
        );
        assert_eq!(class, CanonicalImport::Superseded);
    }

    /// Head below our height with no competing block = the promotion-pending case itself.
    #[test]
    fn classification_head_below_height_is_stalled() {
        let class = classify_canonical_import(
            Some(WANT_NUMBER),
            None,
            Some(WANT_NUMBER - 1),
            want_hash(),
            WANT_NUMBER,
        );
        assert_eq!(class, CanonicalImport::Stalled);
    }

    /// Backoff doubles the configured interval and never exceeds `CANONICAL_PROBE_MAX_INTERVAL_MS`
    /// — both values come from config, there is no hard-coded cap.
    #[test]
    fn probe_backoff_doubles_and_caps_at_configured_max() {
        assert_eq!(
            next_probe_interval(Duration::from_millis(100), 400),
            Duration::from_millis(200)
        );
        assert_eq!(
            next_probe_interval(Duration::from_millis(200), 400),
            Duration::from_millis(400)
        );
        assert_eq!(
            next_probe_interval(Duration::from_millis(400), 400),
            Duration::from_millis(400),
            "must cap at the configured max, not grow unbounded"
        );
        assert_eq!(next_probe_interval(Duration::from_millis(10), 400), Duration::from_millis(20));
        assert_eq!(
            next_probe_interval(Duration::from_millis(10), 15),
            Duration::from_millis(15),
            "cap below the current delay clamps it (config rejects cap < interval anyway)"
        );
    }

    /// Exit-code map: a stalled (but persisted) import is a benign idle outcome → 0; a lost or
    /// superseded import reaches `ValidatorStatus::Failed` → 1.
    #[test]
    fn stalled_is_exit_zero_and_loss_is_exit_one() {
        let stalled = idle("IMPORT_CANONICAL_STALLED", "IMPORT_CANONICAL_STALLED: …");
        assert_eq!(stalled.finish(), 0);
        let lost = ValidatorStatus::Failed { message: "IMPORT_LOST: …".to_string() };
        assert_eq!(lost.finish(), 1);
        let superseded = ValidatorStatus::Failed { message: "IMPORT_SUPERSEDED: …".to_string() };
        assert_eq!(superseded.finish(), 1);
    }

    // ---- latestValidHash must name the block that was just sealed ----

    fn sealed_hash() -> B256 {
        B256::repeat_byte(0x5a)
    }

    /// `latestValidHash` in a `smartnet_newPayload`-shaped response body.
    fn hash_payload(hash: B256) -> serde_json::Value {
        serde_json::json!({ "status": "VALID", "latestValidHash": hash.to_string() })
    }

    /// No field at all → probe the hash we sealed (nothing better exists to watch).
    #[test]
    fn latest_valid_hash_absent_falls_back_to_the_sealed_hash() {
        let got = imported_head_hash(&serde_json::json!({"status":"VALID"}), sealed_hash())
            .expect("absent");
        assert_eq!(got, sealed_hash());
    }

    /// The node echoing our hash is the happy path.
    #[test]
    fn latest_valid_hash_equal_to_sealed_is_accepted() {
        let want = sealed_hash();
        let got = imported_head_hash(&hash_payload(want), want).expect("equal");
        assert_eq!(got, want);
    }

    /// The bug: a node answering VALID while naming someone else's head. Used to be trusted
    /// and sent the probe chasing a hash that was never imported — full timeout, wrong verdict.
    #[test]
    fn latest_valid_hash_of_a_different_head_is_refused() {
        let other = B256::repeat_byte(0x99);
        let err = imported_head_hash(&hash_payload(other), sealed_hash())
            .expect_err("a different head must be refused");
        let msg = err.to_string();
        assert!(msg.contains(&other.to_string()), "got: {msg}");
        assert!(msg.contains(&sealed_hash().to_string()), "got: {msg}");
        assert!(msg.contains("different head"), "got: {msg}");
    }

    /// A present-but-unreadable field is a protocol error, not a licence to guess.
    #[test]
    fn latest_valid_hash_unreadable_is_refused() {
        let malformed = serde_json::json!({ "latestValidHash": "0xzz" });
        let err = imported_head_hash(&malformed, sealed_hash()).expect_err("not a hash");
        assert!(err.to_string().contains("not a hash"), "got: {err}");
        let non_string = serde_json::json!({ "latestValidHash": 42 });
        let err = imported_head_hash(&non_string, sealed_hash()).expect_err("not a string");
        assert!(err.to_string().contains("not a string"), "got: {err}");
    }

    // ---- block-number increment is checked ----

    #[test]
    fn next_block_number_is_checked_not_wrapping() {
        assert_eq!(next_block_number(41).expect("ordinary height"), 42);
        assert_eq!(next_block_number(0).expect("genesis"), 1);
        let err = next_block_number(u64::MAX).expect_err("no height above u64::MAX");
        assert!(err.to_string().contains("u64 maximum"), "got: {err}");
    }

    // ---- in-process e2e: chain-check -> fetch -> select -> simulate -> seal -> submit -> probe
    // (FINDINGS #15). An in-process `ValidatorIo` plays the node, exactly as the merged node
    // will: `run_once_inner` calls it as a method — no HTTP, no URL, no JSON-RPC. The only
    // dynamic state is the sealed head recorded by `submit_block_rlp`, which the canonical
    // probe then serves back.

    use std::sync::Mutex;

    type RpcBlock = alloy_rpc_types_eth::Block<Transaction>;

    /// In-process fake node: the validator's I/O boundary, answered from memory.
    struct MockIo {
        chain_id: u64,
        pool: TxpoolContent<Transaction>,
        sealed: Mutex<Option<(B256, u64)>>,
        seen: Mutex<Vec<String>>,
    }

    impl MockIo {
        fn new(chain_id: u64, pool: TxpoolContent<Transaction>) -> Self {
            Self { chain_id, pool, sealed: Mutex::new(None), seen: Mutex::new(Vec::new()) }
        }

        fn pending_count(&self) -> usize {
            self.pool.pending.values().map(BTreeMap::len).sum()
        }

        fn record(&self, method: &str) {
            self.seen.lock().unwrap().push(method.to_string());
        }

        fn has(&self, method: &str) -> bool {
            self.seen.lock().unwrap().iter().any(|m| m == method)
        }

        fn signer(&self) -> Address {
            signer_address(&secret(7))
        }

        fn genesis(&self) -> (Header, B256) {
            genesis_block(self.signer())
        }

        /// The parent of the block we are about to seal, plus its hash.
        fn parent(&self) -> (Header, B256) {
            let (_, genesis_hash) = self.genesis();
            let h = fake_header(1, genesis_hash, sealed_genesis_extra(&[self.signer()]));
            let hash = h.hash_slow();
            (h, hash)
        }

        fn as_rpc_block(&self, h: &Header, hash: B256) -> Option<RpcBlock> {
            serde_json::from_value(block_json(h, hash)).ok()
        }
    }

    impl ValidatorIo for MockIo {
        fn label(&self) -> &str {
            "in-proc-mock"
        }

        async fn chain_id(&self) -> Result<u64> {
            self.record("eth_chainId");
            Ok(self.chain_id)
        }

        fn enabled_validators(&self) -> Vec<Address> {
            vec![signer_address(&secret(7)), signer_address(&secret(3))]
        }

        fn get_val_chair(&self, _block_hash: B256, _ts: u64, _purpose: &str) -> Option<Address> {
            Some(self.signer())
        }

        async fn block_by_number(&self, tag: BlockNumberOrTag) -> Result<Option<RpcBlock>> {
            self.record("eth_getBlockByNumber");
            let (genesis, genesis_hash) = self.genesis();
            let (parent, parent_hash) = self.parent();
            let (h, hash) = match tag {
                BlockNumberOrTag::Earliest => (genesis, genesis_hash),
                BlockNumberOrTag::Latest => match *self.sealed.lock().unwrap() {
                    Some((hash, number)) => (
                        fake_header(number, parent_hash, sealed_genesis_extra(&[self.signer()])),
                        hash,
                    ),
                    None => (parent, parent_hash),
                },
                BlockNumberOrTag::Number(number) => match *self.sealed.lock().unwrap() {
                    Some((hash, n)) if n == number => (
                        fake_header(number, parent_hash, sealed_genesis_extra(&[self.signer()])),
                        hash,
                    ),
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            };
            Ok(self.as_rpc_block(&h, hash))
        }

        async fn block_by_hash(&self, hash: B256) -> Result<Option<RpcBlock>> {
            self.record("eth_getBlockByHash");
            let (_, parent_hash) = self.parent();
            match *self.sealed.lock().unwrap() {
                Some((h, number)) if h == hash => {
                    let header =
                        fake_header(number, parent_hash, sealed_genesis_extra(&[self.signer()]));
                    Ok(self.as_rpc_block(&header, h))
                }
                _ => Ok(None),
            }
        }

        async fn txpool_content(&self) -> Result<TxpoolContent<Transaction>> {
            self.record("txpool_content");
            Ok(self.pool.clone())
        }

        async fn txpool_arrival(&self) -> Result<BTreeMap<B256, Instant>> {
            self.record("txpool_arrival");
            Ok(BTreeMap::new())
        }

        async fn txpool_status(&self) -> Result<serde_json::Value> {
            self.record("txpool_status");
            Ok(serde_json::json!({
                "pending": format!("{:#x}", self.pending_count()),
                "queued": "0x0"
            }))
        }

        async fn balances(&self, owners: &[Address]) -> Vec<Result<U256>> {
            self.record("eth_getBalance");
            (0..owners.len()).map(|_| Ok(U256::from(0xde0b6b3a7640000u64))).collect()
        }

        async fn erc20_balances(&self, pairs: &[(Address, Address)]) -> Vec<Result<U256>> {
            self.record("eth_call");
            (0..pairs.len()).map(|_| Ok(U256::ZERO)).collect()
        }

        async fn nonces(&self, owners: &[Address]) -> Vec<Result<u64>> {
            self.record("eth_getTransactionCount");
            (0..owners.len()).map(|_| Ok(0)).collect()
        }

        async fn simulate_v1(
            &self,
            _payload: SimulatePayload,
        ) -> Result<Vec<SimulatedBlock<RpcBlock>>> {
            self.record("eth_simulateV1");
            let (_, parent_hash) = self.parent();
            let mut block = fake_header(1, parent_hash, sealed_genesis_extra(&[self.signer()]));
            block.state_root = B256::repeat_byte(0x77);
            let mut out = block_json(&block, block.hash_slow());
            if let Some(obj) = out.as_object_mut() {
                let calls: Vec<serde_json::Value> = std::iter::repeat_n(serde_json::json!({
                    "status": "0x1",
                    "returnData": "0x",
                    "logs": [],
                    "gasUsed": "0x0"
                }), self.pending_count().max(1))
                .collect();
                obj.insert("calls".into(), serde_json::json!(calls));
            }
            let simulated: Vec<SimulatedBlock<RpcBlock>> =
                serde_json::from_value(serde_json::json!([out])).expect("decode simulated block");
            Ok(simulated)
        }

        async fn submit_block_rlp(&self, rlp: Bytes) -> Result<serde_json::Value> {
            self.record("smartnet_newPayload");
            let bytes = rlp.to_vec();
            let block: Block<TxEnvelope> = alloy_rlp::Decodable::decode(&mut bytes.as_slice())
                .expect("decode the submitted sealed block");
            let hash = block.header.hash_slow();
            let number = block.header.number;
            *self.sealed.lock().unwrap() = Some((hash, number));
            Ok(serde_json::json!({
                "status": "VALID",
                "latestValidHash": format!("{hash:#x}"),
                "validationError": null,
                "latency_us": 0,
                "persistence_wait_us": 0
            }))
        }
    }

    /// Genesis `extraData` with the given signer set: 32 vanity + N*20 addresses + 65 sig.
    fn sealed_genesis_extra(signers: &[Address]) -> Bytes {
        let mut extra = vec![0u8; 32];
        for a in signers {
            extra.extend_from_slice(a.as_slice());
        }
        extra.extend_from_slice(&[0u8; 65]);
        extra.into()
    }

    /// Minimal fake block header: only the fields that drive the flow are meaningful.
    fn fake_header(number: u64, parent: B256, extra: Bytes) -> Header {
        Header {
            parent_hash: parent,
            number,
            timestamp: 1_700_000_000u64 + number,
            gas_limit: 30_000_000u64,
            base_fee_per_gas: Some(
                calibri_lite_consensus::pre_consensus::SMARTNET_FIXED_GAS_PRICE_WEI,
            ),
            extra_data: extra,
            ..Default::default()
        }
    }

    /// RPC header JSON, serialized by the real alloy serializer (flat consensus + hash).
    fn header_json(h: &Header, hash: B256) -> serde_json::Value {
        // `Header::new` recomputes the hash from the inner header — which for
        // proto-headers (fabricated pre-image) may differ from the record we must serve;
        // override the hash so the fake node answers with the identity the flow recorded.
        let mut rpc_header = alloy_rpc_types_eth::Header::new(h.clone());
        rpc_header.hash = hash;
        serde_json::to_value(&rpc_header).expect("serialize rpc header")
    }

    /// Whole fake block: flattened header + empty txs/uncles + withdrawals null.
    fn block_json(h: &Header, hash: B256) -> serde_json::Value {
        let mut v = header_json(h, hash);
        if let Some(obj) = v.as_object_mut() {
            obj.insert("uncles".into(), serde_json::json!([]));
            obj.insert("transactions".into(), serde_json::json!([]));
            obj.insert("withdrawals".into(), serde_json::json!(null));
        }
        v
    }

    fn genesis_block(signer: Address) -> (Header, B256) {
        let h = fake_header(0, B256::ZERO, sealed_genesis_extra(&[signer]));
        let hash = h.hash_slow();
        (h, hash)
    }

    fn run_once_args(secret_key: SigningKey, allow_empty: bool) -> RunOnceArgs {
        RunOnceArgs {
            coinbase: Address::repeat_byte(0x01),
            fee_receiver: Address::repeat_byte(0x01),
            key: crate::keystore_util::test_material(&secret_key),
            dump_payload: None,
            allow_empty,
            chain_id: 315,
            canonical_probe_timeout_ms: 2_000,
            canonical_probe_interval_ms: 5,
            canonical_probe_max_interval_ms: 20,
            selection_caps: crate::mempool_policy::SelectionCaps::default(),
            txpool_val_max_txs: 512,
            sealed_height: Arc::new(AtomicU64::new(0)),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Happy path: one funded sender with a ready native transfer → selected → simulated
    /// (all calls executable) → sealed with a real 65-byte signature → submitted to the
    /// in-process node VALID → the canonical probe sees the very block → Sealed.
    ///
    /// Deliberately a plain `#[test]` driving its own current-thread runtime: the tx
    /// helpers start fresh runtimes of their own, which must not nest inside a
    /// `#[tokio::test]`.
    #[test]
    fn inproc_e2e_seals_and_admits_one_native_tx() {
        use crate::mempool_policy::tests::signed_native_tx;
        // A previous test in the same binary may have installed the process-level
        // CryptoProvider already; `try`-style install errors on that, which is fine.
        let _ = smartnet_tls_provider::install_default();
        let key = secret(7);
        let sender = Address::repeat_byte(0xaa);
        let tx = signed_native_tx(
            0,
            U256::from(100u64),
            21_000,
            calibri_lite_consensus::pre_consensus::SMARTNET_FIXED_GAS_PRICE_WEI as u128,
        );
        let mut pool = alloy_rpc_types_txpool::TxpoolContent::default();
        pool.pending.insert(sender, {
            let mut inner = std::collections::BTreeMap::new();
            inner.insert("0".to_string(), tx);
            inner
        });
        let io = MockIo::new(315, pool);
        let args = run_once_args(key, false);
        let out = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(run_once_inner(&io, &args))
            .expect("the in-process happy path must seal");
        assert!(matches!(out, InnerOutcome::Sealed), "got: {out:?}");
        // Equivocation guard (XCURR D-2): a successful submission records its height.
        assert!(
            args.sealed_height.load(std::sync::atomic::Ordering::Acquire) != 0,
            "the sealed height must be recorded on submission"
        );
        for m in [
            "eth_chainId",
            "txpool_content",
            "eth_getBalance",
            "eth_simulateV1",
            "smartnet_newPayload",
        ] {
            assert!(io.has(m), "flow must call {m}");
        }
        assert!(
            io.sealed.lock().unwrap().is_some(),
            "the submit must record a sealed head for the probe"
        );
    }

    /// Empty txpool with `ALLOW_EMPTY = false` → the idle skip, after the full
    /// pre-selection path (chain id, genesis signers, txpool fetch) exercised.
    #[test]
    fn inproc_e2e_empty_pool_is_nothing_to_seal() {
        let _ = smartnet_tls_provider::install_default();
        let io = MockIo::new(315, alloy_rpc_types_txpool::TxpoolContent::default());
        let args = run_once_args(secret(7), false);
        let out = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(run_once_inner(&io, &args))
            .expect("the in-process idle path must complete");
        assert!(matches!(out, InnerOutcome::NothingToSeal), "got: {out:?}");
        assert!(!io.has("smartnet_newPayload"), "idle must not submit");
        assert_eq!(
            args.sealed_height.load(std::sync::atomic::Ordering::Acquire),
            0,
            "an idle pass must not record a sealed height"
        );
        // The pass lane must be clear again — the primitive is reusable.
        assert!(
            !args.in_flight.load(std::sync::atomic::Ordering::Acquire),
            "the single-flight latch must clear on exit"
        );
    }

    /// Single-flight (fork prevention): a kick that arrives while a pass is in flight is
    /// coalesced **inside the primitive** — Idle `VAL_BUSY`, no work, no submit. The
    /// guarantee lives here (not only in the daemon's busy lane) so no caller — heartbeat
    /// loop, DAEMON2-style daemon, or tests — can fork the seal by kicking twice.
    #[test]
    fn in_flight_kick_is_coalesced_inside_the_primitive() {
        let _ = smartnet_tls_provider::install_default();
        let io = MockIo::new(315, alloy_rpc_types_txpool::TxpoolContent::default());
        let args = run_once_args(secret(3), false);
        args.in_flight.store(true, std::sync::atomic::Ordering::Release); // an active pass

        let status = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(run_once(&io, &args));
        match status {
            ValidatorStatus::Idle { event, .. } => {
                assert_eq!(event, "VAL_BUSY", "the second kick must coalesce")
            }
            other => panic!("expected the coalesced Idle(VAL_BUSY), got {other:?}"),
        }
        assert!(!io.has("smartnet_newPayload"), "a coalesced kick must never submit");
    }
}
