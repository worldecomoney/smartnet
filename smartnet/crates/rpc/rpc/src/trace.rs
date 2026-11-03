//! `trace` namespace — honest stubs: `SmartNet` serves the `DeFi`/web3-wallet RPC surface only.
//!
//! Every method returns an explicit `unsupported` error (operator ruling, 2026-10-28:
//! `DeFi` + web3 wallets decide what lives over RPC; tracing is neither).

use alloy_eips::BlockId;
use alloy_primitives::{map::HashSet, Bytes, B256};
use alloy_rpc_types_eth::{state::StateOverride, BlockOverrides, Index};
use alloy_rpc_types_trace::{
    filter::TraceFilter,
    opcode::{BlockOpcodeGas, TransactionOpcodeGas},
    parity::*,
};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use smartnet_rpc_api::TraceApiServer;
use smartnet_rpc_convert::RpcTxReq;
use smartnet_rpc_eth_api::FullEthApiTypes;
use smartnet_rpc_eth_types::EthConfig;
use smartnet_rpc_server_types::result::internal_rpc_err;
use smartnet_tasks::pool::BlockingTaskGuard;
use std::sync::Arc;

/// `trace` API implementation.
///
/// This type provides the functionality for handling `trace` related requests.
pub struct TraceApi<Eth> {
    inner: Arc<TraceApiInner<Eth>>,
}

impl<Eth> TraceApi<Eth> {
    /// Create a new instance of the [`TraceApi`]
    pub fn new(
        eth_api: Eth,
        _blocking_task_guard: BlockingTaskGuard,
        _eth_config: EthConfig,
    ) -> Self {
        let inner = Arc::new(TraceApiInner { eth_api });
        Self { inner }
    }

    /// Access the underlying `Eth` API.
    pub fn eth_api(&self) -> &Eth {
        &self.inner.eth_api
    }
}

impl<Eth> std::fmt::Debug for TraceApi<Eth> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceApi").finish_non_exhaustive()
    }
}

impl<Eth> Clone for TraceApi<Eth> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

struct TraceApiInner<Eth> {
    /// Access to commonly used code of the `eth` namespace
    eth_api: Eth,
}

#[async_trait]
impl<Eth> TraceApiServer<RpcTxReq<Eth::NetworkTypes>> for TraceApi<Eth>
where
    Eth: FullEthApiTypes + 'static,
{
    async fn trace_call(
        &self,
        _call: RpcTxReq<Eth::NetworkTypes>,
        _trace_types: HashSet<TraceType>,
        _block_id: Option<BlockId>,
        _state_overrides: Option<StateOverride>,
        _block_overrides: Option<Box<BlockOverrides>>,
    ) -> RpcResult<TraceResults> {
        Err(internal_rpc_err("unsupported: trace_call"))
    }

    async fn trace_call_many(
        &self,
        _calls: Vec<(RpcTxReq<Eth::NetworkTypes>, HashSet<TraceType>)>,
        _block_id: Option<BlockId>,
    ) -> RpcResult<Vec<TraceResults>> {
        Err(internal_rpc_err("unsupported: trace_call_many"))
    }

    async fn trace_raw_transaction(
        &self,
        _data: Bytes,
        _trace_types: HashSet<TraceType>,
        _block_id: Option<BlockId>,
    ) -> RpcResult<TraceResults> {
        Err(internal_rpc_err("unsupported: trace_raw_transaction"))
    }

    async fn replay_block_transactions(
        &self,
        _block_id: BlockId,
        _trace_types: HashSet<TraceType>,
    ) -> RpcResult<Option<Vec<TraceResultsWithTransactionHash>>> {
        Err(internal_rpc_err("unsupported: replay_block_transactions"))
    }

    async fn replay_transaction(
        &self,
        _transaction: B256,
        _trace_types: HashSet<TraceType>,
    ) -> RpcResult<TraceResults> {
        Err(internal_rpc_err("unsupported: replay_transaction"))
    }

    async fn trace_block(
        &self,
        _block_id: BlockId,
    ) -> RpcResult<Option<Vec<LocalizedTransactionTrace>>> {
        Err(internal_rpc_err("unsupported: trace_block"))
    }

    async fn trace_filter(
        &self,
        _filter: TraceFilter,
    ) -> RpcResult<Vec<LocalizedTransactionTrace>> {
        Err(internal_rpc_err("unsupported: trace_filter"))
    }

    async fn trace_get(
        &self,
        _hash: B256,
        _indices: Vec<Index>,
    ) -> RpcResult<Option<LocalizedTransactionTrace>> {
        Err(internal_rpc_err("unsupported: trace_get"))
    }

    async fn trace_transaction(
        &self,
        _hash: B256,
    ) -> RpcResult<Option<Vec<LocalizedTransactionTrace>>> {
        Err(internal_rpc_err("unsupported: trace_transaction"))
    }

    async fn trace_transaction_opcode_gas(
        &self,
        _tx_hash: B256,
    ) -> RpcResult<Option<TransactionOpcodeGas>> {
        Err(internal_rpc_err("unsupported: trace_transaction_opcode_gas"))
    }

    async fn trace_block_opcode_gas(
        &self,
        _block_id: BlockId,
    ) -> RpcResult<Option<BlockOpcodeGas>> {
        Err(internal_rpc_err("unsupported: trace_block_opcode_gas"))
    }
}
