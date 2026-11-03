//! clap [Args](clap::Args) for engine purposes

use clap::Args;
use eyre::ensure;
use smartnet_engine_primitives::{
    TreeConfig, DEFAULT_MULTIPROOF_TASK_CHUNK_SIZE, DEFAULT_PERSISTENCE_BACKPRESSURE_THRESHOLD,
    DEFAULT_SPARSE_TRIE_MAX_HOT_ACCOUNTS, DEFAULT_SPARSE_TRIE_MAX_HOT_SLOTS,
};
use std::{sync::OnceLock, time::Duration};

use crate::node_config::{
    DEFAULT_CROSS_BLOCK_CACHE_SIZE_MB, DEFAULT_MEMORY_BLOCK_BUFFER_TARGET,
    DEFAULT_PERSISTENCE_THRESHOLD, DEFAULT_RESERVED_CPU_CORES,
};

/// Global static engine defaults
static ENGINE_DEFAULTS: OnceLock<DefaultEngineValues> = OnceLock::new();

/// Default values for engine that can be customized
///
/// Global defaults can be set via [`DefaultEngineValues::try_init`].
#[derive(Debug, Clone)]
pub struct DefaultEngineValues {
    persistence_threshold: u64,
    persistence_backpressure_threshold: u64,
    memory_block_buffer_target: u64,
    legacy_state_root_task_enabled: bool,
    state_cache_disabled: bool,
    prewarming_disabled: bool,
    state_provider_metrics: bool,
    cross_block_cache_size: usize,
    state_root_task_compare_updates: bool,
    accept_execution_requests_hash: bool,
    multiproof_chunk_size: usize,
    reserved_cpu_cores: usize,
    precompile_cache_disabled: bool,
    always_process_payload_attributes_on_canonical_head: bool,
    storage_worker_count: Option<usize>,
    account_worker_count: Option<usize>,
    prewarming_threads: Option<usize>,
    cache_metrics_disabled: bool,
    sparse_trie_max_hot_slots: usize,
    sparse_trie_max_hot_accounts: usize,
    slow_block_threshold: Option<Duration>,
    disable_sparse_trie_cache_pruning: bool,
    state_root_task_timeout: Option<String>,
    share_execution_cache_with_payload_builder: bool,
    share_sparse_trie_with_payload_builder: bool,
}

impl DefaultEngineValues {
    /// Initialize the global engine defaults with this configuration
    pub fn try_init(self) -> Result<(), Self> {
        ENGINE_DEFAULTS.set(self)
    }

