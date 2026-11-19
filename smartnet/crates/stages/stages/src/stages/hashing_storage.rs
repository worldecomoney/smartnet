use alloy_primitives::{b256, bytes::BufMut, keccak256, Address, B256};
use itertools::Itertools;
use smartnet_primitives_traits::StorageEntry;
use smartnet_stages_api::{
    BlockRangeOutput, EntitiesCheckpoint, ExecInput, ExecOutput, Stage, StageCheckpoint,
    StageError, StageId, StorageHashingCheckpoint,
};
use smartnet_config::config::{EtlConfig, HashingConfig};
use smartnet_db_api::{
    cursor::{DbCursorRO, DbDupCursorRW},
    models::{BlockNumberAddress, CompactU256},
    table::Decompress,
    tables,
    transaction::{DbTx, DbTxMut},
};
use smartnet_provider::etl::Collector;
use smartnet_provider::{DBProvider, HashingWriter, StatsReader, StorageReader};
use smartnet_storage_api::StorageSettingsCache;
use smartnet_storage_errors::provider::ProviderResult;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    io,
    sync::mpsc::{self, Receiver},
};
use tracing::*;

/// Maximum number of channels that can exist in memory.
const MAXIMUM_CHANNELS: usize = 10_000;

/// Maximum number of storage entries to hash per rayon worker job.
const WORKER_CHUNK_SIZE: usize = 100;

/// One worker's result stream plus its expected entry count — a worker that dies
/// mid-chunk (panic) shows up as a short count instead of a silent successful collect.
type HashChannel = Receiver<(Vec<u8>, CompactU256)>;

/// Keccak256 hash of the zero address.
const HASHED_ZERO_ADDRESS: B256 =
    b256!("0x5380c7b7ae81a58eb98d9c78de4a1fd7fd9535fc953ed2be602daaa41767312a");

/// Storage hashing stage hashes plain storage.
/// This is preparation before generating intermediate hashes and calculating Merkle tree root.
#[derive(Debug)]
pub struct StorageHashingStage {
    /// The threshold (in number of blocks) for switching between incremental
    /// hashing and full storage hashing.
    pub clean_threshold: u64,
    /// The maximum number of slots to process before committing during unwind.
    pub commit_threshold: u64,
    /// The maximum number of changeset entries to process before committing. The stage commits
    /// after either `commit_threshold` blocks or `commit_entries` entries, whichever comes first.
    pub commit_entries: u64,
    /// ETL configuration
    pub etl_config: EtlConfig,
}

impl StorageHashingStage {
    /// Create new instance of [`StorageHashingStage`].
    pub const fn new(config: HashingConfig, etl_config: EtlConfig) -> Self {
        Self {
            clean_threshold: config.clean_threshold,
            commit_threshold: config.commit_threshold,
            commit_entries: config.commit_entries,
            etl_config,
        }
    }
}

impl Default for StorageHashingStage {
    fn default() -> Self {
        Self {
            clean_threshold: 500_000,
            commit_threshold: 100_000,
            commit_entries: 30_000_000,
            etl_config: EtlConfig::default(),
        }
    }
}

