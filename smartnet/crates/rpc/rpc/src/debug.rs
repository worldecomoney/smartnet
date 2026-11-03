//! `debug` namespace — honest stubs: `SmartNet` serves the `DeFi`/web3-wallet RPC surface only.
//!
//! Every method returns an explicit `unsupported` error (operator ruling, 2026-10-28).
//! No tracing, no witness, no profiling, no chain surgery over RPC.

use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_genesis::ChainConfig;
use alloy_primitives::{Address, Bytes, B256, U64};
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_rpc_types_eth::{Bundle, StateContext};
use alloy_rpc_types_trace::geth::{
    BlockTraceResult, GethDebugTracingCallOptions, GethDebugTracingOptions, GethTrace, TraceResult,
};
use async_trait::async_trait;
use futures::Stream;
use jsonrpsee::core::RpcResult;
use smartnet_engine_primitives::ConsensusEngineEvent;
use smartnet_rpc_api::DebugApiServer;
use smartnet_rpc_convert::RpcTxReq;
use smartnet_rpc_eth_api::{FullEthApiTypes, RpcNodeCore};
use smartnet_rpc_server_types::result::internal_rpc_err;
use smartnet_tasks::{pool::BlockingTaskGuard, Runtime};
use smartnet_trie_common::{updates::TrieUpdates, ExecutionWitnessMode, HashedPostState};
use std::sync::Arc;

/// `debug` API implementation.
///
/// This type provides the functionality for handling `debug` related requests.
pub struct DebugApi<Eth: RpcNodeCore> {
    inner: Arc<DebugApiInner<Eth>>,
}

impl<Eth> DebugApi<Eth>
where
    Eth: RpcNodeCore,
{
    /// Create a new instance of the [`DebugApi`]
    pub fn new(
        eth_api: Eth,
        _blocking_task_guard: BlockingTaskGuard,
        _executor: &Runtime,
        _stream: impl Stream<Item = ConsensusEngineEvent<Eth::Primitives>> + Send + Unpin + 'static,
    ) -> Self {
        let inner = Arc::new(DebugApiInner { eth_api });
        Self { inner }
    }

    /// Access the underlying `Eth` API.
    pub fn eth_api(&self) -> &Eth {
        &self.inner.eth_api
    }

    /// Access the underlying provider.
    pub fn provider(&self) -> &Eth::Provider {
        self.inner.eth_api.provider()
    }
}

impl<Eth: RpcNodeCore> std::fmt::Debug for DebugApi<Eth> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugApi").finish_non_exhaustive()
    }
}

impl<Eth: RpcNodeCore> Clone for DebugApi<Eth> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

struct DebugApiInner<Eth: RpcNodeCore> {
    /// The implementation of `eth` API
    eth_api: Eth,
}

