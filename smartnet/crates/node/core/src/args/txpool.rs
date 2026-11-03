//! Transaction pool arguments

use crate::cli::config::SmartnetTransactionPoolConfig;
use alloy_eips::eip1559::{ETHEREUM_BLOCK_GAS_LIMIT_30M, MIN_PROTOCOL_BASE_FEE};
use alloy_primitives::Address;
use clap::Args;
use smartnet_transaction_pool::{
    blobstore::disk::DEFAULT_MAX_CACHED_BLOBS,
    maintain::MAX_QUEUED_TRANSACTION_LIFETIME,
    pool::{NEW_TX_LISTENER_BUFFER_SIZE, PENDING_TX_LISTENER_BUFFER_SIZE},
    validate::DEFAULT_MAX_TX_INPUT_BYTES,
    LocalTransactionConfig, PoolConfig, PriceBumpConfig, SubPoolLimit, DEFAULT_PRICE_BUMP,
    DEFAULT_TXPOOL_ADDITIONAL_VALIDATION_TASKS, MAX_NEW_PENDING_TXS_NOTIFICATIONS,
    REPLACE_BLOB_PRICE_BUMP, TXPOOL_MAX_ACCOUNT_SLOTS_PER_SENDER,
    TXPOOL_SUBPOOL_MAX_SIZE_MB_DEFAULT, TXPOOL_SUBPOOL_MAX_TXS_DEFAULT,
};
use std::{path::PathBuf, sync::OnceLock, time::Duration};

/// Global static transaction pool defaults
static TXPOOL_DEFAULTS: OnceLock<DefaultTxPoolValues> = OnceLock::new();

/// Default values for transaction pool that can be customized
///
/// Global defaults can be set via [`DefaultTxPoolValues::try_init`].
#[derive(Debug, Clone)]
pub struct DefaultTxPoolValues {
    pending_max_count: usize,
    pending_max_size: usize,
    basefee_max_count: usize,
    basefee_max_size: usize,
    queued_max_count: usize,
    queued_max_size: usize,
    blobpool_max_count: usize,
    blobpool_max_size: usize,
    blob_cache_size: Option<u32>,
    disable_blobs_support: bool,
    max_account_slots: usize,
    price_bump: u128,
    minimal_protocol_basefee: u64,
    minimum_priority_fee: Option<u128>,
    enforced_gas_limit: u64,
    max_tx_gas_limit: Option<u64>,
    blob_transaction_price_bump: u128,
    max_tx_input_bytes: usize,
    max_cached_entries: u32,
    no_locals: bool,
    locals: Vec<Address>,
    no_local_transactions_propagation: bool,
    additional_validation_tasks: usize,
    pending_tx_listener_buffer_size: usize,
    new_tx_listener_buffer_size: usize,
    max_new_pending_txs_notifications: usize,
    max_queued_lifetime: Duration,
    transactions_backup_path: Option<PathBuf>,
    disable_transactions_backup: bool,
    max_batch_size: usize,
}

impl DefaultTxPoolValues {
    /// Initialize the global transaction pool defaults with this configuration
    pub fn try_init(self) -> Result<(), Self> {
        TXPOOL_DEFAULTS.set(self)
    }

