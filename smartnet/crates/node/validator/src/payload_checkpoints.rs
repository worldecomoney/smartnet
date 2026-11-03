//! Deterministic hash checkpoints and local RLP roundtrip validation before `wem_newPayload`.

use alloy_consensus::{Block, Header, TxEnvelope};
use alloy_primitives::{keccak256, Address, B256};
use alloy_rlp::Decodable;
use alloy_rpc_types_engine::{ExecutionPayload, ExecutionPayloadSidecar, ExecutionPayloadV1};
use calibri_lite_consensus::pre_consensus::{
    calibri_lite_seal_hash, recover_calibri_lite_signer, try_into_block_with_sidecar,
};
use eyre::{ensure, Result, WrapErr};
use tracing::debug;

/// All hashes captured immediately before `wem_newPayload`.
#[derive(Debug, Clone)]
pub(crate) struct PayloadCheckpoints {
    pub pre_seal_hash: B256,
    pub post_seal_hash: B256,
    pub recovered_signer: Address,
    pub header_hash_before_rlp: B256,
    pub header_hash_after_roundtrip: B256,
    pub rlp_keccak: B256,
    /// Hash placed in the execution payload envelope (node `got` / consensus side).
    pub simulated_envelope_block_hash: B256,
}

pub(crate) fn log_pre_seal(header: &Header) -> Result<B256> {
    let h = calibri_lite_seal_hash(header)?;
    debug!(
        target: "smartnet_validator",
        event = "CALIBRI_LITE_PAYLOAD_CHECKPOINT_PRE_SEAL",
        seal_hash = %h,
        number = header.number,
        parent_hash = %header.parent_hash,
        extra_data_len = header.extra_data.len(),
        "hash checkpoint: seal digest before ECDSA"
    );
    Ok(h)
}

pub(crate) fn log_post_seal(header: &Header, expect_signer: Address) -> Result<(B256, Address)> {
    let h = calibri_lite_seal_hash(header)?;
    let recovered = recover_calibri_lite_signer(header)?;
    ensure!(
        recovered == expect_signer,
        "post-seal recovered signer {recovered} != key {expect_signer}"
    );
    debug!(
        target: "smartnet_validator",
        event = "CALIBRI_LITE_PAYLOAD_CHECKPOINT_POST_SEAL",
        seal_hash = %h,
        recovered_signer = %recovered,
        header_hash_slow = %header.hash_slow(),
        "hash checkpoint: after seal — seal hash unchanged, header hash_slow available"
    );
    Ok((h, recovered))
}

/// Encode → decode → re-hash; simulate node envelope hash; fail if inconsistent.
pub(crate) fn validate_final_payload(
    sealed: &Block<TxEnvelope>,
    rlp: &[u8],
    pre_seal_hash: B256,
    post_seal_hash: B256,
    recovered_signer: Address,
) -> Result<PayloadCheckpoints> {
    ensure!(
        pre_seal_hash == post_seal_hash,
        "seal hash changed between pre-sign and post-sign (header mutated after seal?)"
    );

    ensure!(
        sealed.header.state_root != B256::ZERO,
        "state_root is zero — eth_simulateV1 must return execution-derived roots"
    );

    let header_hash_before_rlp = sealed.header.hash_slow();
    let rlp_keccak = keccak256(rlp);

    let decoded: Block<TxEnvelope> =
        Decodable::decode(&mut &rlp[..]).wrap_err("local RLP roundtrip decode")?;
    let header_hash_after_roundtrip = decoded.header.hash_slow();

    ensure!(
        header_hash_before_rlp == header_hash_after_roundtrip,
        "RLP roundtrip header hash mismatch: before={header_hash_before_rlp} after={header_hash_after_roundtrip}"
    );
    ensure!(
        decoded.header.extra_data == sealed.header.extra_data,
        "RLP roundtrip extra_data mismatch"
    );
    ensure!(
        decoded.body.transactions.len() == sealed.body.transactions.len(),
        "RLP roundtrip tx count mismatch"
    );

    let v1 = ExecutionPayloadV1::from_block_unchecked(header_hash_before_rlp, sealed);
    let simulated_envelope_block_hash = v1.block_hash;
    let roundtrip: Block<TxEnvelope> =
        try_into_block_with_sidecar(ExecutionPayload::V1(v1), &ExecutionPayloadSidecar::none())
            .wrap_err("Calibri Lite payload roundtrip (node ensure_well_formed_payload path)")?;
    let node_want_hash = roundtrip.header.hash_slow();

    ensure!(
        simulated_envelope_block_hash == header_hash_before_rlp,
        "execution payload block_hash {} != sealed header hash_slow {}",
        simulated_envelope_block_hash,
        header_hash_before_rlp
    );
    ensure!(
        node_want_hash == header_hash_before_rlp,
        "payload roundtrip header hash {} != sealed header {} (node would report block hash mismatch: want {}, got {})",
        node_want_hash,
        header_hash_before_rlp,
        header_hash_before_rlp,
        simulated_envelope_block_hash
    );

    let cp = PayloadCheckpoints {
        pre_seal_hash,
        post_seal_hash,
        recovered_signer,
        header_hash_before_rlp,
        header_hash_after_roundtrip,
        rlp_keccak,
        simulated_envelope_block_hash,
    };

    debug!(
        target: "smartnet_validator",
        event = "CALIBRI_LITE_PAYLOAD_CHECKPOINT",
        pre_seal_hash = %cp.pre_seal_hash,
        post_seal_hash = %cp.post_seal_hash,
        recovered_signer = %cp.recovered_signer,
        header_hash_slow = %cp.header_hash_before_rlp,
        header_hash_after_roundtrip = %cp.header_hash_after_roundtrip,
        rlp_keccak = %cp.rlp_keccak,
        simulated_envelope_block_hash = %cp.simulated_envelope_block_hash,
        state_root = %sealed.header.state_root,
        receipts_root = %sealed.header.receipts_root,
        transactions_root = %sealed.header.transactions_root,
        rlp_len = rlp.len(),
        "hash checkpoint: local roundtrip OK; envelope hash matches header (node want == got)"
    );

    Ok(cp)
}
