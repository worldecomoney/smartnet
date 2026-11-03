//! In-node RPC catch-up service (`SmartNet` join / catch-up path) — isolated in its own
//! crate so no other node feature shares code with it.
//!
//! Implements the node side of the sync design: fetch blocks over JSON-RPC from the
//! operator-configured fixed peer allow-list (`SYNC_PEERS`) via `smartnet-net-rpc-sync`,
//! then import each block through the node's own validated engine path
//! (`ConsensusEngineHandle::new_payload` — the same import routine behind
//! `smartnet_newPayload`, called in-process with no RPC round trip). Block authenticity comes
//! from seal validation + execution inside the engine, never from the peer.
//!
//! **Isolation.** Nothing outside this crate can start the service: the node's launch path
//! makes a single gated call ([`spawn_rpc_sync_service`]), the loop runs as its own critical
//! task (`rpc-sync`) — separate from the mesh task, the JSON-RPC server, and the RLP import
//! path — and none of those depend on anything in this crate. Disabling the service therefore
//! cannot change mesh gossip, block import, or RPC behaviour.
//!
//! **Gates.** The service runs only when the operator opted in on every key (see
//! [`sync_gate`]): `RPC_SYNC = true` (master switch, **default off**) **and** non-empty
//! `SYNC_PEERS` **and** `SYNC_BATCH >= 1` **and** a non-zero `SYNC_POLL_INTERVAL`.
//! `RPC_SYNC` absent or false means the loop is never started; `SYNC_BATCH = 0` is the
//! explicit "do not sync" switch on top of that.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use alloy_primitives::B256;
use alloy_rpc_types_engine::PayloadStatusEnum;
use alloy_rpc_types_eth::BlockTransactions;
use smartnet_engine_primitives::ConsensusEngineHandle;
use smartnet_engine_primitives::ethereum::execution_data_from_block_rlp;
use smartnet_ethereum_primitives::Block as ConsensusBlock;
use smartnet_net_rpc_sync::{
    driver::{run_sync, BlockSink, LocalChainView, SyncConfig, SyncOutcome},
    fetch::{connect_http, RpcBlock},
};
use smartnet_payload_primitives::PayloadTypes;
use smartnet_storage_api::{BlockNumReader, HeaderProvider};
use smartnet_tasks::TaskExecutor;
use smartnet_timing::{fmt_wall, fmt_wall_opt};
use std::{
    fmt,
    time::{Duration, Instant},
};
use tracing::{info, warn};
use url::Url;

/// Imports fetched blocks through the node's engine (full validation + execution).
pub struct EngineBlockSink<P: PayloadTypes<ExecutionData = alloy_rpc_types_engine::ExecutionData>> {
    engine: ConsensusEngineHandle<P>,
}

impl<P> EngineBlockSink<P>
where
    P: PayloadTypes<ExecutionData = alloy_rpc_types_engine::ExecutionData>,
{
    /// Wraps an engine handle so fetched blocks are imported through the node's own
    /// validated `smartnet_newPayload` path.
    pub const fn new(engine: ConsensusEngineHandle<P>) -> Self {
        Self { engine }
    }
}

impl<P> fmt::Debug for EngineBlockSink<P>
where
    P: PayloadTypes<ExecutionData = alloy_rpc_types_engine::ExecutionData>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineBlockSink").finish_non_exhaustive()
    }
}

impl<P> BlockSink for EngineBlockSink<P>
where
    P: PayloadTypes<ExecutionData = alloy_rpc_types_engine::ExecutionData>,
{
    async fn import_blocks(&self, blocks: Vec<RpcBlock>) -> Result<(), String> {
        let batch_start = Instant::now();
        let mut confirmed = 0usize;
        for block in blocks {
            let number = block.header.number;
            let parent_hash = block.header.parent_hash;
            let block_start = Instant::now();
            let consensus_block = into_engine_block(block)?;
            // Same conversion the `smartnet_newPayload` BlockRlp path uses (honest header
            // fields; hash recomputed from the header inside the engine).
            let payload = execution_data_from_block_rlp(consensus_block);
            let (status, timings) = self
                .engine
                .new_payload(payload, true, true)
                .await
                .map_err(|e| format!("block {number} engine submit failed: {e}"))?;
            // Per-block split: `submit_wall` is the full in-process engine round trip,
            // the `*_wall` breakdown is what the engine spent it on (queue, validation,
            // persistence). The wall *after* the engine confirmed is logged by the
            // engine itself (`ENGINE_POST_CONFIRMATION`).
            info!(
                target: "smartnet::rpc_sync",
                event = "SYNC_BLOCK_CONFIRMED",
                number,
                parent_hash = %parent_hash,
                status = ?status.status,
                submit_wall = %fmt_wall(block_start.elapsed()),
                latency_wall = %fmt_wall(timings.latency),
                queue_wait_wall = %fmt_wall(timings.queue_wait),
                persistence_wait_wall = %fmt_wall(timings.persistence_wait),
                execution_cache_wait_wall = %fmt_wall_opt(timings.execution_cache_wait),
                sparse_trie_wait_wall = %fmt_wall_opt(timings.sparse_trie_wait),
                "sync block submitted to engine"
            );
            match status.status {
                PayloadStatusEnum::Valid => confirmed += 1,
                other => {
                    return Err(format!(
                        "block {number} (parent {parent_hash}) rejected by engine: {other:?} — {}",
                        other.validation_error().unwrap_or_default()
                    ));
                }
            }
        }
        info!(
            target: "smartnet::rpc_sync",
            event = "SYNC_IMPORT_BATCH",
            blocks = confirmed,
            import_wall = %fmt_wall(batch_start.elapsed()),
            "imported fetched batch through the engine"
        );
        Ok(())
    }
}

