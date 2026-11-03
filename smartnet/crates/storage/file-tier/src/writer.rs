//! The append-side writer for a sub-shard: one plain-JSONL appender per file (AMEND1).
//!
//! The active shard is written as plain JSONL (`{name}.jsonl`) — one JSON record per line,
//! uncompressed — so hot reads never decompress. At the sub-shard boundary the engine seals it
//! by compressing each `.jsonl` into `.gz` ([`crate::seal::seal_shard`]).
//!
//! The writer is passive: it appends records and opens the next sub-shard lazily, but it never
//! compresses, renames, or rotates — that is the engine's seal step. Each file keeps one
//! append-only handle open and **buffers** its lines; the engine flushes the active sub-shard
//! once per save batch (before the DB commit) and the seal flushes before compressing. The file
//! on disk is therefore complete lines (plus possibly a partial last line after a crash, which
//! the line-based recovery truncates).
//!
//! Empty blocks are first-class: they append their `b` record (with `tc: 0`) like any other
//! block; the other files simply hold no line until their first record.

use crate::{
    error::FileTierResult,
    extract::{extract_empty_block, BlockRecords},
    records::{BlockLogsRecord, BtRecord, Erc4626Record, TxRecord, TxStubRecord},
    seal::active_file_path,
    shard::{get_shard, ShardId},
};
use alloy_consensus::BlockHeader;
use alloy_primitives::B256;
use smartnet_fs_util::JsonlAppender;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Per-file "landed" marks for one append batch: how long after the batch start each payload
/// file's last write reached the OS (no fsync — the batch's single DB commit is the
/// durability point). `None` means the file took no lines this batch (nothing landed,
/// nothing to measure).
///
/// The persistence log reports these as the batch's per-file close times; [`Self::all_landed`]
/// is the moment the batch's payload was fully in the files — the "block sealed" instant the
/// save path measures against its budget.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FileWriteTimes {
    /// `b.jsonl` (block headers) last write landed.
    pub b: Option<Duration>,
    /// `txs.jsonl` last write landed.
    pub txs: Option<Duration>,
    /// `logs.jsonl` last write landed.
    pub logs: Option<Duration>,
    /// `4626.jsonl` last write landed (only exists for shards with events).
    pub erc4626: Option<Duration>,
}

impl FileWriteTimes {
    /// The latest landing across every file written — when the batch's payload was complete
    /// in the files (the batch's "block sealed" mark), or `None` if the batch wrote nothing.
    pub fn all_landed(&self) -> Option<Duration> {
        [self.b, self.txs, self.logs, self.erc4626].into_iter().flatten().max()
    }

    /// Merges another flush round's marks into these (a batch that crosses a sub-shard
    /// boundary flushes twice — the closing shard, then the next one), keeping the later
    /// mark per file. Both rounds measure from the same batch start, so the larger duration
    /// is the later landing; a file written in only one round keeps that round's mark
    /// (`None < Some`).
    fn merge(&mut self, other: Self) {
        self.b = self.b.max(other.b);
        self.txs = self.txs.max(other.txs);
        self.logs = self.logs.max(other.logs);
        self.erc4626 = self.erc4626.max(other.erc4626);
    }
}

/// The appenders for one sub-shard: `b`, `txs` and `logs` always exist, `4626` lazily.
/// Each appender holds one open handle and buffers its lines; [`Self::flush_since`] pushes
/// them to the OS (called once per save batch and at the seal step).
#[derive(Debug)]
pub struct SubShardFiles {
    /// Block-header records (`b.jsonl`).
    pub bt: JsonlAppender,
    /// Transaction records.
    pub txs: JsonlAppender,
    /// Per-block log records; a block with no logs appends no line.
    pub logs: JsonlAppender,
    /// ERC-4626 event records; opened on the first event and absent for sub-shards with none
    /// (seal skips `4626` if it does not exist).
    erc4626: Option<JsonlAppender>,
}

impl SubShardFiles {
    /// Returns appenders for `shard` under `base`, creating the shard directory. The
    /// `b`, `txs` and `logs` files appear on their first record; `4626` is created lazily
    /// on the first [`Self::append_4626`].
    pub fn open(base: impl AsRef<Path>, shard: ShardId) -> FileTierResult<Self> {
        Ok(Self {
            bt: JsonlAppender::open(active_file_path(base.as_ref(), shard, "b"))?,
            txs: JsonlAppender::open(active_file_path(base.as_ref(), shard, "txs"))?,
            logs: JsonlAppender::open(active_file_path(base.as_ref(), shard, "logs"))?,
            erc4626: None,
        })
    }

