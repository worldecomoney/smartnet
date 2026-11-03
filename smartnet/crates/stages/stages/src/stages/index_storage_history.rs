use super::collect_history_indices;
use crate::{stages::utils::load_storage_history, StageCheckpoint, StageId};
use smartnet_prune_types::{PruneCheckpoint, PruneMode, PrunePurpose, PruneSegment};
use smartnet_stages_api::{ExecInput, ExecOutput, Stage, StageError};
use smartnet_config::config::{EtlConfig, IndexHistoryConfig};
use smartnet_db_api::{
    models::{storage_sharded_key::StorageShardedKey, AddressStorageKey, BlockNumberAddress},
    tables,
    transaction::DbTxMut,
};
use smartnet_provider::{
    AuxDbProviderFactory, DBProvider, EitherWriter, HistoryWriter, PruneCheckpointReader,
    PruneCheckpointWriter, StorageChangeSetReader, StorageSettingsCache,
};
use std::fmt::Debug;
use tracing::info;

/// Stage is indexing history the storage changesets generated in
/// [`ExecutionStage`][crate::stages::ExecutionStage]. For more information
/// on index sharding take a look at [`tables::StoragesHistory`].
#[derive(Debug)]
pub struct IndexStorageHistoryStage {
    /// Number of blocks after which the control
    /// flow will be returned to the pipeline for commit.
    pub commit_threshold: u64,
    /// Pruning configuration.
    pub prune_mode: Option<PruneMode>,
    /// ETL configuration
    pub etl_config: EtlConfig,
}

impl IndexStorageHistoryStage {
    /// Create new instance of [`IndexStorageHistoryStage`].
    pub const fn new(
        config: IndexHistoryConfig,
        etl_config: EtlConfig,
        prune_mode: Option<PruneMode>,
    ) -> Self {
        Self { commit_threshold: config.commit_threshold, etl_config, prune_mode }
    }
}

impl Default for IndexStorageHistoryStage {
    fn default() -> Self {
        Self { commit_threshold: 100_000, prune_mode: None, etl_config: EtlConfig::default() }
    }
}

