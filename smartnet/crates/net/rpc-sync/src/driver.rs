//! Sync orchestration: height check → ancestor match → batched fetch → verify → import.
//!
//! The driver owns no validation beyond hash-chain linkage ([`crate::verify`]); consensus
//! and execution happen inside the node when the [`BlockSink`] imports blocks through its
//! native pipeline. This keeps transport and trust separated per the `SmartNet` design.

use crate::{
    ancestor::find_common_ancestor,
    fetch::{BlockSource, RpcBlock},
    verify::{batch_tip, verify_batch, verify_block0_shape},
};
use alloy_primitives::B256;
use smartnet_timing::fmt_wall;
use std::time::{Duration, Instant};
use tracing::info;

/// Import callback into the node's native block pipeline.
pub trait BlockSink: Send + Sync {
    /// Import a verified, in-order batch. On error the sync run aborts (caller decides
    /// whether to retry/re-peer); blocks already imported stay imported.
    fn import_blocks(
        &self,
        blocks: Vec<RpcBlock>,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// How the local chain is read for the ancestor search.
pub trait LocalChainView: Send + Sync {
    /// Local tip height.
    fn tip_number(&self) -> impl std::future::Future<Output = u64> + Send;
    /// Local header hash at `number` (`None` = not available).
    fn hash_at(&self, number: u64) -> impl std::future::Future<Output = Option<B256>> + Send;
}

/// Driver configuration.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// Blocks fetched per batch round.
    pub batch: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self { batch: crate::DEFAULT_FETCH_BATCH }
    }
}

/// One sync run outcome.
#[derive(Debug)]
pub enum SyncOutcome {
    /// Caught up to the peer's height at run start.
    CaughtUp {
        /// Local tip height before the run started.
        from: u64,
        /// Peer height the run stopped at.
        to: u64,
    },
    /// Local chain is already at or ahead of the peer; nothing to do.
    AlreadySynced {
        /// Local tip height.
        local: u64,
        /// Peer's advertised height.
        remote: u64,
    },
    /// Peer is on a different chain (genesis mismatch).
    WrongChain,
    #[allow(dead_code)]
    /// Peer lacks blocks we need (pruned range).
    PeerMissingBlocks {
        /// First block of the range the peer could not serve.
        from: u64,
    },
}

