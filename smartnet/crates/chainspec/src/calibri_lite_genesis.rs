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
/// **Two genesis types** (GENESIS.md): a genesis carrying the legacy `config.clique`
/// block is a clique genesis — the legacy init path keeps its rules byte-for-byte, and
/// the clique seal needs its signer section (required). Anything else is **calibri\***
/// — `extraData` may be absent, `"0x0"`, or carry a signer section: signers are NOT
/// required and NOT the authority (the registry `validators.jsonl` is; an empty or
/// missing registry fails closed at boot, before any seal or acceptance).
pub fn validate_calibri_lite_genesis(genesis: &Genesis) -> Result<(), String> {
    if genesis.config.clique.is_some()
        && extra_data_validators(&genesis.extra_data).is_empty()
    {
        return Err("legacy clique genesis requires a signer section in `extraData`".to_string());
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

/// Calibri\* genesis defaults (GENESIS.md): absent keys are **replaced by defaults in
/// the DB at init** — the operator's file stays minimal. Only applies when the file is
/// NOT legacy clique (`config.clique` absent — the legacy path is never touched).
///
/// Required for calibri (fail closed): `config.chainId` and `timestamp` — both are
/// identity/determinism keys that must never be invented (a defaulted timestamp would
/// make the genesis hash depend on the init moment).
pub fn normalize_calibri_genesis_raw(raw: &mut serde_json::Value) -> Result<(), String> {
    let is_clique = raw.get("config").and_then(|config| config.get("clique")).is_some();
    if is_clique {
        return Ok(());
    }
    let object = match raw.as_object_mut() {
        Some(object) => object,
        None => return Ok(()), // not an object — let the real parser report it
    };

    // Required identity keys — never defaulted.
    let config = object.get("config").ok_or_else(|| {
        "calibri genesis is missing `config` (chainId is required)".to_string()
    })?;
    if config.get("chainId").is_none() {
        return Err("calibri genesis is missing `config.chainId`".to_string());
    }
    if object.get("timestamp").is_none() {
        return Err("calibri genesis is missing `timestamp` — write it fixed (determinism: \
                    a defaulted timestamp would change the genesis hash per init run)"
            .to_string());
    }

    // Defaults — filled ONLY when absent, never overriding an operator value.
    let zero32 = format!("0x{}", "0".repeat(64));
    let defaults: [(&str, serde_json::Value); 7] = [
        ("nonce", serde_json::json!("0x0")),
        ("difficulty", serde_json::json!("0x1")),
        ("mixHash", serde_json::json!(zero32)),
        ("coinbase", serde_json::json!("0x0000000000000000000000000000000000000000")),
        ("parentHash", serde_json::json!(zero32)),
        ("gasLimit", serde_json::json!("0x47b760")),
        ("gasUsed", serde_json::json!("0x0")),
    ];
    for (key, value) in defaults {
        object.entry(key.to_string()).or_insert(value);
    }
    Ok(())
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

    #[test]
    fn calibri_genesis_needs_no_signers_clique_does() {
        // Calibri*: extraData absent/empty is VALID — authority is the registry.
        assert!(validate_calibri_lite_genesis(&Genesis::default()).is_ok());

        // Legacy clique: the signer section is required (the clique seal needs it).
        let mut clique = Genesis::default();
        clique.config.clique = Some(alloy_genesis::CliqueConfig { period: Some(5), epoch: Some(30_000) });
        let err = validate_calibri_lite_genesis(&clique).expect_err("clique without signers");
        assert!(err.contains("legacy clique genesis requires a signer section"), "{err}");
    }

    #[test]
    fn normalize_fills_calibri_defaults_and_requires_identity_keys() {
        use serde_json::json;
        // Not a clique file → defaults fill; identity keys required.
        let mut raw = json!({"config": {"chainId": 315}, "timestamp": "0x675fd019", "alloc": {}});
        normalize_calibri_genesis_raw(&mut raw).expect("normalizes");
        assert_eq!(raw["difficulty"], json!("0x1"));
        assert_eq!(raw["nonce"], json!("0x0"));
        assert_eq!(raw["gasLimit"], json!("0x47b760"));
        // Required keys are never invented.
        let mut no_ts = json!({"config": {"chainId": 315}, "alloc": {}});
        no_ts.as_object_mut().unwrap().remove("timestamp");
        // (timestamp was absent to begin with — the helper must fail)
        let err = normalize_calibri_genesis_raw(&mut no_ts).expect_err("timestamp required");
        assert!(err.contains("timestamp"), "{err}");
        // Legacy clique files are never touched.
        let mut clique_raw =
            json!({"config": {"chainId": 315, "clique": {"period": 5, "epoch": 30000}}});
        normalize_calibri_genesis_raw(&mut clique_raw).expect("clique untouched");
        assert!(clique_raw.get("difficulty").is_none(), "no defaults injected into clique");
    }
}
