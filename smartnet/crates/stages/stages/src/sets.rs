//! Built-in [`StageSet`]s.
//!
//! The easiest set to use is [`DefaultStages`], which provides all stages required to run an
//! instance of the node.
//!
//! It is also possible to run parts of the node standalone given the required data is present in
//! the environment, such as [`ExecutionStages`] or [`HashingStages`].
//!
//!
//! # Examples
//!
//! ```no_run
//! # use smartnet_stages::Pipeline;
//! # use smartnet_stages::sets::{OfflineStages};
//! # use smartnet_chainspec::MAINNET;
//! # use smartnet_prune_types::PruneModes;
//! # use smartnet_evm_ethereum::EthEvmConfig;
//! # use smartnet_evm::ConfigureEvm;
//! # use smartnet_provider::test_utils::{create_test_provider_factory, MockNodeTypesWithDB};
//! # use smartnet_config::config::StageConfig;
//! # use smartnet_ethereum_primitives::EthPrimitives;
//! # use std::sync::Arc;
//! # use smartnet_consensus::FullConsensus;
//!
//! # fn create(exec: impl ConfigureEvm<Primitives = EthPrimitives> + 'static, consensus: impl FullConsensus<EthPrimitives> + 'static) {
//!
//! let provider_factory = create_test_provider_factory();
//! // Build a pipeline with all offline stages.
//! let pipeline = Pipeline::<MockNodeTypesWithDB>::builder()
//!     .add_stages(OfflineStages::new(exec, Arc::new(consensus), StageConfig::default(), PruneModes::default()))
//!     .build(provider_factory);
//!
//! # }
//! ```
use crate::{
    stages::{
        AccountHashingStage, ExecutionStage, FinishStage, IndexAccountHistoryStage,
        IndexStorageHistoryStage, MerkleStage, PruneSenderRecoveryStage, PruneStage,
        SenderRecoveryStage, StorageHashingStage, TransactionLookupStage,
    },
    StageSet, StageSetBuilder,
};
use smartnet_evm::ConfigureEvm;
use smartnet_prune_types::{PruneMode, PruneModes};
use smartnet_stages_api::Stage;
use smartnet_config::config::StageConfig;
use smartnet_consensus::FullConsensus;
use std::sync::Arc;

/// The default stage set: the offline stages plus the finish stage.
///
/// `SmartNet` has no network download path — blocks reach storage through the engine
/// (`new_payload` / import / mesh / rpc-sync). The former online download stages
/// (`HeaderStage` and the p2p downloader generics) were removed together with the
/// p2p stack; this set is what remains for pipeline compatibility.
#[derive(Debug)]
pub struct DefaultStages<E: ConfigureEvm> {
    /// Executor factory needs for execution stage
    evm_config: E,
    /// Consensus instance
    consensus: Arc<dyn FullConsensus<E::Primitives>>,
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
    /// Prune configuration for every segment that can be pruned
    prune_modes: PruneModes,
}

impl<E: ConfigureEvm> DefaultStages<E> {
    /// Create a new set of default stages with default values.
    pub const fn new(
        evm_config: E,
        consensus: Arc<dyn FullConsensus<E::Primitives>>,
        stages_config: StageConfig,
        prune_modes: PruneModes,
    ) -> Self {
        Self { evm_config, consensus, stages_config, prune_modes }
    }

    /// Appends the default offline stages and default finish stage to the given builder.
    pub fn add_offline_stages<Provider>(
        default_offline: StageSetBuilder<Provider>,
        evm_config: E,
        consensus: Arc<dyn FullConsensus<E::Primitives>>,
        stages_config: StageConfig,
        prune_modes: PruneModes,
    ) -> StageSetBuilder<Provider>
    where
        OfflineStages<E>: StageSet<Provider>,
    {
        StageSetBuilder::default()
            .add_set(default_offline)
            .add_set(OfflineStages::new(evm_config, consensus, stages_config, prune_modes))
            .add_stage(FinishStage)
    }
}

impl<E, Provider> StageSet<Provider> for DefaultStages<E>
where
    E: ConfigureEvm,
    OfflineStages<E>: StageSet<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        Self::add_offline_stages(
            StageSetBuilder::default(),
            self.evm_config,
            self.consensus,
            self.stages_config,
            self.prune_modes,
        )
    }
}

/// A set containing all stages that do not require network access.
///
/// A combination of (in order)
///
/// - [`ExecutionStages`]
/// - [`PruneSenderRecoveryStage`]
/// - [`HashingStages`]
/// - [`HistoryIndexingStages`]
/// - [`PruneStage`]
#[derive(Debug)]
#[non_exhaustive]
pub struct OfflineStages<E: ConfigureEvm> {
    /// Executor factory needs for execution stage
    evm_config: E,
    /// Consensus instance for validating blocks.
    consensus: Arc<dyn FullConsensus<E::Primitives>>,
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
    /// Prune configuration for every segment that can be pruned
    prune_modes: PruneModes,
}

