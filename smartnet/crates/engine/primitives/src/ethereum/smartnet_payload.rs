//! `SmartNet` engine payload helpers — honest `baseFeePerGas` in the V1 envelope (no field stash).

use alloy_rpc_types_engine::ExecutionPayloadV1;

/// Read wallet-visible `baseFeePerGas` from an execution payload (zero → `None`).
pub fn base_fee_from_payload_v1(v1: &ExecutionPayloadV1) -> Option<u64> {
    if v1.base_fee_per_gas.is_zero() {
        return None;
    }
    v1.base_fee_per_gas.try_into().ok()
}
