//! SmartNet-specific engine API extensions (dual-published with `wem_*` names).

pub use smartnet_payload_types::{SmartNetNewPayloadInput, SmartNetPayloadStatus};

use alloy_rpc_types_engine::{ForkchoiceState, ForkchoiceUpdated};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

/// SmartNet engine namespace — same semantics as [`crate::wem_engine::WemEngineApi`].
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "smartnet"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "smartnet"))]
pub trait SmartnetEngineApi<ExecutionData> {
    /// Canonical `SmartNet` seal/import path (`smartnet_newPayload`; `wem_newPayload` alias).
    #[method(name = "newPayload")]
    async fn smartnet_new_payload(
        &self,
        payload: SmartNetNewPayloadInput<ExecutionData>,
        wait_for_persistence: Option<bool>,
        wait_for_caches: Option<bool>,
    ) -> RpcResult<SmartNetPayloadStatus>;

    /// Diagnostic forkchoice (no payload attributes); `wem_forkchoiceUpdated` alias kept for
    /// tooling parity.
    #[method(name = "forkchoiceUpdated")]
    async fn smartnet_forkchoice_updated(
        &self,
        forkchoice_state: ForkchoiceState,
    ) -> RpcResult<ForkchoiceUpdated>;
}
