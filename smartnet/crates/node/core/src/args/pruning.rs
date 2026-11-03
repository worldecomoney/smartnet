//! Pruning and full node arguments

use crate::primitives::EthereumHardfork;
use alloy_primitives::BlockNumber;
use clap::Args;
use smartnet_prune_types::{
    PruneMode, PruneModes, ReceiptsLogPruneConfig, MINIMUM_DISTANCE, MINIMUM_UNWIND_SAFE_DISTANCE,
};
use smartnet_chainspec::EthereumHardforks;
use smartnet_config::config::PruneConfig;
use std::{ops::Not, sync::OnceLock};

/// Global static pruning defaults
static PRUNING_DEFAULTS: OnceLock<DefaultPruningValues> = OnceLock::new();

/// Default values for `--full` and `--minimal` pruning modes that can be customized.
///
/// Global defaults can be set via [`DefaultPruningValues::try_init`].
#[derive(Debug, Clone)]
pub struct DefaultPruningValues {
    /// Prune modes for `--full` flag.
    ///
    /// Note: `bodies_history` is ignored when `full_bodies_history_use_pre_merge` is `true`.
    pub full_prune_modes: PruneModes,
    /// If `true`, `--full` will set `bodies_history` to prune everything before the merge block
    /// (Paris hardfork). If `false`, uses `full_prune_modes.bodies_history` directly.
    pub full_bodies_history_use_pre_merge: bool,
    /// Prune modes for `--minimal` flag.
    pub minimal_prune_modes: PruneModes,
}

impl DefaultPruningValues {
    /// Initialize the global pruning defaults with this configuration.
    ///
    /// Returns `Err(self)` if already initialized.
    pub fn try_init(self) -> Result<(), Self> {
        PRUNING_DEFAULTS.set(self)
    }

    /// Get a reference to the global pruning defaults.
    pub fn get_global() -> &'static Self {
        PRUNING_DEFAULTS.get_or_init(Self::default)
    }

    /// Set the prune modes for `--full` flag.
    pub fn with_full_prune_modes(mut self, modes: PruneModes) -> Self {
        self.full_prune_modes = modes;
        self
    }

    /// Set whether `--full` should use pre-merge pruning for bodies history.
    ///
    /// When `true` (default), bodies are pruned before the Paris hardfork block.
    /// When `false`, uses `full_prune_modes.bodies_history` directly.
    pub const fn with_full_bodies_history_use_pre_merge(mut self, use_pre_merge: bool) -> Self {
        self.full_bodies_history_use_pre_merge = use_pre_merge;
        self
    }

    /// Set the prune modes for `--minimal` flag.
    pub fn with_minimal_prune_modes(mut self, modes: PruneModes) -> Self {
        self.minimal_prune_modes = modes;
        self
    }
}

impl Default for DefaultPruningValues {
    fn default() -> Self {
        Self {
            full_prune_modes: PruneModes {
                sender_recovery: Some(PruneMode::Full),
                transaction_lookup: None,
                receipts: Some(PruneMode::Distance(MINIMUM_UNWIND_SAFE_DISTANCE)),
                account_history: Some(PruneMode::Distance(MINIMUM_UNWIND_SAFE_DISTANCE)),
                storage_history: Some(PruneMode::Distance(MINIMUM_UNWIND_SAFE_DISTANCE)),
                // This field is ignored when full_bodies_history_use_pre_merge is true
                bodies_history: None,
                receipts_log_filter: Default::default(),
            },
            full_bodies_history_use_pre_merge: true,
            minimal_prune_modes: PruneModes {
                sender_recovery: Some(PruneMode::Full),
                transaction_lookup: Some(PruneMode::Full),
                receipts: Some(PruneMode::Distance(MINIMUM_DISTANCE)),
                account_history: Some(PruneMode::Distance(MINIMUM_UNWIND_SAFE_DISTANCE)),
                storage_history: Some(PruneMode::Distance(MINIMUM_UNWIND_SAFE_DISTANCE)),
                bodies_history: Some(PruneMode::Distance(MINIMUM_UNWIND_SAFE_DISTANCE)),
                receipts_log_filter: Default::default(),
            },
        }
    }
}

/// Parameters for pruning and full node
#[derive(Debug, Clone, Args, PartialEq, Eq, Default)]
#[command(next_help_heading = "Pruning")]
pub struct PruningArgs {
    /// Run full node. Only the most recent [`MINIMUM_UNWIND_SAFE_DISTANCE`] block states are
    /// stored.
    #[arg(skip)]
    pub full: bool,

    /// Run minimal storage mode with maximum pruning and smaller static files.
    ///
    /// This mode configures the node to use minimal disk space by:
    /// - Fully pruning sender recovery, transaction lookup, receipts
    /// - Leaving 10,064 blocks for account, storage history and block bodies
    /// - Using 10,000 blocks per static file segment
    #[arg(skip)]
    pub minimal: bool,

    /// Minimum pruning interval measured in blocks.
    #[arg(skip)]
    pub block_interval: Option<u64>,