    /// Appends one block-header record.
    pub fn append_bt(&mut self, record: &BtRecord) -> FileTierResult<()> {
        self.bt.append_json_line(record)?;
        Ok(())
    }

    /// Appends one full transaction record.
    pub fn append_tx(&mut self, record: &TxRecord) -> FileTierResult<()> {
        self.txs.append_json_line(record)?;
        Ok(())
    }

    /// Appends one block's logs record.
    pub fn append_logs(&mut self, record: &BlockLogsRecord) -> FileTierResult<()> {
        self.logs.append_json_line(record)?;
        Ok(())
    }

    /// Appends one stub transaction record (unsupported tx, still included).
    pub fn append_tx_stub(&mut self, record: &TxStubRecord) -> FileTierResult<()> {
        self.txs.append_json_line(record)?;
        Ok(())
    }

    /// Appends one block-with-events record, creating `4626.jsonl` on first use.
    pub fn append_4626(
        &mut self,
        base: impl AsRef<Path>,
        shard: ShardId,
        record: &Erc4626Record,
    ) -> FileTierResult<()> {
        if self.erc4626.is_none() {
            self.erc4626 =
                Some(JsonlAppender::open(active_file_path(base.as_ref(), shard, "4626"))?);
        }
        self.erc4626.as_mut().expect("just opened").append_json_line(record)?;
        Ok(())
    }

    /// Whether the lazy `4626` file has been created (i.e. this sub-shard has events).
    pub const fn has_erc4626(&self) -> bool {
        self.erc4626.is_some()
    }

    /// Flushes every open file's buffered lines to the OS (no fsync) and returns the per-file
    /// "landed" marks: how long after `start` each file that had lines this round reached the
    /// OS. Called once per save batch, before the DB commit, so the file records are at or
    /// ahead of the committed head; a file with no buffered lines reports `None` (nothing
    /// landed).
    pub fn flush_since(&mut self, start: Instant) -> FileTierResult<FileWriteTimes> {
        let mut landed = FileWriteTimes::default();
        if self.bt.flush_if_dirty()? {
            landed.b = Some(start.elapsed());
        }
        if self.txs.flush_if_dirty()? {
            landed.txs = Some(start.elapsed());
        }
        if self.logs.flush_if_dirty()? {
            landed.logs = Some(start.elapsed());
        }
        if let Some(erc4626) = self.erc4626.as_mut() &&
            erc4626.flush_if_dirty()?
        {
            landed.erc4626 = Some(start.elapsed());
        }
        Ok(landed)
    }

    /// Seal-step hook: flush every buffered line (marks measured from `start`), then leave
    /// the files at their `.jsonl` paths (the engine's seal step compresses them).
    pub fn finish_all(mut self, start: Instant) -> FileTierResult<FileWriteTimes> {
        self.flush_since(start)
    }
}

/// The result of one [`append_records_and_seal`] batch: the sub-shards that hit their seal
/// boundary (for the caller's off-path compression) and the per-file "landed" marks — how
/// long after the batch start each payload file's last write reached the OS.
#[derive(Debug, Default)]
pub struct AppendOutcome {
    /// Sub-shards completed by this batch (their `.jsonl` files await the engine's seal).
    pub sealed: Vec<ShardId>,
    /// Per-file landing marks for the batch, merged across every flush round it performed.
    pub landed: FileWriteTimes,
}

/// THE shared storage-cycle append routine — the single implementation every accepted
/// block goes through, engine-sealed or pipeline-sourced alike ("one arrives — one fully
/// stored in both Fjall and FileSystem"): for each record, its immutable payload lines are
/// appended to the active sub-shard's files; any sub-shard whose boundary block lands in
/// the batch is completed and returned for the caller's off-path compression; at the end
/// the active files are flushed to the OS so the files are never behind the caller's
/// committed head.
///
/// The batch's landing marks are measured from `start` (the save batch's beginning).
pub fn append_records_and_seal(
    writer: &mut FileTierWriter,
    records: &[BlockRecords],
    start: Instant,
) -> FileTierResult<AppendOutcome> {
    writer.begin_batch(start);
    let mut sealed: Vec<ShardId> = Vec::new();
    for rec in records {
        let number = rec.bt.b;
        let base = writer.base().to_path_buf();
        let files = writer.files_for_block(number)?;
        files.append_bt(&rec.bt)?;
        for tx in &rec.txs {
            files.append_tx(tx)?;
        }
        if let Some(logs) = &rec.logs {
            files.append_logs(logs)?;
        }
        if let Some(ev) = &rec.erc4626 {
            files.append_4626(&base, get_shard(number), ev)?;
        }

        // Pure math, no counting: the boundary block (`stageShardY == 1`) is the seal
        // trigger — every record of the sub-shard has been appended by the time its last
        // block is written, so the shard is complete by construction. The `.jsonl` stays
        // the source until the caller's off-path compression lands (A3); the next shard
        // opens lazily on the next block.
        if writer.is_seal_boundary(number)
            && let Some(finished) = writer.finish_active()? {
                sealed.push(finished);
            }
    }
    // One flush per file per batch: the buffered lines reach the OS before the caller's
    // commit advances the head.
    writer.flush_active()?;
    Ok(AppendOutcome { sealed, landed: writer.take_landed() })
}

