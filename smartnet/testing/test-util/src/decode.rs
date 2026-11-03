//! Decode block RLP from `.dat` bytes and report hashes for node-log correlation.

use alloy_consensus::{Block, TxEnvelope};
use alloy_primitives::{keccak256, B256};
use alloy_rlp::Decodable;
use eyre::{Context, Result};

/// Hashes derived from a block RLP blob.
#[derive(Debug, Clone)]
pub(crate) struct BlockHashReport {
    pub header_hash_slow: B256,
    pub rlp_keccak: B256,
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub number: u64,
    pub extra_data_len: usize,
}

pub(crate) fn inspect_block_rlp(rlp: &[u8]) -> Result<BlockHashReport> {
    let block: Block<TxEnvelope> =
        Decodable::decode(&mut &rlp[..]).context("decode block RLP (execution block envelope)")?;
    Ok(BlockHashReport {
        header_hash_slow: block.header.hash_slow(),
        rlp_keccak: keccak256(rlp),
        state_root: block.header.state_root,
        transactions_root: block.header.transactions_root,
        receipts_root: block.header.receipts_root,
        number: block.header.number,
        extra_data_len: block.header.extra_data.len(),
    })
}