    /// Get a reference to the global engine defaults
    pub fn get_global() -> &'static Self {
        ENGINE_DEFAULTS.get_or_init(Self::default)
    }

    /// Set the default persistence threshold
    pub const fn with_persistence_threshold(mut self, v: u64) -> Self {
        self.persistence_threshold = v;
        self
    }

    /// Set the default persistence backpressure threshold
    pub const fn with_persistence_backpressure_threshold(mut self, v: u64) -> Self {
        self.persistence_backpressure_threshold = v;
        self
    }

    /// Set the default memory block buffer target
    pub const fn with_memory_block_buffer_target(mut self, v: u64) -> Self {
        self.memory_block_buffer_target = v;
        self
    }

    /// Set whether to enable legacy state root task by default
    pub const fn with_legacy_state_root_task_enabled(mut self, v: bool) -> Self {
        self.legacy_state_root_task_enabled = v;
        self
    }

    /// Set whether to disable state cache by default
    pub const fn with_state_cache_disabled(mut self, v: bool) -> Self {
        self.state_cache_disabled = v;
        self
    }

    /// Set whether to disable prewarming by default
    pub const fn with_prewarming_disabled(mut self, v: bool) -> Self {
        self.prewarming_disabled = v;
        self
    }

    /// Set whether to enable state provider metrics by default
    pub const fn with_state_provider_metrics(mut self, v: bool) -> Self {
        self.state_provider_metrics = v;
        self
    }

    /// Set the default cross-block cache size in MB
    pub const fn with_cross_block_cache_size(mut self, v: usize) -> Self {
        self.cross_block_cache_size = v;
        self
    }

    /// Set whether to compare state root task updates by default
    pub const fn with_state_root_task_compare_updates(mut self, v: bool) -> Self {
        self.state_root_task_compare_updates = v;
        self
    }

    /// Set whether to accept execution requests hash by default
    pub const fn with_accept_execution_requests_hash(mut self, v: bool) -> Self {
        self.accept_execution_requests_hash = v;
        self
    }

    /// Set the default multiproof chunk size
    pub const fn with_multiproof_chunk_size(mut self, v: usize) -> Self {
        self.multiproof_chunk_size = v;
        self
    }

    /// Set the default number of reserved CPU cores
    pub const fn with_reserved_cpu_cores(mut self, v: usize) -> Self {
        self.reserved_cpu_cores = v;
        self
    }

    /// Set whether to disable precompile cache by default
    pub const fn with_precompile_cache_disabled(mut self, v: bool) -> Self {
        self.precompile_cache_disabled = v;
        self
    }

    /// Set whether to always process payload attributes on canonical head by default
    pub const fn with_always_process_payload_attributes_on_canonical_head(
        mut self,
        v: bool,
    ) -> Self {
        self.always_process_payload_attributes_on_canonical_head = v;
        self
    }

    /// Set the default storage worker count
    pub const fn with_storage_worker_count(mut self, v: Option<usize>) -> Self {
        self.storage_worker_count = v;
        self
    }

    /// Set the default account worker count
    pub const fn with_account_worker_count(mut self, v: Option<usize>) -> Self {
        self.account_worker_count = v;
        self
    }

    /// Set the default prewarming thread count
    pub const fn with_prewarming_threads(mut self, v: Option<usize>) -> Self {
        self.prewarming_threads = v;
        self
    }

    /// Set whether to disable cache metrics by default
    pub const fn with_cache_metrics_disabled(mut self, v: bool) -> Self {
        self.cache_metrics_disabled = v;
        self
    }

    /// Set the LFU hot-slot capacity for sparse trie pruning by default
    pub const fn with_sparse_trie_max_hot_slots(mut self, v: usize) -> Self {
        self.sparse_trie_max_hot_slots = v;
        self
    }

    /// Set the LFU hot-account capacity for sparse trie pruning by default
    pub const fn with_sparse_trie_max_hot_accounts(mut self, v: usize) -> Self {
        self.sparse_trie_max_hot_accounts = v;
        self
    }

    /// Set the default slow block threshold.
    pub const fn with_slow_block_threshold(mut self, v: Option<Duration>) -> Self {
        self.slow_block_threshold = v;
        self
    }

    /// Set whether to disable sparse trie cache pruning by default
    pub const fn with_disable_sparse_trie_cache_pruning(mut self, v: bool) -> Self {
        self.disable_sparse_trie_cache_pruning = v;
        self
    }

    /// Set the default state root task timeout
    pub fn with_state_root_task_timeout(mut self, v: Option<String>) -> Self {
        self.state_root_task_timeout = v;
        self
    }

    /// Set whether to share the execution cache with the payload builder by default
    pub const fn with_share_execution_cache_with_payload_builder(mut self, v: bool) -> Self {
        self.share_execution_cache_with_payload_builder = v;
        self
    }

    /// Set whether to share the sparse trie with the payload builder by default
    pub const fn with_share_sparse_trie_with_payload_builder(mut self, v: bool) -> Self {
        self.share_sparse_trie_with_payload_builder = v;
        self
    }
}

