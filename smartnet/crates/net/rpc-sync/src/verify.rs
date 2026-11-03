//! Parent-hash chain verification of fetched blocks.
//!
//! Every batch is checked for: sequential numbers, a locally recomputed header hash
//! matching the peer-claimed block hash, `parentHash` linkage to the previous block (or to
//! the trusted anchor hash for the first block of the first batch), and — for the first
//! batch — the block-0 header shape. This is the transport-level integrity check;
//! *consensus* validity (seal recovery against the genesis signer set, execution) happens
//! at import in the node — see `doc/PROJECT.md` — SYNC DESIGN.

use crate::fetch::RpcBlock;
use alloy_primitives::{B256, B64};

/// Chain-verification failure with enough context to diagnose and resume.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    /// Block numbers are not consecutive.
    #[error("block {number} breaks number sequence: expected {expected}, got {actual}")]
    Number {
        /// Number of the offending block.
        number: u64,
        /// Number the sequence required at this position.
        expected: u64,
        /// Number the peer actually served.
        actual: u64,
    },
    /// The recomputed header hash differs from the hash the peer claimed.
    #[error("block {number} hash mismatch: header hashes to {recomputed}, peer claims {claimed}")]
    Hash {
        /// Number of the offending block.
        number: u64,
        /// Hash we computed from the fetched header.
        recomputed: B256,
        /// Hash the peer reported for this block.
        claimed: B256,
    },
    /// `parentHash` does not link to the previous block (or to the trusted anchor).
    #[error("block {number} parent hash mismatch: expected {expected}, got {actual}")]
    Parent {
        /// Number of the offending block.
        number: u64,
        /// Hash of the preceding trusted block.
        expected: B256,
        /// `parentHash` as served by the peer.
        actual: B256,
    },
}

/// A fetched header does not match the chain's header shape. This is a *protocol-era*
/// mismatch, not an encoding bug: Istanbul and London both have correct RLP encodings,
/// they just apply to different eras of the chain.
/// See `doc/istanbul-london-header-rlp.md`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShapeError {
    /// `baseFeePerGas` is present but not the `SmartNet` fixed fee (or missing where required).
    #[error("block {number}: `baseFeePerGas` must be absent (Istanbul era) or the SmartNet fixed fee {SMARTNET_FIXED_GAS_PRICE_WEI}; got {base_fee:?}")]
    BaseFee {
        /// Number of the offending block.
        number: u64,
        /// `baseFeePerGas` as served by the peer.
        base_fee: Option<u64>,
    },
    /// `nonce` is not the block-0 constant zero.
    #[error("block {number}: `nonce` must be zero (block-0 constant); got {nonce:?}")]
    Nonce {
        /// Number of the offending block.
        number: u64,
        /// `nonce` as served by the peer.
        nonce: B64,
    },
    /// `mixHash` is not the block-0 constant zero.
    #[error("block {number}: `mixHash` must be zero (block-0 constant); got {mix_hash:?}")]
    MixHash {
        /// Number of the offending block.
        number: u64,
        /// `mixHash` as served by the peer.
        mix_hash: B256,
    },
}

/// `SmartNet` fixed base fee (wei) — the protocol constant, identical on every SmartNet-era
/// block. Single source: `smartnet-chainspec::fixed_fee` (same definition the consensus
/// layer seals and the fee API quotes), ≈ 238.1 gwei.
pub use smartnet_chainspec::SMARTNET_FIXED_GAS_PRICE_WEI;

/// Verify a batch of blocks continues the chain from `expected_parent` (hash of the last
/// trusted block, i.e. the common ancestor or the previous batch's tip). Blocks must arrive
/// in order; numbers must start at `expected_parent_number + 1`.
///
/// The header hash is recomputed locally (`hash_slow`) and must match the peer-claimed
/// `hash`: the peer cannot forge `parentHash` linkage by lying about a block's own hash.
/// The recomputed hash is what anchors the next block in the batch and what `batch_tip`
/// hands on to the following batch.
pub fn verify_batch(
    expected_parent_number: u64,
    expected_parent: B256,
    blocks: &[RpcBlock],
) -> Result<(), ChainError> {
    let (mut prev_number, mut prev_hash) = (expected_parent_number, expected_parent);
    for block in blocks {
        let header = &block.header;
        if header.number != prev_number + 1 {
            return Err(ChainError::Number {
                number: header.number,
                expected: prev_number + 1,
                actual: header.number,
            });
        }
        let recomputed = header.inner.hash_slow();
        if recomputed != header.hash {
            return Err(ChainError::Hash {
                number: header.number,
                recomputed,
                claimed: header.hash,
            });
        }
        if header.parent_hash != prev_hash {
            return Err(ChainError::Parent {
                number: header.number,
                expected: prev_hash,
                actual: header.parent_hash,
            });
        }
        prev_number = header.number;
        prev_hash = recomputed;
    }
    Ok(())
}

