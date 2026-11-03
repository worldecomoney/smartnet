//! Streaming file import: read the operator's original chain RLP file in place and push
//! every block through the node's engine seam (`execution_data_from_block_rlp` →
//! `new_payload`, full validation — the exact path network blocks take), one block
//! at a time (IMPORT-EXPORT.md).
//!
//! The file is READ ONLY: it is streamed from its original location, never copied and
//! never rewritten. The import layer holds no consensus rules of its own — seal checks,
//! fork rules and execution all live inside the engine; this layer only decodes RLP,
//! keeps chain order (genesis/already-stored blocks are skipped, the first block must
//! extend the local tip, each next block must extend the previous one) and stops at the
//! first block the engine does not accept as [`PayloadStatusEnum::Valid`] — except
//! `Syncing`, the Engine API's "cannot answer yet" status: the parent is known Valid
//! (classify proved continuity), so the same payload is re-sent a bounded number of
//! times before the import gives up. On ctrl-c/SIGTERM the loop stops at the next block
//! boundary, so the last block is always fully processed before shutdown records
//! `SHUTDOWN_CLEAN`. No fork detection: every block follows the same decode → submit
//! path, whatever its era.

use alloy_primitives::B256;
use alloy_rlp::Decodable;
use alloy_rpc_types_engine::PayloadStatusEnum;
use smartnet_engine_primitives::ConsensusEngineHandle;
use smartnet_engine_primitives::ethereum::execution_data_from_block_rlp;
use smartnet_payload_primitives::PayloadTypes;
use smartnet_storage_api::{BlockNumReader, HeaderProvider};
use smartnet_timing::fmt_wall;
use std::{
    io::{self, BufRead},
    path::Path,
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

/// Read-ahead buffer for the chain file (the file is streamed, never mmap'd).
const READ_CAPACITY: usize = 1024 * 1024;

/// Upper bound for a single block RLP item: a corrupt or hostile length prefix must not
/// make the importer allocate an arbitrary amount (real blocks are far below this).
const MAX_BLOCK_RLP_BYTES: usize = 64 * 1024 * 1024;

/// Progress log cadence: one line per 10k scanned items.
const PROGRESS_EVERY: u64 = 10_000;

/// How often a `Syncing` response is re-sent before the import stops. `Syncing` is not
/// a verdict (the Engine API says: re-send later) — the chain checks above already
/// proved the parent is the last accepted block, so a bounded retry cannot skip
/// validation; it only rides out engine-side transient states (e.g. a persistence
/// window between commits).
const SYNCING_RETRIES: u32 = 5;

/// Delay between `Syncing` re-sends.
const SYNCING_BACKOFF: Duration = Duration::from_millis(300);

/// Single-block engine latency above which the block is logged as slow (the aggregate
/// per-progress-line numbers stay the primary signal).
const SLOW_BLOCK_WARN: Duration = Duration::from_secs(5);

/// Where a decoded file block goes: skipped (already on the local chain / genesis),
/// submitted to the engine, or a hard stop (wrong chain or out of order).
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    /// Already on the local chain (or the genesis block) — nothing to import.
    Skip(&'static str),
    /// Next in line — submit it to the engine.
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
/// the middle of an item, a length prefix larger than [`MAX_BLOCK_RLP_BYTES`], or an
/// unreadable stream is an error: a truncated chain must never import as complete.
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

/// Streams `file` (concatenated block RLP items) into the engine until EOF or the first
/// block the engine does not accept. The file is opened read-only in place.
///
/// Resume: the local tip is read once at start; file blocks at or below it are skipped,
/// so a re-run of the same command continues where the previous run stopped.
pub async fn run_import<P, Pr>(
    engine: &ConsensusEngineHandle<P>,
    provider: &Pr,
    file: &Path,
) -> eyre::Result<()>
where
    P: PayloadTypes<ExecutionData = alloy_rpc_types_engine::ExecutionData>,
    Pr: BlockNumReader + HeaderProvider,
{
    // Registered drainable: on ctrl-c/SIGTERM the runner flips the exit flag and waits
    // (bounded) for this future instead of dropping it mid-block, so the last block is
    // fully processed before the clean-shutdown marker is written.
    let _drain_guard = smartnet_tasks::drain::register();

    let started = Instant::now();
    let best = provider.best_block_number()?;
    let start_hash = provider
        .sealed_header(best)?
        .ok_or_else(|| {
            eyre::eyre!(
                "local tip header at block {best} is missing — the database was not \
                 initialised; run `smartnet init <genesis>` first"
            )
        })?
        .hash();
    info!(
        target: "smartnet::import",
        file = %file.display(),
        local_tip = best,
        %start_hash,
        "import starting (file read in place, read-only)"
    );

    let handle = std::fs::File::open(file)
        .map_err(|err| eyre::eyre!("cannot open chain file `{}`: {err}", file.display()))?;
    let mut reader = io::BufReader::with_capacity(READ_CAPACITY, handle);

    let mut offset: u64 = 0;
    let mut scanned: u64 = 0;
    let mut imported: u64 = 0;
    let mut skipped: u64 = 0;
    let mut accepted: Option<(u64, B256)> = None;

    // Per-window timing aggregates (reset at every progress line): wall time between
    // accepted blocks vs the engine's own latency/queue/persistence waits, so a stall
    // (fsync, queue, execution) is attributed instead of guessed.
    let mut stopped_by_signal = false;
    let mut syncing_retries: u64 = 0;
    let mut window = Instant::now();
    let mut w_accepted: u64 = 0;
    let mut w_engine_us: u64 = 0;
    let mut w_queue_us: u64 = 0;
    let mut w_persist_us: u64 = 0;
    let mut w_max_engine_us: u64 = 0;

    loop {
        // Stop signal: only at block boundaries — never abandon an engine call in
        // flight, so the last submitted block is always recorded (or reported) first.
        if smartnet_tasks::drain::exit_requested() {
            stopped_by_signal = true;
            break;
        }
        let Some(item) = read_rlp_item(&mut reader)? else {
            break;
        };
        let item_start = offset;
        offset += item.len() as u64;
        scanned += 1;

        let mut bytes = item.as_ref();
        let block: smartnet_ethereum_primitives::Block =
            Decodable::decode(&mut bytes).map_err(|err| {
                eyre::eyre!("item {scanned} (offset {item_start}): block RLP decode failed: {err}")
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
        match classify(number, parent, (best, start_hash), accepted) {
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

        // One engine call = the full validated import path: seal check + execution +
        // canonical insert, exactly as for a block arriving from the network.
        // `Syncing` is re-sent (bounded); every other answer is taken at face value.
        let payload = execution_data_from_block_rlp(block);
        let mut attempt = 0u32;
        let (status, timings) = loop {
            match engine.new_payload(payload.clone(), true, true).await {
                Ok((status, _timings))
                    if status.status == PayloadStatusEnum::Syncing &&
                        attempt < SYNCING_RETRIES =>
                {
                    attempt += 1;
                    syncing_retries += 1;
                    warn!(
                        target: "smartnet::import",
                        number, attempt, retries = SYNCING_RETRIES,
                        "engine returned Syncing (not a verdict) — re-sending after backoff"
                    );
                    tokio::time::sleep(SYNCING_BACKOFF).await;
                }
                Ok(pair) => break pair,
                Err(err) => {
                    return Err(eyre::eyre!("block {number}: engine newPayload failed: {err}"))
                }
            }
        };
        // Exactly the sync sink's contract (`EngineBlockSink::import_blocks`): the
        // engine confirms VALID or the run stops — no skipping validation, no mayonnaise.
        match status.status {
            PayloadStatusEnum::Valid => {}
            other => {
                return Err(eyre::eyre!(
                    "import stopped at block {number} (parent {parent}, item {scanned}, offset \
                    {item_start}): rejected by engine: {other:?} — {}",
                    other.validation_error().unwrap_or_default()
                ));
            }
        }

        accepted = Some((number, hash));
        imported += 1;
        w_accepted += 1;
        let engine_us = timings.latency.as_micros() as u64;
        w_engine_us += engine_us;
        w_queue_us += timings.queue_wait.as_micros() as u64;
        w_persist_us += timings.persistence_wait.as_micros() as u64;
        w_max_engine_us = w_max_engine_us.max(engine_us);
        if timings.latency > SLOW_BLOCK_WARN {
            warn!(
                target: "smartnet::import",
                number,
                engine_ms = timings.latency.as_millis() as u64,
                queue_ms = timings.queue_wait.as_millis() as u64,
                persist_wait_ms = timings.persistence_wait.as_millis() as u64,
                "slow block"
            );
        }
        if imported == 1 {
            info!(
                target: "smartnet::import",
                number, %hash, "first file block accepted — streaming"
            );
        }
        if scanned.is_multiple_of(PROGRESS_EVERY) {
            let secs = window.elapsed().as_secs_f64();
            let blocks_s = if secs > 0.0 { w_accepted as f64 / secs } else { 0.0 };
            let avg = |us: u64| if w_accepted > 0 { us as f64 / 1_000.0 / w_accepted as f64 } else { 0.0 };
            info!(
                target: "smartnet::import",
                scanned, imported, skipped, block = number,
                blocks_s = format!("{blocks_s:.1}"),
                engine_ms = format!("{:.1}", avg(w_engine_us)),
                queue_ms = format!("{:.1}", avg(w_queue_us)),
                persist_ms = format!("{:.1}", avg(w_persist_us)),
                max_engine_ms = w_max_engine_us / 1_000,
                syncing_retries,
                "import progress"
            );
            window = Instant::now();
            w_accepted = 0;
            w_engine_us = 0;
            w_queue_us = 0;
            w_persist_us = 0;
            w_max_engine_us = 0;
        }
    }

    let tip = accepted.map(|(n, _)| n).unwrap_or(best);
    if stopped_by_signal {
        info!(
            target: "smartnet::import",
            scanned, imported, skipped, tip, syncing_retries,
            import_wall = %fmt_wall(started.elapsed()),
            "import stopped by signal — the last block was fully processed"
        );
    } else {
        info!(
            target: "smartnet::import",
            scanned, imported, skipped, tip, syncing_retries,
            import_wall = %fmt_wall(started.elapsed()),
            "import finished"
        );
    }
    Ok(())
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