    /// Get a reference to the global transaction pool defaults
    pub fn get_global() -> &'static Self {
        TXPOOL_DEFAULTS.get_or_init(Self::default)
    }

    /// Set the default pending sub-pool max transaction count
    pub const fn with_pending_max_count(mut self, v: usize) -> Self {
        self.pending_max_count = v;
        self
    }

    /// Set the default pending sub-pool max size in MB
    pub const fn with_pending_max_size(mut self, v: usize) -> Self {
        self.pending_max_size = v;
        self
    }

    /// Set the default basefee sub-pool max transaction count
    pub const fn with_basefee_max_count(mut self, v: usize) -> Self {
        self.basefee_max_count = v;
        self
    }

    /// Set the default basefee sub-pool max size in MB
    pub const fn with_basefee_max_size(mut self, v: usize) -> Self {
        self.basefee_max_size = v;
        self
    }

    /// Set the default queued sub-pool max transaction count
    pub const fn with_queued_max_count(mut self, v: usize) -> Self {
        self.queued_max_count = v;
        self
    }

    /// Set the default queued sub-pool max size in MB
    pub const fn with_queued_max_size(mut self, v: usize) -> Self {
        self.queued_max_size = v;
        self
    }

    /// Set the default blobpool max transaction count
    pub const fn with_blobpool_max_count(mut self, v: usize) -> Self {
        self.blobpool_max_count = v;
        self
    }

    /// Set the default blobpool max size in MB
    pub const fn with_blobpool_max_size(mut self, v: usize) -> Self {
        self.blobpool_max_size = v;
        self
    }

    /// Set the default blob cache size
    pub const fn with_blob_cache_size(mut self, v: Option<u32>) -> Self {
        self.blob_cache_size = v;
        self
    }

    /// Set whether to disable blob transaction support by default
    pub const fn with_disable_blobs_support(mut self, v: bool) -> Self {
        self.disable_blobs_support = v;
        self
    }

    /// Set the default max account slots
    pub const fn with_max_account_slots(mut self, v: usize) -> Self {
        self.max_account_slots = v;
        self
    }

    /// Set the default price bump percentage
    pub const fn with_price_bump(mut self, v: u128) -> Self {
        self.price_bump = v;
        self
    }

    /// Set the default minimal protocol base fee
    pub const fn with_minimal_protocol_basefee(mut self, v: u64) -> Self {
        self.minimal_protocol_basefee = v;
        self
    }

    /// Set the default minimum priority fee
    pub const fn with_minimum_priority_fee(mut self, v: Option<u128>) -> Self {
        self.minimum_priority_fee = v;
        self
    }

    /// Set the default enforced gas limit
    pub const fn with_enforced_gas_limit(mut self, v: u64) -> Self {
        self.enforced_gas_limit = v;
        self
    }

    /// Set the default max transaction gas limit
    pub const fn with_max_tx_gas_limit(mut self, v: Option<u64>) -> Self {
        self.max_tx_gas_limit = v;
        self
    }

    /// Set the default blob transaction price bump
    pub const fn with_blob_transaction_price_bump(mut self, v: u128) -> Self {
        self.blob_transaction_price_bump = v;
        self
    }

    /// Set the default max transaction input bytes
    pub const fn with_max_tx_input_bytes(mut self, v: usize) -> Self {
        self.max_tx_input_bytes = v;
        self
    }

    /// Set the default max cached entries
    pub const fn with_max_cached_entries(mut self, v: u32) -> Self {
        self.max_cached_entries = v;
        self
    }

    /// Set whether to disable local transaction exemptions by default
    pub const fn with_no_locals(mut self, v: bool) -> Self {
        self.no_locals = v;
        self
    }

    /// Set the default local addresses
    pub fn with_locals(mut self, v: Vec<Address>) -> Self {
        self.locals = v;
        self
    }

    /// Set whether to disable local transaction propagation by default
    pub const fn with_no_local_transactions_propagation(mut self, v: bool) -> Self {
        self.no_local_transactions_propagation = v;
        self
    }

    /// Set the default additional validation tasks
    pub const fn with_additional_validation_tasks(mut self, v: usize) -> Self {
        self.additional_validation_tasks = v;
        self
    }

    /// Set the default pending transaction listener buffer size
    pub const fn with_pending_tx_listener_buffer_size(mut self, v: usize) -> Self {
        self.pending_tx_listener_buffer_size = v;
        self
    }

    /// Set the default new transaction listener buffer size
    pub const fn with_new_tx_listener_buffer_size(mut self, v: usize) -> Self {
        self.new_tx_listener_buffer_size = v;
        self
    }

    /// Set the default max new pending transactions notifications
    pub const fn with_max_new_pending_txs_notifications(mut self, v: usize) -> Self {
        self.max_new_pending_txs_notifications = v;
        self
    }

    /// Set the default max queued lifetime
    pub const fn with_max_queued_lifetime(mut self, v: Duration) -> Self {
        self.max_queued_lifetime = v;
        self
    }

    /// Set the default transactions backup path
    pub fn with_transactions_backup_path(mut self, v: Option<PathBuf>) -> Self {
        self.transactions_backup_path = v;
        self
    }

    /// Set whether to disable transaction backup by default
    pub const fn with_disable_transactions_backup(mut self, v: bool) -> Self {
        self.disable_transactions_backup = v;
        self
    }

    /// Set the default max batch size
    pub const fn with_max_batch_size(mut self, v: usize) -> Self {
        self.max_batch_size = v;
        self
    }
}