impl Default for DefaultEngineValues {
    fn default() -> Self {
        Self {
            persistence_threshold: DEFAULT_PERSISTENCE_THRESHOLD,
            persistence_backpressure_threshold: DEFAULT_PERSISTENCE_BACKPRESSURE_THRESHOLD,
            memory_block_buffer_target: DEFAULT_MEMORY_BLOCK_BUFFER_TARGET,
            legacy_state_root_task_enabled: false,
            state_cache_disabled: false,
            prewarming_disabled: false,
            state_provider_metrics: false,
            cross_block_cache_size: DEFAULT_CROSS_BLOCK_CACHE_SIZE_MB,
            state_root_task_compare_updates: false,
            accept_execution_requests_hash: false,
            multiproof_chunk_size: DEFAULT_MULTIPROOF_TASK_CHUNK_SIZE,
            reserved_cpu_cores: DEFAULT_RESERVED_CPU_CORES,
            precompile_cache_disabled: false,
            always_process_payload_attributes_on_canonical_head: false,
            storage_worker_count: None,
            account_worker_count: None,
            prewarming_threads: None,
            cache_metrics_disabled: false,
            sparse_trie_max_hot_slots: DEFAULT_SPARSE_TRIE_MAX_HOT_SLOTS,
            sparse_trie_max_hot_accounts: DEFAULT_SPARSE_TRIE_MAX_HOT_ACCOUNTS,
            slow_block_threshold: None,
            disable_sparse_trie_cache_pruning: false,
            state_root_task_timeout: Some("1s".to_string()),
            share_execution_cache_with_payload_builder: false,
            share_sparse_trie_with_payload_builder: false,
        }
    }
}

/// Parameters for configuring the engine driver.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "Engine")]
pub struct EngineArgs {
    /// Configure persistence threshold for the engine. This determines how many canonical blocks
    /// must be in-memory, ahead of the last persisted block, before flushing canonical blocks to
    /// disk again.
    ///
    /// To persist blocks as fast as the node receives them, set this value to zero. This will
    /// cause more frequent DB writes.
    #[arg(skip)]
    pub persistence_threshold: u64,

    /// Configure the maximum canonical-minus-persisted gap before engine API processing stalls.
    ///
    /// This value must be greater than `--engine.persistence-threshold`.
    #[arg(skip)]
    pub persistence_backpressure_threshold: u64,

    /// Configure the target number of blocks to keep in memory.
    #[arg(skip)]
    pub memory_block_buffer_target: u64,

    /// Enable legacy state root
    #[arg(skip)]
    pub legacy_state_root_task_enabled: bool,

    /// CAUTION: This CLI flag has no effect anymore, use --engine.disable-caching-and-prewarming
    /// if you want to disable caching and prewarming
    #[arg(skip)]
    #[deprecated]
    pub caching_and_prewarming_enabled: bool,

    /// Disable state cache
    #[arg(skip)]
    pub state_cache_disabled: bool,

    /// Disable parallel prewarming
    #[arg(skip)]
    pub prewarming_disabled: bool,

    /// CAUTION: This CLI flag has no effect anymore. The parallel sparse trie is always enabled.
    #[deprecated]
    #[arg(skip)]
    pub parallel_sparse_trie_enabled: bool,

    /// CAUTION: This CLI flag has no effect anymore. The parallel sparse trie is always enabled.
    #[deprecated]
    #[arg(skip)]
    pub parallel_sparse_trie_disabled: bool,

    /// Enable state provider latency metrics. This allows the engine to collect and report stats
    /// about how long state provider calls took during execution, but this does introduce slight
    /// overhead to state provider calls.
    #[arg(skip)]
    pub state_provider_metrics: bool,

    /// Configure the size of cross-block cache in megabytes
    #[arg(skip)]
    pub cross_block_cache_size: usize,

    /// Enable comparing trie updates from the state root task to the trie updates from the regular
    /// state root calculation.
    #[arg(skip)]
    pub state_root_task_compare_updates: bool,

    /// Enables accepting requests hash instead of an array of requests in `engine_newPayloadV4`.
    #[arg(skip)]
    pub accept_execution_requests_hash: bool,

    /// Multiproof task chunk size for proof targets.
    #[arg(skip)]
    pub multiproof_chunk_size: usize,

    /// Configure the number of reserved CPU cores for non-node processes
    #[arg(skip)]
    pub reserved_cpu_cores: usize,

