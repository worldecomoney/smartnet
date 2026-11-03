//! `smartnet_*` RPC namespace — dual-published with `wem_*` on the same [`OperatorExtensionApi`].

use alloy_eips::BlockId;
use alloy_primitives::{map::AddressMap, U256, U64};
use async_trait::async_trait;
use jsonrpsee::{core::RpcResult, PendingSubscriptionSink};
use smartnet_evm::ConfigureEvm;
use smartnet_primitives_traits::NodePrimitives;
use smartnet_chain_state::{
    CanonStateSubscriptions, ForkChoiceSubscriptions, PersistedBlockSubscriptions,
};
use smartnet_rpc_api::SmartnetApiServer;
use smartnet_rpc_eth_types::EthApiError;
use smartnet_storage_api::{BlockReader, BlockReaderIdExt, ChangeSetReader, StateProviderFactory};

use crate::wem::OperatorExtensionApi;

#[async_trait]
impl<Provider, EvmConfig> SmartnetApiServer for OperatorExtensionApi<Provider, EvmConfig>
where
    Provider: BlockReaderIdExt
        + ChangeSetReader
        + StateProviderFactory
        + BlockReader<Block = <Provider::Primitives as NodePrimitives>::Block>
        + CanonStateSubscriptions
        + ForkChoiceSubscriptions<Header = <Provider::Primitives as NodePrimitives>::BlockHeader>
        + PersistedBlockSubscriptions
        + 'static,
    EvmConfig: ConfigureEvm<Primitives = Provider::Primitives> + 'static,
{
    async fn smartnet_get_balance_changes_in_block(
        &self,
        block_id: BlockId,
    ) -> RpcResult<AddressMap<U256>> {
        Ok(Self::balance_changes_in_block(self, block_id).await?)
    }

    async fn smartnet_get_block_execution_outcome(
        &self,
        block_id: BlockId,
        count: Option<U64>,
    ) -> RpcResult<Option<serde_json::Value>> {
        let outcome = Self::block_execution_outcome(self, block_id, count).await?;
        match outcome {
            Some(outcome) => {
                let value = serde_json::to_value(&outcome).map_err(|e| {
                    EthApiError::Internal(smartnet_errors::SmartnetError::msg(e.to_string()))
                })?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    async fn smartnet_subscribe_chain_notifications(
        &self,
        pending: PendingSubscriptionSink,
    ) -> jsonrpsee::core::SubscriptionResult {
        self.rpc_subscribe_chain_notifications(pending).await
    }

    async fn smartnet_subscribe_persisted_block(
        &self,
        pending: PendingSubscriptionSink,
    ) -> jsonrpsee::core::SubscriptionResult {
        self.rpc_subscribe_persisted_block(pending).await
    }

    async fn smartnet_subscribe_finalized_chain_notifications(
        &self,
        pending: PendingSubscriptionSink,
    ) -> jsonrpsee::core::SubscriptionResult {
        self.rpc_subscribe_finalized_chain_notifications(pending).await
    }
}
