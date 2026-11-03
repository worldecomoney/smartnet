use metrics::{Gauge, Histogram};
use smartnet_metrics::Metrics;
#[cfg(any(test, feature = "test-utils"))]
use smartnet_primitives_traits::FastInstant as Instant;
use std::time::Duration;

// Test-fixture recorder: constructed only by the cfg(test / feature = "test-utils")
// `append_blocks_with_state` fixture — the type and its support are gated to that same cfg.
#[cfg(any(test, feature = "test-utils"))]
#[derive(Debug)]
pub(crate) struct DurationsRecorder<'a> {
    start: Instant,
    current_metrics: &'a DatabaseProviderMetrics,
    pub(crate) actions: Vec<(Action, Duration)>,
    latest: Option<Duration>,
}

#[cfg(any(test, feature = "test-utils"))]
impl<'a> DurationsRecorder<'a> {
    /// Creates a new durations recorder with the given metrics instance.
    pub(crate) fn new(metrics: &'a DatabaseProviderMetrics) -> Self {
        Self { start: Instant::now(), actions: Vec::new(), latest: None, current_metrics: metrics }
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl<'a> DurationsRecorder<'a> {
    /// Records the duration since last record, saves it for future logging and instantly reports as
    /// a metric with `action` label.
    pub(crate) fn record_relative(&mut self, action: Action) {
        let elapsed = self.start.elapsed();
        let duration = elapsed - self.latest.unwrap_or_default();

        self.actions.push((action, duration));
        self.current_metrics.record_duration(action, duration);
        self.latest = Some(elapsed);
    }
}

#[derive(Debug, Copy, Clone)]
#[allow(dead_code)]
pub(crate) enum Action {
    InsertBlock,
    InsertState,
    InsertHashes,
    InsertHistoryIndices,
    UpdatePipelineStages,
    InsertHeaders,
    InsertCanonicalHeaders,
    InsertTransactions,
    InsertReceipts,
    InsertBlockBodyIndices,
    InsertTransactionHashNumbers,
}

/// Database provider metrics
#[derive(Metrics)]
#[metrics(scope = "storage.providers.database")]
pub(crate) struct DatabaseProviderMetrics {
    /// Duration of insert block
    insert_block: Histogram,
    /// Duration of insert state
    insert_state: Histogram,
    /// Duration of insert hashes
    insert_hashes: Histogram,
    /// Duration of insert history indices
    insert_history_indices: Histogram,
    /// Duration of update pipeline stages
    update_pipeline_stages: Histogram,
    /// Duration of insert header payload
    insert_headers: Histogram,
    /// Duration of insert canonical header hashes
    insert_canonical_headers: Histogram,
    /// Duration of insert transaction payload
    insert_transactions: Histogram,
    /// Duration of insert receipt payload
    insert_receipts: Histogram,
    /// Duration of insert block body indices
    insert_block_body_indices: Histogram,
    /// Duration of insert transaction hash numbers
    insert_transaction_hash_numbers: Histogram,
    /// Duration of `save_blocks`
    save_blocks_total: Histogram,
    /// Duration of database work in `save_blocks`
    save_blocks_db: Histogram,
    /// Duration of static file work in `save_blocks`
    save_blocks_sf: Histogram,
    /// Duration of `aux DB` work in `save_blocks`
    save_blocks_aux_db: Histogram,
    /// Duration of `insert_block` in `save_blocks`
    save_blocks_insert_block: Histogram,
    /// Duration of `write_state` in `save_blocks`
    save_blocks_write_state: Histogram,
    /// Duration of `write_hashed_state` in `save_blocks`
    save_blocks_write_hashed_state: Histogram,
    /// Duration of `write_trie_updates` in `save_blocks`
    save_blocks_write_trie_updates: Histogram,
    /// Duration of `update_history_indices` in `save_blocks`
    save_blocks_update_history_indices: Histogram,
    /// Duration of `update_pipeline_stages` in `save_blocks`
    save_blocks_update_pipeline_stages: Histogram,
    /// Number of blocks per `save_blocks` call
    save_blocks_batch_size: Histogram,
    /// Duration of database commit in `save_blocks`
    save_blocks_commit_db: Histogram,
    /// Duration of `aux DB` commit in `save_blocks`
    save_blocks_commit_aux_db: Histogram,
    /// Last duration of `save_blocks`
    save_blocks_total_last: Gauge,
    /// Last duration of database work in `save_blocks`
    save_blocks_db_last: Gauge,
    /// Last duration of static file work in `save_blocks`
    save_blocks_sf_last: Gauge,
    /// Last duration of `aux DB` work in `save_blocks`
    save_blocks_aux_db_last: Gauge,
    /// Last duration of `insert_block` in `save_blocks`
    save_blocks_insert_block_last: Gauge,
    /// Last duration of `write_state` in `save_blocks`
    save_blocks_write_state_last: Gauge,
    /// Last duration of `write_hashed_state` in `save_blocks`
    save_blocks_write_hashed_state_last: Gauge,
    /// Last duration of `write_trie_updates` in `save_blocks`
    save_blocks_write_trie_updates_last: Gauge,
    /// Last duration of `update_history_indices` in `save_blocks`
    save_blocks_update_history_indices_last: Gauge,
    /// Last duration of `update_pipeline_stages` in `save_blocks`
    save_blocks_update_pipeline_stages_last: Gauge,
    /// Last number of blocks per `save_blocks` call
    save_blocks_batch_size_last: Gauge,
    /// Last duration of database commit in `save_blocks`
    save_blocks_commit_db_last: Gauge,
    /// Last duration of `aux DB` commit in `save_blocks`
    save_blocks_commit_aux_db_last: Gauge,
}

/// Timings collected during a `save_blocks` call.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub(crate) struct SaveBlocksTimings {
    pub total: Duration,
    pub db: Duration,
    pub sf: Duration,
    pub aux_db: Duration,
    pub insert_block: Duration,
    pub write_state: Duration,
    pub write_hashed_state: Duration,
    pub write_trie_updates: Duration,
    pub update_history_indices: Duration,
    pub update_pipeline_stages: Duration,
    pub batch_size: u64,
}

/// Timings collected during a `commit` call.
#[derive(Debug, Default)]
pub(crate) struct CommitTimings {
    pub db: Duration,
    pub aux_db: Duration,
}

impl DatabaseProviderMetrics {
    /// Records the trie-node write duration of `save_block_state_and_index`.
    ///
    /// Isolated from the lumped `state_index_commit` segment so the trie write can be
    /// measured against the fsync (audit TRIE-RES §7 gap 1).
    pub(crate) fn record_write_trie_updates(&self, duration: Duration) {
        self.save_blocks_write_trie_updates.record(duration);
        self.save_blocks_write_trie_updates_last.set(duration.as_secs_f64());
    }

