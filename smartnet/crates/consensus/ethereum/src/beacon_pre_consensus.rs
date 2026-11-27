//! Pre-consensus header helpers for [`super::EthBeaconConsensus`].
//!
//! Calibri Lite uses [`calibri_lite_consensus::validate_pre_consensus_header`] when
//! `smartnet_runtime` is active.

use alloc::format;

use super::EthBeaconConsensus;
use calibri_lite_consensus::validate_pre_consensus_timestamp_against_parent;
use core::fmt::Debug;
use smartnet_chainspec::{extra_data_validators, EthChainSpec, EthereumHardforks};
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
        let sealed_genesis = !extra_data_validators(genesis.extra_data.as_ref()).is_empty();
        let has_registry = !calibri_lite_consensus::registry::get_val_all().is_empty();

        // Historical fee fields are never checked here: imported/synced legacy headers may
        // carry any base fee (including none or zero) and are accepted as-is. The fixed price
        // is enforced where it matters — the wallet quote and the current tx's admissibility.
        // (The old `SmartNetBaseFeeInvalid` comparison rejected honest legacy history.)

        // THE validator gate (operator, 2026-11-19): the seal's signer must be the
        // SELECT chair for this slot — and `get_val_chair` only ever answers with a
        // `status: true` validator of `validators.jsonl` (statuses loaded, keccak math,
        // 1-validator detour), so a `status: false` record can never pass here. The
        // gate applies to every chain that HAS validators: a sealed genesis, or a
        // registry loaded at bootstrap (which covers an extraData-less calibri* genesis
        // too — the old gate returned `Ok(())` there and switched validation OFF). Only
        // a legacy chain with neither keeps the old no-seal behaviour. Fail closed: no
        // enabled validator ⇒ no block validates.
        if !sealed_genesis && !has_registry {
            // A legacy chain with no validators at all: nothing to gate against.
            return Ok(());
        }

        // Through the ONE entrance both ingest paths use (operator: "make sure all
        // import and mesh sync both go through this same single entrance").
        let signer =
            calibri_lite_consensus::check_seal(header).map_err(ConsensusError::Other)?;
        tracing::debug!(
            target: "consensus::pre_consensus",
            number = header.number,
            %signer,
            "validator gate passed — signer is the SELECT chair"
        );
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
        // NOTE: there is deliberately **no maximum timestamp-gap check** yet. The deferral
        // is intentional; the Tier 0 rule only requires the child timestamp to exceed the
        // parent's.
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
    fn historical_base_fee_is_never_checked() {
        let consensus = consensus(sealed_spec());

        // Imported/synced legacy headers carry whatever fee history they had — absent, zero,
        // doubled, arbitrary — and none of it is a validation error. The unsigned header then
        // fails at the signer stage, never at a fee rule.
        for base_fee in [None, Some(0), Some(1), Some(476_190_476_190)] {
            let header = Header { base_fee_per_gas: base_fee, ..Default::default() };
            let err = consensus
                .validate_authored_header(&header, false)
                .expect_err("unsigned header cannot pass signer stage");
            assert!(
                !matches!(err, ConsensusError::SmartNetBaseFeeInvalid { .. }),
                "historical fee {base_fee:?} must not be checked, got: {err:?}"
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