impl Default for DefaultTxPoolValues {
    fn default() -> Self {
        Self {
            pending_max_count: TXPOOL_SUBPOOL_MAX_TXS_DEFAULT,
            pending_max_size: TXPOOL_SUBPOOL_MAX_SIZE_MB_DEFAULT,
            basefee_max_count: TXPOOL_SUBPOOL_MAX_TXS_DEFAULT,
            basefee_max_size: TXPOOL_SUBPOOL_MAX_SIZE_MB_DEFAULT,
            queued_max_count: TXPOOL_SUBPOOL_MAX_TXS_DEFAULT,
            queued_max_size: TXPOOL_SUBPOOL_MAX_SIZE_MB_DEFAULT,
            blobpool_max_count: TXPOOL_SUBPOOL_MAX_TXS_DEFAULT,
            blobpool_max_size: TXPOOL_SUBPOOL_MAX_SIZE_MB_DEFAULT,
            blob_cache_size: None,
            disable_blobs_support: false,
            max_account_slots: TXPOOL_MAX_ACCOUNT_SLOTS_PER_SENDER,
            price_bump: DEFAULT_PRICE_BUMP,
            minimal_protocol_basefee: MIN_PROTOCOL_BASE_FEE,
            minimum_priority_fee: None,
            enforced_gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
            max_tx_gas_limit: None,
            blob_transaction_price_bump: REPLACE_BLOB_PRICE_BUMP,
            max_tx_input_bytes: DEFAULT_MAX_TX_INPUT_BYTES,
            max_cached_entries: DEFAULT_MAX_CACHED_BLOBS,
            no_locals: false,
            locals: Vec::new(),
            no_local_transactions_propagation: false,
            additional_validation_tasks: DEFAULT_TXPOOL_ADDITIONAL_VALIDATION_TASKS,
            pending_tx_listener_buffer_size: PENDING_TX_LISTENER_BUFFER_SIZE,
            new_tx_listener_buffer_size: NEW_TX_LISTENER_BUFFER_SIZE,
            max_new_pending_txs_notifications: MAX_NEW_PENDING_TXS_NOTIFICATIONS,
            max_queued_lifetime: MAX_QUEUED_TRANSACTION_LIFETIME,
            transactions_backup_path: None,
            disable_transactions_backup: false,
            max_batch_size: 1,
        }
    }
}

/// Parameters for debugging purposes
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "TxPool")]
pub struct TxPoolArgs {
    /// Max number of transaction in the pending sub-pool.
    #[arg(skip)]
    pub pending_max_count: usize,
    /// Max size of the pending sub-pool in megabytes.
    #[arg(skip)]
    pub pending_max_size: usize,

    /// Max number of transaction in the basefee sub-pool
    #[arg(skip)]
    pub basefee_max_count: usize,
    /// Max size of the basefee sub-pool in megabytes.
    #[arg(skip)]
    pub basefee_max_size: usize,

    /// Max number of transaction in the queued sub-pool
    #[arg(skip)]
    pub queued_max_count: usize,
    /// Max size of the queued sub-pool in megabytes.
    #[arg(skip)]
    pub queued_max_size: usize,

    /// Max number of transaction in the blobpool
    #[arg(skip)]
    pub blobpool_max_count: usize,
    /// Max size of the blobpool in megabytes.
    #[arg(skip)]
    pub blobpool_max_size: usize,

    /// Max number of entries for the in memory cache of the blob store.
    #[arg(skip)]
    pub blob_cache_size: Option<u32>,

    /// Disable EIP-4844 blob transaction support
    #[arg(skip)]
    pub disable_blobs_support: bool,

    /// Max number of executable transaction slots guaranteed per account
    #[arg(skip)]
    pub max_account_slots: usize,

    /// Price bump (in %) for the transaction pool underpriced check.
    #[arg(skip)]
    pub price_bump: u128,

    /// Minimum base fee required by the protocol.
    #[arg(skip)]
    pub minimal_protocol_basefee: u64,

    /// Minimum priority fee required for transaction acceptance into the pool.
    /// Transactions with priority fee below this value will be rejected.
    #[arg(skip)]
    pub minimum_priority_fee: Option<u128>,

    /// The default enforced gas limit for transactions entering the pool
    #[arg(skip)]
    pub enforced_gas_limit: u64,

    /// Maximum gas limit for individual transactions. Transactions exceeding this limit will be
    /// rejected by the transaction pool
    #[arg(skip)]
    pub max_tx_gas_limit: Option<u64>,

    /// Price bump percentage to replace an already existing blob transaction
    #[arg(skip)]
    pub blob_transaction_price_bump: u128,

    /// Max size in bytes of a single transaction allowed to enter the pool
    #[arg(skip)]
    pub max_tx_input_bytes: usize,

