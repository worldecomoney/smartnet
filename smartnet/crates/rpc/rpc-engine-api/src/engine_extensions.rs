//! `wem_*` and canonical `smartnet_*` engine JSON-RPC extensions (dual-publish).

use crate::EngineApiError;
use alloy_rlp::Decodable;
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated};
use async_trait::async_trait;
use jsonrpsee_core::{server::RpcModule, RegisterMethodError, RpcResult};
use smartnet_primitives_traits::SealedBlock;
use smartnet_engine_primitives::ConsensusEngineHandle;
use smartnet_payload_primitives::{ExecutionPayload, PayloadTypes};
use smartnet_rpc_api::{
    SmartNetNewPayloadInput, SmartNetPayloadStatus, SmartnetEngineApiServer, WemEngineApiServer,
};
use smartnet_timing::{fmt_wall, fmt_wall_opt};
use std::time::Instant;
use tracing::{info, trace, warn};

/// Engine extension RPC (`wem_newPayload` / `smartnet_newPayload`, shared implementation).
#[derive(Debug, Clone)]
pub struct EngineExtensionsApi<Payload: PayloadTypes> {
    beacon_engine_handle: ConsensusEngineHandle<Payload>,
}

impl<Payload: PayloadTypes> EngineExtensionsApi<Payload> {
    /// Wraps the consensus engine handle used for `newPayload` / FCU extensions.
    pub const fn new(beacon_engine_handle: ConsensusEngineHandle<Payload>) -> Self {
        Self { beacon_engine_handle }
    }

    /// Submits an execution payload (`BlockRlp` or structured) to the consensus engine.
    pub async fn new_payload(
        &self,
        input: SmartNetNewPayloadInput<Payload::ExecutionData>,
        wait_for_persistence: Option<bool>,
        wait_for_caches: Option<bool>,
    ) -> RpcResult<SmartNetPayloadStatus>
    where
        Payload::ExecutionData: ExecutionPayload,
    {
        let wait_for_persistence = wait_for_persistence.unwrap_or(true);
        let wait_for_caches = wait_for_caches.unwrap_or(true);
        // Whole server-side wall for this RPC call (decode → response): the difference
        // between `request_wall` and `latency_wall` is queue + persistence + cache waits.
        let handler_start = Instant::now();
        trace!(target: "rpc::engine", wait_for_persistence, wait_for_caches, "Serving engine newPayload");

        let payload = match input {
            SmartNetNewPayloadInput::ExecutionData(data) => data,
            SmartNetNewPayloadInput::BlockRlp(rlp) => {
                let rlp_bytes = rlp.len();
                let block = Decodable::decode(&mut rlp.as_ref()).map_err(|err| {
                    warn!(
                        target: "smartnet::cli",
                        event = "ENGINE_NEW_PAYLOAD_BLOCK_RLP_DECODE_FAILED",
                        rlp_bytes,
                        %err,
                        "newPayload BlockRlp RLP decode failed"
                    );
                    EngineApiError::Internal(Box::new(err))
                })?;
                info!(
                    target: "smartnet::cli",
                    event = "ENGINE_NEW_PAYLOAD_BLOCK_RLP_DECODED",
                    rlp_bytes,
                    "newPayload BlockRlp RLP decode OK"
                );
                Payload::block_to_payload(SealedBlock::new_unhashed(block))
            }
        };

        info!(
            target: "smartnet::cli",
            event = "ENGINE_NEW_PAYLOAD_RECEIVED",
            number = payload.block_number(),
            parent_hash = %payload.parent_hash(),
            block_hash = %payload.block_hash(),
            txs = payload.transaction_count(),
            gas_used = payload.gas_used(),
            timestamp = payload.timestamp(),
            "engine newPayload execution payload"
        );

        let (status, timings) = self
            .beacon_engine_handle
            .new_payload(payload, wait_for_persistence, wait_for_caches)
            .await
            .map_err(EngineApiError::from)?;
        info!(
            target: "smartnet::cli",
            event = "ENGINE_NEW_PAYLOAD_RESULT",
            status = ?status.status,
            validation_error = status.status.validation_error().unwrap_or(""),
            latency_us = timings.latency.as_micros(),
            persistence_wait_us = timings.persistence_wait.as_micros(),
            execution_cache_wait_us = ?timings.execution_cache_wait,
            sparse_trie_wait_us = ?timings.sparse_trie_wait,
            latency_wall = %fmt_wall(timings.latency),
            queue_wait_wall = %fmt_wall(timings.queue_wait),
            persistence_wait_wall = %fmt_wall(timings.persistence_wait),
            execution_cache_wait_wall = %fmt_wall_opt(timings.execution_cache_wait),
            sparse_trie_wait_wall = %fmt_wall_opt(timings.sparse_trie_wait),
            request_wall = %fmt_wall(handler_start.elapsed()),
            "engine newPayload finished"
        );
        Ok(SmartNetPayloadStatus {
            status,
            latency_us: timings.latency.as_micros() as u64,
            queue_wait_us: Some(timings.queue_wait.as_micros() as u64),
            persistence_wait_us: timings.persistence_wait.as_micros() as u64,
            execution_cache_wait_us: timings.execution_cache_wait.map(|d| d.as_micros() as u64),
            sparse_trie_wait_us: timings.sparse_trie_wait.map(|d| d.as_micros() as u64),
        })
    }

