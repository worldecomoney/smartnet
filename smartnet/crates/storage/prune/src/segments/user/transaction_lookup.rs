use crate::{
    db_ext::DbTxPruneExt,
    segments::{PruneInput, Segment, SegmentOutput},
    PrunerError,
};
use alloy_eips::eip2718::Encodable2718;
use rayon::prelude::*;
use smartnet_prune_types::{
    PruneMode, PruneProgress, PrunePurpose, PruneSegment, SegmentOutputCheckpoint,
};
use smartnet_db_api::{tables, transaction::DbTxMut};
use smartnet_provider::{AuxDbProviderFactory, BlockReader, DBProvider, PruneCheckpointReader};
use smartnet_storage_api::StorageSettingsCache;
use tracing::{instrument, trace};

#[derive(Debug)]
pub struct TransactionLookup {
    mode: PruneMode,
}

impl TransactionLookup {
    pub const fn new(mode: PruneMode) -> Self {
        Self { mode }
    }
}

impl<Provider> Segment<Provider> for TransactionLookup
where
    Provider: DBProvider<Tx: DbTxMut>
        + BlockReader<Transaction: Encodable2718>
        + PruneCheckpointReader
        + StorageSettingsCache
        + AuxDbProviderFactory,
{
    fn segment(&self) -> PruneSegment {
        PruneSegment::TransactionLookup
    }

    fn mode(&self) -> Option<PruneMode> {
        Some(self.mode)
    }

    fn purpose(&self) -> PrunePurpose {
        PrunePurpose::User
    }

    #[instrument(
        name = "TransactionLookup::prune",
        target = "pruner",
        skip(self, provider),
        ret(level = "trace")
    )]
    fn prune(
        &self,
        provider: &Provider,
        input: PruneInput,
    ) -> Result<SegmentOutput, PrunerError> {
        let (start, end) = match input.get_next_tx_num_range(provider)? {
            Some(range) => range,
            None => {
                trace!(target: "pruner", "No transaction lookup entries to prune");
                return Ok(SegmentOutput::done());
            }
        }
        .into_inner();

        // For PruneMode::Full, clear the entire table in one operation
        if self.mode.is_full() {
            let pruned = provider.tx_ref().clear_table::<tables::TransactionHashNumbers>()?;
            trace!(target: "pruner", %pruned, "Cleared transaction lookup table");

            let last_pruned_block = provider
                .block_by_transaction_id(end)?
                .ok_or(PrunerError::InconsistentData("Block for transaction is not found"))?;

            return Ok(SegmentOutput {
                progress: PruneProgress::Finished,
                pruned,
                checkpoint: Some(SegmentOutputCheckpoint {
                    block_number: Some(last_pruned_block),
                    tx_number: Some(end),
                }),
            });
        }

        let tx_range = start..=
            Some(end)
                .min(
                    input
                        .limiter
                        .deleted_entries_limit_left()
                        // Use saturating addition here to avoid panicking on
                        // `deleted_entries_limit == usize::MAX`
                        .map(|left| start.saturating_add(left as u64) - 1),
                )
                .unwrap();
        let tx_range_end = *tx_range.end();

        // Retrieve transactions in the range and calculate their hashes in parallel
        let mut hashes = provider
            .transactions_by_tx_range(tx_range.clone())?
            .into_par_iter()
            .map(|transaction| transaction.trie_hash())
            .collect::<Vec<_>>();

        // Sort hashes to enable efficient cursor traversal through the TransactionHashNumbers
        // table, which is keyed by hash. Without sorting, each seek would be O(log n) random
        // access; with sorting, the cursor advances sequentially through the B+tree.
        hashes.sort_unstable();

        // Number of transactions retrieved from the database should match the tx range count
        let tx_count = tx_range.count();
        if hashes.len() != tx_count {
            return Err(PrunerError::InconsistentData(
                "Unexpected number of transaction hashes retrieved by transaction number range",
            ));
        }

        let mut limiter = input.limiter;

        let mut last_pruned_transaction = None;
        let (pruned, done) =
            provider.tx_ref().prune_table_with_iterator::<tables::TransactionHashNumbers>(
                hashes,
                &mut limiter,
                |row| {
                    last_pruned_transaction =
                        Some(last_pruned_transaction.unwrap_or(row.1).max(row.1))
                },
            )?;

        let done = done && tx_range_end == end;
        trace!(target: "pruner", %pruned, %done, "Pruned transaction lookup");

        let last_pruned_transaction = last_pruned_transaction.unwrap_or(tx_range_end);

        let last_pruned_block = provider
            .block_by_transaction_id(last_pruned_transaction)?
            .ok_or(PrunerError::InconsistentData("Block for transaction is not found"))?
            // If there's more transaction lookup entries to prune, set the checkpoint block number
            // to previous, so we could finish pruning its transaction lookup entries on the next
            // run.
            .checked_sub(if done { 0 } else { 1 });

        let progress = limiter.progress(done);

        Ok(SegmentOutput {
            progress,
            pruned,
            checkpoint: Some(SegmentOutputCheckpoint {
                block_number: last_pruned_block,
                tx_number: Some(last_pruned_transaction),
            }),
        })
    }
}
