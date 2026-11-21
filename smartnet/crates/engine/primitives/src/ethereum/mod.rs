//! Ethereum specific engine API types and impls.

mod smartnet_payload;
pub use smartnet_payload::base_fee_from_payload_v1;

mod payload;
pub use payload::{BlobSidecars, EthBuiltPayload};

mod error;
pub use error::*;

use alloy_consensus::Header;
use alloy_rpc_types_engine::{ExecutionData, ExecutionPayload, ExecutionPayloadSidecar};
pub use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3, ExecutionPayloadEnvelopeV4,
    ExecutionPayloadEnvelopeV5, ExecutionPayloadEnvelopeV6, ExecutionPayloadV1,
    PayloadAttributes as EthPayloadAttributes,
};
use calibri_lite_consensus::pre_consensus::recover_calibri_lite_signer;
use smartnet_primitives_traits::{NodePrimitives, SealedBlock};
use crate::EngineTypes;
use smartnet_payload_primitives::{BuiltPayload, PayloadTypes};

/// The types used in the default mainnet ethereum beacon consensus engine.
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct EthEngineTypes<T: PayloadTypes = EthPayloadTypes> {
    _marker: core::marker::PhantomData<T>,
}

impl<
        T: PayloadTypes<
            ExecutionData = ExecutionData,
            BuiltPayload: BuiltPayload<
                Primitives: NodePrimitives<Block = smartnet_ethereum_primitives::Block>,
            >,
        >,
    > PayloadTypes for EthEngineTypes<T>
{
    type ExecutionData = T::ExecutionData;
    type BuiltPayload = T::BuiltPayload;
    type PayloadAttributes = T::PayloadAttributes;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        T::block_to_payload(block)
    }
}

impl<T> EngineTypes for EthEngineTypes<T>
where
    T: PayloadTypes<ExecutionData = ExecutionData>,
    T::BuiltPayload: BuiltPayload<Primitives: NodePrimitives<Block = smartnet_ethereum_primitives::Block>>
        + TryInto<ExecutionPayloadV1>
        + TryInto<ExecutionPayloadEnvelopeV2>
        + TryInto<ExecutionPayloadEnvelopeV3>
        + TryInto<ExecutionPayloadEnvelopeV4>
        + TryInto<ExecutionPayloadEnvelopeV5>
        + TryInto<ExecutionPayloadEnvelopeV6>,
{
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    type ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2;
    type ExecutionPayloadEnvelopeV3 = ExecutionPayloadEnvelopeV3;
    type ExecutionPayloadEnvelopeV4 = ExecutionPayloadEnvelopeV4;
    type ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV5;
    type ExecutionPayloadEnvelopeV6 = ExecutionPayloadEnvelopeV6;
}

/// A default payload type for [`EthEngineTypes`]
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct EthPayloadTypes;

/// Build [`ExecutionData`] from a decoded block RLP (honest header fields in the V1 envelope).
///
/// The envelope stays byte-honest: `fee_recipient` is `header.coinbase` (the clique vote
/// marker, often zero), because the engine's payload→header round-trip verifies
/// `keccak(rlp(reconstructed_header)) == payload.block_hash` — any rewrite breaks the
/// hash gate. The actual fee beneficiary is resolved at execution time via
/// [`execution_beneficiary`].
pub fn execution_data_from_block_rlp(block: smartnet_ethereum_primitives::Block) -> ExecutionData {
    let hash = block.header.hash_slow();
    let v1 = ExecutionPayloadV1::from_block_unchecked(hash, &block);
    let payload = ExecutionPayload::V1(v1);
    let sidecar = ExecutionPayloadSidecar::from_block(&block);
    ExecutionData { payload, sidecar }
}

/// Fee beneficiary for an incoming payload: the seal's author, falling back to
/// `fee_recipient`.
///
/// geth credited transaction fees to `Engine().Author(header)` (`core/state_processor.go`
/// passes `author = nil`, so the beneficiary is clique's `ecrecover` of `extra_data`),
/// never to `header.coinbase`, which clique's `Prepare` zeroes (it only carries votes).
/// Reproducing historical state roots therefore requires crediting the recovered seal
/// signer. The header is reconstructed exactly like the payload hash gate does
/// (`calibri_lite_consensus::pre_consensus::into_block_raw`, chain convention: difficulty
/// 2, empty ommers, zero nonce), so recovery sees the header the seal was signed over.
///
/// On `SmartNet`-sealed blocks the sealer IS `header.coinbase` (one keystore = node ID +
/// coinbase), so live mesh/sync execution resolves to the same value as `fee_recipient`
/// and is unchanged; headers without a recoverable seal keep `fee_recipient`.
pub fn execution_beneficiary(payload: &ExecutionData) -> alloy_primitives::Address {
    let fee_recipient = payload.payload.fee_recipient();
    match calibri_lite_consensus::pre_consensus::into_block_raw(payload.payload.clone()) {
        Ok(block) => execution_beneficiary_for_header(&block.header),
        Err(_) => fee_recipient,
    }
}

