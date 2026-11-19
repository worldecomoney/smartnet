//! The encapsulated storage engine: one type owns both stores (file-tier + Fjall).
//!
//! Callers save blocks, recover, and shut down through this type; the FS/DB split,
//! write ordering (FS-append → DB-commit, one fsync), and the recovery contract live
//! only here (STORAGE.md). Reads stay on the existing provider traits — they are
//! already encapsulated by `DatabaseProvider`.

use crate::{
    keys,
    providers::{ProviderFactory, ProviderNodeTypes},
    BlockExecutionWriter, BlockNumReader, ChainStateBlockWriter, DBProvider,
    DatabaseProviderFactory, MetadataProvider, MetadataWriter, ProviderError, ProviderResult,
    StateWriter,
};
use alloy_eips::BlockNumHash;
use smartnet_chain_state::ExecutedBlock;
use smartnet_ethereum_primitives::TransactionSigned;
use smartnet_file_tier::{
    extract_block, seal_shard, BlockRecords, FileTierError, FileTierWriter, FileWriteTimes,
    SignatureAccess,
};
use smartnet_primitives_traits::{BlockBody, NodePrimitives};
use smartnet_tasks::spawn_os_thread;
use smartnet_timing::{fmt_wall, fmt_wall_opt};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use thiserror::Error;
use tracing::{debug, error, instrument};

/// Errors from [`StorageEngine`] operations.
#[derive(Debug, Error)]
pub enum StorageError {
    /// A provider (Fjall) error.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// A file-tier (gz payload files) error.
    #[error(transparent)]
    FileTier(#[from] FileTierError),
}

/// Timing/identity outcome of one [`StorageEngine::save_blocks`] call.
///
/// Segment durations are returned so the caller (persistence service) can record
/// metrics and wall-clock logs without the engine depending on engine-tree types.
#[derive(Debug)]
pub struct SaveBlocksOutcome {
    /// Last block persisted in this batch, if any.
    pub last_block: Option<BlockNumHash>,
    /// Wall time of the whole save (extract → FS append → DB commit).
    pub total: Duration,
    /// FS append + seal duration.
    pub fs_append: Duration,
    /// DB buffer phase (state + trie + index into the open tx).
    pub state_buffer: Duration,
    /// DB commit (the batch's only fsync).
    pub commit_fsync: Duration,
    /// Whether the file tier was enabled for this save.
    pub file_tier_on: bool,
    /// Finalized number to re-queue (was above this batch's last block).
    pub deferred_finalized: Option<u64>,
    /// Safe number to re-queue (was above this batch's last block).
    pub deferred_safe: Option<u64>,
}

/// The encapsulated storage engine: the single write/recovery protocol owner for
/// both the file tier (immutable payload) and Fjall (state, index, control).
///
/// **Write protocol** ([`Self::save_blocks`]): extract records → FS append (page cache,
/// no fsync) → DB index/state/trie → single `SyncData` fsync → changeset-window trim —
/// all in one ordered sequence the caller never re-implements.
///
/// **Unwind:** only [`Self::restore_unwind`] (cold-shutdown −2, Phase 3); no other
/// unwind path is exposed here.
pub struct StorageEngine<N: ProviderNodeTypes> {
    provider: ProviderFactory<N>,
    file_tier: Option<FileTierWriter>,
    changeset_window: u64,
    seal_tasks: Vec<std::thread::JoinHandle<()>>,
}

impl<N: ProviderNodeTypes> std::fmt::Debug for StorageEngine<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageEngine")
            .field("file_tier", &self.file_tier)
            .field("changeset_window", &self.changeset_window)
            .field("seal_tasks", &self.seal_tasks.len())
            .finish_non_exhaustive()
    }
}