/// Verify fetched headers match the chain's header shape.
///
/// The chain is signer-sealed over two eras, both legitimate on this network:
///   * Istanbul-shaped headers (15 RLP fields, **no** `baseFeePerGas`) — chain history before the
///     `SmartNet` era;
///   * SmartNet-era headers carrying the fixed fee `SMARTNET_FIXED_GAS_PRICE_WEI` (the validator
///     seals `child_base_fee_per_gas` = that constant on **every** block past genesis — see
///     `calibri-lite-consensus` `runtime_profile.rs`).
///
/// Across both eras the seal constants hold: zero `nonce`, zero `mixHash`. Any other
/// `baseFeePerGas` value, or a non-zero `nonce`/`mixHash`, is rejected.
pub fn verify_block0_shape(template: &RpcBlock, blocks: &[RpcBlock]) -> Result<(), ShapeError> {
    let t = &template.header;
    // The template (block 0) itself must be shape-valid: seal constants plus a base fee
    // that is either absent or the fixed SmartNet fee. Otherwise the peer is serving a
    // different protocol era entirely.
    if let Some(fee) = t.base_fee_per_gas
        && fee != SMARTNET_FIXED_GAS_PRICE_WEI {
            return Err(ShapeError::BaseFee { number: 0, base_fee: t.base_fee_per_gas });
        }
    if t.nonce != B64::ZERO {
        return Err(ShapeError::Nonce { number: 0, nonce: t.nonce });
    }
    if t.mix_hash != B256::ZERO {
        return Err(ShapeError::MixHash { number: 0, mix_hash: t.mix_hash });
    }
    for block in blocks {
        let h = &block.header;
        if h.base_fee_per_gas.is_some() && h.base_fee_per_gas != Some(SMARTNET_FIXED_GAS_PRICE_WEI)
        {
            return Err(ShapeError::BaseFee { number: h.number, base_fee: h.base_fee_per_gas });
        }
        if h.nonce != B64::ZERO {
            return Err(ShapeError::Nonce { number: h.number, nonce: h.nonce });
        }
        if h.mix_hash != B256::ZERO {
            return Err(ShapeError::MixHash { number: h.number, mix_hash: h.mix_hash });
        }
    }
    Ok(())
}

