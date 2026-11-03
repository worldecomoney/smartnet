use crate::stages::utils::{collect_history_indices, load_account_history};
use smartnet_prune_types::{PruneCheckpoint, PruneMode, PrunePurpose, PruneSegment};
use smartnet_stages_api::{
    ExecInput, ExecOutput, Stage, StageCheckpoint, StageError, StageId,
};
use smartnet_config::config::{EtlConfig, IndexHistoryConfig};
use smartnet_db_api::{models::ShardedKey, tables, transaction::DbTxMut};
use smartnet_provider::{
    AuxDbProviderFactory, DBProvider, EitherWriter, HistoryWriter, PruneCheckpointReader,
    PruneCheckpointWriter, StorageSettingsCache,
};
use std::fmt::Debug;
use tracing::info;

/// Stage is indexing history the account changesets generated in
/// [`ExecutionStage`][crate::stages::ExecutionStage]. For more information
/// on index sharding take a look at [`tables::AccountsHistory`]
#[derive(Debug)]
pub struct IndexAccountHistoryStage {
    /// Number of blocks after which the control
    /// flow will be returned to the pipeline for commit.
    pub commit_threshold: u64,
    /// Pruning configuration.
    pub prune_mode: Option<PruneMode>,
    /// ETL configuration
    pub etl_config: EtlConfig,
}

impl IndexAccountHistoryStage {
    /// Create new instance of [`IndexAccountHistoryStage`].
    pub const fn new(
        config: IndexHistoryConfig,
        etl_config: EtlConfig,
        prune_mode: Option<PruneMode>,
    ) -> Self {
        Self { commit_threshold: config.commit_threshold, etl_config, prune_mode }
    }
}

impl Default for IndexAccountHistoryStage {
    fn default() -> Self {
        Self { commit_threshold: 100_000, prune_mode: None, etl_config: EtlConfig::default() }
    }
}

impl<Provider> Stage<Provider> for IndexAccountHistoryStage
where
    Provider: DBProvider<Tx: DbTxMut>
        + HistoryWriter
        + PruneCheckpointReader
        + PruneCheckpointWriter
        + smartnet_storage_api::ChangeSetReader
        + smartnet_storage_api::NodePrimitivesProvider
        + StorageSettingsCache
        + AuxDbProviderFactory,
{
    /// Return the id of the stage
    fn id(&self) -> StageId {
        StageId::IndexAccountHistory
    }

    /// Execute the stage.
    fn execute(
        &mut self,
        provider: &Provider,
        mut input: ExecInput,
    ) -> Result<ExecOutput, StageError> {
        // Calibri Lite runtime profile: history indices are not persisted (see `StorageSettings`).
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
                    PruneSegment::AccountHistory,
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
            if provider.get_prune_checkpoint(PruneSegment::AccountHistory)?.is_none() {
                provider.save_prune_checkpoint(
                    PruneSegment::AccountHistory,
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
            provider.tx_ref().clear::<tables::AccountsHistory>()?;
            range = 0..=*input.next_block_range().end();
        }

        info!(target: "sync::stages::index_account_history::exec", ?first_sync, "Collecting indices");

        let collector =
            collect_history_indices::<_, tables::AccountChangeSets, tables::AccountsHistory, _>(
                provider,
                range.clone(),
                ShardedKey::new,
                |(index, value)| (index, value.address),
                &self.etl_config,
            )?;

        info!(target: "sync::stages::index_account_history::exec", "Loading indices into database");

        provider.with_aux_db_batch_auto_commit(|aux_db_batch| {
            let mut writer = EitherWriter::new_accounts_history(provider, aux_db_batch)?;
            load_account_history(collector, first_sync, &mut writer)
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
    use alloy_primitives::{address, Address, BlockNumber};
    use smartnet_db_api::{
        models::{sharded_key::NUM_OF_INDICES_IN_SHARD, AccountBeforeTx, ShardedKey},
        BlockNumberList,
    };
    use smartnet_provider::DatabaseProviderFactory;
    use std::collections::BTreeMap;

    const ADDRESS: Address = address!("0x0000000000000000000000000000000000000001");

    const MAX_BLOCK: BlockNumber = NUM_OF_INDICES_IN_SHARD as BlockNumber;

    const fn acc() -> AccountBeforeTx {
        AccountBeforeTx { address: ADDRESS, info: None }
    }

    /// Shard for account
    const fn shard(shard_index: u64) -> ShardedKey<Address> {
        ShardedKey { key: ADDRESS, highest_block_number: shard_index }
    }

    fn cast(
        table: Vec<(ShardedKey<Address>, BlockNumberList)>,
    ) -> BTreeMap<ShardedKey<Address>, Vec<u64>> {
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
                tx.put::<tables::AccountChangeSets>(block, acc())?;
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
        let mut stage = IndexAccountHistoryStage::default();
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
        let table = cast(db.table::<tables::AccountsHistory>().unwrap());
        assert_eq!(table, BTreeMap::from([(shard(u64::MAX), vec![0, 1, 2, 3])]));
    }

    #[tokio::test]
    async fn execute_noop_when_history_indexing_disabled() {
        use smartnet_db_api::models::StorageSettings;

        let db = TestStageDB::default();
        db.factory.set_storage_settings_cache(StorageSettings::calibri_lite_runtime());
        let provider = db.factory.database_provider_rw().unwrap();
        let mut stage = IndexAccountHistoryStage::default();
        let input = ExecInput { target: Some(5), ..Default::default() };
        let out = stage.execute(&provider, input).unwrap();
        assert_eq!(out, ExecOutput { checkpoint: StageCheckpoint::new(5), done: true });
        assert!(db.table::<tables::AccountsHistory>().unwrap().is_empty());
    }
}