    /// Records the duration for the given action.
    ///
    /// Test-fixture support for [`DurationsRecorder`] (gated to its cfg).
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn record_duration(&self, action: Action, duration: Duration) {
        match action {
            Action::InsertBlock => self.insert_block.record(duration),
            Action::InsertState => self.insert_state.record(duration),
            Action::InsertHashes => self.insert_hashes.record(duration),
            Action::InsertHistoryIndices => self.insert_history_indices.record(duration),
            Action::UpdatePipelineStages => self.update_pipeline_stages.record(duration),
            Action::InsertHeaders => self.insert_headers.record(duration),
            Action::InsertCanonicalHeaders => self.insert_canonical_headers.record(duration),
            Action::InsertTransactions => self.insert_transactions.record(duration),
            Action::InsertReceipts => self.insert_receipts.record(duration),
            Action::InsertBlockBodyIndices => self.insert_block_body_indices.record(duration),
            Action::InsertTransactionHashNumbers => {
                self.insert_transaction_hash_numbers.record(duration)
            }
        }
    }

    /// Records all `save_blocks` timings.
    #[allow(dead_code)]
    pub(crate) fn record_save_blocks(&self, timings: &SaveBlocksTimings) {
        self.save_blocks_total.record(timings.total);
        self.save_blocks_db.record(timings.db);
        self.save_blocks_sf.record(timings.sf);
        self.save_blocks_aux_db.record(timings.aux_db);
        self.save_blocks_insert_block.record(timings.insert_block);
        self.save_blocks_write_state.record(timings.write_state);
        self.save_blocks_write_hashed_state.record(timings.write_hashed_state);
        self.save_blocks_write_trie_updates.record(timings.write_trie_updates);
        self.save_blocks_update_history_indices.record(timings.update_history_indices);
        self.save_blocks_update_pipeline_stages.record(timings.update_pipeline_stages);
        self.save_blocks_batch_size.record(timings.batch_size as f64);

        self.save_blocks_total_last.set(timings.total.as_secs_f64());
        self.save_blocks_db_last.set(timings.db.as_secs_f64());
        self.save_blocks_sf_last.set(timings.sf.as_secs_f64());
        self.save_blocks_aux_db_last.set(timings.aux_db.as_secs_f64());
        self.save_blocks_insert_block_last.set(timings.insert_block.as_secs_f64());
        self.save_blocks_write_state_last.set(timings.write_state.as_secs_f64());
        self.save_blocks_write_hashed_state_last.set(timings.write_hashed_state.as_secs_f64());
        self.save_blocks_write_trie_updates_last.set(timings.write_trie_updates.as_secs_f64());
        self.save_blocks_update_history_indices_last
            .set(timings.update_history_indices.as_secs_f64());
        self.save_blocks_update_pipeline_stages_last
            .set(timings.update_pipeline_stages.as_secs_f64());
        self.save_blocks_batch_size_last.set(timings.batch_size as f64);
    }

    /// Records all commit timings.
    pub(crate) fn record_commit(&self, timings: &CommitTimings) {
        self.save_blocks_commit_db.record(timings.db);
        self.save_blocks_commit_aux_db.record(timings.aux_db);

        self.save_blocks_commit_db_last.set(timings.db.as_secs_f64());
        self.save_blocks_commit_aux_db_last.set(timings.aux_db.as_secs_f64());
    }
}