impl<Provider> Stage<Provider> for IndexStorageHistoryStage
where
    Provider: DBProvider<Tx: DbTxMut>
        + HistoryWriter
        + PruneCheckpointReader
        + PruneCheckpointWriter
        + StorageSettingsCache
        + AuxDbProviderFactory
        + StorageChangeSetReader
        + smartnet_provider::NodePrimitivesProvider,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::IndexStorageHistory
    }

    /// Execute the stage.
    fn execute(
        &mut self,
        provider: &Provider,
        mut input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        if !provider.cached_storage_settings().history_indexing_enabled() {
            if input.target_reached() {
                return Ok(ExecOutput::done(input.checkpoint()));
            }
            return Ok(ExecOutput::done(input.checkpoint().with_block_number(input.target())));
        }

        if let Some((target_prunable_block, prune_mode)) = self
            .prune_mode
            .map(|mode| {
                mode.prune_target_block(
                    input.target(),
                    PruneSegment::StorageHistory,
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
            if provider.get_prune_checkpoint(PruneSegment::StorageHistory)?.is_none() {
                provider.save_prune_checkpoint(
                    PruneSegment::StorageHistory,
                    PruneCheckpoint {
                        block_number: Some(target_prunable_block),
                        tx_number: None,
                        prune_mode,
                    },
                )?;
            }
        }

        if input.target_reached() {
            return Ok(ExecOutput::done(input.checkpoint()));
        }

        let mut range = input.next_block_range();
        let first_sync = input.checkpoint().block_number == 0;

        // On first sync we might have history coming from genesis. We clear the table since it's
        // faster to rebuild from scratch.
        if first_sync {
            provider.tx_ref().clear::<tables::StoragesHistory>()?;
            range = 0..=*input.next_block_range().end();
        }

        info!(target: "sync::stages::index_storage_history::exec", ?first_sync, "Collecting indices");
        let collector =
            collect_history_indices::<_, tables::StorageChangeSets, tables::StoragesHistory, _>(
                provider,
                BlockNumberAddress::range(range.clone()),
                |AddressStorageKey((address, storage_key)), highest_block_number| {
                    StorageShardedKey::new(address, storage_key, highest_block_number)
                },
                |(key, value)| (key.block_number(), AddressStorageKey((key.address(), value.key))),
                &self.etl_config,
            )?;

        info!(target: "sync::stages::index_storage_history::exec", "Loading indices into database");

        provider.with_aux_db_batch_auto_commit(|aux_db_batch| {
            let mut writer = EitherWriter::new_storages_history(provider, aux_db_batch)?;
            load_storage_history(collector, first_sync, &mut writer)
                .map_err(|e| smartnet_provider::ProviderError::other(Box::new(e)))?;
            Ok(((), writer.into_raw_aux_db_batch()))
        })?;

        Ok(ExecOutput { checkpoint: StageCheckpoint::new(*range.end()), done: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TestStageDB;
    use alloy_primitives::{address, b256, Address, BlockNumber, B256, U256};
    use smartnet_primitives_traits::StorageEntry;
    use smartnet_db_api::{
        models::{storage_sharded_key::NUM_OF_INDICES_IN_SHARD, ShardedKey},
        BlockNumberList,
    };
    use smartnet_provider::DatabaseProviderFactory;
    use std::collections::BTreeMap;

    const ADDRESS: Address = address!("0x0000000000000000000000000000000000000001");
    const STORAGE_KEY: B256 =
        b256!("0x0000000000000000000000000000000000000000000000000000000000000001");

    const MAX_BLOCK: BlockNumber = NUM_OF_INDICES_IN_SHARD as BlockNumber + 2;

    const fn storage(key: B256) -> StorageEntry {
        // Value is not used in indexing stage.
        StorageEntry { key, value: U256::ZERO }
    }

    const fn block_number_address(block_number: u64) -> BlockNumberAddress {
        BlockNumberAddress((block_number, ADDRESS))
    }

    /// Shard for account
    const fn shard(shard_index: u64) -> StorageShardedKey {
        StorageShardedKey {
            address: ADDRESS,
            sharded_key: ShardedKey { key: STORAGE_KEY, highest_block_number: shard_index },
        }
    }

    fn cast(
        table: Vec<(StorageShardedKey, BlockNumberList)>,
    ) -> BTreeMap<StorageShardedKey, Vec<u64>> {
        table
            .into_iter()
            .map(|(k, v)| {
                let v = v.iter().collect();
                (k, v)
            })
            .collect()
    }

    fn partial_setup(db: &TestStageDB) {
        // setup
        db.commit(|tx| {
            for block in 0..=MAX_BLOCK {
                // setup changeset that is going to be applied to history index
                tx.put::<tables::StorageChangeSets>(
                    block_number_address(block),
                    storage(STORAGE_KEY),
                )?;
            }
            Ok(())
        })
        .unwrap()
    }

    fn run(db: &TestStageDB, run_to: u64, input_checkpoint: Option<BlockNumber>) {
        let input = ExecInput {
            target: Some(run_to),
            checkpoint: input_checkpoint
                .map(|block_number| StageCheckpoint { block_number, stage_checkpoint: None }),
        };
        let mut stage = IndexStorageHistoryStage::default();
        let provider = db.factory.database_provider_rw().unwrap();
        let out = stage.execute(&provider, input).unwrap();
        assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(run_to), done: true });
        provider.commit().unwrap();
    }

    #[tokio::test]
    async fn insert_index_to_genesis() {
        // init
        let db = TestStageDB::default();

        // setup
        partial_setup(&db);

        // run
        run(&db, 3, None);

        // verify
        let table = cast(db.table::<tables::StoragesHistory>().unwrap());
        assert_eq!(table, BTreeMap::from([(shard(u64::MAX), vec![0, 1, 2, 3])]));
    }
}
