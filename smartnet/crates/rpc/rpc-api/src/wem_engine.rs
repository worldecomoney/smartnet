//! `wem_*` engine API extensions (operator-compat aliases of canonical `SmartNet` types).

use crate::smartnet_engine::{SmartNetNewPayloadInput, SmartNetPayloadStatus};
use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

/// `wem_*` engine namespace (`wem_newPayload` / `wem_forkchoiceUpdated`).
///
/// Types are [`SmartNetNewPayloadInput`] / [`SmartNetPayloadStatus`]; JSON-RPC wire format
/// unchanged.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "wem"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "wem"))]
pub trait WemEngineApi<ExecutionData> {
    /// `wem_newPayload` — same semantics as [`SmartnetEngineApi::smartnet_new_payload`].
    #[method(name = "newPayload")]
    async fn wem_new_payload(
        &self,
        payload: SmartNetNewPayloadInput<ExecutionData>,
        wait_for_persistence: Option<bool>,
        wait_for_caches: Option<bool>,
    ) -> RpcResult<SmartNetPayloadStatus>;

    /// `wem_forkchoiceUpdated` — forkchoice update with no payload attributes.
    #[method(name = "forkchoiceUpdated")]
    async fn wem_forkchoice_updated(
        &self,
        forkchoice_state: ForkchoiceState,
    ) -> RpcResult<ForkchoiceUpdated>;
}