impl<N: ProviderNodeTypes> StorageEngine<N> {
    /// Creates the engine over `provider`. `file_tier_base: None` disables the file
    /// tier (tests / tier-off mode) — payload writes then skip the FS append.
    pub fn new(
        provider: ProviderFactory<N>,
        file_tier_base: Option<PathBuf>,
        changeset_window: u64,
    ) -> Self {
        Self {
            provider,
            file_tier: file_tier_base.map(FileTierWriter::new),
            changeset_window,
            seal_tasks: Vec::new(),
        }
    }

    /// The underlying provider factory (for callers that need DB-only operations
    /// outside the save protocol — e.g. the persistence service's pruner).
    pub const fn provider(&self) -> &ProviderFactory<N> {
        &self.provider
    }

    /// The first block number whose changeset rows are retained for a head of `head`.
    pub const fn changeset_floor(&self, head: u64) -> u64 {
        head.saturating_add(1).saturating_sub(self.changeset_window)
    }

    /// Waits for in-flight background seal compressions (clean-shutdown join point).
    pub fn join_seal_tasks(&mut self) {
        for handle in self.seal_tasks.drain(..) {
            let _ = handle.join();
        }
    }

    /// Appends file-tier records and seals any sub-shard whose boundary landed.
    ///
    /// Pure FS: never touches Fjall, never fsyncs (the DB commit is the batch's only
    /// durability point). Returns per-file landing marks.
    pub fn append_file_tier_and_seal(
        &mut self,
        records: &[BlockRecords],
        start: Instant,
    ) -> Result<FileWriteTimes, StorageError> {
        let Some(writer) = self.file_tier.as_mut() else {
            return Ok(FileWriteTimes::default());
        };
        let outcome = smartnet_file_tier::append_records_and_seal(writer, records, start)?;
        for shard in outcome.sealed {
            let base = writer.base().to_path_buf();
            debug!(target: "storage::engine", shard = ?shard, "Closed sub-shard for sealing");
            let handle = spawn_os_thread("file-tier-seal", move || match seal_shard(&base, shard) {
                Ok(sealed) => debug!(
                    target: "storage::engine",
                    shard = ?shard,
                    files = sealed.files.len(),
                    "background seal compression finished"
                ),
                Err(err) => error!(
                    target: "storage::engine",
                    %err,
                    shard = ?shard,
                    "background seal compression failed; reconciled on next boot"
                ),
            });
            self.seal_tasks.push(handle);
        }
        Ok(outcome.landed)
    }

    /// Deletes changeset rows below the retention floor and persists the floor metadata,
    /// in the caller's open transaction (atomic with the head advance).
    fn limit_changesets(
        &self,
        provider_rw: &(impl StateWriter + MetadataWriter),
        head: u64,
    ) -> ProviderResult<usize> {
        let floor = self.changeset_floor(head);
        let removed = provider_rw.remove_changesets_below(floor)?;
        provider_rw.write_metadata(keys::CHANGESET_WINDOW_FLOOR, floor.to_be_bytes().to_vec())?;
        Ok(removed)
    }
}

