//! `SmartNet` RPC namespace (`smartnet_*`), dual-published with legacy `wem_*`.
#![allow(missing_docs)] // jsonrpsee `#[rpc]` trait items; method docs are on the trait below.

use alloy_eips::BlockId;
use alloy_primitives::{map::AddressMap, U256, U64};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

use smartnet_chain_state as _;

/// SmartNet API namespace (operator/debug extensions on the HTTP/WS transport).
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "smartnet"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "smartnet"))]
pub trait SmartnetApi {
    /// Balance deltas per address for one canonical block.
    #[method(name = "getBalanceChangesInBlock")]
    async fn smartnet_get_balance_changes_in_block(
        &self,
        block_id: BlockId,
    ) -> RpcResult<AddressMap<U256>>;

    /// Execution outcome JSON for a block (optional tx count limit).
    #[method(name = "getBlockExecutionOutcome")]
    async fn smartnet_get_block_execution_outcome(
        &self,
        block_id: BlockId,
        count: Option<U64>,
    ) -> RpcResult<Option<serde_json::Value>>;

    /// Stream canonical chain notifications.
    #[subscription(
        name = "subscribeChainNotifications",
        unsubscribe = "unsubscribeChainNotifications",
        item = smartnet_chain_state::CanonStateNotification
    )]
    async fn smartnet_subscribe_chain_notifications(&self) -> jsonrpsee::core::SubscriptionResult;

    /// Stream block num/hash after persistence.
    #[subscription(
        name = "subscribePersistedBlock",
        unsubscribe = "unsubscribePersistedBlock",
        item = alloy_eips::BlockNumHash
    )]
    async fn smartnet_subscribe_persisted_block(&self) -> jsonrpsee::core::SubscriptionResult;

    /// Stream batched finalized chain notifications.
    #[subscription(
        name = "subscribeFinalizedChainNotifications",
        unsubscribe = "unsubscribeFinalizedChainNotifications",
        item = Vec<smartnet_chain_state::CanonStateNotification>
    )]
    async fn smartnet_subscribe_finalized_chain_notifications(
        &self,
    ) -> jsonrpsee::core::SubscriptionResult;
}
