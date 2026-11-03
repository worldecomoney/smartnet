//! Commonly used types for prune usage.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod checkpoint;
mod event;
mod mode;
mod pruner;
mod segment;
mod target;

use alloc::{collections::BTreeMap, vec::Vec};
use alloy_primitives::{Address, BlockNumber};
use core::ops::Deref;

pub use checkpoint::PruneCheckpoint;
pub use event::PrunerEvent;
pub use mode::PruneMode;
pub use pruner::{
    PruneInterruptReason, PruneProgress, PrunedSegmentInfo, PrunerOutput, SegmentOutput,
    SegmentOutputCheckpoint,
};
pub use segment::{PrunePurpose, PruneSegment, PruneSegmentError};
pub use target::{PruneModes, MINIMUM_DISTANCE, MINIMUM_UNWIND_SAFE_DISTANCE};

/// Configuration for pruning receipts not associated with logs emitted by the specified contracts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(any(test, feature = "serde"), derive(serde::Serialize, serde::Deserialize))]
pub struct ReceiptsLogPruneConfig(pub BTreeMap<Address, PruneMode>);

impl ReceiptsLogPruneConfig {
    /// Checks if the configuration is empty
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Given the `tip` block number, consolidates the structure so it can easily be queried for
    /// filtering across a range of blocks.
    ///
    /// Example:
    ///
    /// `{ addrA: Before(872), addrB: Before(500), addrC: Distance(128) }`
    ///
    ///    for `tip: 1000`, gets transformed to a map such as:
    ///
    /// `{ 500: [addrB], 872: [addrA, addrC] }`
    ///
    /// The [`BlockNumber`] key of the new map should be viewed as `PruneMode::Before(block)`, which
    /// makes the previous result equivalent to
    ///
    /// `{ Before(500): [addrB], Before(872): [addrA, addrC] }`
    pub fn group_by_block(
        &self,
        tip: BlockNumber,
        pruned_block: Option<BlockNumber>,
    ) -> Result<BTreeMap<BlockNumber, Vec<&Address>>, PruneSegmentError> {
        let mut map = BTreeMap::new();
        let base_block = pruned_block.unwrap_or_default() + 1;

        for (address, mode) in &self.0 {
            // Getting `None`, means that there is nothing to prune yet, so we need it to include in
            // the BTreeMap (block = 0), otherwise it will be excluded.
            // Reminder that this BTreeMap works as an inclusion list that excludes (prunes) all
            // other receipts.
            //
            // Reminder, that we increment because the [`BlockNumber`] key of the new map should be
            // viewed as `PruneMode::Before(block)`
            let block = base_block.max(
                mode.prune_target_block(tip, PruneSegment::ContractLogs, PrunePurpose::User)?
                    .map(|(block, _)| block)
                    .unwrap_or_default() +
                    1,
            );

            map.entry(block).or_insert_with(Vec::new).push(address)
        }
        Ok(map)
    }

    /// Returns the lowest block where we start filtering logs which use `PruneMode::Distance(_)`.
    pub fn lowest_block_with_distance(
        &self,
        tip: BlockNumber,
        pruned_block: Option<BlockNumber>,
    ) -> Result<Option<BlockNumber>, PruneSegmentError> {
        let pruned_block = pruned_block.unwrap_or_default();
        let mut lowest = None;

        for mode in self.values() {
            if mode.is_distance() &&
                let Some((block, _)) =
                    mode.prune_target_block(tip, PruneSegment::ContractLogs, PrunePurpose::User)?
            {
                lowest = Some(lowest.unwrap_or(u64::MAX).min(block));
            }
        }

        Ok(lowest.map(|lowest| lowest.max(pruned_block)))
    }
}

impl Deref for ReceiptsLogPruneConfig {
    type Target = BTreeMap<Address, PruneMode>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_by_block_empty_config() {
        let config = ReceiptsLogPruneConfig(BTreeMap::new());
        let tip = 1000;
        let pruned_block = None;

        let result = config.group_by_block(tip, pruned_block).unwrap();
        assert!(result.is_empty(), "The result should be empty when the config is empty");
    }