impl<N> StorageEngine<N>
where
    N: ProviderNodeTypes,
    <N::Primitives as NodePrimitives>::SignedTx: SignatureAccess,
{
    /// The single block-write protocol (STORAGE.md W1, closed here).
    ///
    /// Order is load-bearing:
    /// 1. extract `BlockRecords` (stamp `ft` = first tx num),
    /// 2. FS append + seal (page cache, no fsync),
    /// 3. DB `save_block_state_and_index` + finalized/safe + changeset trim,
    /// 4. one `SyncData` commit — the batch's only durability point.
    ///
    /// `finalized`/`safe` are deferred: values above this batch's last block are
    /// returned in the outcome for the caller to re-queue with the next save.
    #[instrument(level = "debug", target = "storage::engine", skip_all, fields(block_count = blocks.len()))]
    pub fn save_blocks(
        &mut self,
        blocks: Vec<ExecutedBlock<N::Primitives>>,
        finalized: Option<u64>,
        safe: Option<u64>,
    ) -> Result<SaveBlocksOutcome, StorageError> {
        let first_block = blocks.first().map(|b| b.recovered_block.num_hash());
        let last_block = blocks.last().map(|b| b.recovered_block.num_hash());
        let block_count = blocks.len();

        debug!(target: "storage::engine", ?block_count, first=?first_block, last=?last_block, "Saving range of blocks");

        let start_time = Instant::now();
        let files_start = std::time::Instant::now();

        let mut seg_fs_append = Duration::ZERO;
        let seg_state_buffer;
        let seg_commit;
        let mut deferred_finalized = None;
        let mut deferred_safe = None;
        let file_tier_on = self.file_tier.is_some();

        if let Some(last) = last_block {
            // Open the write transaction first: file-tier extraction needs the next
            // transaction number, the same counter `save_block_state_and_index` assigns from.
            let provider_rw = self.provider.database_provider_rw()?;

            // Stage 6A: extract file-tier records; stamp each block's first tx number.
            let file_records = if file_tier_on {
                let mut next_tx = provider_rw.next_tx_number()?;
                let mut records = Vec::with_capacity(blocks.len());
                for b in &blocks {
                    let mut record = extract_block(
                        b.recovered_block.header(),
                        b.recovered_block.hash(),
                        b.recovered_block.body().transactions(),
                        b.recovered_block.senders(),
                        &b.execution_outcome().receipts,
                        b.recovered_block.body().withdrawals().map(|w| w.as_slice()),
                    );
                    record.bt.ft = Some(next_tx);
                    next_tx += b.recovered_block.body().transaction_count() as u64;
                    records.push(record);
                }
                Some(records)
            } else {
                None
            };

            // (a) FS append + seal — page cache only; recovery handles a crash (drop-2).
            let file_writes = if let Some(records) = &file_records {
                let seg_start = Instant::now();
                let writes = self.append_file_tier_and_seal(records, files_start)?;
                seg_fs_append = seg_start.elapsed();
                writes
            } else {
                FileWriteTimes::default()
            };
            if let Some(all) = file_writes.all_landed() {
                debug!(
                    target: "storage::engine",
                    first = ?first_block,
                    last = ?last_block,
                    b_landed = %fmt_wall_opt(file_writes.b),
                    txs_landed = %fmt_wall_opt(file_writes.txs),
                    logs_landed = %fmt_wall_opt(file_writes.logs),
                    erc4626_landed = %fmt_wall_opt(file_writes.erc4626),
                    all_landed = %fmt_wall(all),
                    "file writes landed"
                );
            }

            // (b) DB: index + state + finalized/safe + changeset trim, one transaction.
            let seg_start = Instant::now();
            provider_rw.save_block_state_and_index(blocks)?;

            if let Some(finalized) = finalized {
                provider_rw.save_finalized_block_number(finalized.min(last.number))?;
                if finalized > last.number {
                    deferred_finalized = Some(finalized);
                }
            }
            if let Some(safe) = safe {
                provider_rw.save_safe_block_number(safe.min(last.number))?;
                if safe > last.number {
                    deferred_safe = Some(safe);
                }
            }

            let removed = self.limit_changesets(&provider_rw, last.number)?;
            if removed > 0 {
                debug!(
                    target: "storage::engine",
                    head = last.number,
                    floor = self.changeset_floor(last.number),
                    removed,
                    "Trimmed changesets below retention window"
                );
            }
            seg_state_buffer = seg_start.elapsed();

            // The commit is the batch's only durability point (one Fjall batch + SyncData).
            let commit_start = Instant::now();
            provider_rw.commit()?;
            seg_commit = commit_start.elapsed();

            debug!(
                target: "storage::engine",
                first=?first_block,
                last=?last_block,
                file_tier_on,
                total = %fmt_wall(start_time.elapsed()),
                fs_append = %fmt_wall(seg_fs_append),
                state_buffer = %fmt_wall(seg_state_buffer),
                commit_fsync = %fmt_wall(seg_commit),
                "Saved range of blocks"
            );
        } else {
            seg_state_buffer = Duration::ZERO;
            seg_commit = Duration::ZERO;
        }

        Ok(SaveBlocksOutcome {
            last_block,
            total: start_time.elapsed(),
            fs_append: seg_fs_append,
            state_buffer: seg_state_buffer,
            commit_fsync: seg_commit,
            file_tier_on,
            deferred_finalized,
            deferred_safe,
        })
    }
}