/// Reads the local canonical chain for the ancestor search.
pub struct ProviderChainView<P> {
    provider: P,
}

impl<P> ProviderChainView<P> {
    /// Wraps a provider so the sync driver can walk the local canonical chain.
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }
}

impl<P> fmt::Debug for ProviderChainView<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderChainView").finish_non_exhaustive()
    }
}

/// RPC block (full transactions) → consensus block for the engine import path.
fn into_engine_block(block: RpcBlock) -> Result<ConsensusBlock, String> {
    let transactions = match block.transactions {
        BlockTransactions::Full(txs) => txs
            .into_iter()
            .map(|tx| -> alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844> {
                tx.into_inner().into()
            })
            .collect::<Vec<_>>(),
        _ => return Err("peer returned transaction hashes instead of full transactions".into()),
    };
    let RpcBlock { header, withdrawals, .. } = block;
    Ok(alloy_consensus::Block {
        header: header.into_consensus(),
        body: alloy_consensus::BlockBody { transactions, ommers: vec![], withdrawals },
    })
}

impl<P> LocalChainView for ProviderChainView<P>
where
    P: BlockNumReader + HeaderProvider + Send + Sync,
{
    async fn tip_number(&self) -> u64 {
        self.provider.best_block_number().unwrap_or(0)
    }

    async fn hash_at(&self, number: u64) -> Option<B256> {
        self.provider.sealed_header(number).ok().flatten().map(|h| h.hash())
    }
}

/// Single source of truth for "should the catch-up service run?" — everything the operator
/// has to opt into, checked in one place so a misconfiguration can never start a loop.
///
/// `RPC_SYNC` is the master switch and is **off by default**: with it false the service
/// stays dark no matter what the other keys say. Beyond it, sync runs only with a non-empty
/// peer allow-list **and** `SYNC_BATCH >= 1` **and** a non-zero poll interval.
/// `SYNC_BATCH = 0` is the explicit "do not sync" switch (the key is still required — 0 is
/// a value, not an absence); `SYNC_PEERS = []` means "sync from nobody". Either way the
/// node keeps advancing from blocks validators submit through `engine_newPayload`.
///
/// The effective batch is **clamped to 1** regardless of the configured value: protocol
/// rule — no block batches anywhere (only tx batches are supported). This legacy catch-up
/// service is temporary; the rule completes when it is removed.
///
/// Returns `Ok((batch, poll_secs))` when the service should run, `Err(reason)` when it
/// must stay off; the caller logs `reason` once at startup.
fn sync_gate(
    rpc_sync: bool,
    peers_empty: bool,
    batch: Option<u64>,
    poll_secs: Option<u64>,
) -> Result<(u64, u64), String> {
    if !rpc_sync {
        return Err("RPC_SYNC is not true — catch-up service off (default)".to_string());
    }
    if peers_empty {
        return Err("SYNC_PEERS is empty — syncing from nobody".to_string());
    }
    let Some(batch) = batch else {
        return Err("SYNC_BATCH is not set".to_string());
    };
    if batch == 0 {
        return Err("SYNC_BATCH = 0 — sync switched off by the operator".to_string());
    }
    let Some(poll_secs) = poll_secs else {
        return Err("SYNC_POLL_INTERVAL is not set".to_string());
    };
    if poll_secs == 0 {
        return Err("SYNC_POLL_INTERVAL is 0 — would busy-loop".to_string());
    }
    // Protocol rule: one block at a time — no block batches (only tx batches are
    // supported). The configured value only opts in (>= 1); the fetch/import unit
    // is always a single block.
    Ok((1, poll_secs))
}

