use alloy_primitives::{TxHash, TxNumber};
use num_traits::Zero;
use smartnet_prune_types::{PruneCheckpoint, PruneMode, PrunePurpose, PruneSegment};
use smartnet_stages_api::{
    EntitiesCheckpoint, ExecInput, ExecOutput, Stage, StageCheckpoint, StageError, StageId,
};
use smartnet_config::config::{EtlConfig, TransactionLookupConfig};
use smartnet_db_api::{
    table::{Decode, Decompress},
    tables,
    transaction::DbTxMut,
};
use smartnet_provider::etl::Collector;
use smartnet_provider::{
    AuxDbProviderFactory, BlockReader, DBProvider, EitherWriter, PruneCheckpointReader,
    PruneCheckpointWriter, StatsReader, StorageSettingsCache, TransactionsProviderExt,
};
use smartnet_storage_errors::provider::ProviderError;
use tracing::*;

/// The transaction lookup stage.
///
/// This stage walks over existing transactions, and sets the transaction hash of each transaction
/// in a block to the corresponding `BlockNumber` at each block. This is written to the
/// [`tables::TransactionHashNumbers`] This is used for looking up changesets via the transaction
/// hash.
///
/// It uses [`smartnet_provider::etl::Collector`] to collect all entries before finally writing them to disk.
#[derive(Debug, Clone)]
pub struct TransactionLookupStage {
    /// The maximum number of lookup entries to hold in memory before pushing them to
    /// [`smartnet_provider::etl::Collector`].
    chunk_size: u64,
    etl_config: EtlConfig,
    prune_mode: Option<PruneMode>,
}

impl Default for TransactionLookupStage {
    fn default() -> Self {
        Self { chunk_size: 5_000_000, etl_config: EtlConfig::default(), prune_mode: None }
    }
}

impl TransactionLookupStage {
    /// Create new instance of [`TransactionLookupStage`].
    pub const fn new(
        config: TransactionLookupConfig,
        etl_config: EtlConfig,
        prune_mode: Option<PruneMode>,
    ) -> Self {
        Self { chunk_size: config.chunk_size, etl_config, prune_mode }
    }
}

