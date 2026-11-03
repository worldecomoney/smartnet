use smartnet_metrics::{
    metrics::Histogram,
    Metrics,
};

/// Metrics for the `PersistenceService`
#[derive(Metrics)]
#[metrics(scope = "consensus.engine.persistence")]
pub(crate) struct PersistenceMetrics {
    /// How long it took for blocks to be saved
    pub(crate) save_blocks_duration_seconds: Histogram,
    /// How many blocks we persist at once.
    pub(crate) save_blocks_batch_size: Histogram,
    /// How long it took for blocks to be pruned
    pub(crate) prune_before_duration_seconds: Histogram,
}
