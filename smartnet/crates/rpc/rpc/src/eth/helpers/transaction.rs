//! Contains RPC handler implementations specific to transactions

use std::time::Duration;

use crate::EthApi;
use alloy_eips::{eip7594::BlobTransactionSidecarVariant, BlockId, Typed2718};
use alloy_primitives::{hex, B256};
use smartnet_primitives_traits::{AlloyBlockHeader, Recovered, WithEncoded};
use smartnet_transaction_pool::{
    error::Eip4844PoolTransactionError, AddedTransactionOutcome, BlobTransactionValidationError,
    EthBlobTransactionSidecar, EthPoolTransaction, PoolPooledTx, PoolTransaction, TransactionPool,
};
use smartnet_chainspec::{ChainSpecProvider, EthereumHardforks};
use smartnet_rpc_convert::RpcConvert;
use smartnet_rpc_eth_api::{
    helpers::{spec::SignersForRpc, EthTransactions, LoadTransaction},
    FromEvmError, RpcNodeCore,
};
use smartnet_rpc_eth_types::{error::RpcPoolError, EthApiError};
use smartnet_storage_api::BlockReaderIdExt;

impl<N, Rpc> EthTransactions for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
    #[inline]
    fn signers(&self) -> &SignersForRpc<Self::Provider, Self::NetworkTypes> {
        self.inner.signers()
    }

    #[inline]
    fn send_raw_transaction_sync_timeout(&self) -> Duration {
        self.inner.send_raw_transaction_sync_timeout()
    }

    async fn send_transaction(
        &self,
        origin: smartnet_transaction_pool::TransactionOrigin,
        tx: WithEncoded<Recovered<PoolPooledTx<Self::Pool>>>,
    ) -> Result<B256, Self::Error> {
        let (tx, recovered) = tx.split();
        let tx_hash = alloy_primitives::keccak256(&tx);
        let sender = recovered.signer();

        let outcome: Result<B256, Self::Error> = async move {
            // SmartNet mempool entry gate — the ONLY balance-ish check the node makes, and only
            // here at tx entry (RPC + mesh rumor, which funnels through this same seam): the
            // sender must be a known wallet (balance at head >= MIN_TO_TXPOOL_ENTRY, `0` = off)
            // or the tx is rejected before the pool. This is an abuse floor, not consensus:
            // the validator separately compares the real on-chain balance to the tx value at
            // inclusion (`validator/src/mempool_policy.rs` over the `observe_sender_state`
            // seam), and the signed-block import path re-checks nothing (seal check only).
            // The single per-tx `TX` line below carries any gate rejection — one line per tx.
            smartnet_rpc_eth_api::entry_gate::known_wallet_entry_gate(
                self.provider(),
                recovered.signer(),
            )?;

            let mut pool_transaction =
                <Self::Pool as TransactionPool>::Transaction::from_pooled(recovered);

            // Optionally convert legacy blob sidecars to EIP-7594 format when Osaka is active
            // This is opt-in via --rpc.force-blob-sidecar-upcasting
            if self.inner.force_blob_sidecar_upcasting() && pool_transaction.is_eip4844() {
                let EthBlobTransactionSidecar::Present(sidecar) = pool_transaction.take_blob()
                else {
                    return Err(EthApiError::PoolError(RpcPoolError::Eip4844(
                        Eip4844PoolTransactionError::MissingEip4844BlobSidecar,
                    )));
                };

                let sidecar = match sidecar {
                    BlobTransactionSidecarVariant::Eip4844(sidecar) => {
                        let latest = self
                            .provider()
                            .latest_header()?
                            .ok_or(EthApiError::HeaderNotFound(BlockId::latest()))?;
                        // Convert to EIP-7594 if next block is Osaka
                        if self
                            .provider()
                            .chain_spec()
                            .is_osaka_active_at_timestamp(latest.timestamp().saturating_add(12))
                        {
                            BlobTransactionSidecarVariant::Eip7594(
                                self.blob_sidecar_converter().convert(sidecar).await.ok_or_else(
                                    || {
                                        RpcPoolError::Eip4844(
                                            Eip4844PoolTransactionError::InvalidEip4844Blob(
                                                BlobTransactionValidationError::InvalidProof,
                                            ),
                                        )
                                    },
                                )?,
                            )
                        } else {
                            BlobTransactionSidecarVariant::Eip4844(sidecar)
                        }
                    }
                    sidecar => sidecar,
                };

                pool_transaction =
                    EthPoolTransaction::try_from_eip4844(pool_transaction.into_consensus(), sidecar)
                        .ok_or_else(|| {
                            RpcPoolError::Eip4844(
                                Eip4844PoolTransactionError::MissingEip4844BlobSidecar,
                            )
                        })?;
            }

            // forward the transaction to the specific endpoint if configured.
            if let Some(client) = self.raw_tx_forwarder() {
                tracing::debug!(target: "rpc::eth", hash = %pool_transaction.hash(), "forwarding raw transaction to forwarder");
                let rlp_hex = hex::encode_prefixed(&tx);

                // broadcast raw transaction to subscribers if there is any.
                self.broadcast_raw_transaction(tx);

                let hash = client.request("eth_sendRawTransaction", (rlp_hex,)).await.inspect_err(|err| {
                    tracing::debug!(target: "rpc::eth", %err, hash=% *pool_transaction.hash(), "failed to forward raw transaction");
                }).map_err(EthApiError::other)?;

                // Retain tx in local tx pool after forwarding, for local RPC usage.
                let _ = self.inner.add_pool_transaction(origin, pool_transaction).await;

                return Ok(hash);
            }

            // broadcast raw transaction to subscribers if there is any.
            self.broadcast_raw_transaction(tx);

            let AddedTransactionOutcome { hash, .. } =
                self.inner.add_pool_transaction(origin, pool_transaction).await?;

            Ok(hash)
        }
        .await;

        // ONE line per tx at debug with its accepted|rejected verdict. These must not be
        // info: a 100k-tx block would flush 100k lines through the process-wide filter
        // and writer on every seal (VERB = 1 is info).
        match &outcome {
            Ok(_) => tracing::debug!(
                target: "smartnet::cli",
                event = "TX",
                tx = %tx_hash,
                sender = %sender,
                status = "accepted",
                "tx received"
            ),
            Err(err) => tracing::debug!(
                target: "smartnet::cli",
                event = "TX",
                tx = %tx_hash,
                sender = %sender,
                status = "rejected",
                reason = %err,
                "tx received"
            ),
        }
        outcome
    }
}