    /// Transitional FCU bridge — forwards to the consensus engine handle.
    pub async fn forkchoice_updated(
        &self,
        forkchoice_state: ForkchoiceState,
    ) -> RpcResult<ForkchoiceUpdated> {
        trace!(target: "rpc::engine", "Serving engine forkchoiceUpdated");
        self.beacon_engine_handle
            .fork_choice_updated(forkchoice_state, None)
            .await
            .map_err(|e| EngineApiError::from(e).into())
    }
}

/// Merge `wem_*` and canonical `smartnet_*` engine extension methods into `module`.
///
/// **Policy:** one [`EngineExtensionsApi`] instance; both jsonrpsee servers call
/// [`EngineExtensionsApi::new_payload`] (see `docs/smartnet/OWNERSHIP-BOUNDARIES.md`).
pub fn merge_engine_extension_rpc<Payload>(
    module: &mut RpcModule<()>,
    beacon_engine_handle: ConsensusEngineHandle<Payload>,
) -> Result<(), RegisterMethodError>
where
    Payload: PayloadTypes,
    Payload::ExecutionData: ExecutionPayload,
{
    let api = EngineExtensionsApi::new(beacon_engine_handle);
    module.merge(WemEngineApiServer::into_rpc(api.clone()).remove_context())?;
    module.merge(SmartnetEngineApiServer::into_rpc(api).remove_context())?;
    Ok(())
}

macro_rules! delegate_engine_extension_trait {
    ($trait:ident, $new_payload:ident, $fcu:ident) => {
        #[async_trait]
        impl<Payload> $trait<Payload::ExecutionData> for EngineExtensionsApi<Payload>
        where
            Payload: PayloadTypes,
            Payload::ExecutionData: ExecutionPayload,
        {
            async fn $new_payload(
                &self,
                input: SmartNetNewPayloadInput<Payload::ExecutionData>,
                wait_for_persistence: Option<bool>,
                wait_for_caches: Option<bool>,
            ) -> RpcResult<SmartNetPayloadStatus> {
                self.new_payload(input, wait_for_persistence, wait_for_caches).await
            }

            async fn $fcu(
                &self,
                forkchoice_state: ForkchoiceState,
            ) -> RpcResult<ForkchoiceUpdated> {
                self.forkchoice_updated(forkchoice_state).await
            }
        }
    };
}

delegate_engine_extension_trait!(WemEngineApiServer, wem_new_payload, wem_forkchoice_updated);
delegate_engine_extension_trait!(
    SmartnetEngineApiServer,
    smartnet_new_payload,
    smartnet_forkchoice_updated
);