    #[test]
    fn test_group_by_block_single_entry() {
        let mut config_map = BTreeMap::new();
        let address = Address::new([1; 20]);
        let prune_mode = PruneMode::Before(500);
        config_map.insert(address, prune_mode);

        let config = ReceiptsLogPruneConfig(config_map);
        // Big tip to have something to prune for the target block
        let tip = 3000000;
        let pruned_block = Some(400);

        let result = config.group_by_block(tip, pruned_block).unwrap();

        // Expect one entry with block 500 and the corresponding address
        assert_eq!(result.len(), 1);
        assert_eq!(result[&500], vec![&address], "Address should be grouped under block 500");

        // Tip smaller than the target block, so that we have nothing to prune for the block
        let tip = 300;
        let pruned_block = Some(400);

        let result = config.group_by_block(tip, pruned_block).unwrap();

        // Expect one entry with block 400 and the corresponding address
        assert_eq!(result.len(), 1);
        assert_eq!(result[&401], vec![&address], "Address should be grouped under block 400");
    }

    #[test]
    fn test_group_by_block_with_distance_prune_mode() {
        let mut config_map = BTreeMap::new();
        let address = Address::new([1; 20]);
        let prune_mode = PruneMode::Distance(100000);
        config_map.insert(address, prune_mode);

        let config = ReceiptsLogPruneConfig(config_map);
        let tip = 100100;
        // Pruned block is smaller than the target block
        let pruned_block = Some(50);

        let result = config.group_by_block(tip, pruned_block).unwrap();

        // Expect the entry to be grouped under block 100 (tip - distance)
        assert_eq!(result.len(), 1);
        assert_eq!(result[&101], vec![&address], "Address should be grouped under block 100");

        let tip = 100100;
        // Pruned block is larger than the target block
        let pruned_block = Some(800);

        let result = config.group_by_block(tip, pruned_block).unwrap();

        // Expect the entry to be grouped under block 800 which is larger than tip - distance
        assert_eq!(result.len(), 1);
        assert_eq!(result[&801], vec![&address], "Address should be grouped under block 800");
    }

    #[test]
    fn test_lowest_block_with_distance_empty_config() {
        let config = ReceiptsLogPruneConfig(BTreeMap::new());
        let tip = 1000;
        let pruned_block = None;

        let result = config.lowest_block_with_distance(tip, pruned_block).unwrap();
        assert_eq!(result, None, "The result should be None when the config is empty");
    }

    #[test]
    fn test_lowest_block_with_distance_no_distance_mode() {
        let mut config_map = BTreeMap::new();
        let address = Address::new([1; 20]);
        let prune_mode = PruneMode::Before(500);
        config_map.insert(address, prune_mode);

        let config = ReceiptsLogPruneConfig(config_map);
        let tip = 1000;
        let pruned_block = None;

        let result = config.lowest_block_with_distance(tip, pruned_block).unwrap();
        assert_eq!(result, None, "The result should be None when there are no Distance modes");
    }

    #[test]
    fn test_lowest_block_with_distance_single_entry() {
        let mut config_map = BTreeMap::new();
        let address = Address::new([1; 20]);
        let prune_mode = PruneMode::Distance(100000);
        config_map.insert(address, prune_mode);

        let config = ReceiptsLogPruneConfig(config_map);

        let tip = 100100;
        let pruned_block = Some(400);

        // Expect the lowest block to be 400 as 400 > 100100 - 100000 (tip - distance)
        assert_eq!(
            config.lowest_block_with_distance(tip, pruned_block).unwrap(),
            Some(400),
            "The lowest block should be 400"
        );

        let tip = 100100;
        let pruned_block = Some(50);

        // Expect the lowest block to be 100 as 100 > 50 (pruned block)
        assert_eq!(
            config.lowest_block_with_distance(tip, pruned_block).unwrap(),
            Some(100),
            "The lowest block should be 100"
        );
    }
}
