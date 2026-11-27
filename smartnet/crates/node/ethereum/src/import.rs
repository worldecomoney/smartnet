//! RLP import — direct to storage, in ranges (`doc/IMPORT.md` §1, operator 2026-11-18).
//!
//! The import does **not** go through the engine: no `newPayload` round-trip, no engine
//! queue, no cache-lock waits, no tree insert. Measured reason: an empty block spent
//! 13–15 ms of engine machinery around ~0 ms of work.
//!
//! Per [`IMPORT_RANGE`] blocks the run:
//!
//! 1. **reads** the next items from the file in place (read-only, [`READ_CAPACITY`] read-ahead) and
//!    decodes them;
//! 2. **positions** each one with [`classify`] — genesis and anything at or below the committed tip
//!    is skipped (resume), the number must be `prev + 1` and the parent hash must equal the
//!    previous block's, anything else stops the run;
//! 3. **checks and executes** the range against the state of the last committed block: exactly
//!    **two checks** (operator) — the chain position (step 2) and the validator gate: the seal's
//!    signer must be a `status: true` validator of the RAM registry (`validate_static_signer`
//!    against `enabled_set()`, the set `get_val_chair` picks from); execution is one EVM pass per
//!    block with the node's own config and the rtbc create-admission inspector, over a single
//!    `State` anchored at the committed block so an earlier block of the range is visible to its
//!    successors;
//! 4. **writes** the range through the storage engine's block-write protocol — file-tier append, DB
//!    state/index, **one commit** — with the range's trie updates computed once from the disk trie
//!    at the anchor (only the range's changed paths walk) and attached before the commit;
//! 5. **reports** `done N blocks` for the finished range and one `import progress` line per
//!    [`smartnet_tracing::REPORT_INTERVAL`] — always the committed tip (`last_block_number()`),
//!    never an intent.
//!
//! The file is READ ONLY. On ctrl-c/SIGTERM the run stops at the next range boundary —
//! everything so far is already committed — writes `SHUTDOWN_CLEAN` and returns, so a
//! re-run resumes from the durable tip. No fork detection: every block follows the same
//! decode → check → execute → commit path, whatever its era.

