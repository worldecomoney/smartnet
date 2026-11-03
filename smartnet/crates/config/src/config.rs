//! Configuration files.
use smartnet_prune_types::{PruneModes, MINIMUM_UNWIND_SAFE_DISTANCE};
use smartnet_stages_types::ExecutionStageThresholds;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(feature = "serde")]
const EXTENSION: &str = "toml";

/// The default prune block interval
pub const DEFAULT_BLOCK_INTERVAL: usize = 5;

/// The default number of most recent blocks for which state changesets are retained
/// (`AccountChangeSets` / `StorageChangeSets`). Older changesets are deleted in the same
/// transaction that advances the head, keeping the tables bounded. An unwind that would need
/// changesets older than this window is refused.
pub const DEFAULT_CHANGESET_WINDOW: u64 = 10_000;

/// Configuration for the node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    /// Root data directory. Per-chain files live under `<datadir>/<chain_id>/`.
    ///
    /// Required for operator `smartnet --config <path>` workflows (no `--datadir` CLI).
    #[cfg_attr(feature = "serde", serde(default, alias = "DATADIR_PATH"))]
    pub datadir: Option<PathBuf>,

    /// Storage engine dictated by config (`DB_ENGINE`): `fjall` (default when absent,
    /// and the only engine — the historical `sqlite`/`redb` names are rejected with a
    /// dedicated error).
    ///
    /// Stored as a plain string so this crate stays independent of the database crate;
    /// consumers validate the name via `smartnet_db::DatabaseBackend::from_str` and
    /// stamp the resolved engine onto every datadir path as `DATADIR_PATH/DB_ENGINE`
    /// (the engine directory is a child of whatever this key names).
    #[cfg_attr(feature = "serde", serde(default, alias = "DB_ENGINE"))]
    pub db_engine: Option<String>,

    /// Chain for node run: built-in name (e.g. `mainnet`) or path to a chainspec JSON file.
    ///
    /// Required when starting the node with `smartnet --config <path>` (no subcommand). Ignored for
    /// `smartnet --config <path> init <genesis>` (the genesis argument defines the chain).
    #[cfg_attr(feature = "serde", serde(default, alias = "CHAIN"))]
    pub chain: Option<String>,

    /// Configuration for each stage in the pipeline.
    pub stages: StageConfig,
    /// Configuration for pruning.
    #[cfg_attr(feature = "serde", serde(default))]
    pub prune: PruneConfig,
}

/// Load a [`Config`] from an existing file. Fails if the file is missing or invalid TOML.
#[cfg(feature = "serde")]
pub fn load_config_file(path: impl AsRef<Path>) -> eyre::Result<Config> {
    let path = path.as_ref();
    let cfg_string = std::fs::read_to_string(path)
        .map_err(|e| eyre::eyre!("Failed to read {}: {e}", path.display()))?;
    toml::from_str(&cfg_string)
        .map_err(|e| eyre::eyre!("Failed to parse TOML {}: {e}", path.display()))
}

impl Config {
    /// Sets the pruning configuration.
    pub fn set_prune_config(&mut self, prune_config: PruneConfig) {
        self.prune = prune_config;
    }
}

#[cfg(feature = "serde")]
impl Config {
    /// Load a [`Config`] from a specified path.
    ///
    /// A new configuration file is created with default values if none
    /// exists.
    pub fn from_path(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(cfg_string) => {
                toml::from_str(&cfg_string).map_err(|e| eyre::eyre!("Failed to parse TOML: {e}"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| eyre::eyre!("Failed to create directory: {e}"))?;
                }
                let cfg = Self::default();
                let s = toml::to_string_pretty(&cfg)
                    .map_err(|e| eyre::eyre!("Failed to serialize to TOML: {e}"))?;
                std::fs::write(path, s)
                    .map_err(|e| eyre::eyre!("Failed to write configuration file: {e}"))?;
                Ok(cfg)
            }
            Err(e) => Err(eyre::eyre!("Failed to load configuration: {e}")),
        }
    }

    /// Save the configuration to toml file.
    pub fn save(&self, path: &Path) -> Result<(), std::io::Error> {
        if path.extension() != Some(std::ffi::OsStr::new(EXTENSION)) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("config file extension must be '{EXTENSION}'"),
            ));
        }

        std::fs::write(
            path,
            toml::to_string(self)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
        )
    }
}

