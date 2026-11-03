use crate::segments::{
    user::ReceiptsByLogs, AccountHistory, Bodies, Segment, SenderRecovery, StorageHistory,
    TransactionLookup, UserReceipts,
};
use alloy_eips::eip2718::Encodable2718;
use smartnet_primitives_traits::NodePrimitives;
use smartnet_prune_types::PruneModes;
use smartnet_db_api::{table::Value, transaction::DbTxMut};
use smartnet_provider::{
    AuxDbProviderFactory, BlockReader, ChainStateBlockReader, DBProvider, NodePrimitivesProvider,
    PruneCheckpointReader, PruneCheckpointWriter, TransactionsProvider,
};
use smartnet_storage_api::{ChangeSetReader, StorageChangeSetReader, StorageSettingsCache};

/// Collection of [`Segment`]. Thread-safe, allocated on the heap.
#[derive(Debug)]
pub struct SegmentSet<Provider> {
    inner: Vec<Box<dyn Segment<Provider>>>,
}

impl<Provider> SegmentSet<Provider> {
    /// Returns empty [`SegmentSet`] collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds new [`Segment`] to collection.
    pub fn segment<S: Segment<Provider> + 'static>(mut self, segment: S) -> Self {
        self.inner.push(Box::new(segment));
        self
    }

    /// Adds new [Segment] to collection if it's [Some].
    pub fn segment_opt<S: Segment<Provider> + 'static>(self, segment: Option<S>) -> Self {
        if let Some(segment) = segment {
            return self.segment(segment);
        }
        self
    }

    /// Consumes [`SegmentSet`] and returns a [Vec].
    pub fn into_vec(self) -> Vec<Box<dyn Segment<Provider>>> {
        self.inner
    }
}

impl<Provider> SegmentSet<Provider>
where
    Provider: DBProvider<Tx: DbTxMut>
        + PruneCheckpointWriter
        + PruneCheckpointReader
        + BlockReader<Transaction: Encodable2718>
        + ChainStateBlockReader
        + StorageSettingsCache
        + ChangeSetReader
        + StorageChangeSetReader
        + AuxDbProviderFactory
        + TransactionsProvider<Transaction: smartnet_codecs::Compact>
        + NodePrimitivesProvider<Primitives: NodePrimitives<SignedTx: Value, Receipt: Value, BlockHeader: Value>>,
{
    /// Creates a [`SegmentSet`] from the given [`PruneModes`].
    pub fn from_components(prune_modes: PruneModes) -> Self {
        let PruneModes {
            sender_recovery,
            transaction_lookup,
            receipts,
            account_history,
            storage_history,
            bodies_history,
            receipts_log_filter,
        } = prune_modes;

        Self::default()
            .segment_opt(transaction_lookup.map(TransactionLookup::new))
            // Bodies: no-op (block bodies live in the file tier); kept for config compat.
            .segment_opt(bodies_history.map(|mode| Bodies::new(mode, transaction_lookup)))
            // Account history
            .segment_opt(account_history.map(AccountHistory::new))
            // Storage history
            .segment_opt(storage_history.map(StorageHistory::new))
            // User receipts
            .segment_opt(receipts.map(UserReceipts::new))
            // Receipts by logs
            .segment_opt(
                (!receipts_log_filter.is_empty())
                    .then(|| ReceiptsByLogs::new(receipts_log_filter.clone())),
            )
            // Sender recovery
            .segment_opt(sender_recovery.map(SenderRecovery::new))
    }
}

impl<Provider> Default for SegmentSet<Provider> {
    fn default() -> Self {
        Self { inner: Vec::new() }
    }
}