/// Writes block 0 (the genesis header) as the first `b` record of the file tier.
///
/// Genesis is header-only: its allocations are state, not transactions, so this appends one
/// `b` line (with `tc: 0`) and creates no `txs`/`logs`/`4626` file. The header is stored
/// verbatim — including its parent hash (zero for a normal genesis) and the supplied block
/// hash — so the first historical record is at height 0 instead of starting at block 1.
pub fn write_genesis_block<H: BlockHeader>(
    base: impl AsRef<Path>,
    header: &H,
    block_hash: B256,
) -> FileTierResult<()> {
    let records = extract_empty_block(header, block_hash);
    let mut writer = FileTierWriter::new(base.as_ref().to_path_buf());
    append_records_and_seal(&mut writer, &[records], Instant::now())?;
    writer.finish_active()?;
    Ok(())
}

/// The file-tier writer: holds the currently active sub-shard's appenders, opening a new
/// sub-shard lazily when the block height crosses a boundary. It also carries the current
/// save batch's landing measurement: [`Self::batch_start`] is the clock every flush's
/// [`FileWriteTimes`] are read against, and [`Self::landed`] accumulates the marks until
/// the caller takes them ([`Self::take_landed`]).
#[derive(Debug)]
pub struct FileTierWriter {
    base: PathBuf,
    active: Option<(ShardId, SubShardFiles)>,
    /// The current batch's start; marks are `start.elapsed()` at each file's flush.
    batch_start: Instant,
    /// Merged landing marks for the current batch, taken at batch end.
    landed: FileWriteTimes,
}