/// Configuration for each stage in the pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct StageConfig {
    /// Header stage configuration.
    pub headers: HeadersConfig,
    /// Body stage configuration.
    pub bodies: BodiesConfig,
    /// Sender Recovery stage configuration.
    pub sender_recovery: SenderRecoveryConfig,
    /// Execution stage configuration.
    pub execution: ExecutionConfig,
    /// Prune stage configuration.
    pub prune: PruneStageConfig,
    /// Account Hashing stage configuration.
    pub account_hashing: HashingConfig,
    /// Storage Hashing stage configuration.
    pub storage_hashing: HashingConfig,
    /// Merkle stage configuration.
    pub merkle: MerkleConfig,
    /// Transaction Lookup stage configuration.
    pub transaction_lookup: TransactionLookupConfig,
    /// Index Account History stage configuration.
    pub index_account_history: IndexHistoryConfig,
    /// Index Storage History stage configuration.
    pub index_storage_history: IndexHistoryConfig,
    /// Common ETL related configuration.
    pub etl: EtlConfig,
}

impl StageConfig {
    /// The highest threshold (in number of blocks) for switching between incremental and full
    /// calculations across `MerkleStage`, `AccountHashingStage` and `StorageHashingStage`. This is
    /// required to figure out if can prune or not changesets on subsequent pipeline runs during
    /// `ExecutionStage`
    pub fn execution_external_clean_threshold(&self) -> u64 {
        self.merkle
            .incremental_threshold
            .max(self.account_hashing.clean_threshold)
            .max(self.storage_hashing.clean_threshold)
    }
}

/// Header stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct HeadersConfig {
    /// The maximum number of requests to send concurrently.
    ///
    /// Default: 100
    pub downloader_max_concurrent_requests: usize,
    /// The minimum number of requests to send concurrently.
    ///
    /// Default: 5
    pub downloader_min_concurrent_requests: usize,
    /// Maximum amount of responses to buffer internally.
    /// The response contains multiple headers.
    pub downloader_max_buffered_responses: usize,
    /// The maximum number of headers to request from a peer at a time.
    pub downloader_request_limit: u64,
    /// The maximum number of headers to download before committing progress to the database.
    pub commit_threshold: u64,
}

impl Default for HeadersConfig {
    fn default() -> Self {
        Self {
            commit_threshold: 10_000,
            downloader_request_limit: 1_000,
            downloader_max_concurrent_requests: 100,
            downloader_min_concurrent_requests: 5,
            downloader_max_buffered_responses: 100,
        }
    }
}

/// Body stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct BodiesConfig {
    /// The batch size of non-empty blocks per one request
    ///
    /// Default: 200
    pub downloader_request_limit: u64,
    /// The maximum number of block bodies returned at once from the stream
    ///
    /// Default: `1_000`
    pub downloader_stream_batch_size: usize,
    /// The size of the internal block buffer in bytes.
    ///
    /// Default: 2GB
    pub downloader_max_buffered_blocks_size_bytes: usize,
    /// The minimum number of requests to send concurrently.
    ///
    /// Default: 5
    pub downloader_min_concurrent_requests: usize,
    /// The maximum number of requests to send concurrently.
    /// This is equal to the max number of peers.
    ///
    /// Default: 100
    pub downloader_max_concurrent_requests: usize,
}

impl Default for BodiesConfig {
    fn default() -> Self {
        Self {
            downloader_request_limit: 200,
            downloader_stream_batch_size: 1_000,
            downloader_max_buffered_blocks_size_bytes: 2 * 1024 * 1024 * 1024, // ~2GB
            downloader_min_concurrent_requests: 5,
            downloader_max_concurrent_requests: 100,
        }
    }
}

/// Sender recovery stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct SenderRecoveryConfig {
    /// The maximum number of transactions to process before committing progress to the database.
    pub commit_threshold: u64,
}

impl Default for SenderRecoveryConfig {
    fn default() -> Self {
        Self { commit_threshold: 5_000_000 }
    }
}

/// Execution stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct ExecutionConfig {
    /// The maximum number of blocks to process before the execution stage commits.
    pub max_blocks: Option<u64>,
    /// The maximum number of state changes to keep in memory before the execution stage commits.
    pub max_changes: Option<u64>,
    /// The maximum cumulative amount of gas to process before the execution stage commits.
    pub max_cumulative_gas: Option<u64>,
    /// The maximum time spent on blocks processing before the execution stage commits.
    #[cfg_attr(
        feature = "serde",
        serde(
            serialize_with = "humantime_serde::serialize",
            deserialize_with = "deserialize_duration"
        )
    )]
    pub max_duration: Option<Duration>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_blocks: Some(500_000),
            max_changes: Some(5_000_000),
            // 50k full blocks of 30M gas
            max_cumulative_gas: Some(30_000_000 * 50_000),
            // 10 minutes
            max_duration: Some(Duration::from_secs(10 * 60)),
        }
    }
}