impl<N, Rpc> LoadTransaction for EthApi<N, Rpc>
where
    N: RpcNodeCore,
    EthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError>,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::helpers::types::EthRpcConverter;
    use alloy_consensus::{Block, Header, Transaction};
    use alloy_primitives::{map::AddressMap, Address, U256};
    use alloy_rpc_types_eth::request::TransactionRequest;
    use smartnet_network_api::noop::NoopNetwork;
    use smartnet_transaction_pool::test_utils::{testing_pool, TestPool};
    use revm_primitives::Bytes;
    use smartnet_chainspec::{ChainSpec, ChainSpecBuilder, EthChainSpec};
    use smartnet_evm_ethereum::EthEvmConfig;
    use smartnet_provider::{
        test_utils::{ExtendedAccount, MockEthProvider},
        ChainSpecProvider,
    };
    use smartnet_rpc_eth_api::node::RpcNodeCoreAdapter;
    use smartnet_rpc_eth_types::EthApiError;

    fn mock_eth_api(
        accounts: AddressMap<ExtendedAccount>,
    ) -> EthApi<
        RpcNodeCoreAdapter<MockEthProvider, TestPool, NoopNetwork, EthEvmConfig>,
        EthRpcConverter<ChainSpec>,
    > {
        let chain_spec = ChainSpecBuilder::mainnet().build();
        let chain_id = chain_spec.chain_id();
        let mock_provider = MockEthProvider::default().with_chain_spec(chain_spec);
        mock_provider.extend_accounts(accounts);

        let evm_config = EthEvmConfig::new(mock_provider.chain_spec());
        let pool = testing_pool();

        let genesis_header = Header {
            number: 0,
            gas_limit: 30_000_000,
            timestamp: 1,
            excess_blob_gas: Some(0),
            base_fee_per_gas: Some(1000000000),
            blob_gas_used: Some(0),
            ..Default::default()
        };

        let genesis_hash = B256::ZERO;
        mock_provider.add_block(genesis_hash, Block::new(genesis_header, Default::default()));

        // The fill path derives the tx chain id from the network, so the mock network must
        // carry the chain spec's chain id (see the node builder's NoopNetwork wiring).
        EthApi::builder(
            mock_provider,
            pool,
            NoopNetwork::default().with_chain_id(chain_id),
            evm_config,
        )
        .build()
    }

    /// A locally signed EIP-1559 transfer, raw RLP bytes — nothing external, we are a closed
    /// local network (mock chain id: the mock builds `ChainSpecBuilder::mainnet()`).
    async fn locally_signed_raw_tx() -> (Address, Bytes) {
        use alloy_network::{EthereumWallet, NetworkTransactionBuilder, TransactionBuilder};
        use alloy_signer_local::PrivateKeySigner;

        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(7)).unwrap();
        let from = signer.address();
        let req = TransactionRequest::default()
            .with_to(Address::repeat_byte(0x11))
            .with_nonce(0)
            .with_gas_limit(21_000)
            .with_max_fee_per_gas(2_000_000_000u128)
            .with_max_priority_fee_per_gas(1_000_000_000u128)
            .with_value(U256::ZERO);
        let envelope = req.build(&EthereumWallet::from(signer)).await.unwrap();
        (from, Bytes::from(alloy_eips::eip2718::Encodable2718::encoded_2718(&envelope)))
    }

    #[tokio::test]
    async fn send_raw_transaction() {
        let (from, raw) = locally_signed_raw_tx().await;
        // The mempool entry gate (knownWallet/minBalance) runs in `send_transaction`, so the
        // test funds the recovered sender in the mock state.
        let accounts =
            AddressMap::from_iter([(from, ExtendedAccount::new(0, U256::from(u128::MAX)))]);
        let eth_api = mock_eth_api(accounts);
        let pool = eth_api.pool();

        let hash = eth_api.send_raw_transaction(raw).await.unwrap();
        assert_eq!(
            pool.len(),
            1,
            "expect 1 transaction in the pool, but pool size is {}",
            pool.len()
        );
        assert!(pool.get(&hash).is_some(), "tx not found in the pool");
    }

    #[tokio::test]
    async fn send_raw_transaction_rejects_unknown_wallet() {
        let (_, raw) = locally_signed_raw_tx().await;
        // No accounts in the mock: zero balance is below the entry floor, the gate rejects at
        // the edge and nothing enters the pool.
        let eth_api = mock_eth_api(Default::default());
        let pool = eth_api.pool();

        let err = eth_api.send_raw_transaction(raw).await.unwrap_err();
        assert!(
            matches!(err, EthApiError::InvalidTransaction(_)),
            "expected an underfunded-sender rejection, got {err:?}"
        );
        assert_eq!(pool.len(), 0, "nothing must enter the pool");
    }

    #[tokio::test]
    async fn test_fill_transaction_fills_chain_id() {
        let address = Address::random();
        let accounts = AddressMap::from_iter([(
            address,
            ExtendedAccount::new(0, U256::from(10_000_000_000_000_000_000u64)), // 10 ETH
        )]);

        let eth_api = mock_eth_api(accounts);

        let tx_req = TransactionRequest {
            from: Some(address),
            to: Some(Address::random().into()),
            gas: Some(21_000),
            ..Default::default()
        };

        let filled =
            eth_api.fill_transaction(tx_req).await.expect("fill_transaction should succeed");

        // Should fill with the chain id from provider
        assert!(filled.tx.chain_id().is_some());
    }
}
