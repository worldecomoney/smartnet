//! `otterscan` namespace — honest stubs: `SmartNet` serves the `DeFi`/web3-wallet RPC surface only.
//! `SmartNet` has its own approach to block exploration (operator ruling, 2026-10-28).

use alloy_eips::{eip1898::LenientBlockNumberOrTag, BlockId};
use alloy_primitives::{Address, Bytes, TxHash, B256};
use alloy_rpc_types_trace::otterscan::{
    BlockDetails, ContractCreator, InternalOperation, OtsBlockTransactions, TraceEntry,
    TransactionsWithReceipts,
};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use smartnet_rpc_api::OtterscanServer;
use smartnet_rpc_convert::RpcTransaction;
use smartnet_rpc_eth_api::{FullEthApiTypes, RpcHeader};
use smartnet_rpc_server_types::result::internal_rpc_err;

/// Otterscan API.
#[derive(Debug)]
pub struct OtterscanApi<Eth> {
    eth: Eth,
}

impl<Eth> OtterscanApi<Eth> {
    /// Creates a new instance of `Otterscan`.
    pub const fn new(eth: Eth) -> Self {
        Self { eth }
    }

    /// Access the underlying `Eth` API.
    pub const fn eth_api(&self) -> &Eth {
        &self.eth
    }
}

#[async_trait]
impl<Eth> OtterscanServer<RpcTransaction<Eth::NetworkTypes>, RpcHeader<Eth::NetworkTypes>>
    for OtterscanApi<Eth>
where
    Eth: FullEthApiTypes + 'static,
{
    async fn get_header_by_number(
        &self,
        _block_number: LenientBlockNumberOrTag,
    ) -> RpcResult<Option<RpcHeader<Eth::NetworkTypes>>> {
        Err(internal_rpc_err("unsupported: ots_getHeaderByNumber"))
    }

    async fn has_code(
        &self,
        _address: Address,
        _block_id: Option<BlockId>,
    ) -> RpcResult<bool> {
        Err(internal_rpc_err("unsupported: ots_hasCode"))
    }

    async fn get_api_level(&self) -> RpcResult<u64> {
        Err(internal_rpc_err("unsupported: ots_getApiLevel"))
    }

    async fn get_internal_operations(
        &self,
        _tx_hash: TxHash,
    ) -> RpcResult<Vec<InternalOperation>> {
        Err(internal_rpc_err("unsupported: ots_getInternalOperations"))
    }

    async fn get_transaction_error(&self, _tx_hash: TxHash) -> RpcResult<Option<Bytes>> {
        Err(internal_rpc_err("unsupported: ots_getTransactionError"))
    }

    async fn trace_transaction(
        &self,
        _tx_hash: TxHash,
    ) -> RpcResult<Option<Vec<TraceEntry>>> {
        Err(internal_rpc_err("unsupported: ots_traceTransaction"))
    }

    async fn get_block_details(
        &self,
        _block_number: LenientBlockNumberOrTag,
    ) -> RpcResult<BlockDetails<RpcHeader<Eth::NetworkTypes>>> {
        Err(internal_rpc_err("unsupported: ots_getBlockDetails"))
    }

    async fn get_block_details_by_hash(
        &self,
        _block_hash: B256,
    ) -> RpcResult<BlockDetails<RpcHeader<Eth::NetworkTypes>>> {
        Err(internal_rpc_err("unsupported: ots_getBlockDetailsByHash"))
    }

    async fn get_block_transactions(
        &self,
        _block_number: LenientBlockNumberOrTag,
        _page_number: usize,
        _page_size: usize,
    ) -> RpcResult<OtsBlockTransactions<RpcTransaction<Eth::NetworkTypes>, RpcHeader<Eth::NetworkTypes>>>
    {
        Err(internal_rpc_err("unsupported: ots_getBlockTransactions"))
    }

    async fn search_transactions_before(
        &self,
        _address: Address,
        _block_number: LenientBlockNumberOrTag,
        _page_size: usize,
    ) -> RpcResult<TransactionsWithReceipts> {
        Err(internal_rpc_err("unsupported: ots_searchTransactionsBefore"))
    }

    async fn search_transactions_after(
        &self,
        _address: Address,
        _block_number: LenientBlockNumberOrTag,
        _page_size: usize,
    ) -> RpcResult<TransactionsWithReceipts> {
        Err(internal_rpc_err("unsupported: ots_searchTransactionsAfter"))
    }

    async fn get_transaction_by_sender_and_nonce(
        &self,
        _sender: Address,
        _nonce: u64,
    ) -> RpcResult<Option<TxHash>> {
        Err(internal_rpc_err("unsupported: ots_getTransactionBySenderAndNonce"))
    }

    async fn get_contract_creator(
        &self,
        _address: Address,
    ) -> RpcResult<Option<ContractCreator>> {
        Err(internal_rpc_err("unsupported: ots_getContractCreator"))
    }
}
