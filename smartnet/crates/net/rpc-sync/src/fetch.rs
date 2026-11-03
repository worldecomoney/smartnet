//! Block fetching over JSON-RPC (`eth_blockNumber` / `eth_getBlockByNumber`), batched.
//!
//! Peers are geth or smartnet nodes; both serve the standard `eth` RPC. Blocks are fetched
//! with full transactions in parallel batches — never one round trip per block.

use alloy_primitives::B256;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::Block;
use futures::StreamExt;
use smartnet_timing::fmt_wall;
use std::{fmt::Debug, time::Instant};
use tracing::debug;

/// A full block as served by `eth_getBlockByNumber(n, true)` (typed transactions).
pub type RpcBlock = Block;

/// Why a fetch failed.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The JSON-RPC transport or the call itself failed.
    #[error("RPC call failed: {0}")]
    Rpc(String),
    /// The peer answered but does not have that block (pruned or reorged out).
    #[error("block {0} missing on peer (pruned or reorged out)")]
    Missing(u64),
}

/// Abstraction over the remote peer so the sync driver is testable without a node.
pub trait BlockSource: Send + Sync {
    /// Remote chain height (latest block number).
    fn block_number(&self) -> impl std::future::Future<Output = Result<u64, FetchError>> + Send;
    /// Hash of block `number` (header fetch). `None` if the peer lacks the block.
    fn block_hash(
        &self,
        number: u64,
    ) -> impl std::future::Future<Output = Result<Option<B256>, FetchError>> + Send;
    /// Full blocks for `start..=end_inclusive`, in order. The whole range must exist.
    fn fetch_blocks(
        &self,
        start: u64,
        end_inclusive: u64,
    ) -> impl std::future::Future<Output = Result<Vec<RpcBlock>, FetchError>> + Send;
}

/// HTTP JSON-RPC [`BlockSource`] backed by an alloy provider.
#[derive(Debug, Clone)]
pub struct HttpBlockSource<P> {
    provider: P,
}

impl<P> HttpBlockSource<P> {
    /// Build from any alloy provider.
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }
}

/// Connect to a peer's JSON-RPC HTTP endpoint.
pub fn connect_http(url: url::Url) -> HttpBlockSource<impl Provider + Clone + 'static> {
    HttpBlockSource::new(ProviderBuilder::new().connect_http(url))
}

impl<P> BlockSource for HttpBlockSource<P>
where
    P: Provider + Send + Sync + Clone + 'static,
{
    async fn block_number(&self) -> Result<u64, FetchError> {
        self.provider.get_block_number().await.map_err(|e| FetchError::Rpc(format!("{e:?}")))
    }

    async fn block_hash(&self, number: u64) -> Result<Option<B256>, FetchError> {
        // Header-only fetch (transaction hashes suffice — we only need `hash`).
        let block = self
            .provider
            .get_block_by_number(number.into())
            .hashes()
            .await
            .map_err(|e| FetchError::Rpc(format!("{e:?}")))?;
        Ok(block.map(|b| b.header.hash))
    }

    async fn fetch_blocks(
        &self,
        start: u64,
        end_inclusive: u64,
    ) -> Result<Vec<RpcBlock>, FetchError> {
        if start > end_inclusive {
            return Ok(Vec::new());
        }
        // Strictly sequential requests: the peer's proxy counts concurrent in-flight
        // requests as a batch and rejects >~5 with -32000/-32603 ("too many requests").
        // Speed is not a concern for the join path (measured rate reality + owner
        // decision: strictly sequential until a speedup lands — doc/check/result.md).
        let concurrency = 1usize;
        let provider = self.provider.clone();
        let range_start = Instant::now();
        let blocks = futures::stream::iter(start..=end_inclusive)
            .map(|n| {
                let provider = provider.clone();
                async move {
                    let call_start = Instant::now();
                    let block = provider
                        .get_block_by_number(n.into())
                        .full()
                        .await
                        .map_err(|e| FetchError::Rpc(format!("{e:?}")))?;
                    // One sequential round trip per block — the per-block wall is the
                    // peer RTT, so a slow pass localizes to network vs import.
                    debug!(
                        target: "smartnet::rpc_sync",
                        event = "SYNC_FETCH_BLOCK",
                        number = n,
                        fetch_wall = %fmt_wall(call_start.elapsed()),
                        "fetched one block from peer"
                    );
                    block.ok_or(FetchError::Missing(n))
                }
            })
            .buffered(concurrency)
            .collect::<Vec<Result<RpcBlock, FetchError>>>()
            .await;
        debug!(
            target: "smartnet::rpc_sync",
            event = "SYNC_FETCH_RANGE",
            start,
            end = end_inclusive,
            fetch_wall = %fmt_wall(range_start.elapsed()),
            "fetched block range from peer (sequential round trips)"
        );
        // Preserve canonical order and fail on the first error.
        let mut out = Vec::with_capacity(blocks.len());
        for b in blocks {
            out.push(b?);
        }
        Ok(out)
    }
}