    /// CAUTION: This CLI flag has no effect anymore, use --engine.disable-precompile-cache
    /// if you want to disable precompile cache
    #[arg(skip)]
    #[deprecated]
    pub precompile_cache_enabled: bool,

    /// Disable precompile cache
    #[arg(skip)]
    pub precompile_cache_disabled: bool,

    /// Always process payload attributes and begin a payload build process even if
    /// `forkchoiceState.headBlockHash` is already the canonical head or an ancestor. See
    /// `TreeConfig::always_process_payload_attributes_on_canonical_head` for more details.
    ///
    /// Note: This is a no-op on OP Stack.
    #[arg(skip)]
    pub always_process_payload_attributes_on_canonical_head: bool,

    /// Configure the number of storage proof workers in the Tokio blocking pool.
    /// If not specified, defaults to 2x available parallelism.
    #[arg(skip)]
    pub storage_worker_count: Option<usize>,

    /// Configure the number of account proof workers in the Tokio blocking pool.
    /// If not specified, defaults to the same count as storage workers.
    #[arg(skip)]
    pub account_worker_count: Option<usize>,

    /// Configure the number of prewarming threads.
    /// If not specified, defaults to available parallelism.
    #[arg(skip)]
    pub prewarming_threads: Option<usize>,

    /// Disable cache metrics recording, which can take up to 50ms with large cached state.
    #[arg(skip)]
    pub cache_metrics_disabled: bool,

    /// LFU hot-slot capacity: max storage slots retained across sparse trie prune cycles.
    #[arg(skip)]
    pub sparse_trie_max_hot_slots: usize,

    /// LFU hot-account capacity: max account addresses retained across sparse trie prune cycles.
    #[arg(skip)]
    pub sparse_trie_max_hot_accounts: usize,

    /// Configure the slow block logging threshold in milliseconds.
    ///
    /// When set, blocks that take longer than this threshold to execute will be logged
    /// with detailed metrics including timing, state operations, and cache statistics.
    ///
    /// Set to 0 to log all blocks (useful for debugging/profiling).
    ///
    /// When not set, slow block logging is disabled (default).
    #[arg(skip)]
    pub slow_block_threshold: Option<Duration>,

    /// Fully disable sparse trie cache pruning. When set, the cached sparse trie is preserved
    /// without any node pruning or storage trie eviction between blocks. Useful for benchmarking
    /// the effects of retaining the full trie cache.
    #[arg(skip)]
    pub disable_sparse_trie_cache_pruning: bool,

    /// Configure the timeout for the state root task before spawning a sequential fallback.
    /// If the state root task takes longer than this, a sequential computation starts in
    /// parallel and whichever finishes first is used.
    ///
    /// --engine.state-root-task-timeout 1s
    /// --engine.state-root-task-timeout 400ms
    ///
    /// Set to 0s to disable.
    #[arg(skip)]
    pub state_root_task_timeout: Option<Duration>,

    /// Whether to share execution cache with the payload builder.
    ///
    /// When enabled, each payload job will get an instance of cross-block execution cache from the
    /// engine.
    ///
    /// Note: this should only be enabled if node would not be requested to process any payloads in
    /// parallel with payload building.
    #[arg(skip)]
    pub share_execution_cache_with_payload_builder: bool,

    /// Whether to share the sparse trie with the payload builder.
    ///
    /// Replaces the payload builder's blocking `state_root_with_updates()` call with the
    /// sparse trie, computing the state root concurrently with transaction execution.
    ///
    /// The engine and payload builder contend for the same trie — if a builder task is
    /// still running when `newPayload` arrives, the engine will block until the trie is
    /// stored back.
    ///
    /// The builder also anchors the trie at the built block's state root, so if the next
    /// `newPayload` is not on top of that block, the trie cache is invalidated and cleared.
    #[arg(skip)]
    pub share_sparse_trie_with_payload_builder: bool,

