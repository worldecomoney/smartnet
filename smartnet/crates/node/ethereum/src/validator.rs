//! The merged validator's in-process node interface.
//!
//! [`InProcValidatorIo`] implements the validator crate's [`ValidatorIo`] boundary directly on top
//! of the node's own `EthApi`, transaction pool and consensus-engine handle. Nothing leaves the
//! process: pool reads come from the pool, state reads from the `EthApi`, preflight from
//! `eth_simulateV1`'s in-process implementation, and a sealed block is handed to the engine as an
//! [`ExecutionData`] payload — there is no JSON-RPC client, URL, or loopback socket anywhere in
//! the validator's path.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use alloy_eips::BlockId;
use alloy_network::Ethereum;
use alloy_primitives::{Address, Bytes, TxHash, B256, U256};
use alloy_rpc_types_engine::ExecutionData;
use alloy_rpc_types_eth::{
    simulate::{SimulatePayload, SimulatedBlock},
    Block, BlockNumberOrTag, Transaction,
};
use alloy_rpc_types_txpool::TxpoolContent;
use smartnet_ethereum_primitives::EthPrimitives;
use smartnet_transaction_pool::{PoolTransaction, TransactionPool};
use serde_json::Value;
use smartnet_engine_primitives::ConsensusEngineHandle;
use smartnet_engine_primitives::ethereum::execution_data_from_block_rlp;
use smartnet_payload_primitives::PayloadTypes;
use smartnet_rpc_eth_api::{EthApiServer, FullEthApiServer, RpcConvert};
use smartnet_validator::io::{erc20_balance_of_call, erc20_decode_balance, ValidatorIo};

/// [`ValidatorIo`] wired to the node's live components.
pub struct InProcValidatorIo<A, P>
where
    P: PayloadTypes,
{
    eth_api: A,
    engine: ConsensusEngineHandle<P>,
    chain_id: u64,
}

impl<A, P> std::fmt::Debug for InProcValidatorIo<A, P>
where
    P: PayloadTypes,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcValidatorIo")
            .field("chain_id", &self.chain_id)
            .finish_non_exhaustive()
    }
}

impl<A, P> InProcValidatorIo<A, P>
where
    A: FullEthApiServer<NetworkTypes = Ethereum, Primitives = EthPrimitives>,
    P: PayloadTypes<ExecutionData = ExecutionData>,
{
    /// Builds the in-process interface from the node's live `EthApi` and engine handle.
    pub const fn new(eth_api: A, engine: ConsensusEngineHandle<P>, chain_id: u64) -> Self {
        Self { eth_api, engine, chain_id }
    }
}

fn rpc_err<E: std::fmt::Display>(what: &str, e: E) -> eyre::Report {
    eyre::eyre!("{what}: {e}")
}