/// Spawn the background sync service: catch-up loop over the configured peer allow-list.
///
/// Does nothing (and logs why) unless [`sync_gate`] approves, so `RPC_SYNC = false`
/// (the default), `SYNC_BATCH = 0`, or an empty `SYNC_PEERS` leaves the node running with
/// no sync traffic at all — mesh gossip, RLP import, and the JSON-RPC server are entirely
/// unaffected either way.
///
/// Each pass tries peers in order; on error or rejection it moves to the next peer. After a
/// completed pass the service sleeps `SYNC_POLL_INTERVAL` seconds and re-checks — this keeps a
/// joined node following the chain without any p2p involvement. The loop is its own critical
/// task (`rpc-sync`), separate from every other node task.
pub fn spawn_rpc_sync_service<P, Pr>(
    rpc_sync: bool,
    peers: Vec<Url>,
    sync_batch: Option<u64>,
    sync_poll_interval: Option<u64>,
    engine: ConsensusEngineHandle<P>,
    provider: Pr,
    tasks: TaskExecutor,
) where
    P: PayloadTypes<ExecutionData = alloy_rpc_types_engine::ExecutionData> + 'static,
    Pr: BlockNumReader + HeaderProvider + Send + Sync + 'static,
{
    let (batch, poll_secs) =
        match sync_gate(rpc_sync, peers.is_empty(), sync_batch, sync_poll_interval) {
            Ok(enabled) => enabled,
            Err(reason) => {
                info!(target: "smartnet::rpc_sync", %reason, "RPC sync service not started");
                return;
            }
        };
    let sink = EngineBlockSink::new(engine);
    let view = ProviderChainView::new(provider);
    let config = SyncConfig { batch };
    let poll_interval = Duration::from_secs(poll_secs);
    tasks.spawn_critical_task("rpc-sync", async move {
        info!(target: "smartnet::rpc_sync", peers = ?peers, batch = config.batch, poll_secs = poll_interval.as_secs(), "RPC sync service started");
        loop {
            for url in &peers {
                let source = connect_http(url.clone());
                match run_sync(&source, &sink, &view, &config).await {
                    Ok(SyncOutcome::CaughtUp { from, to }) => {
                        info!(target: "smartnet::rpc_sync", peer = %url, from, to, "sync pass caught up");
                        break;
                    }
                    Ok(SyncOutcome::AlreadySynced { local, remote }) => {
                        info!(target: "smartnet::rpc_sync", peer = %url, local, remote, "already synced");
                        break;
                    }
                    Ok(SyncOutcome::WrongChain) => {
                        warn!(target: "smartnet::rpc_sync", peer = %url, "peer is on a different chain (genesis mismatch) — skipping");
                    }
                    Ok(SyncOutcome::PeerMissingBlocks { from }) => {
                        warn!(target: "smartnet::rpc_sync", peer = %url, from, "peer lacks needed blocks (pruned?) — trying next peer");
                    }
                    Err(e) => {
                        warn!(target: "smartnet::rpc_sync", peer = %url, %e, "sync pass failed — trying next peer");
                    }
                }
            }
            tokio::time::sleep(poll_interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::sync_gate;

    /// `RPC_SYNC` is the master switch and defaults to off: with it false the service
    /// never starts, whatever the other keys say.
    #[test]
    fn rpc_sync_off_disables_service_even_when_everything_else_opts_in() {
        let err = sync_gate(false, false, Some(4), Some(3)).unwrap_err();
        assert!(err.contains("RPC_SYNC"), "got: {err}");
    }

    #[test]
    fn batch_zero_disables_sync() {
        let err = sync_gate(true, false, Some(0), Some(3)).unwrap_err();
        assert!(err.contains("SYNC_BATCH = 0"), "got: {err}");
    }

    #[test]
    fn empty_peers_disables_sync() {
        let err = sync_gate(true, true, Some(4), Some(3)).unwrap_err();
        assert!(err.contains("SYNC_PEERS"), "got: {err}");
    }

    #[test]
    fn missing_values_disable_sync_instead_of_defaulting() {
        assert!(sync_gate(true, false, None, Some(3)).unwrap_err().contains("SYNC_BATCH"));
        assert!(sync_gate(true, false, Some(4), None).unwrap_err().contains("SYNC_POLL_INTERVAL"));
    }

    #[test]
    fn zero_poll_interval_disables_sync() {
        let err = sync_gate(true, false, Some(4), Some(0)).unwrap_err();
        assert!(err.contains("SYNC_POLL_INTERVAL"), "got: {err}");
    }

    #[test]
    fn sync_runs_only_when_every_key_opts_in() {
        // The configured batch only opts in (>= 1); the effective batch is always 1
        // (protocol rule: no block batches — only tx batches are supported).
        assert_eq!(sync_gate(true, false, Some(4), Some(3)).unwrap(), (1, 3));
        assert_eq!(sync_gate(true, false, Some(1), Some(7)).unwrap(), (1, 7));
    }
}
