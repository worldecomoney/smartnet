//! Payload id: keccak256(raw RLP bytes).

use alloy_primitives::{keccak256, B256};

pub(crate) fn payload_hash(rlp: &[u8]) -> B256 {
    keccak256(rlp)
}