impl From<ExecutionConfig> for ExecutionStageThresholds {
    fn from(config: ExecutionConfig) -> Self {
        Self {
            max_blocks: config.max_blocks,
            max_changes: config.max_changes,
            max_cumulative_gas: config.max_cumulative_gas,
            max_duration: config.max_duration,
        }
    }
}

/// Prune stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct PruneStageConfig {
    /// The maximum number of entries to prune before committing progress to the database.
    pub commit_threshold: usize,
}

impl Default for PruneStageConfig {
    fn default() -> Self {
        Self { commit_threshold: 1_000_000 }
    }
}

/// Hashing stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct HashingConfig {
    /// The threshold (in number of blocks) for switching between
    /// incremental hashing and full hashing.
    pub clean_threshold: u64,
    /// The maximum number of entities to process before committing progress to the database.
    pub commit_threshold: u64,
    /// The maximum number of changeset entries to process before committing progress. The stage
    /// commits after either `commit_threshold` blocks or `commit_entries` entries, whichever
    /// comes first. This bounds memory usage when blocks contain many state changes.
    pub commit_entries: u64,
}

impl Default for HashingConfig {
    fn default() -> Self {
        Self { clean_threshold: 500_000, commit_threshold: 100_000, commit_entries: 30_000_000 }
    }
}

/// Merkle stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct MerkleConfig {
    /// The number of blocks we will run the incremental root method for when we are catching up on
    /// the merkle stage for a large number of blocks.
    ///
    /// When we are catching up for a large number of blocks, we can only run the incremental root
    /// for a limited number of blocks, otherwise the incremental root method may cause the node to
    /// OOM. This number determines how many blocks in a row we will run the incremental root
    /// method for.
    pub incremental_threshold: u64,
    /// The threshold (in number of blocks) for switching from incremental trie building of changes
    /// to whole rebuild.
    pub rebuild_threshold: u64,
}

impl Default for MerkleConfig {
    fn default() -> Self {
        Self { incremental_threshold: 7_000, rebuild_threshold: 100_000 }
    }
}

/// Transaction Lookup stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct TransactionLookupConfig {
    /// The maximum number of transactions to process before writing to disk.
    pub chunk_size: u64,
}

impl Default for TransactionLookupConfig {
    fn default() -> Self {
        Self { chunk_size: 5_000_000 }
    }
}

/// Common ETL related configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct EtlConfig {
    /// Data directory where temporary files are created.
    pub dir: Option<PathBuf>,
    /// The maximum size in bytes of data held in memory before being flushed to disk as a file.
    pub file_size: usize,
}

impl Default for EtlConfig {
    fn default() -> Self {
        Self { dir: None, file_size: Self::default_file_size() }
    }
}

impl EtlConfig {
    /// Creates an ETL configuration
    pub const fn new(dir: Option<PathBuf>, file_size: usize) -> Self {
        Self { dir, file_size }
    }

    /// Return default ETL directory from datadir path.
    pub fn from_datadir(path: &Path) -> PathBuf {
        path.join("etl-tmp")
    }

    /// Default size in bytes of data held in memory before being flushed to disk as a file.
    pub const fn default_file_size() -> usize {
        // 500 MB
        500 * (1024 * 1024)
    }
}

/// History stage configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct IndexHistoryConfig {
    /// The maximum number of blocks to process before committing progress to the database.
    pub commit_threshold: u64,
}

impl Default for IndexHistoryConfig {
    fn default() -> Self {
        Self { commit_threshold: 100_000 }
    }
}