    /// Add random jitter before each proof computation (trie-debug only).
    /// Each proof worker sleeps for a random duration up to this value before
    /// starting work. Useful for stress-testing timing-sensitive proof logic.
    ///
    /// --engine.proof-jitter 100ms
    /// --engine.proof-jitter 1s
    #[cfg(feature = "trie-debug")]
    #[arg(skip)]
    pub proof_jitter: Option<Duration>,
}

#[allow(deprecated)]
impl Default for EngineArgs {
    fn default() -> Self {
        let DefaultEngineValues {
            persistence_threshold,
            persistence_backpressure_threshold,
            memory_block_buffer_target,
            legacy_state_root_task_enabled,
            state_cache_disabled,
            prewarming_disabled,
            state_provider_metrics,
            cross_block_cache_size,
            state_root_task_compare_updates,
            accept_execution_requests_hash,
            multiproof_chunk_size,
            reserved_cpu_cores,
            precompile_cache_disabled,
            always_process_payload_attributes_on_canonical_head,
            storage_worker_count,
            account_worker_count,
            prewarming_threads,
            cache_metrics_disabled,
            sparse_trie_max_hot_slots,
            sparse_trie_max_hot_accounts,
            slow_block_threshold,
            disable_sparse_trie_cache_pruning,
            state_root_task_timeout,
            share_execution_cache_with_payload_builder,
            share_sparse_trie_with_payload_builder,
        } = DefaultEngineValues::get_global().clone();
        Self {
            persistence_threshold,
            persistence_backpressure_threshold,
            memory_block_buffer_target,
            legacy_state_root_task_enabled,
            state_root_task_compare_updates,
            caching_and_prewarming_enabled: true,
            state_cache_disabled,
            prewarming_disabled,
            parallel_sparse_trie_enabled: true,
            parallel_sparse_trie_disabled: false,
            state_provider_metrics,
            cross_block_cache_size,
            accept_execution_requests_hash,
            multiproof_chunk_size,
            reserved_cpu_cores,
            precompile_cache_enabled: true,
            precompile_cache_disabled,
            always_process_payload_attributes_on_canonical_head,
            storage_worker_count,
            account_worker_count,
            prewarming_threads,
            cache_metrics_disabled,
            sparse_trie_max_hot_slots,
            sparse_trie_max_hot_accounts,
            slow_block_threshold,
            disable_sparse_trie_cache_pruning,
            state_root_task_timeout: state_root_task_timeout
                .as_deref()
                .map(|s| humantime::parse_duration(s).expect("valid default duration")),
            share_execution_cache_with_payload_builder,
            share_sparse_trie_with_payload_builder,
            #[cfg(feature = "trie-debug")]
            proof_jitter: None,
        }
    }
}

impl EngineArgs {
    /// Validates cross-field engine arguments.
    pub fn validate(&self) -> eyre::Result<()> {
        ensure!(
            self.persistence_backpressure_threshold > self.persistence_threshold,
            "--engine.persistence-backpressure-threshold ({}) must be greater than --engine.persistence-threshold ({})",
            self.persistence_backpressure_threshold,
            self.persistence_threshold
        );
        Ok(())
    }