impl<A, P> ValidatorIo for InProcValidatorIo<A, P>
where
    A: FullEthApiServer<NetworkTypes = Ethereum, Primitives = EthPrimitives>
        + Send
        + Sync
        + 'static,
    P: PayloadTypes<ExecutionData = ExecutionData> + Send + Sync + 'static,
{
    fn label(&self) -> &str {
        "in-proc"
    }

    async fn chain_id(&self) -> eyre::Result<u64> {
        Ok(self.chain_id)
    }

    async fn block_by_number(
        &self,
        tag: BlockNumberOrTag,
    ) -> eyre::Result<Option<Block<Transaction>>> {
        EthApiServer::block_by_number(&self.eth_api, tag, true)
            .await
            .map_err(|e| rpc_err("block_by_number", e))
    }

    async fn block_by_hash(&self, hash: B256) -> eyre::Result<Option<Block<Transaction>>> {
        EthApiServer::block_by_hash(&self.eth_api, hash, true)
            .await
            .map_err(|e| rpc_err("block_by_hash", e))
    }

    async fn txpool_content(&self) -> eyre::Result<TxpoolContent<Transaction>> {
        let pool = self.eth_api.pool();
        let all = pool.all_transactions();
        let converter = self.eth_api.converter();

        let mut content = TxpoolContent::default();
        // Pending and queued stay in their own buckets: selection never reads `queued`,
        // and `TXPOOL_VAL_MAX_TXS` truncation must budget only the promotable set — folding
        // queued in here would let stale gap-blocked txs evict the executable head.
        for tx in all.pending {
            let sender = tx.sender();
            let nonce = tx.nonce();
            let rpc = converter
                .fill_pending(tx.transaction.clone_into_consensus())
                .map_err(|e| rpc_err("convert pool tx", e))?;
            content.pending.entry(sender).or_default().insert(nonce.to_string(), rpc);
        }
        for tx in all.queued {
            let sender = tx.sender();
            let nonce = tx.nonce();
            let rpc = converter
                .fill_pending(tx.transaction.clone_into_consensus())
                .map_err(|e| rpc_err("convert pool tx", e))?;
            content.queued.entry(sender).or_default().insert(nonce.to_string(), rpc);
        }
        Ok(content)
    }

    async fn txpool_arrival(&self) -> eyre::Result<BTreeMap<TxHash, Instant>> {
        let pool = self.eth_api.pool();
        let all = pool.all_transactions();
        let mut out = BTreeMap::new();
        for tx in all.pending.iter().chain(all.queued.iter()) {
            out.insert(*tx.hash(), tx.timestamp);
        }
        Ok(out)
    }

    async fn txpool_status(&self) -> eyre::Result<Value> {
        let pool = self.eth_api.pool();
        let all = pool.all_transactions();
        Ok(serde_json::json!({
            "pending": format!("{:#x}", all.pending.len()),
            "queued": format!("{:#x}", all.queued.len()),
        }))
    }

    async fn balances(&self, owners: &[Address]) -> Vec<eyre::Result<U256>> {
        let mut out = Vec::with_capacity(owners.len());
        for owner in owners {
            out.push(
                EthApiServer::balance(&self.eth_api, *owner, Some(BlockId::latest()))
                    .await
                    .map_err(|e| rpc_err("balance", e)),
            );
        }
        out
    }

    async fn erc20_balances(&self, pairs: &[(Address, Address)]) -> Vec<eyre::Result<U256>> {
        let mut out = Vec::with_capacity(pairs.len());
        for (token, owner) in pairs {
            let result = EthApiServer::call(
                &self.eth_api,
                erc20_balance_of_call(*token, *owner),
                Some(BlockId::latest()),
                None,
                None,
            )
            .await
            .map_err(|e| rpc_err("eth_call balanceOf", e))
            .and_then(|raw| erc20_decode_balance(&raw, *token, *owner));
            out.push(result);
        }
        out
    }

    async fn nonces(&self, owners: &[Address]) -> Vec<eyre::Result<u64>> {
        let mut out = Vec::with_capacity(owners.len());
        for owner in owners {
            let result =
                EthApiServer::transaction_count(&self.eth_api, *owner, Some(BlockId::latest()))
                    .await
                    .map_err(|e| rpc_err("transaction_count", e))
                    .and_then(|nonce| {
                        u64::try_from(nonce)
                            .map_err(|_| eyre::eyre!("nonce {nonce} does not fit in u64"))
                    });
            out.push(result);
        }
        out
    }

    async fn simulate_v1(
        &self,
        payload: SimulatePayload,
    ) -> eyre::Result<Vec<SimulatedBlock<Block<Transaction>>>> {
        EthApiServer::simulate_v1(&self.eth_api, payload, Some(BlockId::latest()))
            .await
            .map_err(|e| rpc_err("simulate_v1", e))
    }

    async fn submit_block_rlp(&self, rlp: Bytes) -> eyre::Result<Value> {
        let block: smartnet_ethereum_primitives::Block =
            alloy_rlp::Decodable::decode(&mut rlp.as_ref())
                .map_err(|e| eyre::eyre!("decode sealed block RLP: {e}"))?;

        let (status, timings) = self
            .engine
            .new_payload(execution_data_from_block_rlp(block), true, true)
            .await
            .map_err(|e| eyre::eyre!("engine new_payload: {e}"))?;

        let mut value = serde_json::to_value(&status)
            .map_err(|e| eyre::eyre!("serialize payload status: {e}"))?;
        if let Value::Object(obj) = &mut value {
            obj.insert("latency_us".into(), micros(timings.latency).into());
            obj.insert("queue_wait_us".into(), micros(timings.queue_wait).into());
            obj.insert("persistence_wait_us".into(), micros(timings.persistence_wait).into());
        }
        Ok(value)
    }
}

const fn micros(d: Duration) -> u64 {
    d.as_micros() as u64
}