// ---------------------------------------------------------------------------
// Genesis (W2 / W4): the dual-store knowledge lives here, not in `init/genesis`.
// ---------------------------------------------------------------------------

/// Where a genesis bytecode write landed: the file tier or the `Bytecodes` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BytecodeDestination {
    /// Written to the file tier (production; tier enabled).
    FileTier,
    /// Caller must write the `Bytecodes` table (tier-disabled test mode only).
    DatabaseTable,
}

/// Writes the genesis payload to the file tier if not already present (idempotent),
/// then flushes. The DB index rows (`CanonicalChain`, `HeaderNumbers`) stay in the
/// caller's open transaction — this function owns only the FS half of the dual write.
/// `txs` = the calibri\* pre-fund records (GENESIS.md — real tx listings in block 0,
/// state still comes from the alloc); empty for legacy clique genesis.
pub fn save_genesis_payload<H: alloy_consensus::BlockHeader>(
    base: &std::path::Path,
    header: &H,
    block_hash: alloy_primitives::B256,
    txs: &[TransactionSigned],
) -> ProviderResult<()> {
    let existing = smartnet_file_tier::read_block_at(base, header.number())
        .map_err(ProviderError::other)?;
    if existing.is_none() {
        smartnet_file_tier::write_genesis_block_with_txs(base, header, block_hash, txs)
            .map_err(ProviderError::other)?;
        smartnet_file_tier::flush(base).map_err(ProviderError::other)?;
    }
    Ok(())
}

/// Writes a contract's bytecode to its tier home: the file tier when enabled (the
/// `Bytecodes` table is never written then), otherwise reports
/// [`BytecodeDestination::DatabaseTable`] so the caller writes the table. The file is
/// keyed by the deployed address (operator layout).
pub fn write_bytecode(
    base: Option<&std::path::Path>,
    address: &alloy_primitives::Address,
    bytes: &[u8],
) -> ProviderResult<BytecodeDestination> {
    match base {
        Some(base) => {
            smartnet_file_tier::write_bytecode(base, address, bytes)
                .map_err(ProviderError::other)?;
            Ok(BytecodeDestination::FileTier)
        }
        None => Ok(BytecodeDestination::DatabaseTable),
    }
}

// ---------------------------------------------------------------------------
// Boot recovery / shutdown (W5): the only place both stores are touched together
// for lifecycle. `restore_unwind(2)` is the ONLY unwind (STORAGE.md §3.1).
// ---------------------------------------------------------------------------

impl<N: ProviderNodeTypes> StorageEngine<N> {
    /// Reads and clears `SHUTDOWN_CLEAN`; returns whether the last exit was clean.
    ///
    /// Presence is the whole signal — no clock comparison. The marker is cleared
    /// immediately so this run's exit state is decided by whether the shutdown
    /// handler reaches the exit path, not by a stale value.
    pub fn take_shutdown_clean(&self) -> bool {
        let raw = match self.provider.provider() {
            Ok(provider) => provider.get_metadata(keys::SHUTDOWN_CLEAN).ok().flatten(),
            Err(err) => {
                tracing::warn!(
                    target: "storage::engine",
                    %err, "cannot read SHUTDOWN_CLEAN; assuming no clean shutdown"
                );
                None
            }
        };
        let clean = raw.is_some_and(|bytes| !bytes.is_empty());
        match self.provider.database_provider_rw() {
            Ok(provider_rw) => {
                let cleared = provider_rw
                    .write_metadata(keys::SHUTDOWN_CLEAN, Vec::new())
                    .and_then(|()| provider_rw.commit());
                if let Err(err) = cleared {
                    tracing::error!(
                        target: "storage::engine",
                        %err, "failed to clear SHUTDOWN_CLEAN"
                    );
                }
            }
            Err(err) => {
                tracing::error!(
                    target: "storage::engine",
                    %err, "failed to open provider to clear SHUTDOWN_CLEAN"
                );
            }
        }
        clean
    }