#[async_trait]
impl<Eth> DebugApiServer<RpcTxReq<Eth::NetworkTypes>> for DebugApi<Eth>
where
    Eth: FullEthApiTypes + 'static,
{
    async fn raw_header(&self, _block_id: BlockId) -> RpcResult<Bytes> {
        Err(internal_rpc_err("unsupported: debug_getRawHeader"))
    }

    async fn raw_block(&self, _block_id: BlockId) -> RpcResult<Bytes> {
        Err(internal_rpc_err("unsupported: debug_getRawBlock"))
    }

    async fn raw_transaction(&self, _hash: B256) -> RpcResult<Option<Bytes>> {
        Err(internal_rpc_err("unsupported: debug_getRawTransaction"))
    }

    async fn raw_transactions(&self, _block_id: BlockId) -> RpcResult<Vec<Bytes>> {
        Err(internal_rpc_err("unsupported: debug_getRawTransactions"))
    }

    async fn raw_receipts(&self, _block_id: BlockId) -> RpcResult<Vec<Bytes>> {
        Err(internal_rpc_err("unsupported: debug_getRawReceipts"))
    }

    async fn bad_blocks(&self) -> RpcResult<Vec<serde_json::Value>> {
        Err(internal_rpc_err("unsupported: debug_getBadBlocks"))
    }

    async fn debug_trace_chain(
        &self,
        _start_exclusive: BlockNumberOrTag,
        _end_inclusive: BlockNumberOrTag,
    ) -> RpcResult<Vec<BlockTraceResult>> {
        Err(internal_rpc_err("unsupported: debug_traceChain"))
    }

    async fn debug_trace_block(
        &self,
        _rlp_block: Bytes,
        _opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        Err(internal_rpc_err("unsupported: debug_traceBlock"))
    }

    async fn debug_trace_block_by_hash(
        &self,
        _block: B256,
        _opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        Err(internal_rpc_err("unsupported: debug_traceBlockByHash"))
    }

    async fn debug_trace_block_by_number(
        &self,
        _block: BlockNumberOrTag,
        _opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        Err(internal_rpc_err("unsupported: debug_traceBlockByNumber"))
    }

    async fn debug_trace_transaction(
        &self,
        _tx_hash: B256,
        _opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<GethTrace> {
        Err(internal_rpc_err("unsupported: debug_traceTransaction"))
    }

    async fn debug_trace_call(
        &self,
        _request: RpcTxReq<Eth::NetworkTypes>,
        _block_id: Option<BlockId>,
        _opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<GethTrace> {
        Err(internal_rpc_err("unsupported: debug_traceCall"))
    }

    async fn debug_trace_call_many(
        &self,
        _bundles: Vec<Bundle<RpcTxReq<Eth::NetworkTypes>>>,
        _state_context: Option<StateContext>,
        _opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<Vec<Vec<GethTrace>>> {
        Err(internal_rpc_err("unsupported: debug_traceCallMany"))
    }

    async fn debug_execution_witness(
        &self,
        _block: BlockNumberOrTag,
        _mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness> {
        Err(internal_rpc_err("unsupported: debug_executionWitness"))
    }

    async fn debug_execution_witness_by_block_hash(
        &self,
        _hash: B256,
        _mode: Option<ExecutionWitnessMode>,
    ) -> RpcResult<ExecutionWitness> {
        Err(internal_rpc_err("unsupported: debug_executionWitnessByBlockHash"))
    }

    async fn debug_backtrace_at(&self, _location: &str) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_backtraceAt"))
    }

    async fn debug_account_range(
        &self,
        _block_number: BlockNumberOrTag,
        _start: Bytes,
        _max_results: u64,
        _nocode: bool,
        _nostorage: bool,
        _incompletes: bool,
    ) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_accountRange"))
    }

    async fn debug_block_profile(&self, _file: String, _seconds: u64) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_blockProfile"))
    }

    async fn debug_chaindb_compact(&self) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_chaindbCompact"))
    }

    async fn debug_chain_config(&self) -> RpcResult<ChainConfig> {
        Err(internal_rpc_err("unsupported: debug_chainConfig"))
    }

    async fn debug_chaindb_property(&self, _property: String) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_chaindbProperty"))
    }

    async fn debug_code_by_hash(
        &self,
        _hash: B256,
        _block_id: Option<BlockId>,
    ) -> RpcResult<Option<Bytes>> {
        Err(internal_rpc_err("unsupported: debug_codeByHash"))
    }

    async fn debug_cpu_profile(&self, _file: String, _seconds: u64) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_cpuProfile"))
    }

    async fn debug_db_ancient(&self, _kind: String, _number: u64) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_dbAncient"))
    }

    async fn debug_db_ancients(&self) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_dbAncients"))
    }

    async fn debug_db_get(&self, _key: String) -> RpcResult<Option<Bytes>> {
        Err(internal_rpc_err("unsupported: debug_dbGet"))
    }

    async fn debug_dump_block(&self, _number: BlockId) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_dumpBlock"))
    }

    async fn debug_free_os_memory(&self) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_freeOSMemory"))
    }

    async fn debug_freeze_client(&self, _node: String) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_freezeClient"))
    }

    async fn debug_gc_stats(&self) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_gcStats"))
    }

    async fn debug_get_accessible_state(
        &self,
        _from: BlockNumberOrTag,
        _to: BlockNumberOrTag,
    ) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_getAccessibleState"))
    }

    async fn debug_get_modified_accounts_by_hash(
        &self,
        _start_hash: B256,
        _end_hash: B256,
    ) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_getModifiedAccountsByHash"))
    }

    async fn debug_get_modified_accounts_by_number(
        &self,
        _start_number: u64,
        _end_number: u64,
    ) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_getModifiedAccountsByNumber"))
    }

    async fn debug_go_trace(&self, _file: String, _seconds: u64) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_goTrace"))
    }

    async fn debug_intermediate_roots(
        &self,
        _block_hash: B256,
        _opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<Vec<B256>> {
        Err(internal_rpc_err("unsupported: debug_intermediateRoots"))
    }

    async fn debug_mem_stats(&self) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_memStats"))
    }

    async fn debug_mutex_profile(&self, _file: String, _nsec: u64) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_mutexProfile"))
    }

    async fn debug_preimage(&self, _hash: B256) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_preimage"))
    }

    async fn debug_print_block(&self, _number: u64) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_printBlock"))
    }

    async fn debug_seed_hash(&self, _number: u64) -> RpcResult<B256> {
        Err(internal_rpc_err("unsupported: debug_seedHash"))
    }

    async fn debug_set_block_profile_rate(&self, _rate: u64) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_setBlockProfileRate"))
    }

    async fn debug_set_gc_percent(&self, _v: i32) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_setGCPercent"))
    }

    async fn debug_set_head(&self, _number: U64) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_setHead"))
    }

    async fn debug_set_mutex_profile_fraction(&self, _rate: i32) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_setMutexProfileFraction"))
    }

    async fn debug_set_trie_flush_interval(&self, _interval: String) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_setTrieFlushInterval"))
    }

    async fn debug_stacks(&self) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_stacks"))
    }

    async fn debug_standard_trace_bad_block_to_file(
        &self,
        _block: BlockNumberOrTag,
        _opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_standardTraceBadBlockToFile"))
    }

    async fn debug_standard_trace_block_to_file(
        &self,
        _block: BlockNumberOrTag,
        _opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_standardTraceBlockToFile"))
    }

    async fn debug_start_cpu_profile(&self, _file: String) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_startCPUProfile"))
    }

    async fn debug_start_go_trace(&self, _file: String) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_startGoTrace"))
    }

    async fn debug_state_root_with_updates(
        &self,
        _hashed_state: HashedPostState,
        _block_id: Option<BlockId>,
    ) -> RpcResult<(B256, TrieUpdates)> {
        Err(internal_rpc_err("unsupported: debug_stateRootWithUpdates"))
    }

    async fn debug_stop_cpu_profile(&self) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_stopCPUProfile"))
    }

    async fn debug_stop_go_trace(&self) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_stopGoTrace"))
    }

    async fn debug_storage_range_at(
        &self,
        _block_hash: B256,
        _tx_idx: usize,
        _contract_address: Address,
        _key_start: B256,
        _max_result: u64,
    ) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_storageRangeAt"))
    }

    async fn debug_trace_bad_block(
        &self,
        _block_hash: B256,
        _opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        Err(internal_rpc_err("unsupported: debug_traceBadBlock"))
    }

    async fn debug_verbosity(&self, _level: usize) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_verbosity"))
    }

    async fn debug_vmodule(&self, _pattern: String) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_vmodule"))
    }

    async fn debug_write_block_profile(&self, _file: String) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_writeBlockProfile"))
    }

    async fn debug_write_mem_profile(&self, _file: String) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_writeMemProfile"))
    }

    async fn debug_write_mutex_profile(&self, _file: String) -> RpcResult<()> {
        Err(internal_rpc_err("unsupported: debug_writeMutexProfile"))
    }
}
