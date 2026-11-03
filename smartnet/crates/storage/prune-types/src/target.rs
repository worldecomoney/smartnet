use crate::{PruneMode, ReceiptsLogPruneConfig};

/// Minimum distance from the tip necessary for the node to work correctly:
/// 1. Minimum 2 epochs (32 blocks per epoch) required to handle any reorg according to the
///    consensus protocol.
/// 2. Another 10k blocks to have a room for maneuver in case when things go wrong and a manual
///    unwind is required.
pub const MINIMUM_UNWIND_SAFE_DISTANCE: u64 = 32 * 2 + 10_000;

/// Minimum blocks to retain for receipts and bodies to ensure reorg safety.
/// This prevents pruning data that may be needed when handling chain reorganizations,
/// specifically when `canonical_block_by_hash` needs to reconstruct `ExecutedBlock` from disk.
pub const MINIMUM_DISTANCE: u64 = 64;

/// Pruning configuration for every segment of the data that can be pruned.
#[derive(Debug, Clone, Eq, PartialEq, Default)]
#[cfg_attr(any(test, feature = "serde"), derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(any(test, feature = "serde"), serde(default))]
pub struct PruneModes {
    /// Sender Recovery pruning configuration.
    #[cfg_attr(any(test, feature = "serde"), serde(skip_serializing_if = "Option::is_none"))]
    pub sender_recovery: Option<PruneMode>,
    /// Transaction Lookup pruning configuration.
    #[cfg_attr(any(test, feature = "serde"), serde(skip_serializing_if = "Option::is_none"))]
    pub transaction_lookup: Option<PruneMode>,
    /// Receipts pruning configuration. This setting overrides `receipts_log_filter`
    /// and offers improved performance.
    #[cfg_attr(any(test, feature = "serde"), serde(skip_serializing_if = "Option::is_none"))]
    pub receipts: Option<PruneMode>,
    /// Account History pruning configuration.
    #[cfg_attr(
        any(test, feature = "serde"),
        serde(
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_opt_prune_mode_with_min_blocks::<MINIMUM_UNWIND_SAFE_DISTANCE, _>"
        )
    )]
    pub account_history: Option<PruneMode>,
    /// Storage History pruning configuration.
    #[cfg_attr(
        any(test, feature = "serde"),
        serde(
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_opt_prune_mode_with_min_blocks::<MINIMUM_UNWIND_SAFE_DISTANCE, _>"
        )
    )]
    pub storage_history: Option<PruneMode>,
    /// Bodies History pruning configuration.
    #[cfg_attr(any(test, feature = "serde"), serde(skip_serializing_if = "Option::is_none"))]
    pub bodies_history: Option<PruneMode>,
    /// Receipts pruning configuration by retaining only those receipts that contain logs emitted
    /// by the specified addresses, discarding others. This setting is overridden by `receipts`.
    ///
    /// The [`BlockNumber`](`crate::BlockNumber`) represents the starting block from which point
    /// onwards the receipts are preserved.
    #[cfg_attr(
        any(test, feature = "serde"),
        serde(skip_serializing_if = "ReceiptsLogPruneConfig::is_empty")
    )]
    pub receipts_log_filter: ReceiptsLogPruneConfig,
}

impl PruneModes {
    /// Sets pruning to all targets.
    pub fn all() -> Self {
        Self {
            sender_recovery: Some(PruneMode::Full),
            transaction_lookup: Some(PruneMode::Full),
            receipts: Some(PruneMode::Full),
            account_history: Some(PruneMode::Full),
            storage_history: Some(PruneMode::Full),
            bodies_history: Some(PruneMode::Full),
            receipts_log_filter: Default::default(),
        }
    }

    /// Returns whether there is any kind of receipt pruning configuration.
    pub fn has_receipts_pruning(&self) -> bool {
        self.receipts.is_some() || !self.receipts_log_filter.is_empty()
    }

    /// Migrates deprecated prune mode values to their new defaults.
    ///
    /// Returns `true` if any migration was performed.
    pub const fn migrate(&mut self) -> bool {
        match &self.receipts {
            Some(PruneMode::Full | PruneMode::Distance(0..MINIMUM_DISTANCE)) => {
                self.receipts = Some(PruneMode::Distance(MINIMUM_DISTANCE));
                true
            }
            _ => false,
        }
    }
}

/// Deserializes [`Option<PruneMode>`] and validates that the value is not less than the const
/// generic parameter `MIN_BLOCKS`. This parameter represents the number of blocks that needs to be
/// left in database after the pruning.
///
/// 1. For [`PruneMode::Full`], it fails if `MIN_BLOCKS > 0`.
/// 2. For [`PruneMode::Distance`], it fails if `distance < MIN_BLOCKS + 1`. `+ 1` is needed because
///    `PruneMode::Distance(0)` means that we leave zero blocks from the latest, meaning we have one
///    block in the database.
#[cfg(any(test, feature = "serde"))]
fn deserialize_opt_prune_mode_with_min_blocks<
    'de,
    const MIN_BLOCKS: u64,
    D: serde::Deserializer<'de>,
>(
    deserializer: D,
) -> Result<Option<PruneMode>, D::Error> {
    use serde::Deserialize;
    let prune_mode = Option::<PruneMode>::deserialize(deserializer)?;
    if let Some(prune_mode) = prune_mode.as_ref() {
        serde_deserialize_validate::<MIN_BLOCKS, D>(prune_mode)?;
    }
    Ok(prune_mode)
}

#[cfg(any(test, feature = "serde"))]
fn serde_deserialize_validate<'a, 'de, const MIN_BLOCKS: u64, D: serde::Deserializer<'de>>(
    prune_mode: &'a PruneMode,
) -> Result<(), D::Error> {
    use alloc::format;
    match prune_mode {
        PruneMode::Full if MIN_BLOCKS > 0 => {
            Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str("full"),
                // This message should have "expected" wording
                &format!("prune mode that leaves at least {MIN_BLOCKS} blocks in the database")
                    .as_str(),
            ))
        }
        PruneMode::Distance(distance) if *distance < MIN_BLOCKS => {
            Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Unsigned(*distance),
                // This message should have "expected" wording
                &format!("prune mode that leaves at least {MIN_BLOCKS} blocks in the database")
                    .as_str(),
            ))
        }
        _ => Ok(()),
    }
}