    // Sender Recovery
    /// Prunes all sender recovery data.
    #[arg(skip)]
    pub sender_recovery_full: bool,
    /// Prune sender recovery data before the `head-N` block number. In other words, keep last N +
    /// 1 blocks.
    #[arg(skip)]
    pub sender_recovery_distance: Option<u64>,
    /// Prune sender recovery data before the specified block number. The specified block number is
    /// not pruned.
    #[arg(skip)]
    pub sender_recovery_before: Option<BlockNumber>,

    // Transaction Lookup
    /// Prunes all transaction lookup data.
    #[arg(skip)]
    pub transaction_lookup_full: bool,
    /// Prune transaction lookup data before the `head-N` block number. In other words, keep last N
    /// + 1 blocks.
    #[arg(skip)]
    pub transaction_lookup_distance: Option<u64>,
    /// Prune transaction lookup data before the specified block number. The specified block number
    /// is not pruned.
    #[arg(skip)]
    pub transaction_lookup_before: Option<BlockNumber>,

    // Receipts
    /// Prunes all receipt data.
    #[arg(skip)]
    pub receipts_full: bool,
    /// Prune receipts before the merge block.
    #[arg(skip)]
    pub receipts_pre_merge: bool,
    /// Prune receipts before the `head-N` block number. In other words, keep last N + 1 blocks.
    #[arg(skip)]
    pub receipts_distance: Option<u64>,
    /// Prune receipts before the specified block number. The specified block number is not pruned.
    #[arg(skip)]
    pub receipts_before: Option<BlockNumber>,
    // Receipts Log Filter
    /// Configure receipts log filter. Format:
    /// <`address`>:<`prune_mode`>... where <`prune_mode`> can be 'full', 'distance:<`blocks`>', or
    /// 'before:<`block_number`>'
    #[arg(skip)]
    pub receipts_log_filter: Option<ReceiptsLogPruneConfig>,

    // Account History
    /// Prunes all account history.
    #[arg(skip)]
    pub account_history_full: bool,
    /// Prune account before the `head-N` block number. In other words, keep last N + 1 blocks.
    #[arg(skip)]
    pub account_history_distance: Option<u64>,
    /// Prune account history before the specified block number. The specified block number is not
    /// pruned.
    #[arg(skip)]
    pub account_history_before: Option<BlockNumber>,

    // Storage History
    /// Prunes all storage history data.
    #[arg(skip)]
    pub storage_history_full: bool,
    /// Prune storage history before the `head-N` block number. In other words, keep last N + 1
    /// blocks.
    #[arg(skip)]
    pub storage_history_distance: Option<u64>,
    /// Prune storage history before the specified block number. The specified block number is not
    /// pruned.
    #[arg(skip)]
    pub storage_history_before: Option<BlockNumber>,

    // Bodies
    /// Prune bodies before the merge block.
    #[arg(skip)]
    pub bodies_pre_merge: bool,
    /// Prune bodies before the `head-N` block number. In other words, keep last N + 1
    /// blocks.
    #[arg(skip)]
    pub bodies_distance: Option<u64>,
    /// Prune storage history before the specified block number. The specified block number is not
    /// pruned.
    #[arg(skip)]
    pub bodies_before: Option<BlockNumber>,

    /// Minimum pruning distance from the tip. This controls the safety margin for reorgs and
    /// manual unwinds.
    #[arg(skip)]
    pub minimum_distance: Option<u64>,

    /// Number of most recent blocks for which state changesets are retained. Older changesets are
    /// deleted in the same transaction that advances the head, keeping the changeset tables
    /// bounded. An unwind that would need changesets older than this window is refused.
    #[arg(skip)]
    pub changeset_window: Option<u64>,
}