    /// The maximum number of blobs to keep in the in memory blob cache.
    #[arg(skip)]
    pub max_cached_entries: u32,

    /// Flag to disable local transaction exemptions.
    #[arg(skip)]
    pub no_locals: bool,
    /// Flag to allow certain addresses as local.
    #[arg(skip)]
    pub locals: Vec<Address>,
    /// Flag to toggle local transaction propagation.
    #[arg(skip)]
    pub no_local_transactions_propagation: bool,

    /// Number of additional transaction validation tasks to spawn.
    #[arg(skip)]
    pub additional_validation_tasks: usize,

    /// Maximum number of pending transactions from the network to buffer
    #[arg(skip)]
    pub pending_tx_listener_buffer_size: usize,

    /// Maximum number of new transactions to buffer
    #[arg(skip)]
    pub new_tx_listener_buffer_size: usize,

    /// How many new pending transactions to buffer and send to in progress pending transaction
    /// iterators.
    #[arg(skip)]
    pub max_new_pending_txs_notifications: usize,

    /// Maximum amount of time non-executable transaction are queued.
    #[arg(skip)]
    pub max_queued_lifetime: Duration,

    /// Path to store the local transaction backup at, to survive node restarts.
    #[arg(skip)]
    pub transactions_backup_path: Option<PathBuf>,

    /// Disables transaction backup to disk on node shutdown.
    #[arg(skip)]
    pub disable_transactions_backup: bool,

    /// Max batch size for transaction pool insertions
    #[arg(skip)]
    pub max_batch_size: usize,
}

impl TxPoolArgs {
    /// Sets the minimal protocol base fee to 0, effectively disabling checks that enforce that a
    /// transaction's fee must be higher than the [`MIN_PROTOCOL_BASE_FEE`] which is the lowest
    /// value the ethereum EIP-1559 base fee can reach.
    pub const fn with_disabled_protocol_base_fee(self) -> Self {
        self.with_protocol_base_fee(0)
    }

    /// Configures the minimal protocol base fee that should be enforced.
    ///
    /// Ethereum's EIP-1559 base fee can't drop below [`MIN_PROTOCOL_BASE_FEE`] hence this is
    /// enforced by default in the pool.
    pub const fn with_protocol_base_fee(mut self, protocol_base_fee: u64) -> Self {
        self.minimal_protocol_basefee = protocol_base_fee;
        self
    }
}

impl Default for TxPoolArgs {
    fn default() -> Self {
        let DefaultTxPoolValues {
            pending_max_count,
            pending_max_size,
            basefee_max_count,
            basefee_max_size,
            queued_max_count,
            queued_max_size,
            blobpool_max_count,
            blobpool_max_size,
            blob_cache_size,
            disable_blobs_support,
            max_account_slots,
            price_bump,
            minimal_protocol_basefee,
            minimum_priority_fee,
            enforced_gas_limit,
            max_tx_gas_limit,
            blob_transaction_price_bump,
            max_tx_input_bytes,
            max_cached_entries,
            no_locals,
            locals,
            no_local_transactions_propagation,
            additional_validation_tasks,
            pending_tx_listener_buffer_size,
            new_tx_listener_buffer_size,
            max_new_pending_txs_notifications,
            max_queued_lifetime,
            transactions_backup_path,
            disable_transactions_backup,
            max_batch_size,
        } = DefaultTxPoolValues::get_global().clone();
        Self {
            pending_max_count,
            pending_max_size,
            basefee_max_count,
            basefee_max_size,
            queued_max_count,
            queued_max_size,
            blobpool_max_count,
            blobpool_max_size,
            blob_cache_size,
            disable_blobs_support,
            max_account_slots,
            price_bump,
            minimal_protocol_basefee,
            minimum_priority_fee,
            enforced_gas_limit,
            max_tx_gas_limit,
            blob_transaction_price_bump,
            max_tx_input_bytes,
            max_cached_entries,
            no_locals,
            locals,
            no_local_transactions_propagation,
            additional_validation_tasks,
            pending_tx_listener_buffer_size,
            new_tx_listener_buffer_size,
            max_new_pending_txs_notifications,
            max_queued_lifetime,
            transactions_backup_path,
            disable_transactions_backup,
            max_batch_size,
        }
    }
}

