//! Shared wire types for `smartnet_newPayload` and legacy `wem_newPayload`.

use alloy_primitives::Bytes;
use alloy_rpc_types_engine::PayloadStatus;
use serde::{Deserialize, Serialize};

/// Payload status for `smartnet_newPayload` / legacy `wem_newPayload`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartNetPayloadStatus {
    /// The standard payload status.
    #[serde(flatten)]
    pub status: PayloadStatus,
    /// Server-side execution latency in microseconds.
    pub latency_us: u64,
    /// Time the message spent queued behind engine backpressure, in microseconds
    /// (enqueue → engine dequeue; before any validation ran).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_wait_us: Option<u64>,
    /// Time spent waiting for in-flight persistence to complete, in microseconds
    /// (`0` when `wait_for_persistence` was not requested).
    pub persistence_wait_us: u64,
    /// Time spent waiting for the execution cache lock, in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_cache_wait_us: Option<u64>,
    /// Time spent waiting for the sparse trie lock, in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sparse_trie_wait_us: Option<u64>,
}

/// Input for `smartnet_newPayload`: `ExecutionData` or RLP `BlockRlp`.
///
/// Untagged: serde tries `ExecutionData` (JSON object) first, then `BlockRlp` (hex bytes).
/// Input matching neither fails to decode, so a malformed payload is rejected at the
/// deserialization boundary rather than reaching the handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SmartNetNewPayloadInput<ExecutionData> {
    /// Standard execution data (payload + sidecar).
    ExecutionData(ExecutionData),
    /// An RLP-encoded block.
    BlockRlp(Bytes),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_hex_bytes_as_block_rlp() {
        // With a concrete non-Value type, hex bytes fall through to BlockRlp.
        #[derive(Debug, Deserialize)]
        struct DummyPayload;
        let json = r#""0x01020304""#;
        let input: SmartNetNewPayloadInput<DummyPayload> = serde_json::from_str(json).unwrap();
        assert!(matches!(input, SmartNetNewPayloadInput::BlockRlp(_)));
    }

    #[test]
    fn decode_number_as_execution_data() {
        // Untagged tries ExecutionData first; a bare number matches a u64 payload.
        let json = r#"42"#;
        let input: SmartNetNewPayloadInput<u64> = serde_json::from_str(json).unwrap();
        assert!(matches!(input, SmartNetNewPayloadInput::ExecutionData(42)));
    }

    #[test]
    fn malformed_input_fails_to_decode() {
        // Anything matching neither real variant must be rejected at the deserialization
        // boundary — not silently captured and handed to the handler. (Byte arrays such as
        // `[1, 2, 3]` are valid `BlockRlp`, so only shapes that match neither are listed.)
        for json in [
            r#"true"#,           // bool matches neither
            r#"null"#,           // null matches neither
            r#"[256]"#,          // array element out of u8 range
            r#"{"foo": "bar"}"#, // object that is not a valid ExecutionData
            r#""not-hex""#,      // string that is not 0x-prefixed hex
            r#"3.5"#,            // float matches neither
        ] {
            assert!(
                serde_json::from_str::<SmartNetNewPayloadInput<u64>>(json).is_err(),
                "expected {json} to fail to decode"
            );
        }
    }
}