    /// Creates a [`TreeConfig`] from the engine arguments (`SmartNet`: linear valid `newPayload`
    /// promotes canonical head; see `TreeConfig::auto_canonical_new_payload_on_linear_extend`).
    pub fn tree_config(&self) -> TreeConfig {
        // Backpressure first: both setters assert `backpressure > threshold` against the
        // config's *current* values, so the higher target (import batches) must land
        // before the threshold that has to sit below it.
        let config = TreeConfig::default()
            .with_persistence_backpressure_threshold(self.persistence_backpressure_threshold)
            .with_persistence_threshold(self.persistence_threshold)
            .with_memory_block_buffer_target(self.memory_block_buffer_target)
            .with_legacy_state_root(self.legacy_state_root_task_enabled)
            .without_state_cache(self.state_cache_disabled)
            .without_prewarming(self.prewarming_disabled)
            .with_state_provider_metrics(self.state_provider_metrics)
            .with_always_compare_trie_updates(self.state_root_task_compare_updates)
            .with_cross_block_cache_size(self.cross_block_cache_size * 1024 * 1024)
            .with_multiproof_chunk_size(self.multiproof_chunk_size)
            .with_reserved_cpu_cores(self.reserved_cpu_cores)
            .without_precompile_cache(self.precompile_cache_disabled)
            .with_always_process_payload_attributes_on_canonical_head(
                self.always_process_payload_attributes_on_canonical_head,
            )
            .without_cache_metrics(self.cache_metrics_disabled)
            .with_sparse_trie_max_hot_slots(self.sparse_trie_max_hot_slots)
            .with_sparse_trie_max_hot_accounts(self.sparse_trie_max_hot_accounts)
            .with_slow_block_threshold(self.slow_block_threshold)
            .with_disable_sparse_trie_cache_pruning(self.disable_sparse_trie_cache_pruning)
            .with_state_root_task_timeout(self.state_root_task_timeout.filter(|d| !d.is_zero()))
            .with_share_execution_cache_with_payload_builder(
                self.share_execution_cache_with_payload_builder,
            )
            .with_share_sparse_trie_with_payload_builder(
                self.share_sparse_trie_with_payload_builder,
            )
            // SmartNet runtime: linear `newPayload` extension promotes canonical head without FCU.
            .with_auto_canonical_new_payload_on_linear_extend(true);
        #[cfg(feature = "trie-debug")]
        let config = config.with_proof_jitter(self.proof_jitter);
        config
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
            "--engine.persistence-threshold",
            "--engine.persistence-backpressure-threshold",
            "--engine.memory-block-buffer-target",
            "--engine.legacy-state-root",
            "--engine.disable-state-cache",
            "--engine.disable-prewarming",
            "--engine.state-provider-metrics",
            "--engine.cross-block-cache-size",
            "--engine.state-root-task-compare-updates",
            "--engine.accept-execution-requests-hash",
            "--engine.multiproof-chunk-size",
            "--engine.reserved-cpu-cores",
            "--engine.disable-precompile-cache",
            "--engine.always-process-payload-attributes-on-canonical-head",
            "--engine.allow-unwind-canonical-header",
            "--engine.storage-worker-count",
            "--engine.account-worker-count",
            "--engine.prewarming-threads",
            "--engine.disable-cache-metrics",
            "--engine.sparse-trie-max-hot-slots",
            "--engine.sparse-trie-max-hot-accounts",
            "--engine.disable-sparse-trie-cache-pruning",
            "--engine.state-root-task-timeout",
            "--engine.slow-block-threshold",
        ];
        for flag in REMOVED {
            assert!(
                CommandParser::<EngineArgs>::try_parse_from(["smartnet", flag, "x"]).is_err(),
                "{flag} must stay rejected (config-file-driven CLI)"
            );
        }
    }

    #[test]
    #[allow(deprecated)]  // asserts the (deprecated) config-driven default fields
    fn test_parse_engine_args() {
        // Defaults are config-driven (`DefaultEngineValues::get_global()`); assert the
        // structural invariants that must hold regardless of configured values.
        let args = EngineArgs::default();
        assert!(args.caching_and_prewarming_enabled);
        assert!(args.parallel_sparse_trie_enabled);
        assert!(!args.parallel_sparse_trie_disabled);
        assert!(args.precompile_cache_enabled);
        args.validate().expect("default engine args must validate");
        CommandParser::<EngineArgs>::parse_from(["smartnet"]);
    }


    #[test]
    fn validate_rejects_invalid_backpressure_threshold() {
        let args = EngineArgs {
            persistence_threshold: 4,
            persistence_backpressure_threshold: 4,
            ..EngineArgs::default()
        };

        let err = args.validate().unwrap_err().to_string();
        assert!(err.contains("engine.persistence-backpressure-threshold"));
        assert!(err.contains("engine.persistence-threshold"));
    }

}