/// Pruning configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct PruneConfig {
    /// Minimum pruning interval measured in blocks.
    pub block_interval: usize,
    /// Pruning configuration for every part of the data that can be pruned.
    #[cfg_attr(feature = "serde", serde(alias = "parts"))]
    pub segments: PruneModes,
    /// Minimum distance from the tip required for pruning. Controls the safety margin for
    /// reorgs and manual unwinds. Defaults to [`MINIMUM_UNWIND_SAFE_DISTANCE`].
    #[cfg_attr(feature = "serde", serde(default = "default_minimum_pruning_distance"))]
    pub minimum_pruning_distance: u64,
    /// Number of most recent blocks for which `AccountChangeSets` / `StorageChangeSets` rows are
    /// retained. Older rows are deleted in the same transaction that advances the head, so the
    /// tables never grow past roughly this many blocks. An unwind that would need changesets
    /// older than this window is refused (`ProviderError::InsufficientChangesets`). Defaults to
    /// [`DEFAULT_CHANGESET_WINDOW`].
    #[cfg_attr(feature = "serde", serde(default = "default_changeset_window"))]
    pub changeset_window: u64,
}

/// Returns the default minimum pruning distance.
#[cfg(feature = "serde")]
const fn default_minimum_pruning_distance() -> u64 {
    MINIMUM_UNWIND_SAFE_DISTANCE
}

/// Returns the default changeset retention window.
#[cfg(feature = "serde")]
const fn default_changeset_window() -> u64 {
    DEFAULT_CHANGESET_WINDOW
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            block_interval: DEFAULT_BLOCK_INTERVAL,
            segments: PruneModes::default(),
            minimum_pruning_distance: MINIMUM_UNWIND_SAFE_DISTANCE,
            changeset_window: DEFAULT_CHANGESET_WINDOW,
        }
    }
}

impl PruneConfig {
    /// Returns whether this configuration is the default one.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Returns whether there is any kind of receipt pruning configuration.
    pub fn has_receipts_pruning(&self) -> bool {
        self.segments.has_receipts_pruning()
    }

    /// Merges values from `other` into `self`.
    /// - `Option<PruneMode>` fields: set from `other` only if `self` is `None`.
    /// - `block_interval`: set from `other` only if `self.block_interval ==
    ///   DEFAULT_BLOCK_INTERVAL`.
    /// - `receipts_log_filter`: set from `other` only if `self` is empty and `other` is non-empty.
    pub fn merge(&mut self, other: Self) {
        let Self {
            block_interval,
            segments:
                PruneModes {
                    sender_recovery,
                    transaction_lookup,
                    receipts,
                    account_history,
                    storage_history,
                    bodies_history,
                    receipts_log_filter,
                },
            minimum_pruning_distance,
            changeset_window,
        } = other;

        // Merge block_interval, only update if it's the default interval
        if self.block_interval == DEFAULT_BLOCK_INTERVAL {
            self.block_interval = block_interval;
        }

        // Merge minimum_pruning_distance, only update if it's the default
        if self.minimum_pruning_distance == MINIMUM_UNWIND_SAFE_DISTANCE {
            self.minimum_pruning_distance = minimum_pruning_distance;
        }

        // Merge changeset_window, only update if it's the default
        if self.changeset_window == DEFAULT_CHANGESET_WINDOW {
            self.changeset_window = changeset_window;
        }

        // Merge the various segment prune modes
        self.segments.sender_recovery = self.segments.sender_recovery.or(sender_recovery);
        self.segments.transaction_lookup = self.segments.transaction_lookup.or(transaction_lookup);
        self.segments.receipts = self.segments.receipts.or(receipts);
        self.segments.account_history = self.segments.account_history.or(account_history);
        self.segments.storage_history = self.segments.storage_history.or(storage_history);
        self.segments.bodies_history = self.segments.bodies_history.or(bodies_history);

        if self.segments.receipts_log_filter.0.is_empty() && !receipts_log_filter.0.is_empty() {
            self.segments.receipts_log_filter = receipts_log_filter;
        }
    }
}