impl FileTierWriter {
    /// Creates a writer rooted at `base` (the chain datadir — the engine dir's parent, so
    /// files land at `<datadir>/blocks/{S}/{Y:02}/`, beside the database, not inside it).
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            active: None,
            batch_start: Instant::now(),
            landed: FileWriteTimes::default(),
        }
    }

    /// The base directory this writer writes under.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The currently active sub-shard, if any.
    pub fn active_shard(&self) -> Option<ShardId> {
        self.active.as_ref().map(|(shard, _)| *shard)
    }

    /// Returns the open files for `block_number`'s sub-shard, opening them if the height moved
    /// to a new sub-shard.
    pub fn files_for_block(&mut self, block_number: u64) -> FileTierResult<&mut SubShardFiles> {
        let shard = get_shard(block_number);
        let needs_open = self.active.as_ref().map(|(cur, _)| *cur != shard).unwrap_or(true);
        if needs_open {
            let files = SubShardFiles::open(&self.base, shard)?;
            self.active = Some((shard, files));
        }
        Ok(&mut self.active.as_mut().expect("just opened").1)
    }

    /// Starts a new measurement batch: every flush from here on records marks relative to
    /// `start`, and any marks left over from a previous batch are dropped.
    fn begin_batch(&mut self, start: Instant) {
        self.batch_start = start;
        self.landed = FileWriteTimes::default();
    }

    /// Takes the batch's merged per-file landing marks (leaving them reset for the next
    /// batch). Called once the batch's final flush has run.
    fn take_landed(&mut self) -> FileWriteTimes {
        std::mem::take(&mut self.landed)
    }

    /// Flushes the active sub-shard's buffered lines and merges its landing marks into the
    /// batch's. The persistence service calls this once per save batch, before the DB
    /// commit, so the file records are never behind the head.
    pub fn flush_active(&mut self) -> FileTierResult<()> {
        if let Some((_, files)) = self.active.as_mut() {
            let marks = files.flush_since(self.batch_start)?;
            self.landed.merge(marks);
        }
        Ok(())
    }

    /// Clears the active sub-shard (seal-step hook), merges its landing marks into the
    /// batch's, and returns the shard that was active. Files remain at their `.jsonl`
    /// names; the caller (the engine) compresses and seals them.
    pub fn finish_active(&mut self) -> FileTierResult<Option<ShardId>> {
        let Some((shard, files)) = self.active.take() else { return Ok(None) };
        let marks = files.finish_all(self.batch_start)?;
        self.landed.merge(marks);
        Ok(Some(shard))
    }

    /// Whether `block_number` is the last in its sub-shard (the seal trigger).
    pub const fn is_seal_boundary(&self, block_number: u64) -> bool {
        crate::shard::is_last_block_of_shard(block_number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        records::{BtRecord, Erc4626Event, Erc4626Record},
        seal::sealed_file_path,
    };
    use flate2::read::MultiGzDecoder;
    use std::io::Read;

    /// Reads a file's lines, decompressing if it is a sealed `.gz`.
    fn read_all_lines(path: &Path) -> Vec<String> {
        let out = if path.extension().is_some_and(|e| e == "gz") {
            let mut decoder = MultiGzDecoder::new(std::fs::File::open(path).unwrap());
            let mut s = String::new();
            decoder.read_to_string(&mut s).unwrap();
            s
        } else {
            std::fs::read_to_string(path).unwrap()
        };
        out.lines().map(str::to_string).collect()
    }

    fn bt(b: u64) -> BtRecord {
        BtRecord {
            b,
            h: format!("0x{b:04x}"),
            ph: "0xparent".into(),
            ts: 1,
            tc: 0,
            gu: 0,
            gl: 30_000_000,
            m: "0xminer".into(),
            ex: "0x".into(),
            ..Default::default()
        }
    }

    /// Genesis is written as a single `b` record at height 0: empty body (no `txs`/`logs`/
    /// `4626` files) and the header's zero parent hash, and it round-trips through the reader.
    #[test]
    fn write_genesis_block_is_one_b_record_with_zero_parent() {
        let dir = tempfile::tempdir().unwrap();
        let header = alloy_consensus::Header {
            number: 0,
            gas_limit: 30_000_000,
            timestamp: 1_700_000_000,
            ..Default::default()
        };
        let hash = B256::repeat_byte(0xab);
        write_genesis_block(dir.path(), &header, hash).unwrap();

        let shard = get_shard(0);
        let b_path = active_file_path(dir.path(), shard, "b");
        let lines = read_all_lines(&b_path);
        assert_eq!(lines.len(), 1, "genesis is exactly one b line: {lines:?}");
        assert!(lines[0].contains("\"b\":0"), "genesis height is 0: {}", lines[0]);
        assert!(lines[0].contains("\"tc\":0"), "genesis body is empty: {}", lines[0]);
        assert_eq!(
            header.parent_hash,
            B256::ZERO,
            "a genesis header must carry a zero parent hash"
        );
        // No transaction/log/4626 line is created for a header-only genesis.
        assert!(!active_file_path(dir.path(), shard, "txs").exists());
        assert!(!active_file_path(dir.path(), shard, "logs").exists());
        assert!(!active_file_path(dir.path(), shard, "4626").exists());

        // The record round-trips back into the exact genesis header.
        let bundle = crate::reader::read_block_at(dir.path(), 0).unwrap().expect("block 0");
        assert_eq!(bundle.bt.b, 0);
        assert!(bundle.txs.is_empty());
        let rebuilt = crate::reader::build_header(&bundle.bt).unwrap();
        assert_eq!(rebuilt, header);
    }

    /// Appending records leaves plain `.jsonl` files; the engine seals them into `.gz`.
    /// No file exists before its first record: a bt-only block seals only `b` — txs/logs
    /// simply never existed (missing = empty; the reader serves empty for absent files).
    #[test]
    fn append_then_finish_then_engine_seal() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = FileTierWriter::new(dir.path());

        for b in 0..3u64 {
            writer.files_for_block(b).unwrap().append_bt(&bt(b)).unwrap();
        }
        let finished = writer.finish_active().unwrap();
        assert_eq!(finished, Some(ShardId { s: 0, y: 0 }));
        // Writer did not seal: the file is still the plain active `.jsonl`.
        assert!(active_file_path(dir.path(), ShardId { s: 0, y: 0 }, "b").exists());

        let sealed = crate::seal::seal_shard(dir.path(), ShardId { s: 0, y: 0 }).unwrap();
        assert_eq!(sealed.files.len(), 1, "only b existed; txs/logs never created");
        let shard = get_shard(0);
        let lines = read_all_lines(&sealed_file_path(dir.path(), shard, "b"));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\"b\":0"));
        assert!(lines[2].contains("\"b\":2"));
    }

    /// Appends are buffered: the `b.jsonl` line reaches the OS at the per-batch flush (or drop),
    /// not on the append itself.
    #[test]
    fn append_populates_active_before_finish() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = FileTierWriter::new(dir.path());
        writer.files_for_block(0).unwrap().append_bt(&bt(0)).unwrap();

        let active = active_file_path(dir.path(), get_shard(0), "b");
        let buffered = std::fs::read_to_string(&active).unwrap_or_default();
        assert!(
            !buffered.contains("\"b\":0"),
            "the record must be buffered until flush: {buffered:?}"
        );

        writer.flush_active().unwrap();
        let out = std::fs::read_to_string(&active).unwrap();
        assert!(out.contains("\"b\":0"), "the record must be a plain line after flush: {out:?}");
    }

    /// The 4626 file is created lazily: a sub-shard with no events leaves no `4626.jsonl`.
    #[test]
    fn erc4626_is_lazy_absent_without_events() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = FileTierWriter::new(dir.path());
        writer.files_for_block(0).unwrap().append_bt(&bt(0)).unwrap();
        let files = writer.files_for_block(0).unwrap();
        assert!(!files.has_erc4626());
        writer.finish_active().unwrap();
        assert!(!active_file_path(dir.path(), get_shard(0), "4626").exists());
    }

    /// The first `append_4626` creates the file; events round-trip.
    #[test]
    fn erc4626_created_on_first_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = FileTierWriter::new(dir.path());
        let shard = get_shard(0);
        {
            let files = writer.files_for_block(0).unwrap();
            assert!(!files.has_erc4626());
            files
                .append_4626(
                    dir.path(),
                    shard,
                    &Erc4626Record {
                        b: 0,
                        ev: vec![Erc4626Event {
                            addr: "0xvault".into(),
                            t: vec!["0xsig".into()],
                            d: "0x".into(),
                            tx: "0xtx".into(),
                            i: 0,
                        }],
                    },
                )
                .unwrap();
            assert!(files.has_erc4626());
        }
        writer.finish_active().unwrap();
        let lines = read_all_lines(&active_file_path(dir.path(), shard, "4626"));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"addr\":\"0xvault\""));
    }

    /// `is_seal_boundary` fires only on the last block of a sub-shard.
    #[test]
    fn seal_boundary_matches_shard_end() {
        let dir = tempfile::tempdir().unwrap();
        let writer = FileTierWriter::new(dir.path());
        assert!(!writer.is_seal_boundary(9_998));
        assert!(writer.is_seal_boundary(9_999));
    }

    /// A writer that crosses a sub-shard boundary opens the new shard's files lazily.
    #[test]
    fn crossing_boundary_opens_new_shard() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = FileTierWriter::new(dir.path());

        writer.files_for_block(9_998).unwrap();
        assert_eq!(writer.active_shard(), Some(ShardId { s: 0, y: 0 }));
        writer.files_for_block(10_000).unwrap();
        assert_eq!(writer.active_shard(), Some(ShardId { s: 0, y: 1 }));
    }

    /// `finish_active` is a no-op when nothing is open.
    #[test]
    fn finish_active_without_open_shard_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = FileTierWriter::new(dir.path());
        assert_eq!(writer.finish_active().unwrap(), None);
    }

    /// A "crash" (drop without finish) loses nothing: each line was already on disk. A fresh
    /// handle appends more lines via `O_APPEND`; sealing reads them all.
    #[test]
    fn reopen_after_crash_preserves_records() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut writer = FileTierWriter::new(dir.path());
            writer.files_for_block(0).unwrap().append_bt(&bt(0)).unwrap();
            // drop without finish_active = crash before seal
        }
        {
            let mut writer = FileTierWriter::new(dir.path());
            writer.files_for_block(1).unwrap().append_bt(&bt(1)).unwrap();
            writer.finish_active().unwrap();
            crate::seal::seal_shard(dir.path(), get_shard(0)).unwrap();
        }

        let lines = read_all_lines(&sealed_file_path(dir.path(), get_shard(0), "b"));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"b\":0"));
        assert!(lines[1].contains("\"b\":1"));
    }
}