    /// The ONLY unwind: cold-shutdown −2 surgical cut (STORAGE.md §3.1).
    ///
    /// 1. FS: drop the last 2 whole lines from the active `b.jsonl` (keeps genesis),
    ///    align `txs`/`logs`/`4626` to the resulting `BX`.
    /// 2. Read `BX` = last complete block that remains.
    /// 3. DB: `remove_block_and_execution_above(BX)` if head > BX; commit.
    /// 4. Re-sync resumes at `BX + 1`.
    ///
    /// Hard-wired to −2 — there is no general `restore_unwind(n)`. No other unwind
    /// path exists or may exist (sync failures drop and retry; they never unwind).
    pub fn restore_unwind(&self, base: &std::path::Path) -> Result<u64, StorageError> {
        let file_tip = smartnet_file_tier::remove_last_lines(base)?;
        let provider_rw = self.provider.database_provider_rw()?;
        let head = provider_rw.last_block_number()?;
        // The DB commit is the durability point: a range save appends its lines first
        // and commits once, so the file may legitimately lead the committed DB after a
        // crash. Anchor on whichever is BEHIND and pull the file back to it — never
        // re-sync from a block that was never committed (that would leave a permanent
        // hole), and never keep file lines the DB does not have (a later import would
        // append the same blocks twice into a shard a seal has already closed).
        let bx = file_tip.min(head);
        if file_tip > bx {
            smartnet_file_tier::align_active_to_block(base, bx)?;
            tracing::info!(
                target: "storage::engine",
                file_tip, head, bx,
                "file tier led the committed DB — trimmed the active files back to the committed tip"
            );
        }
        if head > bx {
            provider_rw.remove_block_and_execution_above(bx)?;
            provider_rw.commit()?;
            tracing::info!(
                target: "storage::engine",
                bx, head,
                "restore_unwind(2): dropped last 2 FS lines and truncated DB to BX; re-sync from BX + 1"
            );
        } else {
            tracing::info!(
                target: "storage::engine",
                bx, head,
                "restore_unwind(2): DB head at or below BX; nothing to truncate; re-sync from BX + 1"
            );
        }
        Ok(bx)
    }

    /// Boot-time recovery: reconcile half-finished seals, then — only on a cold
    /// restart (no `SHUTDOWN_CLEAN`) — [`Self::restore_unwind`]. Returns the head
    /// after recovery (BX on cold path, current head on clean path).
    pub fn recover_on_boot(
        &self,
        base: &std::path::Path,
    ) -> Result<u64, StorageError> {
        smartnet_file_tier::reconcile_seals(base).map_err(StorageError::FileTier)?;
        if self.take_shutdown_clean() {
            tracing::info!(target: "storage::engine", "clean shutdown detected; skipping crash recovery");
            let head = self
                .provider
                .database_provider_rw()?
                .last_block_number()?;
            return Ok(head);
        }
        self.restore_unwind(base)
    }

    /// Graceful-exit protocol: flush the FS (gz appends are page-cached), then write
    /// `SHUTDOWN_CLEAN` **last** — its durability implies both stores are durable.
    pub fn shutdown_clean(&self, base: &std::path::Path) -> Result<(), StorageError> {
        smartnet_file_tier::flush(base).map_err(StorageError::FileTier)?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default();
        let provider_rw = self.provider.database_provider_rw()?;
        provider_rw
            .write_metadata(keys::SHUTDOWN_CLEAN, timestamp.to_be_bytes().to_vec())?;
        provider_rw.commit()?;
        tracing::info!(
            target: "storage::engine",
            timestamp, "recorded clean shutdown (SHUTDOWN_CLEAN)"
        );
        Ok(())
    }
}