/// Run one full sync pass against `source`.
pub async fn run_sync<S, K, L>(
    source: &S,
    sink: &K,
    local: &L,
    config: &SyncConfig,
) -> Result<SyncOutcome, String>
where
    S: BlockSource,
    K: BlockSink,
    L: LocalChainView,
{
    if config.batch == 0 {
        // `SYNC_BATCH = 0` means "do not sync": the caller must not start a pass at all.
        // Without this guard the fetch range `[anchor+1, anchor]` is empty, every pass
        // reports `PeerMissingBlocks`, and the service retries forever without progress.
        return Err(
            "sync batch is 0 — nothing to fetch (set SYNC_BATCH >= 1, or 0 to switch sync off)"
                .to_string(),
        );
    }

    let pass_start = Instant::now();
    let height_start = Instant::now();
    let remote_height = source.block_number().await.map_err(|e| e.to_string())?;
    let local_height = local.tip_number().await;
    let height_wall = height_start.elapsed();

    if local_height >= remote_height {
        // Even at equal heights hashes could differ, but that is the live node's
        // fork-choice concern, not the join path's; do nothing unless behind.
        return Ok(SyncOutcome::AlreadySynced { local: local_height, remote: remote_height });
    }

    let ancestor_start = Instant::now();
    let ancestor = find_common_ancestor(
        local_height,
        |n| local.hash_at(n),
        |n| async move { source.block_hash(n).await.map_err(|e| e.to_string()) },
    )
    .await?;
    let ancestor_wall = ancestor_start.elapsed();

    let Some(ancestor) = ancestor else {
        return Ok(SyncOutcome::WrongChain);
    };

    // Block 0 fixes the header shape for the whole historical range (Istanbul: no
    // `baseFeePerGas`; zero nonce/mixHash). Use it as the template every fetched
    // batch must match. If the peer can't serve block 0, shape checks are skipped — the
    // parent-hash linkage still rejects any differently-encoded header (a London-shaped
    // header hashes differently). See `doc/istanbul-london-header-rlp.md`.
    let template_start = Instant::now();
    let template = match source.fetch_blocks(0, 0).await {
        Ok(mut blocks) => blocks.pop(),
        Err(_) => None,
    };
    let template_wall = template_start.elapsed();

    // Setup is pure peer round trips (height + ancestor + block-0 template); it happens
    // once per pass and is reported separately from the fetch/import split below.
    info!(
        target: "smartnet::rpc_sync",
        event = "SYNC_PASS_SETUP",
        ancestor = ancestor.number,
        height_wall = %fmt_wall(height_wall),
        ancestor_wall = %fmt_wall(ancestor_wall),
        template_wall = %fmt_wall(template_wall),
        setup_wall = %fmt_wall(height_wall + ancestor_wall + template_wall),
        "sync pass setup (peer round trips: height + ancestor + block-0 template)"
    );

    let mut anchor_number = ancestor.number;
    let mut anchor_hash = ancestor.hash;
    let start_from = anchor_number;
    let mut total_fetch = Duration::ZERO;
    let mut total_verify = Duration::ZERO;
    let mut total_import = Duration::ZERO;
    while anchor_number < remote_height {
        let batch_start = Instant::now();
        let end = anchor_number + config.batch;
        let end = end.min(remote_height);
        let fetch_start = Instant::now();
        let blocks =
            source.fetch_blocks(anchor_number + 1, end).await.map_err(|e| e.to_string())?;
        let fetch_wall = fetch_start.elapsed();
        if blocks.is_empty() {
            return Ok(SyncOutcome::PeerMissingBlocks { from: anchor_number + 1 });
        }
        let verify_start = Instant::now();
        if let Some(template) = &template {
            verify_block0_shape(template, &blocks).map_err(|e| e.to_string())?;
        }
        verify_batch(anchor_number, anchor_hash, &blocks).map_err(|e| e.to_string())?;
        let verify_wall = verify_start.elapsed();
        let import_start = Instant::now();
        sink.import_blocks(blocks.clone()).await?;
        let import_wall = import_start.elapsed();
        total_fetch += fetch_wall;
        total_verify += verify_wall;
        total_import += import_wall;
        // The split the bottleneck question turns on: `fetch_wall` is peer network time,
        // `import_wall` is in-process engine confirmation (validation + execution), and
        // everything the engine does after VALID shows up under `engine::tree` targets.
        info!(
            target: "smartnet::rpc_sync",
            event = "SYNC_BATCH_TIMINGS",
            from = anchor_number + 1,
            to = end,
            blocks = blocks.len(),
            fetch_wall = %fmt_wall(fetch_wall),
            verify_wall = %fmt_wall(verify_wall),
            import_wall = %fmt_wall(import_wall),
            batch_wall = %fmt_wall(batch_start.elapsed()),
            "sync batch split: peer fetch vs verify vs engine import"
        );
        let Some((n, h)) = batch_tip(&blocks) else {
            return Ok(SyncOutcome::PeerMissingBlocks { from: anchor_number + 1 });
        };
        anchor_number = n;
        anchor_hash = h;
    }

    info!(
        target: "smartnet::rpc_sync",
        event = "SYNC_PASS_TIMINGS",
        from = start_from,
        to = remote_height,
        fetch_wall = %fmt_wall(total_fetch),
        verify_wall = %fmt_wall(total_verify),
        import_wall = %fmt_wall(total_import),
        pass_wall = %fmt_wall(pass_start.elapsed()),
        "sync pass split: fetch = peer RPC time, import = engine confirmation time"
    );

    Ok(SyncOutcome::CaughtUp { from: start_from, to: remote_height })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FetchError;
    use alloy_consensus::Header as CHeader;
    use alloy_rpc_types_eth::Block;
    use std::{collections::HashMap, sync::Mutex};

    // ---- mock source: synthetic chain 0..=100 ----

    struct MockSource {
        chain: HashMap<u64, RpcBlock>,
    }

    impl MockSource {
        fn build(height: u64) -> (Self, HashMap<u64, B256>) {
            let mut chain = HashMap::new();
            let mut hashes = HashMap::new();
            let mut parent = B256::ZERO;
            for n in 0..=height {
                let header = CHeader { number: n, parent_hash: parent, ..Default::default() };
                let hash = header.hash_slow();
                chain.insert(
                    n,
                    Block {
                        header: alloy_rpc_types_eth::Header::from_sealed(
                            alloy_consensus::Sealed::new_unchecked(header, hash),
                        ),
                        ..Default::default()
                    },
                );
                hashes.insert(n, hash);
                parent = hash;
            }
            (Self { chain }, hashes)
        }
    }

    impl BlockSource for MockSource {
        async fn block_number(&self) -> Result<u64, FetchError> {
            Ok(self.chain.keys().copied().max().unwrap())
        }
        async fn block_hash(&self, number: u64) -> Result<Option<B256>, FetchError> {
            Ok(self.chain.get(&number).map(|b| b.header.hash))
        }
        async fn fetch_blocks(
            &self,
            start: u64,
            end_inclusive: u64,
        ) -> Result<Vec<RpcBlock>, FetchError> {
            let mut out = Vec::new();
            for n in start..=end_inclusive {
                out.push(self.chain.get(&n).cloned().ok_or(FetchError::Missing(n))?);
            }
            Ok(out)
        }
    }

    struct MockSink {
        imported: Mutex<Vec<u64>>,
    }

    impl BlockSink for MockSink {
        async fn import_blocks(&self, blocks: Vec<RpcBlock>) -> Result<(), String> {
            self.imported.lock().unwrap().extend(blocks.iter().map(|b| b.header.number));
            Ok(())
        }
    }

    struct MockLocal {
        hashes: HashMap<u64, B256>,
        tip: u64,
    }

    impl LocalChainView for MockLocal {
        async fn tip_number(&self) -> u64 {
            self.tip
        }
        async fn hash_at(&self, number: u64) -> Option<B256> {
            self.hashes.get(&number).copied()
        }
    }

    #[tokio::test]
    async fn fresh_join_syncs_all_blocks() {
        let (source, hashes) = MockSource::build(100);
        let local = MockLocal { hashes: HashMap::from([(0u64, hashes[&0])]), tip: 0 };
        let sink = MockSink { imported: Mutex::new(vec![]) };
        let out = run_sync(&source, &sink, &local, &SyncConfig { batch: 30 }).await.unwrap();
        assert!(matches!(out, SyncOutcome::CaughtUp { from: 0, to: 100 }));
        let imported = sink.imported.lock().unwrap();
        assert_eq!(imported.len(), 100);
        assert_eq!(imported.first(), Some(&1));
        assert_eq!(imported.last(), Some(&100));
    }

    #[tokio::test]
    async fn partial_join_resumes_from_ancestor() {
        let (source, hashes) = MockSource::build(100);
        let local = MockLocal { hashes: hashes.clone(), tip: 40 };
        let sink = MockSink { imported: Mutex::new(vec![]) };
        let out = run_sync(&source, &sink, &local, &SyncConfig { batch: 128 }).await.unwrap();
        assert!(matches!(out, SyncOutcome::CaughtUp { from: 40, to: 100 }));
        assert_eq!(sink.imported.lock().unwrap().first(), Some(&41));
    }

    #[tokio::test]
    async fn already_synced_does_nothing() {
        let (source, hashes) = MockSource::build(100);
        let local = MockLocal { hashes, tip: 100 };
        let sink = MockSink { imported: Mutex::new(vec![]) };
        let out = run_sync(&source, &sink, &local, &SyncConfig::default()).await.unwrap();
        assert!(matches!(out, SyncOutcome::AlreadySynced { local: 100, remote: 100 }));
        assert!(sink.imported.lock().unwrap().is_empty());
    }

    /// `SYNC_BATCH = 0` must not silently turn into an empty fetch range: with `batch = 0`
    /// the loop would request `[anchor+1, anchor]`, always get nothing back and report
    /// `PeerMissingBlocks`, i.e. retry forever with zero progress. The driver refuses.
    #[tokio::test]
    async fn zero_batch_is_an_error_not_an_empty_fetch_loop() {
        let (source, hashes) = MockSource::build(100);
        let local = MockLocal { hashes: HashMap::from([(0u64, hashes[&0])]), tip: 0 };
        let sink = MockSink { imported: Mutex::new(vec![]) };
        let err = run_sync(&source, &sink, &local, &SyncConfig { batch: 0 }).await.unwrap_err();
        assert!(err.contains("sync batch is 0"), "got: {err}");
        assert!(sink.imported.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn wrong_genesis_rejected() {
        let (source, _) = MockSource::build(100);
        // A different chain entirely: every local hash differs from the peer's.
        let local_hashes: HashMap<u64, B256> = (0..=10)
            .map(|n| {
                let mut bytes = [0xEEu8; 32];
                bytes[1] = (n + 1) as u8;
                (n, B256::from(bytes))
            })
            .collect();
        let local = MockLocal { hashes: local_hashes, tip: 10 };
        let sink = MockSink { imported: Mutex::new(vec![]) };
        let out = run_sync(&source, &sink, &local, &SyncConfig::default()).await.unwrap();
        assert!(matches!(out, SyncOutcome::WrongChain));
        assert!(sink.imported.lock().unwrap().is_empty());
    }
}