/// Helper type to support older versions of Duration deserialization.
#[cfg(feature = "serde")]
fn deserialize_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum AnyDuration {
        #[serde(deserialize_with = "humantime_serde::deserialize")]
        Human(Option<Duration>),
        Duration(Option<Duration>),
    }

    <AnyDuration as serde::Deserialize>::deserialize(deserializer).map(|d| match d {
        AnyDuration::Human(duration) | AnyDuration::Duration(duration) => duration,
    })
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::{Config, DEFAULT_CHANGESET_WINDOW, EXTENSION};
    use crate::PruneConfig;
    use alloy_primitives::Address;
    use smartnet_prune_types::{
        PruneMode, PruneModes, ReceiptsLogPruneConfig, MINIMUM_UNWIND_SAFE_DISTANCE,
    };
    use std::{collections::BTreeMap, path::Path};

    fn with_tempdir(filename: &str, proc: fn(&std::path::Path)) {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join(filename).with_extension(EXTENSION);

        proc(&config_path);

        temp_dir.close().unwrap()
    }

    /// Run a test function with a temporary config path as fixture.
    fn with_config_path(test_fn: fn(&Path)) {
        // Create a temporary directory for the config file
        let config_dir = tempfile::tempdir().expect("creating test fixture failed");
        // Create the config file path
        let config_path =
            config_dir.path().join("example-app").join("example-config").with_extension("toml");
        // Run the test function with the config path
        test_fn(&config_path);
        config_dir.close().expect("removing test fixture failed");
    }

    #[test]
    fn test_load_path_reads_existing_config() {
        with_config_path(|path| {
            let config = Config::default();

            // Create the parent directory if it doesn't exist
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("Failed to create directories");
            }

            // Write the config to the file
            std::fs::write(path, toml::to_string(&config).unwrap())
                .expect("Failed to write config");

            // Load the config from the file and compare it
            let loaded = Config::from_path(path).expect("load_path failed");
            assert_eq!(config, loaded);
        })
    }

    #[test]
    fn test_store_config() {
        with_tempdir("config-store-test", |config_path| {
            let config = Config::default();
            std::fs::write(
                config_path,
                toml::to_string(&config).expect("Failed to serialize config"),
            )
            .expect("Failed to write config file");
        })
    }

    #[test]
    fn test_store_config_method() {
        with_tempdir("config-store-test-method", |config_path| {
            let config = Config::default();
            config.save(config_path).expect("Failed to store config");
        })
    }

    #[test]
    fn test_load_config() {
        with_tempdir("config-load-test", |config_path| {
            let config = Config::default();

            // Write the config to a file
            std::fs::write(
                config_path,
                toml::to_string(&config).expect("Failed to serialize config"),
            )
            .expect("Failed to write config file");

            // Load the config from the file
            let loaded_config = Config::from_path(config_path).unwrap();

            // Compare the loaded config with the original config
            assert_eq!(config, loaded_config);
        })
    }

    #[test]
    fn test_prune_config_merge() {
        let mut config1 = PruneConfig {
            block_interval: 5,
            minimum_pruning_distance: MINIMUM_UNWIND_SAFE_DISTANCE,
            changeset_window: DEFAULT_CHANGESET_WINDOW,
            segments: PruneModes {
                sender_recovery: Some(PruneMode::Full),
                transaction_lookup: None,
                receipts: Some(PruneMode::Distance(1000)),
                account_history: None,
                storage_history: Some(PruneMode::Before(5000)),
                bodies_history: None,
                receipts_log_filter: ReceiptsLogPruneConfig(BTreeMap::from([(
                    Address::random(),
                    PruneMode::Full,
                )])),
            },
        };

        let config2 = PruneConfig {
            block_interval: 10,
            minimum_pruning_distance: MINIMUM_UNWIND_SAFE_DISTANCE,
            changeset_window: DEFAULT_CHANGESET_WINDOW,
            segments: PruneModes {
                sender_recovery: Some(PruneMode::Distance(500)),
                transaction_lookup: Some(PruneMode::Full),
                receipts: Some(PruneMode::Full),
                account_history: Some(PruneMode::Distance(2000)),
                storage_history: Some(PruneMode::Distance(3000)),
                bodies_history: None,
                receipts_log_filter: ReceiptsLogPruneConfig(BTreeMap::from([
                    (Address::random(), PruneMode::Distance(1000)),
                    (Address::random(), PruneMode::Before(2000)),
                ])),
            },
        };

        let original_filter = config1.segments.receipts_log_filter.clone();
        config1.merge(config2);

        // Check that the configuration has been merged. Any configuration present in config1
        // should not be overwritten by config2
        assert_eq!(config1.block_interval, 10);
        assert_eq!(config1.segments.sender_recovery, Some(PruneMode::Full));
        assert_eq!(config1.segments.transaction_lookup, Some(PruneMode::Full));
        assert_eq!(config1.segments.receipts, Some(PruneMode::Distance(1000)));
        assert_eq!(config1.segments.account_history, Some(PruneMode::Distance(2000)));
        assert_eq!(config1.segments.storage_history, Some(PruneMode::Before(5000)));
        assert_eq!(config1.segments.receipts_log_filter, original_filter);
    }
}
