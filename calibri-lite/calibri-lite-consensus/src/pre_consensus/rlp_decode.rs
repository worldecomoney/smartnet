//! Pre-consensus execution payload → block decode (long `extraData`; honest `baseFeePerGas`).

use alloy_consensus::{Block, BlockBody, EMPTY_OMMER_ROOT_HASH, Header, proofs};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{B256, Bytes, U256, keccak256};
use alloy_rpc_types_engine::{
    ExecutionPayload, ExecutionPayloadSidecar, ExecutionPayloadV1, PayloadError,
};

fn base_fee_from_payload_v1(v1: &ExecutionPayloadV1) -> Option<u64> {
    if v1.base_fee_per_gas.is_zero() {
        return None;
    }
    v1.base_fee_per_gas.try_into().ok()
}

fn v1_into_block_raw(
    payload: ExecutionPayloadV1,
    transactions_root: Option<B256>,
) -> Result<Block<Bytes>, PayloadError> {
    let transactions_root = transactions_root
        .unwrap_or_else(|| proofs::ordered_trie_root_encoded(&payload.transactions));

    let header = Header {
        parent_hash: payload.parent_hash,
        beneficiary: payload.fee_recipient,
        state_root: payload.state_root,
        transactions_root,
        receipts_root: payload.receipts_root,
        withdrawals_root: None,
        logs_bloom: payload.logs_bloom,
        number: payload.block_number,
        gas_limit: payload.gas_limit,
        gas_used: payload.gas_used,
        timestamp: payload.timestamp,
        // Engine-API convention: payload.prev_randao IS the header mixHash. Genesis
        // (block 0) is seal-compatible and never passes through this payload path;
        // only blocks 1+ (post-London) are reconstructed here.
        mix_hash: payload.prev_randao,
        base_fee_per_gas: base_fee_from_payload_v1(&payload),
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: None,
        requests_hash: None,
        block_access_list_hash: None,
        slot_number: None,
        extra_data: payload.extra_data,
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        // Chain convention (sealed lineage, single authorized signer): difficulty is
        // ALWAYS 2 (in-turn). Uniform across the existing chain (verified blocks 1..1M+)
        // and on SmartNet-sealed blocks, so payload→header reconstruction is exact.
        difficulty: U256::from(2),
        nonce: Default::default(),
    };

    Ok(Block {
        header,
        body: BlockBody {
            transactions: payload.transactions,
            ommers: vec![],
            withdrawals: None,
        },
    })
}

/// [`ExecutionPayload`] → raw block, preserving Calibri Lite-length `extraData`.
pub fn into_block_raw(payload: ExecutionPayload) -> Result<Block<Bytes>, PayloadError> {
    match payload {
        ExecutionPayload::V1(p) => v1_into_block_raw(p, None),
        ExecutionPayload::V2(p) => {
            let mut base = v1_into_block_raw(p.payload_inner, None)?;
            let withdrawals_root = proofs::calculate_withdrawals_root(&p.withdrawals);
            base.body.withdrawals = Some(p.withdrawals.into());
            base.header.withdrawals_root = Some(withdrawals_root);
            Ok(base)
        }
        ExecutionPayload::V3(p) => {
            let mut base = v1_into_block_raw(p.payload_inner.payload_inner, None)?;
            let withdrawals_root = proofs::calculate_withdrawals_root(&p.payload_inner.withdrawals);
            base.body.withdrawals = Some(p.payload_inner.withdrawals.into());
            base.header.withdrawals_root = Some(withdrawals_root);
            base.header.blob_gas_used = Some(p.blob_gas_used);
            base.header.excess_blob_gas = Some(p.excess_blob_gas);
            Ok(base)
        }
        ExecutionPayload::V4(p) => {
            let mut base = v1_into_block_raw(p.payload_inner.payload_inner.payload_inner, None)?;
            let withdrawals_root =
                proofs::calculate_withdrawals_root(&p.payload_inner.payload_inner.withdrawals);
            base.body.withdrawals = Some(p.payload_inner.payload_inner.withdrawals.into());
            base.header.withdrawals_root = Some(withdrawals_root);
            base.header.blob_gas_used = Some(p.payload_inner.blob_gas_used);
            base.header.excess_blob_gas = Some(p.payload_inner.excess_blob_gas);
            base.header.block_access_list_hash = Some(keccak256(&p.block_access_list));
            base.header.slot_number = Some(p.slot_number);
            Ok(base)
        }
    }
}

/// [`ExecutionPayload`] + sidecar → typed block (long `extraData` allowed).
pub fn try_into_block_with_sidecar<T: Decodable2718>(
    payload: ExecutionPayload,
    sidecar: &ExecutionPayloadSidecar,
) -> Result<Block<T>, PayloadError> {
    let mut block = into_block_raw(payload)?;
    block.header.parent_beacon_block_root = sidecar.parent_beacon_block_root();
    block.header.requests_hash = sidecar.requests_hash();
    block
        .try_map_transactions(|tx| {
            T::decode_2718_exact(tx.as_ref())
                .map_err(alloy_rlp::Error::from)
                .map_err(PayloadError::from)
        })
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn zero_v1() -> ExecutionPayloadV1 {
        ExecutionPayloadV1 {
            base_fee_per_gas: U256::ZERO,
            extra_data: Bytes::new(),
            fee_recipient: Address::ZERO,
            gas_limit: 0,
            gas_used: 0,
            logs_bloom: Default::default(),
            parent_hash: B256::ZERO,
            receipts_root: B256::ZERO,
            state_root: B256::ZERO,
            timestamp: 0,
            transactions: vec![],
            block_number: 0,
            prev_randao: B256::ZERO,
            block_hash: B256::ZERO,
        }
    }

    #[test]
    fn base_fee_zero_returns_none() {
        let v1 = zero_v1();
        assert!(base_fee_from_payload_v1(&v1).is_none());
    }

    #[test]
    fn base_fee_valid_value_returns_some() {
        let mut v1 = zero_v1();
        v1.base_fee_per_gas = U256::from(238_095_238_095u64);
        assert_eq!(base_fee_from_payload_v1(&v1), Some(238_095_238_095));
    }

    #[test]
    fn v1_into_block_raw_sets_difficulty_2() {
        let mut v1 = zero_v1();
        v1.block_number = 1;
        let block = v1_into_block_raw(v1, None).unwrap();
        assert_eq!(block.header.difficulty, U256::from(2));
    }
}
