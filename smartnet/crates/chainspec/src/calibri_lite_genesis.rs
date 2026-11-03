//! Genesis-only parsing of the validator set from a genesis `extraData` (no consensus logic).
//! Works for any genesis; chains without a signer section simply yield no validators.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};
use alloy_genesis::Genesis;
use alloy_primitives::Address;

const GENESIS_EXTRA_VANITY_BYTES: usize = 32;
const GENESIS_EXTRA_SIGNATURE_BYTES: usize = 65;
const GENESIS_EXTRA_SIGNER_BYTES: usize = 20;
const GENESIS_EXTRA_MIN_BYTES: usize =
    GENESIS_EXTRA_VANITY_BYTES + GENESIS_EXTRA_SIGNATURE_BYTES + GENESIS_EXTRA_SIGNER_BYTES;

/// Maximum header `extraData` for Calibri Lite chains (vanity + validator list + seal).
pub const CALIBRI_LITE_MAX_EXTRA_DATA_SIZE: usize = GENESIS_EXTRA_VANITY_BYTES +
    (GENESIS_EXTRA_SIGNER_BYTES * 1024) +
    GENESIS_EXTRA_SIGNATURE_BYTES;

/// Validates the genesis validator `extraData` (genesis-only).
///
/// Deliberately independent of the genesis `config` object: the legacy
/// `clique {period, epoch}` block carries nothing the node reads (period has zero consumers;
/// epoch is inert) and may be present, absent, or garbage — the node validates the
/// `extraData` signer section, which is the actual consensus-bearing header field, and which
/// alone decides whether a chain is seal-validated. See `GENESIS-LEGACY-CONFIG.md`.
pub fn validate_calibri_lite_genesis(genesis: &Genesis) -> Result<(), String> {
    let validators = parse_extra_data_validators(&genesis.extra_data)?;
    if validators.is_empty() {
        return Err("invalid genesis: expected at least one signer in `extraData`".to_string());
    }

    Ok(())
}

/// Parses authorized signers from genesis `extraData` (`32 vanity | N×20 addresses | 65 seal`).
pub fn parse_extra_data_validators(extra_data: &[u8]) -> Result<Vec<Address>, String> {
    if extra_data.len() < GENESIS_EXTRA_MIN_BYTES {
        return Err(format!(
            "invalid genesis `extraData` length: expected at least {GENESIS_EXTRA_MIN_BYTES} bytes, got {}",
            extra_data.len()
        ));
    }

    let signers_len = extra_data
        .len()
        .checked_sub(GENESIS_EXTRA_VANITY_BYTES + GENESIS_EXTRA_SIGNATURE_BYTES)
        .ok_or_else(|| {
            "invalid genesis `extraData`: underflow while parsing signer section".to_string()
        })?;

    if signers_len % GENESIS_EXTRA_SIGNER_BYTES != 0 {
        return Err(format!(
            "invalid genesis `extraData` signer section length: {signers_len} is not divisible by {GENESIS_EXTRA_SIGNER_BYTES}"
        ));
    }

    let signers = extra_data[GENESIS_EXTRA_VANITY_BYTES..GENESIS_EXTRA_VANITY_BYTES + signers_len]
        .as_chunks::<GENESIS_EXTRA_SIGNER_BYTES>().0.iter()
        .map(|signer| Address::from_slice(signer))
        .collect();

    Ok(signers)
}

/// The validator set a genesis carries in its `extraData` signer section, or `[]` when the
/// section is absent/short/garbled. This — not the legacy `config.clique` object — is what
/// "is this chain seal-validated" keys off (see `GENESIS-LEGACY-CONFIG.md`).
pub fn extra_data_validators(extra_data: &[u8]) -> Vec<Address> {
    parse_extra_data_validators(extra_data).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_genesis::ChainConfig;
    use alloy_primitives::address;

    fn build_genesis(extra_data: Vec<u8>) -> Genesis {
        Genesis {
            config: ChainConfig { chain_id: 1337, ..Default::default() },
            extra_data: extra_data.into(),
            ..Default::default()
        }
    }

    #[test]
    fn parse_single_signer() {
        let signer = address!("0x24bf191a358c09e79b9ead2d8999d08590b5af31");
        let extra_data = [
            &[0u8; GENESIS_EXTRA_VANITY_BYTES][..],
            signer.as_slice(),
            &[0u8; GENESIS_EXTRA_SIGNATURE_BYTES][..],
        ]
        .concat();

        let signers = parse_extra_data_validators(&extra_data).unwrap();
        assert_eq!(signers, vec![signer]);
    }

    #[test]
    fn rejects_invalid_extra_data_length() {
        let err = parse_extra_data_validators(&[0u8; 96]).unwrap_err();
        assert!(err.contains("expected at least"));
    }

    #[test]
    fn legacy_clique_config_is_inert() {
        let signer = address!("0x24bf191a358c09e79b9ead2d8999d08590b5af31");
        let extra_data = [
            &[0u8; GENESIS_EXTRA_VANITY_BYTES][..],
            signer.as_slice(),
            &[0u8; GENESIS_EXTRA_SIGNATURE_BYTES][..],
        ]
        .concat();
        // Any epoch/period — even zero or garbage — is not read; only the extraData signers
        // matter.
        let genesis = build_genesis(extra_data);
        assert!(validate_calibri_lite_genesis(&genesis).is_ok());
    }
}