impl<Provider> Stage<Provider> for StorageHashingStage
where
    Provider: DBProvider<Tx: DbTxMut>
        + StorageReader
        + HashingWriter
        + StatsReader
        + StorageSettingsCache,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::StorageHashing
    }

    /// Execute the stage.
    fn execute(&mut self, provider: &Provider, input: ExecInput) -> Result<ExecOutput, StageError> {
        let tx = provider.tx_ref();
        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()));
        }

        // Use the total remaining range to decide clean vs incremental.
        let total_range = input.target() - input.checkpoint().block_number;
        let from_block = input.next_block();

        if total_range > self.clean_threshold || from_block == 1 {
            // if there are more blocks than threshold it is faster to go over Plain state and
            // hash all storage otherwise take changesets aggregate the sets and apply
            // hashing to HashedStorages table. Also, if we start from genesis, we need to
            // hash from scratch, as genesis accounts are not in changeset, along with their
            // storages.

            // clear table, load all entries and hash them
            tx.clear::<tables::HashedStorages>()?;

            let mut storage_cursor = tx.cursor_read::<tables::PlainStorageState>()?;
            let mut collector =
                Collector::new(self.etl_config.file_size, self.etl_config.dir.clone());
            let mut channels: Vec<(HashChannel, usize)> = Vec::with_capacity(MAXIMUM_CHANNELS);

            for chunk in &storage_cursor.walk(None)?.chunks(WORKER_CHUNK_SIZE) {
                // An _unordered_ channel to receive results from a rayon job
                let chunk = chunk.collect::<Result<Vec<_>, _>>()?;
                let (tx, rx) = mpsc::sync_channel(chunk.len());
                let expected = chunk.len();
                channels.push((rx, expected));
                // Spawn the hashing task onto the global rayon pool
                rayon::spawn(move || {
                    // Cache hashed address since PlainStorageState is sorted by address
                    let (mut last_addr, mut hashed_addr) = (Address::ZERO, HASHED_ZERO_ADDRESS);
                    for (address, slot) in chunk {
                        if address != last_addr {
                            last_addr = address;
                            hashed_addr = keccak256(address);
                        }
                        let mut addr_key = Vec::with_capacity(64);
                        addr_key.put_slice(hashed_addr.as_slice());
                        addr_key.put_slice(keccak256(slot.key).as_slice());
                        let _ = tx.send((addr_key, CompactU256::from(slot.value)));
                    }
                });

                // Flush to ETL when channels length reaches MAXIMUM_CHANNELS
                if !channels.is_empty() && channels.len().is_multiple_of(MAXIMUM_CHANNELS) {
                    collect(&mut channels, &mut collector)?;
                }
            }

            collect(&mut channels, &mut collector)?;

            let total_hashes = collector.len();
            let interval = (total_hashes / 10).max(1);
            let mut cursor = tx.cursor_dup_write::<tables::HashedStorages>()?;
            for (index, item) in collector.iter()?.enumerate() {
                if index > 0 && index.is_multiple_of(interval) {
                    info!(
                        target: "sync::stages::hashing_storage",
                        progress = %format!("{:.2}%", (index as f64 / total_hashes as f64) * 100.0),
                        "Inserting hashes"
                    );
                }

                let (addr_key, value) = item?;
                cursor.append_dup(
                    B256::from_slice(&addr_key[..32]),
                    StorageEntry {
                        key: B256::from_slice(&addr_key[32..]),
                        value: CompactU256::decompress_owned(value)?.into(),
                    },
                )?;
            }

            let checkpoint = StageCheckpoint::new(input.target())
                .with_storage_hashing_stage_checkpoint(StorageHashingCheckpoint {
                    progress: stage_checkpoint_progress(provider)?,
                    ..Default::default()
                });

            Ok(ExecOutput { checkpoint, done: true })
        } else {
            // Stream changesets entry-by-entry, bounded by both block count
            // (commit_threshold) and entry count (commit_entries), whichever comes first.
            let BlockRangeOutput { block_range, is_final_range } =
                input.next_block_range_with_threshold(self.commit_threshold);
            let (from_block, to_block) = block_range.into_inner();

            let mut changeset_cursor = tx.cursor_read::<tables::StorageChangeSets>()?;
            let mut changed: BTreeMap<Address, BTreeSet<B256>> = BTreeMap::new();
            let mut total_entries = 0u64;
            let mut last_block = from_block;

            for entry in
                changeset_cursor.walk_range(BlockNumberAddress::range(from_block..=to_block))?
            {
                let (BlockNumberAddress((block_number, address)), storage_entry) = entry?;

                // Check the entry limit only at block boundaries so we never
                // checkpoint mid-block (which would skip the remaining entries
                // for that block on the next invocation).
                if block_number != last_block && total_entries >= self.commit_entries {
                    break;
                }

                last_block = block_number;
                changed.entry(address).or_default().insert(storage_entry.key);
                total_entries += 1;
            }

            let storages = provider.plain_state_storages(changed)?;
            provider.insert_storage_for_hashing(storages)?;

            let exhausted = total_entries < self.commit_entries;
            let done = exhausted && is_final_range;
            let progress_block = if exhausted { to_block } else { last_block };

            let checkpoint = StageCheckpoint::new(progress_block)
                .with_storage_hashing_stage_checkpoint(StorageHashingCheckpoint {
                    progress: stage_checkpoint_progress(provider)?,
                    ..Default::default()
                });

            Ok(ExecOutput { checkpoint, done })
        }
    }
}

/// Flushes channels hashes to ETL collector.
///
/// Iterates each channel to its natural end and fails loudly when a worker died
/// early (panic mid-chunk): the stage must not report success over a partially
/// collected set — that would silently corrupt the hashed state.
fn collect(
    channels: &mut Vec<(HashChannel, usize)>,
    collector: &mut Collector<Vec<u8>, CompactU256>,
) -> Result<(), StageError> {
    for (channel, expected) in channels.iter_mut() {
        let mut received = 0usize;
        // Sender dropped ends the loop — either the worker finished every entry or it
        // died mid-chunk; the count check below tells the two apart.
        while let Ok((key, v)) = channel.recv() {
            collector.insert(key, v)?;
            received += 1;
        }
        if received != *expected {
            return Err(io::Error::other(format!(
                "storage hashing worker channel closed after {received}/{expected} entries"
            ))
            .into());
        }
    }
    info!(target: "sync::stages::hashing_storage", "Hashed {} entries", collector.len());
    channels.clear();
    Ok(())
}

fn stage_checkpoint_progress(provider: &impl StatsReader) -> ProviderResult<EntitiesCheckpoint> {
    Ok(EntitiesCheckpoint {
        processed: provider.count_entries::<tables::HashedStorages>()? as u64,
        total: provider.count_entries::<tables::PlainStorageState>()? as u64,
    })
}
