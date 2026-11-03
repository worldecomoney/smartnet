//! Pre-consensus header helpers for [`super::EthBeaconConsensus`].
//!
//! Calibri Lite uses [`calibri_lite_consensus::validate_pre_consensus_header`] when
//! `smartnet_runtime` is active.

use alloc::format;

use super::EthBeaconConsensus;
use calibri_lite_consensus::{
    get_quorum_addresses, validate_pre_consensus_header,
    validate_pre_consensus_timestamp_against_parent, validate_static_signer,
};
use core::fmt::Debug;
use smartnet_chainspec::{
    extra_data_validators, EthChainSpec, EthereumHardforks, SMARTNET_FIXED_GAS_PRICE_WEI,
};
use smartnet_consensus::ConsensusError;

impl<ChainSpec> EthBeaconConsensus<ChainSpec>
where
    ChainSpec:
        EthChainSpec<Header = alloy_consensus::Header> + EthereumHardforks + Debug + Send + Sync,
{
    pub(super) fn validate_authored_header(
        &self,
        header: &alloy_consensus::Header,
        _is_post_merge: bool,
    ) -> Result<(), ConsensusError> {
        let genesis = self.chain_spec.genesis();
        // Seal validation is gated on the genesis carrying a validator section in `extraData`
        // — NOT on the legacy `config.clique` object (which is inert; see
        // `GENESIS-LEGACY-CONFIG.md`). A chain without validators is unsealed.
        if extra_data_validators(genesis.extra_data.as_ref()).is_empty() {
            return Ok(());
        }

        // SmartNet fixed-fee validity check (`doc/FEES-CURR.md`): a base fee, when present,
        // must be the protocol constant. Absent stays legal — pre SmartNet-era headers
        // legitimately carry no `baseFeePerGas` (see `smartnet-net-rpc-sync` `verify_block0_shape`).
        if let Some(got) = header.base_fee_per_gas &&
            got != SMARTNET_FIXED_GAS_PRICE_WEI
        {
            return Err(ConsensusError::SmartNetBaseFeeInvalid {
                expected: SMARTNET_FIXED_GAS_PRICE_WEI,
                got,
            });
        }

        let quorum = get_quorum_addresses(genesis, header.number)
            .map_err(|err| ConsensusError::Other(format!("quorum resolution failed: {err}")))?;

        if let Some(profile) = &self.smartnet_runtime {
            let signer =
                validate_pre_consensus_header(profile, header, &quorum).map_err(|err| {
                    ConsensusError::Other(format!(
                        "invalid Calibri Lite pre-consensus header: {err}"
                    ))
                })?;
            tracing::debug!(
                target: "consensus::pre_consensus",
                number = header.number,
                %signer,
                proposal = true,
                "validator signature + proposal meta validated"
            );
        } else {
            let signer = validate_static_signer(header, &quorum)
                .map_err(|err| ConsensusError::Other(format!("invalid header signer: {err}")))?;
            tracing::debug!(
                target: "consensus::pre_consensus",
                number = header.number,
                %signer,
                proposal = false,
                "validator signature validated (static signer path)"
            );
        }

        Ok(())
    }

    pub(super) fn validate_authored_header_against_parent(
        &self,
        header: &alloy_consensus::Header,
        parent: &alloy_consensus::Header,
    ) -> Result<(), ConsensusError> {
        // Validator-section presence gates the timestamp/seal pre-consensus header checks (the
        // legacy `config.clique` object is inert; see `GENESIS-LEGACY-CONFIG.md`).
        if extra_data_validators(self.chain_spec.genesis().extra_data.as_ref()).is_empty() {
            return Ok(());
        }

        // Tier 0 sparse time: `header.timestamp > parent.timestamp`. No wall-clock future
        // drift cap in the interim model (removed with `pre_consensus_slot_drift_seconds`).
        //
        // NOTE: there is deliberately **no maximum timestamp-gap check** yet — see
        // `24H.md` at the repo root for the deferred rule (why it is not implemented,
        // observed chain-gap history, and where it would go).
        validate_pre_consensus_timestamp_against_parent(
            parent.timestamp,
            header.timestamp,
            None,
        )
        .map_err(|err| ConsensusError::Other(format!("invalid pre-consensus timestamp: {err}")))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EthBeaconConsensus;
    use alloc::sync::Arc;
    use alloy_consensus::Header;
    use alloy_genesis::{ChainConfig, Genesis};
    use smartnet_chainspec::{Chain, ChainSpec, ChainSpecBuilder};

    fn sealed_spec() -> ChainSpec {
        // A chain is seal-validated when its genesis `extraData` carries a validator section
        // (vanity + N×20 + seal). No `config.clique` at all — the legacy object is inert.
        let signer = alloy_primitives::Address::new([
            0x24, 0xbf, 0x19, 0x1a, 0x35, 0x8c, 0x09, 0xe7, 0x9b, 0x9e, 0xad, 0x2d, 0x89, 0x99,
            0xd0, 0x85, 0x90, 0xb5, 0xaf, 0x31,
        ]);
        let extra_data = [&[0u8; 32][..], signer.as_slice(), &[0u8; 65][..]].concat();
        ChainSpecBuilder::mainnet()
            .chain(Chain::from(315u64))
            .genesis(Genesis {
                config: ChainConfig { chain_id: 315, ..Default::default() },
                extra_data: extra_data.into(),
                ..Default::default()
            })
            .build()
    }

    fn consensus(spec: ChainSpec) -> EthBeaconConsensus<ChainSpec> {
        EthBeaconConsensus::new(Arc::new(spec))
    }

    #[test]
    fn rejects_base_fee_other_than_protocol_constant() {
        let consensus = consensus(sealed_spec());

        // The doubled price observed on blocks 238-240 (fee double-count bug).
        for got in [476_190_476_190u64, SMARTNET_FIXED_GAS_PRICE_WEI + 1, 1] {
            let header = Header { base_fee_per_gas: Some(got), ..Default::default() };
            let err = consensus
                .validate_authored_header(&header, false)
                .expect_err("non-constant base fee must be rejected");
            assert!(
                matches!(
                    err,
                    ConsensusError::SmartNetBaseFeeInvalid { got: g, .. } if g == got
                ),
                "expected SmartNetBaseFeeInvalid for {got}, got: {err:?}"
            );
        }
    }

    #[test]
    fn absent_or_fixed_base_fee_reaches_signer_stage() {
        let consensus = consensus(sealed_spec());

        // Absent (pre SmartNet-era) and exactly the constant both clear the fee gate; the
        // header then proceeds to quorum/signer validation and fails there (empty genesis
        // extra data → no signers) — never with a fee error.
        for base_fee in [None, Some(SMARTNET_FIXED_GAS_PRICE_WEI)] {
            let header = Header { base_fee_per_gas: base_fee, ..Default::default() };
            let err = consensus
                .validate_authored_header(&header, false)
                .expect_err("unsigned header cannot pass signer stage");
            assert!(
                !matches!(err, ConsensusError::SmartNetBaseFeeInvalid { .. }),
                "fee gate must pass through {base_fee:?}, got: {err:?}"
            );
        }
    }

    #[test]
    fn unsealed_chain_skips_fixed_fee_check() {
        let consensus = consensus(ChainSpecBuilder::mainnet().build());
        let header = Header { base_fee_per_gas: Some(12_345), ..Default::default() };
        assert!(consensus.validate_authored_header(&header, false).is_ok());
    }

    #[test]
    fn timestamp_must_not_move_backwards() {
        let consensus = consensus(sealed_spec());
        let parent = Header { timestamp: 1_000, ..Default::default() };
        let child = Header { timestamp: 999, ..Default::default() };
        assert!(consensus.validate_authored_header_against_parent(&child, &parent).is_err());
    }
}