impl<Provider> Stage<Provider> for TransactionLookupStage
where
    Provider: DBProvider<Tx: DbTxMut>
        + PruneCheckpointWriter
        + BlockReader
        + PruneCheckpointReader
        + StatsReader
        + TransactionsProviderExt
        + StorageSettingsCache
        + smartnet_storage_api::NodePrimitivesProvider
        + AuxDbProviderFactory,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::TransactionLookup
    }

    /// Write transaction hash -> id entries
    fn execute(
        &mut self,
        provider: &Provider,
        mut input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        if let Some((target_prunable_block, prune_mode)) = self
            .prune_mode
            .map(|mode| {
                mode.prune_target_block(
                    input.target(),
                    PruneSegment::TransactionLookup,
                    PrunePurpose::User,
                )
            })
            .transpose()?
            .flatten() &&
            target_prunable_block > input.checkpoint().block_number
        {
            input.checkpoint = Some(StageCheckpoint::new(target_prunable_block));

            // Save prune checkpoint only if we don't have one already.
            // Otherwise, pruner may skip the unpruned range of blocks.
            if provider.get_prune_checkpoint(PruneSegment::TransactionLookup)?.is_none() {
                let target_prunable_tx_number = provider
                    .block_body_indices(target_prunable_block)?
                    .ok_or(ProviderError::BlockBodyIndicesNotFound(target_prunable_block))?
                    .last_tx_num();

                provider.save_prune_checkpoint(
                    PruneSegment::TransactionLookup,
                    PruneCheckpoint {
                        block_number: Some(target_prunable_block),
                        tx_number: Some(target_prunable_tx_number),
                        prune_mode,
                    },
                )?;
            }
        }
        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()));
        }

        // 500MB temporary files
        let mut hash_collector: Collector<TxHash, TxNumber> =
            Collector::new(self.etl_config.file_size, self.etl_config.dir.clone());

        info!(
            target: "sync::stages::transaction_lookup",
            tx_range = ?input.checkpoint().block_number..=input.target(),
            "Updating transaction lookup"
        );

        loop {
            let Some(range_output) =
                input.next_block_range_with_transaction_threshold(provider, self.chunk_size)?
            else {
                input.checkpoint = Some(
                    StageCheckpoint::new(input.target())
                        .with_entities_stage_checkpoint(stage_checkpoint(provider)?),
                );
                break;
            };

            let end_block = *range_output.block_range.end();

            info!(target: "sync::stages::transaction_lookup", tx_range = ?range_output.tx_range, "Calculating transaction hashes");

            for (key, value) in provider.transaction_hashes_by_range(range_output.tx_range)? {
                hash_collector.insert(key, value)?;
            }

            input.checkpoint = Some(
                StageCheckpoint::new(end_block)
                    .with_entities_stage_checkpoint(stage_checkpoint(provider)?),
            );

            if range_output.is_final_range {
                let total_hashes = hash_collector.len();
                let interval = (total_hashes / 10).max(1);

                // Use append mode when table is empty (first sync) - significantly faster
                let append_only =
                    provider.count_entries::<tables::TransactionHashNumbers>()?.is_zero();

                // Auto-commits on threshold; consistency check heals any crash.
                provider.with_aux_db_batch_auto_commit(|aux_db_batch| {
                    let mut writer =
                        EitherWriter::new_transaction_hash_numbers(provider, aux_db_batch)?;

                    let iter = hash_collector
                        .iter()
                        .map_err(|e| ProviderError::other(Box::new(e)))?;

                    for (index, hash_to_number) in iter.enumerate() {
                        let (hash_bytes, number_bytes) =
                            hash_to_number.map_err(|e| ProviderError::other(Box::new(e)))?;
                        if index > 0 && index.is_multiple_of(interval) {
                            info!(
                                target: "sync::stages::transaction_lookup",
                                ?append_only,
                                progress = %format!("{:.2}%", (index as f64 / total_hashes as f64) * 100.0),
                                "Inserting hashes"
                            );
                        }

                        // Decode from raw ETL bytes
                        let hash = TxHash::decode(&hash_bytes)?;
                        let tx_num = TxNumber::decompress(&number_bytes)?;
                        writer.put_transaction_hash_number(hash, tx_num, append_only)?;
                    }

                    Ok(((), writer.into_raw_aux_db_batch()))
                })?;

                trace!(target: "sync::stages::transaction_lookup",
                    total_hashes,
                    "Transaction hashes inserted"
                );

                break;
            }
        }

        Ok(ExecOutput {
            checkpoint: StageCheckpoint::new(input.target())
                .with_entities_stage_checkpoint(stage_checkpoint(provider)?),
            done: true,
        })
    }
}

fn stage_checkpoint<Provider>(provider: &Provider) -> Result<EntitiesCheckpoint, StageError>
where
    Provider: PruneCheckpointReader + StatsReader,
{
    let pruned_entries = provider
        .get_prune_checkpoint(PruneSegment::TransactionLookup)?
        .and_then(|checkpoint| checkpoint.tx_number)
        // `+1` is needed because `TxNumber` is 0-indexed
        .map(|tx_number| tx_number + 1)
        .unwrap_or_default();
    Ok(EntitiesCheckpoint {
        // If `TransactionHashNumbers` table was pruned, we will have a number of entries in it not
        // matching the actual number of processed transactions. To fix that, we add the
        // number of pruned `TransactionHashNumbers` entries.
        processed: provider.count_entries::<tables::TransactionHashNumbers>()? as u64 +
            pruned_entries,
        // Every indexed transaction has exactly one `TxLocations` row.
        total: provider.count_entries::<tables::TxLocations>()? as u64,
    })
}
