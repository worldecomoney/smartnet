use alloy_primitives::{BlockNumber, TxNumber};

/// The persistence watermark of the last committed database transaction.
///
/// A single-row table (the key type lives next to the table declaration in
/// `smartnet-db-api`), written *inside the same transaction* as the data it describes,
/// so the watermark is always exactly the data that reached durable storage:
///
/// * `commit_sequence` — monotonic count of data-changing commits; returned as the database's
///   `last_txnid`, the RO-secondary sync watermark.
/// * `last_persisted_block` — inclusive canonical block number of the last persisted batch.
/// * `last_persisted_tx` — exclusive transaction-number watermark: `last(tx_number) + 1` of that
///   block, computed inside the write transaction, rewound by unwinds like every other positional
///   `TxNumber`.
#[derive(Debug, Default, Eq, PartialEq, Clone, Copy)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[cfg_attr(any(test, feature = "smartnet-codec"), derive(smartnet_codecs::Compact))]
#[cfg_attr(any(test, feature = "smartnet-codec"), smartnet_codecs::add_arbitrary_tests(compact))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PersistenceWatermark {
    /// Monotonic count of data-changing commits; `0` only before the first bump.
    pub commit_sequence: u64,
    /// Inclusive canonical block number of the last persisted batch.
    pub last_persisted_block: BlockNumber,
    /// Exclusive transaction-number watermark: `last(tx_number) + 1`.
    pub last_persisted_tx: TxNumber,
}

#[cfg(any(test, feature = "smartnet-codec"))]
smartnet_codecs::impl_compression_for_compact!(PersistenceWatermark);

#[cfg(test)]
mod tests {
    use super::*;
    

    #[test]
    fn default_is_absent_watermark() {
        let progress = PersistenceWatermark::default();
        assert_eq!(progress.commit_sequence, 0);
        assert_eq!(progress.last_persisted_block, 0);
        assert_eq!(progress.last_persisted_tx, 0);
    }
}