impl<E: ConfigureEvm> OfflineStages<E> {
    /// Create a new set of offline stages with default values.
    pub const fn new(
        evm_config: E,
        consensus: Arc<dyn FullConsensus<E::Primitives>>,
        stages_config: StageConfig,
        prune_modes: PruneModes,
    ) -> Self {
        Self { evm_config, consensus, stages_config, prune_modes }
    }
}

impl<E, Provider> StageSet<Provider> for OfflineStages<E>
where
    E: ConfigureEvm,
    ExecutionStages<E>: StageSet<Provider>,
    PruneSenderRecoveryStage: Stage<Provider>,
    HashingStages: StageSet<Provider>,
    HistoryIndexingStages: StageSet<Provider>,
    PruneStage: Stage<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        ExecutionStages::new(
            self.evm_config,
            self.consensus,
            self.stages_config.clone(),
            self.prune_modes.sender_recovery,
        )
        .builder()
        // If sender recovery prune mode is set, add the prune sender recovery stage.
        .add_stage_opt(self.prune_modes.sender_recovery.map(|prune_mode| {
            PruneSenderRecoveryStage::new(prune_mode, self.stages_config.prune.commit_threshold)
        }))
        .add_set(HashingStages { stages_config: self.stages_config.clone() })
        .add_set(HistoryIndexingStages {
            stages_config: self.stages_config.clone(),
            prune_modes: self.prune_modes.clone(),
        })
        // Prune stage should be added after all hashing stages, because otherwise it will
        // delete
        .add_stage(PruneStage::new(
            self.prune_modes.clone(),
            self.stages_config.prune.commit_threshold,
        ))
    }
}

/// A set containing all stages that are required to execute pre-existing block data.
#[derive(Debug)]
#[non_exhaustive]
pub struct ExecutionStages<E: ConfigureEvm> {
    /// Executor factory that will create executors.
    evm_config: E,
    /// Consensus instance for validating blocks.
    consensus: Arc<dyn FullConsensus<E::Primitives>>,
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
    /// Prune mode for sender recovery
    sender_recovery_prune_mode: Option<PruneMode>,
}

impl<E: ConfigureEvm> ExecutionStages<E> {
    /// Create a new set of execution stages with default values.
    pub const fn new(
        executor_provider: E,
        consensus: Arc<dyn FullConsensus<E::Primitives>>,
        stages_config: StageConfig,
        sender_recovery_prune_mode: Option<PruneMode>,
    ) -> Self {
        Self { evm_config: executor_provider, consensus, stages_config, sender_recovery_prune_mode }
    }
}

impl<E, Provider> StageSet<Provider> for ExecutionStages<E>
where
    E: ConfigureEvm + 'static,
    SenderRecoveryStage: Stage<Provider>,
    ExecutionStage<E>: Stage<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        StageSetBuilder::default()
            .add_stage(SenderRecoveryStage::new(
                self.stages_config.sender_recovery,
                self.sender_recovery_prune_mode,
            ))
            .add_stage(ExecutionStage::from_config(
                self.evm_config,
                self.consensus,
                self.stages_config.execution,
                self.stages_config.execution_external_clean_threshold(),
            ))
    }
}

/// A set containing all stages that hash account state.
///
/// This includes:
/// - [`MerkleStage`] (unwind)
/// - [`AccountHashingStage`]
/// - [`StorageHashingStage`]
/// - [`MerkleStage`] (execute)
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct HashingStages {
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
}

impl<Provider> StageSet<Provider> for HashingStages
where
    MerkleStage: Stage<Provider>,
    AccountHashingStage: Stage<Provider>,
    StorageHashingStage: Stage<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        StageSetBuilder::default()
            .add_stage(MerkleStage::default_unwind())
            .add_stage(AccountHashingStage::new(
                self.stages_config.account_hashing,
                self.stages_config.etl.clone(),
            ))
            .add_stage(StorageHashingStage::new(
                self.stages_config.storage_hashing,
                self.stages_config.etl.clone(),
            ))
            .add_stage(MerkleStage::new_execution(
                self.stages_config.merkle.rebuild_threshold,
                self.stages_config.merkle.incremental_threshold,
            ))
    }
}

/// A set containing all stages that do additional indexing for historical state.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct HistoryIndexingStages {
    /// Configuration for each stage in the pipeline
    stages_config: StageConfig,
    /// Prune configuration for every segment that can be pruned
    prune_modes: PruneModes,
}

impl<Provider> StageSet<Provider> for HistoryIndexingStages
where
    TransactionLookupStage: Stage<Provider>,
    IndexStorageHistoryStage: Stage<Provider>,
    IndexAccountHistoryStage: Stage<Provider>,
{
    fn builder(self) -> StageSetBuilder<Provider> {
        StageSetBuilder::default()
            .add_stage(TransactionLookupStage::new(
                self.stages_config.transaction_lookup,
                self.stages_config.etl.clone(),
                self.prune_modes.transaction_lookup,
            ))
            .add_stage(IndexStorageHistoryStage::new(
                self.stages_config.index_storage_history,
                self.stages_config.etl.clone(),
                self.prune_modes.storage_history,
            ))
            .add_stage(IndexAccountHistoryStage::new(
                self.stages_config.index_account_history,
                self.stages_config.etl.clone(),
                self.prune_modes.account_history,
            ))
    }
}
