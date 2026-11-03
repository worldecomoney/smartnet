//! Transaction-location models.

use alloy_primitives::BlockNumber;

/// Where a canonical transaction lives: the block that contains it and the transaction's
/// index within that block (`fjall4a.md` §6.3, realized as the `tx_num → (block, index)`
/// half of the `canonical_tx_lookup` chain).
///
/// Part of the index `save_block_state_and_index` writes alongside the state. Together with
/// `TransactionHashNumbers` (`hash → tx_num`) it makes `tx_num`- and hash-keyed lookups
/// resolvable; the transaction payload itself lives in the file tier, not in Fjall.
#[derive(Debug, Default, Eq, PartialEq, Clone, Copy)]
#[cfg_attr(any(test, feature = "arbitrary"), derive(arbitrary::Arbitrary))]
#[cfg_attr(any(test, feature = "smartnet-codec"), derive(smartnet_codecs::Compact))]
#[cfg_attr(any(test, feature = "smartnet-codec"), smartnet_codecs::add_arbitrary_tests(compact))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StoredTransactionLocation {
    /// The number of the canonical block that contains the transaction.
    pub block_number: BlockNumber,
    /// The transaction's index within its block (0-based).
    pub index: u64,
}

impl StoredTransactionLocation {
    /// The tx location of `index` inside block `block_number`.
    pub const fn new(block_number: BlockNumber, index: u64) -> Self {
        Self { block_number, index }
    }
}

#[cfg(any(test, feature = "smartnet-codec"))]
smartnet_codecs::impl_compression_for_compact!(StoredTransactionLocation);
