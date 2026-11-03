//! Wire-shape guardrails for full-block RLP before `wem_newPayload`.

use alloy_rlp::{Header, PayloadView};
use tracing::warn;

/// Warn if `block_rlp` does not look like a canonical execution-layer **full block**
/// (outer RLP list whose first child is a **nested** header list).
pub(crate) fn warn_block_rlp_wire_envelope(block_rlp: &[u8]) {
    let mut buf = block_rlp;
    match Header::decode_raw(&mut buf) {
        Ok(PayloadView::List(items)) => {
            let n = items.len();
            if !(2..=4).contains(&n) {
                warn!(
                    target: "smartnet_validator",
                    event = "BLOCK_RLP_TOP_COUNT_UNUSUAL",
                    top_level_children = n,
                    "expected 2..=4 top-level RLP items for a full execution block"
                );
            }
            if let Some(first) = items.first().copied() {
                let mut head = first;
                match Header::decode(&mut head) {
                    Ok(h) if !h.list => {
                        warn!(
                            target: "smartnet_validator",
                            event = "BLOCK_RLP_FIRST_CHILD_NOT_HEADER_LIST",
                            top_level_children = n,
                            first_child_len = first.len(),
                            "first top-level RLP item should be a nested header list"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(
                            target: "smartnet_validator",
                            event = "BLOCK_RLP_FIRST_CHILD_DECODE_ERR",
                            error = %e,
                            "could not read RLP header of first top-level child"
                        );
                    }
                }
            } else {
                warn!(
                    target: "smartnet_validator",
                    event = "BLOCK_RLP_TOP_EMPTY",
                    "top-level block RLP list has zero children"
                );
            }
        }
        Ok(PayloadView::String(_)) => {
            warn!(
                target: "smartnet_validator",
                event = "BLOCK_RLP_TOP_NOT_LIST",
                "full block RLP must be a top-level list"
            );
        }
        Err(e) => {
            warn!(
                target: "smartnet_validator",
                event = "BLOCK_RLP_TOP_DECODE_ERR",
                error = %e,
                "could not decode top-level block RLP"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard only ever warns — it must never panic, whatever the wire bytes look like.
    /// Each case drives a different branch of the decoder.
    #[test]
    fn warn_block_rlp_never_panics_on_varied_input() {
        // Empty input → top-level decode error.
        warn_block_rlp_wire_envelope(&[]);
        // `0xc0` = empty list → zero top-level children.
        warn_block_rlp_wire_envelope(&[0xc0]);
        // `0xc1 0x01` = one-item list whose single child is not a nested header list.
        warn_block_rlp_wire_envelope(&[0xc1, 0x01]);
        // `0x83 "abc"` = a top-level RLP string, not a list.
        warn_block_rlp_wire_envelope(&[0x83, b'a', b'b', b'c']);
        // `0xf5` + 5 single-byte items → five top-level children (outside the expected 2..=4).
        warn_block_rlp_wire_envelope(&[0xf5, 0x01, 0x02, 0x03, 0x04, 0x05]);
        // Truncated list header → decode error.
        warn_block_rlp_wire_envelope(&[0xc2]);
    }
}