use alloy_eips::Typed2718;
use alloy_primitives::B256;
use alloy_rlp::Decodable;
use smartnet_chain_state::{ComputedTrieData, DeferredTrieData, ExecutedBlock};
use smartnet_ethereum_primitives::{calculate_receipt_root_no_memo, Receipt};
use smartnet_evm::{
    execute::{BlockExecutionOutput, BlockExecutor},
    rtbc_inspector::rtbc,
    ConfigureEvm, Evm as _,
};
use smartnet_file_tier::{read_block_at, SignatureAccess};
use smartnet_primitives_traits::{
    proofs::calculate_transaction_root, NodePrimitives, RecoveredBlock, SealedBlock,
};
use smartnet_provider::{
    providers::{BlockchainProvider, ProviderNodeTypes},
    ProviderFactory, StateProviderFactory, StorageEngine,
};
use smartnet_revm::{
    database::StateProviderDatabase,
    db::{states::bundle_state::BundleRetention, State},
};
use smartnet_storage_api::{BlockNumReader, HeaderProvider, StateProviderBox};
use smartnet_timing::fmt_wall;
use smartnet_trie_common::{updates::TrieUpdatesSorted, HashedPostState};
use std::{
    io::{self, BufRead},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

/// Read-ahead buffer for the chain file — **100 MB** (operator, 2026-11-18: was 1 MB).
/// The file is streamed from its original location, never mmap'd, never copied.
const READ_CAPACITY: usize = 100 * 1024 * 1024;

/// Upper bound for a single block RLP item: a corrupt or hostile length prefix must not
/// make the importer allocate an arbitrary amount (real blocks are far below this).
const MAX_BLOCK_RLP_BYTES: usize = 64 * 1024 * 1024;

/// Blocks per range: decoded, checked and executed together, then written with **one**
/// commit. The range is the durability unit, and `done N blocks` is its report — the
/// number on that line is the range's last block, so the tip is always visible.
/// 1000 (operator, 2026-11-19: "extend from 100 to `1_000`, it may be even faster").
const IMPORT_RANGE: usize = 1000;

/// A single range write above which it is logged as slow (diagnostics only — the report
/// stays the two fields below).
const SLOW_RANGE_WARN: Duration = Duration::from_secs(10);

// TEMPORARY F5 DIAGNOSTIC — opt-in with `SMARTNET_IMPORT_DIAG=1`, logging only.
// It prints every block in the pinned range (plus the anchor root) and never gates the import.
const DIAG_START: u64 = 6_660_200;
const DIAG_END: u64 = 6_660_220;
static DIAG_PRINTED: AtomicBool = AtomicBool::new(false);

fn diag_enabled() -> bool {
    matches!(std::env::var("SMARTNET_IMPORT_DIAG").as_deref(), Ok("1" | "true" | "yes"))
}

fn diag_for(number: u64) -> bool {
    diag_enabled() && (DIAG_START..=DIAG_END).contains(&number)
}

/// Logging-only single-block state-delta dump, opt-in with `SMARTNET_DIAG_BLOCK=<number>`.
fn delta_diag_block() -> Option<u64> {
    std::env::var("SMARTNET_DIAG_BLOCK").ok()?.parse().ok()
}

fn delta_diag_for(number: u64) -> bool {
    delta_diag_block() == Some(number)
}

/// `MARS` accepts legacy (`0x0`) and dynamic-fee (`0x2`) transactions at every height. An
/// unexpected type in history is diagnostic only — warn, never fail the import (we assume there
/// are none). Type `0x1`/`0x3+` are refused at mempool entry, but a sealed block is executed as-is.
fn warn_unexpected_tx_types(
    number: u64,
    recovered: &RecoveredBlock<smartnet_ethereum_primitives::Block>,
) {
    for (index, tx) in recovered.transactions_recovered().enumerate() {
        match tx.ty() {
            0 | 2 => {}
            ty => warn!(
                target: "smartnet::import",
                block = number,
                tx = index,
                tx_type = ty,
                "unexpected transaction type in imported block (MARS executes 0x0 and 0x2)"
            ),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn report_delta_diag(
    number: u64,
    header: &smartnet_primitives_traits::Header,
    seal_signer: alloy_primitives::Address,
    recovered: &RecoveredBlock<smartnet_ethereum_primitives::Block>,
    bundle: &smartnet_revm::db::BundleState,
    decoded_receipts_root: B256,
    computed_receipts_root: B256,
    gas_used: u64,
    receipts: &[Receipt],
) {
    info!(
        target: "smartnet::import_diag",
        block = number,
        %decoded_receipts_root,
        %computed_receipts_root,
        receipts_root_ok = decoded_receipts_root == computed_receipts_root,
        gas_used,
        receipts = ?receipts,
        "delta diagnostic block result"
    );
    for (index, receipt) in receipts.iter().enumerate() {
        let mut enc = Vec::new();
        alloy_eips::eip2718::Encodable2718::encode_2718(
            &alloy_consensus::TxReceipt::with_bloom_ref(receipt),
            &mut enc,
        );
        info!(
            target: "smartnet::import_diag",
            block = number,
            tx = index,
            enc2718 = %alloy_primitives::hex::encode(&enc),
            "delta diagnostic receipt bytes"
        );
    }
    info!(
        target: "smartnet::import_diag",
        block = number,
        header_beneficiary = %header.beneficiary,
        seal_signer = %seal_signer,
        execution_beneficiary = %smartnet_engine_primitives::ethereum::execution_beneficiary_for_header(header),
        "delta diagnostic addresses"
    );
    for (index, tx) in recovered.transactions_recovered().enumerate() {
        info!(
            target: "smartnet::import_diag",
            block = number,
            tx = index,
            sender = %tx.signer(),
            tx_data = ?tx,
            "delta diagnostic tx"
        );
    }
    for (address, account) in bundle.state() {
        let before = account.original_info.as_ref().map(|info| (info.balance, info.nonce));
        let after = account.info.as_ref().map(|info| (info.balance, info.nonce));
        info!(
            target: "smartnet::import_diag",
            block = number,
            %address,
            ?before,
            ?after,
            code_hash = ?account.info.as_ref().map(|info| info.code_hash),
            status = ?account.status,
            storage_slots = account.storage.len(),
            "delta diagnostic account"
        );
        for (slot, value) in &account.storage {
            info!(
                target: "smartnet::import_diag",
                block = number,
                %address,
                slot = %slot,
                original = %value.previous_or_original_value,
                present = %value.present_value,
                "delta diagnostic storage"
            );
        }
    }
}

/// Result of executing one range: blocks ready to write, the range's net hashed state,
/// and (only under the temporary diagnostic) the pinned-range comparison records.
type RangeExecution<N> = (Vec<ExecutedBlock<N>>, HashedPostState, Vec<DiagRecord>);

/// One block's decoded values and the values execution/FS later produced. Only built
/// inside the pinned diagnostic range; empty everywhere else.
#[derive(Debug)]
struct DiagRecord {
    number: u64,
    decoded_hash: B256,
    decoded_state_root: B256,
    decoded_tx_root: B256,
    decoded_receipts_root: B256,
    computed_state_root: Option<B256>,
    computed_tx_root: B256,
    computed_receipts_root: B256,
    body_tx_count: u64,
}

/// The state database one range executes against: the committed anchor, with the
/// range's own earlier blocks visible through the accumulating `State`.
type RangeState = State<StateProviderDatabase<StateProviderBox>>;

/// Set while `run_import` is streaming: the heartbeat's status line yields to the
/// import's own line, so a run shows exactly one line per interval.
static IMPORTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether an import is streaming right now (read by the node heartbeat).
pub fn import_active() -> bool {
    IMPORTING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Clears [`IMPORTING`] on every exit path — success, error or early return.
struct ImportingGuard;

impl Drop for ImportingGuard {
    fn drop(&mut self) {
        IMPORTING.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Where a decoded file block goes: skipped (already on the local chain / genesis),
/// taken into the range, or a hard stop (wrong chain or out of order).
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    /// Already on the local chain (or the genesis block) — nothing to import.
    Skip(&'static str),
    /// Next in line — import it.
    Proceed,
    /// The file does not continue the local chain — stop the whole import.
    Stop(String),
}

/// Chain-position decision for one decoded file block, in order:
///
/// 1. Genesis (`number == 0`) is skipped — a mismatch with the stored genesis is caught by the
///    parent check on the first real block.
/// 2. Anything at or below the local tip at import start is skipped (resume semantics: a previous
///    run already imported it).
/// 3. The next block must extend the last block accepted from this file, or — before any has been
///    accepted — the local tip itself: right number **and** right parent hash. Anything else (gap,
///    reorder, different chain) stops the import.
fn classify(number: u64, parent: B256, start: (u64, B256), accepted: Option<(u64, B256)>) -> Flow {
    if number == 0 {
        return Flow::Skip("genesis block");
    }
    if number <= start.0 {
        return Flow::Skip("already at or below the local tip");
    }
    let (want_number, want_parent) = match accepted {
        Some((n, h)) => (n + 1, h),
        None => (start.0 + 1, start.1),
    };
    if number != want_number {
        return Flow::Stop(format!(
            "file block {number} appears where block {want_number} is expected — the file is \
             out of order, has a gap, or is not the chain stored here"
        ));
    }
    if parent != want_parent {
        return Flow::Stop(format!(
            "block {number} parent {parent} does not extend the local chain at block \
             {want_number} ({want_parent}) — different chain or a gap"
        ));
    }
    Flow::Proceed
}

/// Reads exactly one RLP item — the complete encoded value, prefix included — from the
/// stream.
///
/// Returns `Ok(None)` only when the stream ends cleanly on an item boundary. An EOF in
/// the middle of an item, a length prefix larger than [`MAX_BLOCK_RLP_BYTES`], or a
/// truncated item is an error: a truncated chain must never import as complete.
fn read_rlp_item<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut prefix = [0u8; 1];
    match reader.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let p = prefix[0];
    // RLP header: how the payload length is encoded — nothing after the prefix for the
    // single-byte form and the short forms (`length` lives in the prefix itself), an
    // 1..=8-byte big-endian length for the long forms.
    let (len_len, inline_len): (usize, u64) = match p {
        0x00..=0x7f => (0, 0),                     // the prefix byte IS the item
        0x80..=0xb7 => (0, u64::from(p - 0x80)),   // short string
        0xb8..=0xbf => (usize::from(p - 0xb7), 0), // long string
        0xc0..=0xf7 => (0, u64::from(p - 0xc0)),   // short list
        0xf8..=0xff => (usize::from(p - 0xf7), 0), // long list
    };
    let mut item = Vec::with_capacity(len_len + 64);
    item.push(p);
    let payload_len = if len_len > 0 {
        let mut len_bytes = [0u8; 8];
        reader.read_exact(&mut len_bytes[..len_len])?;
        item.extend_from_slice(&len_bytes[..len_len]);
        let mut len: u64 = 0;
        for b in &len_bytes[..len_len] {
            len = (len << 8) | u64::from(*b);
        }
        len
    } else {
        inline_len
    };
    if payload_len > MAX_BLOCK_RLP_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("RLP item length {payload_len} exceeds the {MAX_BLOCK_RLP_BYTES}-byte limit"),
        ));
    }
    if p <= 0x7f {
        return Ok(Some(item)); // single-byte item — the prefix is the whole payload
    }
    let mut payload = vec![0u8; payload_len as usize];
    reader.read_exact(&mut payload)?;
    item.extend_from_slice(&payload);
    Ok(Some(item))
}

/// Imports `file` into the stores, in [`IMPORT_RANGE`]-block ranges — `doc/IMPORT.md` §1.
///
/// Returns `Ok(true)` when the file was imported to its end, `Ok(false)` when a stop
/// signal ended the run at a range boundary (everything committed so far is on disk and
/// `SHUTDOWN_CLEAN` is written either way). An error means nothing further was written:
/// the last commit stands and the next run resumes from it.
pub fn run_import<P, Evm>(
    factory: &ProviderFactory<P>,
    evm: &Evm,
    file_tier_base: std::path::PathBuf,
    changeset_window: u64,
    file: &Path,
    val_file: &Path,
) -> eyre::Result<bool>
where
    P: ProviderNodeTypes,
    // The Ethereum shapes this file decodes and writes: the RLP is a
    // `smartnet_ethereum_primitives::Block`, its header/tx/receipt types are the ones
    // the node's EVM and the storage write protocol are built around.
    P::Primitives: NodePrimitives<
        Block = smartnet_ethereum_primitives::Block,
        BlockHeader = smartnet_primitives_traits::Header,
        SignedTx = smartnet_ethereum_primitives::TransactionSigned,
        Receipt = Receipt,
    >,
    <P::Primitives as NodePrimitives>::SignedTx: SignatureAccess,
    Evm: ConfigureEvm<Primitives = P::Primitives>,
{
    let _drain = smartnet_tasks::drain::register();
    let _importing = ImportingGuard;
    IMPORTING.store(true, std::sync::atomic::Ordering::Relaxed);

    // The authority for check 2 is the RAM registry, loaded once here from the path the
    // operator named in `VAL_FILE`: an import run never boots the validator, so nothing
    // else would load it. Fail closed — without a readable registry holding a
    // `status: true` validator no block can be accepted.
    crate::validator_registry::load_validators(val_file).map_err(|err| {
        eyre::eyre!(
            "validator registry unreadable ({}) : {err} — no block can be accepted (fail closed)",
            val_file.display()
        )
    })?;
    // Re-apply the inert self-label after this authoritative load replaces the RAM array.
    if let Some(address) = crate::bootstrap_files::self_address() {
        calibri_lite_consensus::registry::mark_self(address);
    }
    let enabled = crate::validator_registry::enabled_set();
    if enabled.is_empty() {
        return Err(eyre::eyre!(
            "validator registry at {} holds no `status: true` validator — no block can be \
             accepted (fail closed); the genesis signer list is legacy and is not used",
            val_file.display()
        ));
    }

    let started = Instant::now();
    let handle = std::fs::File::open(file)
        .map_err(|err| eyre::eyre!("cannot open chain file `{}`: {err}", file.display()))?;
    let mut reader = io::BufReader::with_capacity(READ_CAPACITY, handle);

    // The durable anchor: the last block that is actually on disk. Everything reported
    // by this run is measured from it, and a re-run resumes from it.
    let (mut anchor_number, mut anchor_hash) = {
        let provider = factory.provider()?;
        let number = provider.last_block_number()?;
        let header = provider.sealed_header(number)?.ok_or_else(|| {
            eyre::eyre!(
                "local tip header at block {number} is missing — the database was not \
                 initialised; run `smartnet init <genesis>` first"
            )
        })?;
        (number, header.hash())
    };
    info!(
        target: "smartnet::import",
        file = %file.display(),
        local_tip = anchor_number,
        %anchor_hash,
        "import starting (file read in place, read-only)"
    );

    let diag = diag_enabled();
    if diag {
        info!(
            target: "smartnet::import_diag",
            start = DIAG_START,
            end = DIAG_END,
            "temporary divergence diagnostic enabled (logging only, not a gate)"
        );
    }

    let base = file_tier_base.clone();
    let mut engine = StorageEngine::new(factory.clone(), Some(file_tier_base), changeset_window);

    let mut offset: u64 = 0;
    let mut scanned: u64 = 0;
    let mut skipped: u64 = 0;
    let mut committed: u64 = 0;
    let mut accepted: Option<(u64, B256)> = None;
    let mut stopped_by_signal = false;

    // Progress bookkeeping: one line per REPORT_INTERVAL, over the blocks this window
    // actually put on disk.
    let mut line_at = Instant::now();
    let mut line_base: u64 = 0;

    let mut pending: Vec<smartnet_ethereum_primitives::Block> = Vec::with_capacity(IMPORT_RANGE);

    loop {
        if smartnet_tasks::drain::exit_requested() {
            stopped_by_signal = true;
            break;
        }

        // Fill one range. Skipped items (genesis, resume) do not count towards it —
        // they are not imported, so there is nothing to commit for them.
        while pending.len() < IMPORT_RANGE {
            if smartnet_tasks::drain::exit_requested() {
                stopped_by_signal = true;
                break;
            }
            let Some(item) = read_rlp_item(&mut reader)? else {
                break; // clean EOF on an item boundary
            };
            let item_start = offset;
            offset += item.len() as u64;
            scanned += 1;

            let mut bytes = item.as_ref();
            let block: smartnet_ethereum_primitives::Block = Decodable::decode(&mut bytes)
                .map_err(|err| {
                    eyre::eyre!(
                        "item {scanned} (offset {item_start}): block RLP decode failed: {err}"
                    )
                })?;
            if !bytes.is_empty() {
                return Err(eyre::eyre!(
                    "item {scanned} (offset {item_start}): {} trailing bytes after the block RLP",
                    bytes.len()
                ));
            }

            let number = block.header.number;
            let parent = block.header.parent_hash;
            let hash = block.header.hash_slow();
            match classify(number, parent, (anchor_number, anchor_hash), accepted) {
                Flow::Skip(reason) => {
                    skipped += 1;
                    if skipped == 1 {
                        info!(
                            target: "smartnet::import",
                            number, reason, "skipping file blocks already on the local chain"
                        );
                    } else {
                        debug!(target: "smartnet::import", number, reason, offset = item_start, "skipped");
                    }
                    continue;
                }
                Flow::Stop(reason) => {
                    return Err(eyre::eyre!(
                        "import stopped at item {scanned} (offset {item_start}, block {number}): {reason}"
                    ));
                }
                Flow::Proceed => {}
            }
            // The file's logical chain tip: the next item must extend this one, whether
            // or not this range has been written yet (an error below ends the run, so
            // the two can never disagree in the output).
            accepted = Some((number, hash));
            pending.push(block);
        }

        if pending.is_empty() {
            break; // EOF — nothing left to execute
        }

        // Checks + execution for the whole range, against the committed anchor. The
        // provider is built per range: it caches the chain tip, and this run moves it.
        let state = BlockchainProvider::new(factory.clone())?;
        let (blocks, hashed_state, diag_records) =
            execute_range(&state, evm, anchor_hash, std::mem::take(&mut pending), diag)?;
        let range_len = blocks.len() as u64;
        // The fill loop classified exactly these blocks in order, so the chain tip it
        // tracked IS this range's last block.
        let (last_number, last_hash) = accepted.expect("a range is never empty here");

        // The range's trie updates, computed ONCE from the disk trie at the anchor over
        // the range's net hashed state — only the changed paths walk — and attached to
        // the last block, which is where the write merges them from.
        let range_root;
        let mut blocks = blocks;
        {
            // The anchor state (the DB tip before this range) — nothing has been
            // committed yet, so the provider built for execution is still exact.
            let (root, updates) =
                state.state_by_block_hash(anchor_hash)?.state_root_with_updates(hashed_state)?;
            range_root = root;
            if let Some(last) = blocks.last_mut() {
                last.trie_data = DeferredTrieData::ready(ComputedTrieData::without_trie_input(
                    last.trie_data().hashed_state,
                    Arc::new(updates.into_sorted()),
                ));
            }
        }

        // The write: file-tier append → DB state/index → ONE commit. The commit is the
        // range's only durability point, so nothing is reported before this returns.
        let save_start = Instant::now();
        engine.save_blocks(blocks, None, None)?;
        // Seals only ever complete after their commit (this call joins what the append
        // spawned), so a compressed shard can never hold blocks the DB does not have.
        engine.join_seal_tasks();
        if diag {
            report_diagnostic_range(&base, &diag_records);
        }
        let save_wall = save_start.elapsed();
        if save_wall > SLOW_RANGE_WARN {
            warn!(
                target: "smartnet::import",
                blocks = range_len,
                block = last_number,
                save_ms = save_wall.as_millis() as u64,
                "slow range save"
            );
        }
        debug!(
            target: "smartnet::import",
            blocks = range_len,
            block = last_number,
            %range_root,
            save_ms = save_wall.as_millis() as u64,
            "range committed"
        );

        committed += range_len;
        anchor_number = last_number;
        anchor_hash = last_hash;

        // DONE: one line per finished range — the count AND the block it reached
        // (operator: "add blocknr it imports instead of double 100").
        info!(
            target: "smartnet::import",
            block = last_number,
            "done {range_len} blocks"
        );

        // One progress line per REPORT_INTERVAL: the committed tip + this window's
        // import speed. Nothing else — a wall of counters says nothing while running.
        if line_at.elapsed() >= smartnet_tracing::REPORT_INTERVAL {
            let secs = line_at.elapsed().as_secs_f64();
            let saved = committed - line_base;
            let blocks_s = if secs > 0.0 { saved as f64 / secs } else { 0.0 };
            let tip = factory.provider()?.last_block_number()?;
            info!(
                target: "smartnet::import",
                block = tip,
                blocks_s = format!("{blocks_s:.1}"),
                "import progress"
            );
            line_at = Instant::now();
            line_base = committed;
        }
    }

    // Flush the seals spawned along the way, then record the clean stop: both exit paths
    // (finished and signal) leave every committed range durable in DB *and* FS, so the
    // boot that follows takes the clean path and nothing is trimmed.
    engine.join_seal_tasks();
    engine.shutdown_clean(&base)?;

    let tip = factory.provider()?.last_block_number()?;
    if stopped_by_signal {
        info!(
            target: "smartnet::import",
            block = tip,
            import_wall = %fmt_wall(started.elapsed()),
            "import stopped by signal — the committed ranges are on disk"
        );
    } else {
        info!(
            target: "smartnet::import",
            block = tip,
            scanned, skipped,
            import_wall = %fmt_wall(started.elapsed()),
            "import finished"
        );
    }
    Ok(!stopped_by_signal)
}

/// Checks and executes one range against the state of `anchor_hash`, returning the
/// blocks ready to write and the range's net hashed state.
///
/// One `State` is built for the whole range, so block N+1 reads block N's changes; each
/// block's bundle is taken after it executes, which is what the write needs per block
/// (receipts for the file tier, changesets for the DB).
fn execute_range<P, Evm>(
    state: &BlockchainProvider<P>,
    evm: &Evm,
    anchor_hash: B256,
    blocks: Vec<smartnet_ethereum_primitives::Block>,
    diag: bool,
) -> eyre::Result<RangeExecution<P::Primitives>>
where
    P: ProviderNodeTypes,
    P::Primitives: NodePrimitives<
        Block = smartnet_ethereum_primitives::Block,
        BlockHeader = smartnet_primitives_traits::Header,
        SignedTx = smartnet_ethereum_primitives::TransactionSigned,
        Receipt = Receipt,
    >,
    Evm: ConfigureEvm<Primitives = P::Primitives>,
{
    let diag_provider = if diag { Some(state.state_by_block_hash(anchor_hash)?) } else { None };
    if diag && blocks.iter().any(|b| diag_for(b.header.number)) {
        match diag_provider.as_ref().map(|p| p.state_root(HashedPostState::default())) {
            Some(Ok(root)) => info!(
                target: "smartnet::import_diag",
                %root,
                "diagnostic anchor state root (empty overlay)"
            ),
            Some(Err(err)) => {
                warn!(target: "smartnet::import_diag", %err, "anchor root probe failed")
            }
            None => {}
        }
    }
    let anchor_provider = state.state_by_block_hash(anchor_hash)?;
    let mut db: RangeState = State::builder()
        .with_database(StateProviderDatabase::new(anchor_provider))
        .with_bundle_update()
        .build();

    let mut executed: Vec<ExecutedBlock<P::Primitives>> = Vec::with_capacity(blocks.len());
    let mut net_state = HashedPostState::default();
    let mut diag_records = Vec::new();
    // Contracts whose token metadata was already probed this range (the sidecar is
    // keyed by deployed address and write-once, so one probe per address is enough).
    let mut probed = std::collections::HashSet::new();

    for block in blocks {
        let number = block.header.number;
        let want_diag = diag_for(number);
        let decoded_hash = block.header.hash_slow();
        let decoded_state_root = block.header.state_root;
        let decoded_tx_root = block.header.transactions_root;
        let decoded_receipts_root = block.header.receipts_root;
        let sealed = SealedBlock::seal_slow(block);
        let recovered = RecoveredBlock::try_recover_sealed(sealed.clone())
            .map_err(|err| eyre::eyre!("block {number}: sender recovery failed: {err}"))?;

        // Check 2 — THE validator gate, through the ONE entrance both ingest paths use
        // (operator: "make sure all import and mesh sync both go through this same single
        // entrance"): `check_seal` — the seal's signer must be the SELECT chair for this
        // slot, and `get_val_chair` only ever answers with a `status: true` validator of
        // `validators.jsonl`. Check 1 (previous block hash + number) already happened in
        // `classify`; nothing else gates acceptance — no fee rule, no genesis branch, no
        // body check.
        let seal_signer = calibri_lite_consensus::check_seal(sealed.header())
            .map_err(|err| eyre::eyre!("block {number}: validator check failed: {err}"))?;

        // One EVM pass: the node's own config, the rtbc create-admission inspector —
        // the same execution a live block gets, with no engine in between. This is
        // `execute_block`'s exact body, except the EVM is kept at the end so the rtbc
        // inspector's create log (creator → created contract) can be read out — the
        // creator is not derivable from state.
        let (result, creations) = {
            let evm_env = evm
                .evm_env(sealed.header())
                .map_err(|err| eyre::eyre!("block {number}: evm env failed: {err}"))?;
            let inspected = evm.evm_with_env_and_inspector(&mut db, evm_env, rtbc());
            let ctx = evm
                .context_for_block(&sealed)
                .map_err(|err| eyre::eyre!("block {number}: execution ctx failed: {err}"))?;
            let mut executor = evm.create_executor(inspected, ctx);
            executor
                .apply_pre_execution_changes()
                .map_err(|err| eyre::eyre!("block {number}: pre-execution failed: {err}"))?;
            warn_unexpected_tx_types(number, &recovered);
            for tx in recovered.transactions_recovered() {
                executor
                    .execute_transaction(tx)
                    .map_err(|err| eyre::eyre!("block {number}: execution failed: {err}"))?;
            }
            let (mut done, result) = executor
                .finish()
                .map_err(|err| eyre::eyre!("block {number}: execution finish failed: {err}"))?;
            let creations = done.components_mut().1.take_creations();
            (result, creations)
            // the executor and the inspected EVM drop here, releasing `&mut db`
        };
        db.merge_transitions(BundleRetention::Reverts);
        let bundle = db.take_bundle();

        if delta_diag_for(number) {
            report_delta_diag(
                number,
                sealed.header(),
                seal_signer,
                &recovered,
                &bundle,
                decoded_receipts_root,
                calculate_receipt_root_no_memo(&result.receipts),
                result.gas_used,
                &result.receipts,
            );
        }

        // Contract metadata — through the ONE entrance both ingest paths use
        // (`capture_block_meta`, operator: "make sure all import and mesh sync both go
        // through this same single entrance"); the records ride the block to the commit.
        // Probe state is discarded — this cannot change what the block commits.
        let probe_env = evm
            .evm_env(sealed.header())
            .map_err(|err| eyre::eyre!("block {number}: evm env failed: {err}"))?;
        let token_meta = smartnet_evm::token_meta::capture_block_meta(
            evm,
            &mut db,
            &probe_env,
            &bundle,
            creations,
            |address| probed.insert(address),
        );

        let hashed_state = db.database.0.hashed_post_state(&bundle);
        net_state.extend_ref(&hashed_state);

        if want_diag {
            let computed_state_root = match &diag_provider {
                Some(provider) => Some(provider.state_root(net_state.clone())?),
                None => None,
            };
            let body = recovered.body();
            let body_txs = body.transactions().cloned().collect::<Vec<_>>();
            diag_records.push(DiagRecord {
                number,
                decoded_hash,
                decoded_state_root,
                decoded_tx_root,
                decoded_receipts_root,
                computed_state_root,
                computed_tx_root: calculate_transaction_root(&body_txs),
                computed_receipts_root: calculate_receipt_root_no_memo(&result.receipts),
                body_tx_count: body_txs.len() as u64,
            });
        }

        let trie_data = ComputedTrieData::without_trie_input(
            Arc::new(hashed_state.into_sorted()),
            Arc::new(TrieUpdatesSorted::default()),
        );
        executed.push(
            ExecutedBlock::new(
                Arc::new(recovered),
                Arc::new(BlockExecutionOutput { result, state: bundle }),
                trie_data,
            )
            .with_token_meta(token_meta),
        );
    }

    Ok((executed, net_state, diag_records))
}

/// Temporary logging-only diagnostic report. Every pinned block is logged once — including
/// the block with the transaction (BUG2A) — so a mismatch in an early empty block cannot hide
/// what a later block computed. Nothing here changes acceptance or the write path.
fn report_diagnostic_range(base: &Path, records: &[DiagRecord]) {
    if DIAG_PRINTED.load(Ordering::Relaxed) || records.is_empty() {
        return;
    }
    for r in records {
        let fs = match read_block_at(base, r.number) {
            Ok(bundle) => bundle.map(|b| b.bt),
            Err(err) => {
                warn!(target: "smartnet::import_diag", block = r.number, %err, "FS read failed");
                continue;
            }
        };
        let fs_hash_ok = fs
            .as_ref()
            .and_then(|bt| bt.h.parse::<B256>().ok())
            .map(|hash| hash == r.decoded_hash)
            .unwrap_or(false);
        let fs_tc = fs.as_ref().map(|bt| bt.tc);
        let computed_sr = r.computed_state_root;
        let sr_ok = computed_sr.map(|c| c == r.decoded_state_root).unwrap_or(false);
        let txr_ok = r.decoded_tx_root == r.computed_tx_root;
        let rcr_ok = r.decoded_receipts_root == r.computed_receipts_root;
        let tc_ok = fs_tc == Some(r.body_tx_count);
        let computed = computed_sr.map(|c| c.to_string()).unwrap_or_else(|| "n/a".to_string());
        let outcome =
            if fs_hash_ok && sr_ok && txr_ok && rcr_ok && tc_ok { "match" } else { "DIVERGENCE" };
        warn!(
            target: "smartnet::import_diag",
            block = r.number,
            decoded_sr = %r.decoded_state_root,
            computed_sr = %computed,
            %sr_ok,
            %txr_ok,
            %rcr_ok,
            %fs_hash_ok,
            body_txs = r.body_tx_count,
            ?fs_tc,
            %tc_ok,
            outcome,
            "F5 diagnostic block (logging only, import continues)"
        );
    }
    DIAG_PRINTED.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Block as ConsensusBlock, TxEnvelope};
    use std::io::Cursor;

    fn read(bytes: &[u8]) -> io::Result<Option<Vec<u8>>> {
        read_rlp_item(&mut Cursor::new(bytes))
    }

    #[test]
    fn single_byte_item_is_returned_verbatim() {
        assert_eq!(read(&[0x01]).unwrap(), Some(vec![0x01]));
        assert_eq!(read(&[0x7f]).unwrap(), Some(vec![0x7f]));
    }

    #[test]
    fn short_string_and_short_list_round_trip() {
        // 0x83 'a' 'b' 'c' — short string of 3 bytes.
        assert_eq!(read(&[0x83, b'a', b'b', b'c']).unwrap(), Some(vec![0x83, b'a', b'b', b'c']));
        // 0xc2 0x01 0x02 — short list of 2 bytes.
        assert_eq!(read(&[0xc2, 0x01, 0x02]).unwrap(), Some(vec![0xc2, 0x01, 0x02]));
        // 0x80 — empty string.
        assert_eq!(read(&[0x80]).unwrap(), Some(vec![0x80]));
        // 0xc0 — empty list.
        assert_eq!(read(&[0xc0]).unwrap(), Some(vec![0xc0]));
    }

    #[test]
    fn the_55_56_byte_boundary_switches_forms() {
        // 55-byte string: short form 0x80 + 55 = 0xb7, payload follows directly.
        let mut short = vec![0xb7];
        short.extend(std::iter::repeat_n(b'x', 55));
        assert_eq!(read(&short).unwrap().map(|v| v.len()), Some(56));
        // 56-byte string: long form 0xb8 0x38 then 56 payload bytes.
        let mut long = vec![0xb8, 56];
        long.extend(std::iter::repeat_n(b'x', 56));
        assert_eq!(read(&long).unwrap().map(|v| v.len()), Some(58));
        // The same boundary for lists: 55-byte list (0xf7 + payload) vs 56-byte list
        // (0xf8 0x38 + payload).
        let mut short_list = vec![0xf7];
        short_list.extend(std::iter::repeat_n(0x00, 55));
        assert_eq!(read(&short_list).unwrap().map(|v| v.len()), Some(56));
        let mut long_list = vec![0xf8, 56];
        long_list.extend(std::iter::repeat_n(0x00, 56));
        assert_eq!(read(&long_list).unwrap().map(|v| v.len()), Some(58));
    }

    #[test]
    fn clean_eof_is_none_mid_item_is_error() {
        // Empty stream: clean EOF.
        assert_eq!(read(&[]).unwrap(), None);
        // A complete item followed by EOF: the item first, then None.
        let mut cursor = Cursor::new(vec![0x01]);
        assert_eq!(read_rlp_item(&mut cursor).unwrap(), Some(vec![0x01]));
        assert_eq!(read_rlp_item(&mut cursor).unwrap(), None);
        // Truncated: length prefix promises 3 payload bytes, only 1 follows.
        let err = read(&[0x83, b'a']).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        // Truncated long-form length itself.
        let err = read(&[0xb9]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_item_is_rejected_before_allocation() {
        // Long string claiming 2^56 - 1 bytes of payload.
        let bytes = [0xbf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        let err = read(&bytes).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[test]
    fn concatenated_items_are_read_one_by_one() {
        let mut cursor = Cursor::new(vec![0x01, 0x82, b'h', b'i', 0xc0]);
        assert_eq!(read_rlp_item(&mut cursor).unwrap(), Some(vec![0x01]));
        assert_eq!(read_rlp_item(&mut cursor).unwrap(), Some(vec![0x82, b'h', b'i']));
        assert_eq!(read_rlp_item(&mut cursor).unwrap(), Some(vec![0xc0]));
        assert_eq!(read_rlp_item(&mut cursor).unwrap(), None);
    }

    #[test]
    fn classify_skips_genesis_and_the_local_chain() {
        let g = B256::repeat_byte(0xaa);
        let t = (100, g);
        assert!(matches!(classify(0, g, t, None), Flow::Skip(_)));
        assert!(matches!(classify(100, g, t, None), Flow::Skip(_)));
        assert!(matches!(classify(42, g, t, None), Flow::Skip(_)));
        // Already accepted in this run: still below the running tip of the file? No —
        // once accepted, chains are checked against the last accepted block.
        assert!(matches!(classify(101, g, t, Some((150, g))), Flow::Stop(_)));
    }

    #[test]
    fn classify_first_block_must_extend_the_local_tip() {
        let g = B256::repeat_byte(0xaa);
        let t = (100, g);
        assert_eq!(classify(101, g, t, None), Flow::Proceed);
        let other = B256::repeat_byte(0xbb);
        assert!(matches!(classify(102, g, t, None), Flow::Stop(_)));
        assert!(matches!(classify(101, other, t, None), Flow::Stop(_)));
    }

    #[test]
    fn classify_chains_block_after_block() {
        let g = B256::repeat_byte(0xaa);
        let t = (0, g);
        let prev = (5, B256::repeat_byte(0x55));
        assert_eq!(classify(6, prev.1, t, Some(prev)), Flow::Proceed);
        // Gap or reorder: the file jumped ahead / went back.
        assert!(matches!(classify(8, prev.1, t, Some(prev)), Flow::Stop(_)));
        assert!(matches!(classify(5, prev.1, t, Some(prev)), Flow::Stop(_)));
        // Right number, wrong parent: a different chain.
        assert!(matches!(classify(6, B256::repeat_byte(0x66), t, Some(prev)), Flow::Stop(_)));
    }

    #[test]
    fn block_rlp_round_trips_through_the_reader() {
        // The wire shape: encoded as the mesh/sync sender encodes it, decoded the way
        // `new_payload` (and this importer) decodes it.
        let consensus: ConsensusBlock<TxEnvelope> =
            ConsensusBlock { header: Default::default(), body: Default::default() };
        let mut encoded = Vec::new();
        alloy_rlp::Encodable::encode(&consensus, &mut encoded);
        let item = read(&encoded).unwrap().expect("one item");
        let mut bytes = item.as_ref();
        let block: smartnet_ethereum_primitives::Block =
            Decodable::decode(&mut bytes).expect("decodes");
        assert!(bytes.is_empty(), "no trailing bytes");
        assert_eq!(block.header.number, consensus.header.number);
        assert_eq!(block.header.hash_slow(), consensus.header.hash_slow());
    }
}