impl PruningArgs {
    /// Returns pruning configuration.
    ///
    /// Returns [`None`] if no parameters are specified and default pruning configuration should be
    /// used.
    pub fn prune_config<ChainSpec>(&self, chain_spec: &ChainSpec) -> Option<PruneConfig>
    where
        ChainSpec: EthereumHardforks,
    {
        // Initialize with a default prune configuration.
        let mut config = PruneConfig::default();

        // If --full is set, use full node defaults.
        if self.full {
            let defaults = DefaultPruningValues::get_global();
            let mut segments = defaults.full_prune_modes.clone();
            if defaults.full_bodies_history_use_pre_merge {
                segments.bodies_history = chain_spec
                    .ethereum_fork_activation(EthereumHardfork::Paris)
                    .block_number()
                    .map(PruneMode::Before);
            }
            config = PruneConfig {
                block_interval: config.block_interval,
                segments,
                minimum_pruning_distance: config.minimum_pruning_distance,
                changeset_window: config.changeset_window,
            }
        }

        // If --minimal is set, use minimal storage mode with aggressive pruning.
        if self.minimal {
            config = PruneConfig {
                block_interval: config.block_interval,
                segments: DefaultPruningValues::get_global().minimal_prune_modes.clone(),
                minimum_pruning_distance: config.minimum_pruning_distance,
                changeset_window: config.changeset_window,
            }
        }

        // Override with any explicitly set prune.* flags.
        if let Some(block_interval) = self.block_interval {
            config.block_interval = block_interval as usize;
        }
        if let Some(distance) = self.minimum_distance {
            config.minimum_pruning_distance = distance;
        }
        if let Some(window) = self.changeset_window {
            config.changeset_window = window;
        }
        if let Some(mode) = self.sender_recovery_prune_mode() {
            config.segments.sender_recovery = Some(mode);
        }
        if let Some(mode) = self.transaction_lookup_prune_mode() {
            config.segments.transaction_lookup = Some(mode);
        }
        if let Some(mode) = self.receipts_prune_mode(chain_spec) {
            config.segments.receipts = Some(mode);
        }
        if let Some(mode) = self.account_history_prune_mode() {
            config.segments.account_history = Some(mode);
        }
        if let Some(mode) = self.bodies_prune_mode(chain_spec) {
            config.segments.bodies_history = Some(mode);
        }
        if let Some(mode) = self.storage_history_prune_mode() {
            config.segments.storage_history = Some(mode);
        }
        if let Some(receipt_logs) =
            self.receipts_log_filter.as_ref().filter(|c| !c.is_empty()).cloned()
        {
            config.segments.receipts_log_filter = receipt_logs;
            // need to remove the receipts segment filter entirely because that takes precedence
            // over the logs filter
            config.segments.receipts.take();
        }

        config.is_default().not().then_some(config)
    }

    fn bodies_prune_mode<ChainSpec>(&self, chain_spec: &ChainSpec) -> Option<PruneMode>
    where
        ChainSpec: EthereumHardforks,
    {
        if self.bodies_pre_merge {
            chain_spec
                .ethereum_fork_activation(EthereumHardfork::Paris)
                .block_number()
                .map(PruneMode::Before)
        } else if let Some(distance) = self.bodies_distance {
            Some(PruneMode::Distance(distance))
        } else {
            self.bodies_before.map(PruneMode::Before)
        }
    }

    const fn sender_recovery_prune_mode(&self) -> Option<PruneMode> {
        if self.sender_recovery_full {
            Some(PruneMode::Full)
        } else if let Some(distance) = self.sender_recovery_distance {
            Some(PruneMode::Distance(distance))
        } else if let Some(block_number) = self.sender_recovery_before {
            Some(PruneMode::Before(block_number))
        } else {
            None
        }
    }

    const fn transaction_lookup_prune_mode(&self) -> Option<PruneMode> {
        if self.transaction_lookup_full {
            Some(PruneMode::Full)
        } else if let Some(distance) = self.transaction_lookup_distance {
            Some(PruneMode::Distance(distance))
        } else if let Some(block_number) = self.transaction_lookup_before {
            Some(PruneMode::Before(block_number))
        } else {
            None
        }
    }

    fn receipts_prune_mode<ChainSpec>(&self, chain_spec: &ChainSpec) -> Option<PruneMode>
    where
        ChainSpec: EthereumHardforks,
    {
        if self.receipts_pre_merge {
            chain_spec
                .ethereum_fork_activation(EthereumHardfork::Paris)
                .block_number()
                .map(PruneMode::Before)
        } else if self.receipts_full {
            Some(PruneMode::Full)
        } else if let Some(distance) = self.receipts_distance {
            Some(PruneMode::Distance(distance))
        } else {
            self.receipts_before.map(PruneMode::Before)
        }
    }

    const fn account_history_prune_mode(&self) -> Option<PruneMode> {
        if self.account_history_full {
            Some(PruneMode::Full)
        } else if let Some(distance) = self.account_history_distance {
            Some(PruneMode::Distance(distance))
        } else if let Some(block_number) = self.account_history_before {
            Some(PruneMode::Before(block_number))
        } else {
            None
        }
    }

    const fn storage_history_prune_mode(&self) -> Option<PruneMode> {
        if self.storage_history_full {
            Some(PruneMode::Full)
        } else if let Some(distance) = self.storage_history_distance {
            Some(PruneMode::Distance(distance))
        } else if let Some(block_number) = self.storage_history_before {
            Some(PruneMode::Before(block_number))
        } else {
            None
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    /// The operator CLI is config-file driven: these per-flag options were removed
    /// (all struct fields are `#[arg(skip)]`). The legacy flags must stay rejected.
    #[test]
    fn removed_cli_flags_are_rejected() {
        const REMOVED: &[&str] = &[
            "--prune.receiptslogfilter",
        ];
        for flag in REMOVED {
            assert!(
                CommandParser::<PruningArgs>::try_parse_from(["smartnet", flag, "x"]).is_err(),
                "{flag} must stay rejected (config-file-driven CLI)"
            );
        }
    }

    #[test]
    fn test_parse_default_pruning_args() {
        let default_args = PruningArgs::default();
        let args = CommandParser::<PruningArgs>::parse_from(["smartnet"]).args;
        assert_eq!(args, default_args);
    }

}
