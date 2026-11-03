use crate::{
    db_ext::DbTxPruneExt,
    segments::{
        user::history::{finalize_history_prune, HistoryPruneResult},
        PruneInput, Segment,
    },
    PrunerError,
};
use alloy_primitives::BlockNumber;
use smartnet_prune_types::{
    PruneMode, PrunePurpose, PruneSegment, SegmentOutput, SegmentOutputCheckpoint,
};
use rustc_hash::FxHashMap;
use smartnet_db_api::{models::ShardedKey, tables, transaction::DbTxMut};
use smartnet_provider::{AuxDbProviderFactory, DBProvider};
use smartnet_storage_api::StorageSettingsCache;
use tracing::{instrument, trace};

/// Number of account history tables to prune in one step.
///
/// Account History consists of two tables: [`tables::AccountChangeSets`] (in the primary
/// database since Stage 3, 3A.3) and [`tables::AccountsHistory`]. We want to prune them to the
/// same block number.
const ACCOUNT_HISTORY_TABLES_TO_PRUNE: usize = 2;

#[derive(Debug)]
pub struct AccountHistory {
    mode: PruneMode,
}

impl AccountHistory {
    pub const fn new(mode: PruneMode) -> Self {
        Self { mode }
    }
}

impl<Provider> Segment<Provider> for AccountHistory
where
    Provider: DBProvider<Tx: DbTxMut> + StorageSettingsCache + AuxDbProviderFactory,
{
    fn segment(&self) -> PruneSegment {
        PruneSegment::AccountHistory
    }

    fn mode(&self) -> Option<PruneMode> {
        Some(self.mode)
    }

    fn purpose(&self) -> PrunePurpose {
        PrunePurpose::User
    }

    #[instrument(
        name = "AccountHistory::prune",
        target = "pruner",
        skip(self, provider),
        ret(level = "trace")
    )]
    fn prune(&self, provider: &Provider, input: PruneInput) -> Result<SegmentOutput, PrunerError> {
        if !provider.cached_storage_settings().history_indexing_enabled() {
            return Ok(SegmentOutput::done());
        }

        let range = match input.get_next_block_range() {
            Some(range) => range,
            None => {
                trace!(target: "pruner", "No account history to prune");
                return Ok(SegmentOutput::done());
            }
        };
        let range_end = *range.end();

        // Changesets live in the primary database only since Stage 3 (3A.3).
        self.prune_database(provider, input, range, range_end)
    }
}

impl AccountHistory {
    fn prune_database<Provider>(
        &self,
        provider: &Provider,
        input: PruneInput,
        range: std::ops::RangeInclusive<BlockNumber>,
        range_end: BlockNumber,
    ) -> Result<SegmentOutput, PrunerError>
    where
        Provider: DBProvider<Tx: DbTxMut>,
    {
        let mut limiter = if let Some(limit) = input.limiter.deleted_entries_limit() {
            input.limiter.set_deleted_entries_limit(limit / ACCOUNT_HISTORY_TABLES_TO_PRUNE)
        } else {
            input.limiter
        };

        if limiter.is_limit_reached() {
            return Ok(SegmentOutput::not_done(
                limiter.interrupt_reason(),
                input.previous_checkpoint.map(SegmentOutputCheckpoint::from_prune_checkpoint),
            ));
        }

        // Deleted account changeset keys (account addresses) with the highest block number deleted
        // for that key.
        //
        // The size of this map is limited by `prune_delete_limit * blocks_since_last_run /
        // ACCOUNT_HISTORY_TABLES_TO_PRUNE`, and with the current defaults it's usually `3500 * 5 /
        // 2`, so 8750 entries. Each entry is `160 bit + 64 bit`, so the total size should be up to
        // ~0.25MB + some hashmap overhead. `blocks_since_last_run` is additionally limited by the
        // `max_reorg_depth`, so no OOM is expected here.
        let mut last_changeset_pruned_block = None;
        let mut highest_deleted_accounts = FxHashMap::default();
        let (pruned_changesets, done) =
            provider.tx_ref().prune_table_with_range::<tables::AccountChangeSets>(
                range,
                &mut limiter,
                |_| false,
                |(block_number, account)| {
                    highest_deleted_accounts.insert(account.address, block_number);
                    last_changeset_pruned_block = Some(block_number);
                },
            )?;
        trace!(target: "pruner", pruned = %pruned_changesets, %done, "Pruned account history (changesets from database)");

        let result = HistoryPruneResult {
            highest_deleted: highest_deleted_accounts,
            last_pruned_block: last_changeset_pruned_block,
            pruned_count: pruned_changesets,
            done,
        };
        finalize_history_prune::<_, tables::AccountsHistory, _, _>(
            provider,
            result,
            range_end,
            &limiter,
            ShardedKey::new,
            |a, b| a.key == b.key,
        )
        .map_err(Into::into)
    }