/// Convenience: hash of the last block in a verified batch (next batch's anchor).
pub fn batch_tip(blocks: &[RpcBlock]) -> Option<(u64, B256)> {
    blocks.last().map(|b| (b.header.number, b.header.hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_rpc_types_eth::Block;

    fn block(number: u64, parent: B256) -> RpcBlock {
        let header = Header { number, parent_hash: parent, ..Default::default() };
        Block { header: alloy_rpc_types_eth::Header::new(header), ..Default::default() }
    }

    fn chain3(start: u64, parent: B256) -> Vec<RpcBlock> {
        let b1 = block(start, parent);
        let b2 = block(start + 1, b1.header.hash);
        let b3 = block(start + 2, b2.header.hash);
        vec![b1, b2, b3]
    }

    #[test]
    fn verifies_contiguous_batch() {
        let anchor = B256::from([9u8; 32]);
        let blocks = chain3(10, anchor);
        assert!(verify_batch(9, anchor, &blocks).is_ok());
        assert_eq!(batch_tip(&blocks), Some((12, blocks[2].header.hash)));
    }

    #[test]
    fn rejects_parent_gap() {
        let anchor = B256::from([9u8; 32]);
        let mut blocks = chain3(10, anchor);
        // Forge the linkage: point b3 at a decoy parent *and* refresh its claimed hash so
        // the header self-hashes consistently — the parent-anchor check must still catch it.
        blocks[2].header.inner.parent_hash = B256::ZERO;
        blocks[2].header.hash = blocks[2].header.inner.hash_slow();
        assert!(matches!(
            verify_batch(9, anchor, &blocks),
            Err(ChainError::Parent { number: 12, .. })
        ));
    }

    #[test]
    fn rejects_claimed_hash_mismatch() {
        // A peer claiming a hash that does not hash-recompose to the header it served is
        // forging chain linkage — caught before any parent check is applied.
        let anchor = B256::from([9u8; 32]);
        let mut blocks = chain3(10, anchor);
        blocks[2].header.hash = B256::ZERO; // stale/forged claimed hash
        assert!(matches!(
            verify_batch(9, anchor, &blocks),
            Err(ChainError::Hash { number: 12, .. })
        ));
    }

    #[test]
    fn rejects_number_gap() {
        let anchor = B256::from([9u8; 32]);
        let blocks = chain3(11, anchor); // first block skips 10
        assert!(matches!(
            verify_batch(9, anchor, &blocks),
            Err(ChainError::Number { expected: 10, actual: 11, .. })
        ));
    }

    #[test]
    fn empty_batch_is_ok() {
        assert!(verify_batch(5, B256::ZERO, &[]).is_ok());
        assert_eq!(batch_tip(&[]), None);
    }

    // ---- header-shape verification (Istanbul + SmartNet eras) ----

    fn istanbul_block(number: u64, parent: B256) -> RpcBlock {
        // Default consensus header: base_fee None, zero nonce, zero mixHash — the
        // Istanbul/sealed shape (canonical block 0; chain history before the SmartNet era).
        let header = Header { number, parent_hash: parent, ..Default::default() };
        Block { header: alloy_rpc_types_eth::Header::new(header), ..Default::default() }
    }

    fn smartnet_block(number: u64, parent: B256) -> RpcBlock {
        // SmartNet-era header: fixed protocol base fee, seal constants preserved.
        let mut block = istanbul_block(number, parent);
        block.header.inner.base_fee_per_gas = Some(SMARTNET_FIXED_GAS_PRICE_WEI);
        block.header.hash = block.header.inner.hash_slow();
        block
    }

    #[test]
    fn block0_shape_accepts_istanbul_headers() {
        let template = istanbul_block(0, B256::ZERO);
        let b1 = istanbul_block(1, template.header.hash);
        let b2 = istanbul_block(2, b1.header.hash);
        assert!(verify_block0_shape(&template, &[b1, b2]).is_ok());
    }

    #[test]
    fn block0_shape_accepts_smartnet_era_fixed_fee() {
        // The validator seals the fixed fee on every block past genesis — a SmartNet-era
        // header must pass the shape check (the historical gate no longer blocks it).
        let template = istanbul_block(0, B256::ZERO);
        let b1 = smartnet_block(1, template.header.hash);
        let b2 = smartnet_block(2, b1.header.hash);
        assert!(verify_block0_shape(&template, &[b1, b2]).is_ok());
    }

    #[test]
    fn block0_shape_rejects_foreign_base_fee() {
        // `baseFeePerGas` must be absent or the fixed SmartNet fee — any other value is a
        // corrupt/foreign header, caught per block.
        let template = istanbul_block(0, B256::ZERO);
        let mut london = istanbul_block(1, template.header.hash);
        london.header.inner.base_fee_per_gas = Some(1_000_000_000);
        assert_eq!(
            verify_block0_shape(&template, &[london]),
            Err(ShapeError::BaseFee { number: 1, base_fee: Some(1_000_000_000) })
        );
    }

    #[test]
    fn block0_shape_rejects_nonzero_nonce() {
        let template = istanbul_block(0, B256::ZERO);
        let mut bad = istanbul_block(1, template.header.hash);
        bad.header.inner.nonce = B64::from([1u8; 8]);
        assert!(matches!(verify_block0_shape(&template, &[bad]), Err(ShapeError::Nonce { .. })));
    }

    #[test]
    fn block0_shape_rejects_nonzero_mix_hash() {
        let template = istanbul_block(0, B256::ZERO);
        let mut bad = istanbul_block(1, template.header.hash);
        bad.header.inner.mix_hash = B256::repeat_byte(0xEE);
        assert!(matches!(verify_block0_shape(&template, &[bad]), Err(ShapeError::MixHash { .. })));
    }

    #[test]
    fn block0_shape_rejects_foreign_shaped_template() {
        // Block 0 with a base fee that is neither absent nor the fixed SmartNet fee is a
        // different protocol era entirely.
        let mut template = istanbul_block(0, B256::ZERO);
        template.header.inner.base_fee_per_gas = Some(7);
        assert!(matches!(verify_block0_shape(&template, &[]), Err(ShapeError::BaseFee { .. })));
    }
}