/// Same rule for a decoded header, used by historical import (which has the real
/// header in hand rather than an engine payload). Falls back to the header's own
/// beneficiary when no seal can be recovered, so unsigned/legacy-shaped headers
/// keep their previous behavior.
pub fn execution_beneficiary_for_header(header: &Header) -> alloy_primitives::Address {
    recover_calibri_lite_signer(header).unwrap_or(header.beneficiary)
}

impl PayloadTypes for EthPayloadTypes {
    type BuiltPayload = EthBuiltPayload;
    type PayloadAttributes = EthPayloadAttributes;
    type ExecutionData = ExecutionData;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        let hash = block.hash();
        let inner = block.into_block();
        let v1 = ExecutionPayloadV1::from_block_unchecked(hash, &inner);
        let payload = ExecutionPayload::V1(v1);
        let sidecar = ExecutionPayloadSidecar::from_block(&inner);
        ExecutionData { payload, sidecar }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::{address, hex, Address};
    use alloy_rlp::Decodable;

    /// Full block 17214 RLP (framed list item from chaindata.rlp, item 17215): the
    /// chain's first transaction block — 1 tx, zeroed coinbase vote marker,
    /// 97-byte extraData ending in the sealer's 65-byte sig.
    const BLOCK_17214_RLP: &str = concat!(
        "f902d1f9025ba026129d98cbe54047b3b91454bb847222fe924eb07f24e021ea2b75f75a553a96a01dcc4de8dec75d7a",
        "ab85b567b6ccd41ad312451b948a7413f0a142fd40d49347940000000000000000000000000000000000000000a0ce45",
        "6f0573acf30a3e39bd8b538b59eb1214e73d5c439100feee727b9da587baa0b2d845752dd5db9caa99cbb1d2121e31da",
        "b79f5a4b664ff9b325040d632d7d7fa0056b23fbba480696b65fe5a59b8f2148a1299103c4f57df839233af2cf4ca2d2",
        "b90100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        "000000000000000000000000000000000000000282433e8401c9c38082520884684661cab861d883010d0f8467657468",
        "88676f312e32312e33856c696e757800000000000000d8bc0c59c31d12d303642f34bd531fee91cb84a764b27b0e8828",
        "7cc115dbf1c749b2a0a596717e61eb5b0c350ee1258b7584594593a72b9bd083a0ce041e269f00a00000000000000000",
        "000000000000000000000000000000000000000000000000880000000000000000f870f86e80843b9aca008252089455",
        "92a508e36703e84b6fd1b500808e3c79cc5be8890ad78ebc5ac62000008082029aa0c897ad85fc6f46d1dd26f43016c8",
        "86b152d96a6b0e15fd86cea74877bb8962c9a016c7549c8132eea34f1e3184e669d93a77eb5051ec0f8ce9c8aa51889f",
        "cd6923c0"
    );

    fn block_17214() -> ExecutionData {
        let bytes = hex::decode(BLOCK_17214_RLP).unwrap();
        let block: smartnet_ethereum_primitives::Block =
            Decodable::decode(&mut bytes.as_slice()).unwrap();
        execution_data_from_block_rlp(block)
    }

    #[test]
    fn envelope_stays_honest_and_round_trips_block_hash() {
        let data = block_17214();
        // the vote-marker coinbase is preserved: rewriting fee_recipient would break the
        // engine's keccak(rlp(header)) == payload.block_hash gate
        assert_eq!(data.payload.fee_recipient(), Address::ZERO);
        let rebuilt =
            calibri_lite_consensus::pre_consensus::into_block_raw(data.payload.clone()).unwrap();
        assert_eq!(rebuilt.header.hash_slow(), *data.payload.block_hash());
        // the reconstructed header is exactly what the seal was signed over
        let signer = recover_calibri_lite_signer(&rebuilt.header).unwrap();
        assert_eq!(signer, address!("0x24bf191a358c09e79b9ead2d8999d08590b5af31"));
    }

    #[test]
    fn beneficiary_is_recovered_sealer_for_first_tx_block() {
        let data = block_17214();
        // geth credited the fee to the seal's author, not the zeroed coinbase
        assert_eq!(
            execution_beneficiary(&data),
            address!("0x24bf191a358c09e79b9ead2d8999d08590b5af31")
        );
        assert_ne!(execution_beneficiary(&data), data.payload.fee_recipient());

        // the header-based helper used by historical import resolves the same sealer
        let rebuilt =
            calibri_lite_consensus::pre_consensus::into_block_raw(data.payload.clone()).unwrap();
        assert_eq!(execution_beneficiary_for_header(&rebuilt.header), execution_beneficiary(&data));
    }

    #[test]
    fn unrecoverable_seal_falls_back_to_fee_recipient() {
        let fee_recipient = address!("0x1111111111111111111111111111111111111111");
        let header = Header {
            beneficiary: fee_recipient,
            extra_data: vec![0u8; 32].into(),
            ..Default::default()
        };
        let block = smartnet_ethereum_primitives::Block { header, body: Default::default() };
        let data = execution_data_from_block_rlp(block);
        assert_eq!(execution_beneficiary(&data), fee_recipient);

        let rebuilt = calibri_lite_consensus::pre_consensus::into_block_raw(data.payload).unwrap();
        assert_eq!(execution_beneficiary_for_header(&rebuilt.header), fee_recipient);
    }
}