    /// Prunes account history when indices are stored in `aux DB`.
    ///
    /// Deletes account changesets from the primary database and prunes the corresponding
    /// `aux DB` history shards.
    #[allow(dead_code)]
    fn prune_aux_db<Provider>(
        &self,
        provider: &Provider,
        input: PruneInput,
        range: std::ops::RangeInclusive<BlockNumber>,
        range_end: BlockNumber,
    ) -> Result<SegmentOutput, PrunerError>
    where
        Provider: DBProvider<Tx: DbTxMut>
            + AuxDbProviderFactory
            + smartnet_storage_api::StorageSettingsCache,
    {
        // Unlike the primary-database path, we don't divide the limit by 2 because aux DB path only
        // prunes history shards (no separate primary-database history table to delete
        // from). The changesets are deleted from the primary database tables below.
        let mut limiter = input.limiter;

        if limiter.is_limit_reached() {
            return Ok(SegmentOutput::not_done(
                limiter.interrupt_reason(),
                input.previous_checkpoint.map(SegmentOutputCheckpoint::from_prune_checkpoint),
            ));
        }

        let mut highest_deleted_accounts = FxHashMap::default();
        let mut last_changeset_pruned_block = None;

        // Delete account changesets from the primary database and track the highest block number
        // seen for each address to determine which history shard entries need pruning.
        //
        // The size of the `highest_deleted_accounts` map is limited by `prune_delete_limit *
        // blocks_since_last_run`, and with current defaults it's usually `3500 * 5`, so 17500
        // entries. Each entry is `160 bit + 64 bit`, so the total size should be up to ~0.5MB +
        // some hashmap overhead. `blocks_since_last_run` is additionally limited by the
        // `max_reorg_depth`, so no OOM is expected here.
        let (pruned_changesets, done) =
            provider.tx_ref().prune_table_with_range::<tables::AccountChangeSets>(
                range,
                &mut limiter,
                |_| false,
                |(block_number, account)| {
                    highest_deleted_accounts.insert(account.address, block_number);
                    last_changeset_pruned_block = Some(block_number);
                },
            )?;
        trace!(target: "pruner", pruned = %pruned_changesets, %done, "Pruned account history (changesets from database)");

        let last_changeset_pruned_block = last_changeset_pruned_block
            .map(|block_number| if done { block_number } else { block_number.saturating_sub(1) })
            .unwrap_or(range_end);

        // Prune aux DB history shards for affected accounts
        let mut deleted_shards = 0usize;
        let mut updated_shards = 0usize;

        // Sort by address for better aux DB cache locality
        let mut sorted_accounts: Vec<_> = highest_deleted_accounts.into_iter().collect();
        sorted_accounts.sort_unstable_by_key(|(addr, _)| *addr);

        provider.with_aux_db_batch(|mut batch| {
            let targets: Vec<_> = sorted_accounts
                .iter()
                .map(|(addr, highest)| (*addr, (*highest).min(last_changeset_pruned_block)))
                .collect();

            let outcomes = batch.prune_account_history_batch(&targets)?;
            deleted_shards = outcomes.deleted;
            updated_shards = outcomes.updated;

            Ok(((), {
                let _: () = batch.into_inner();
                Some(())
            }))
        })?;
        trace!(target: "pruner", deleted = deleted_shards, updated = updated_shards, %done, "Pruned account history (aux DB indices)");

        // During provider.commit(), aux DB batch is committed before the database checkpoint. If
        // crash occurs after aux DB commit but before the primary-database commit, on restart the
        // pruner checkpoint indicates data needs re-pruning, but the aux DB shards are already
        // pruned - this is safe because pruning is idempotent (re-pruning already-pruned shards
        // is a no-op).
        let progress = limiter.progress(done);

        Ok(SegmentOutput {
            progress,
            pruned: pruned_changesets + deleted_shards + updated_shards,
            checkpoint: Some(SegmentOutputCheckpoint {
                block_number: Some(last_changeset_pruned_block),
                tx_number: None,
            }),
        })
    }
}