impl SmartnetTransactionPoolConfig for TxPoolArgs {
    /// Returns transaction pool configuration.
    fn pool_config(&self) -> PoolConfig {
        let default_config = PoolConfig::default();
        PoolConfig {
            local_transactions_config: LocalTransactionConfig {
                no_exemptions: self.no_locals,
                local_addresses: self.locals.iter().copied().collect(),
                propagate_local_transactions: !self.no_local_transactions_propagation,
            },
            pending_limit: SubPoolLimit {
                max_txs: self.pending_max_count,
                max_size: self.pending_max_size.saturating_mul(1024 * 1024),
            },
            basefee_limit: SubPoolLimit {
                max_txs: self.basefee_max_count,
                max_size: self.basefee_max_size.saturating_mul(1024 * 1024),
            },
            queued_limit: SubPoolLimit {
                max_txs: self.queued_max_count,
                max_size: self.queued_max_size.saturating_mul(1024 * 1024),
            },
            blob_limit: SubPoolLimit {
                max_txs: self.blobpool_max_count,
                max_size: self.blobpool_max_size.saturating_mul(1024 * 1024),
            },
            blob_cache_size: self.blob_cache_size,
            max_account_slots: self.max_account_slots,
            price_bumps: PriceBumpConfig {
                default_price_bump: self.price_bump,
                replace_blob_tx_price_bump: self.blob_transaction_price_bump,
            },
            minimal_protocol_basefee: self.minimal_protocol_basefee,
            minimum_priority_fee: self.minimum_priority_fee,
            gas_limit: self.enforced_gas_limit,
            pending_tx_listener_buffer_size: self.pending_tx_listener_buffer_size,
            new_tx_listener_buffer_size: self.new_tx_listener_buffer_size,
            max_new_pending_txs_notifications: self.max_new_pending_txs_notifications,
            max_queued_lifetime: self.max_queued_lifetime,
            max_inflight_delegated_slot_limit: default_config.max_inflight_delegated_slot_limit,
        }
    }

    /// Returns max batch size for transaction batch insertion.
    fn max_batch_size(&self) -> usize {
        self.max_batch_size
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
            "--txpool.lifetime",
            "--txpool.pending-max-count",
            "--txpool.pending-max-size",
            "--txpool.basefee-max-count",
            "--txpool.basefee-max-size",
            "--txpool.queued-max-count",
            "--txpool.queued-max-size",
            "--txpool.blobpool-max-count",
            "--txpool.blobpool-max-size",
            "--txpool.blob-cache-size",
            "--txpool.max-account-slots",
            "--txpool.pricebump",
            "--txpool.minimal-protocol-fee",
            "--txpool.minimum-priority-fee",
            "--txpool.gas-limit",
            "--txpool.max-tx-gas",
            "--blobpool.pricebump",
            "--txpool.max-tx-input-bytes",
            "--txpool.max-cached-entries",
            "--txpool.nolocals",
            "--txpool.locals",
            "--txpool.no-local-transactions-propagation",
            "--txpool.additional-validation-tasks",
            "--txpool.max-pending-txns",
            "--txpool.max-new-txns",
            "--txpool.max-new-pending-txs-notifications",
            "--txpool.transactions-backup",
            "--txpool.max-batch-size",
        ];
        for flag in REMOVED {
            assert!(
                CommandParser::<TxPoolArgs>::try_parse_from(["smartnet", flag, "x"]).is_err(),
                "{flag} must stay rejected (config-file-driven CLI)"
            );
        }
    }

    #[test]
    fn txpool_args_default_sanity_test() {
        // Defaults are config-driven (`DefaultTxPoolValues::get_global()`); lock the
        // key pool-sizing values.
        let args = TxPoolArgs::default();
        assert_eq!(args.pending_max_count, 100_000);
        assert_eq!(args.pending_max_size, 64);
        assert_eq!(args.basefee_max_count, 100_000);
        assert_eq!(args.queued_max_count, 100_000);
        assert_eq!(args.blobpool_max_count, 100_000);
        assert_eq!(args.max_account_slots, 100_000);
        assert_eq!(args.price_bump, 10);
        assert_eq!(args.minimal_protocol_basefee, 7);
        assert_eq!(args.enforced_gas_limit, 30_000_000);
        assert_eq!(args.blob_transaction_price_bump, 100);
        assert_eq!(args.max_tx_input_bytes, 131_072);
        assert_eq!(args.max_cached_entries, 100);
        assert_eq!(args.max_batch_size, 1);
        assert_eq!(args.max_queued_lifetime, Duration::from_secs(28_800));
        assert_eq!(args.additional_validation_tasks, 1);
        CommandParser::<TxPoolArgs>::parse_from(["smartnet"]);
    }



}
