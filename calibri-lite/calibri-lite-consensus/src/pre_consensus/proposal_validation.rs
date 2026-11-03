//! SmartNet-native proposal checks (replaces seal turn / London transition consensus rules).

use super::proposal_meta::SmartNetProposalMeta;
use super::runtime_profile::SmartNetRuntimeProfile;
use alloy_consensus::Header;
use alloy_primitives::Address;

/// Pre-consensus proposal admission failures (SmartNet-native rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmartNetValidationError {
    /// Recovered proposer not in genesis authorized set.
    UnauthorizedSigner(Address),
    /// `SmartNetProposalMeta.version` mismatch.
    UnsupportedProposalVersion {
        /// Version byte from header metadata.
        got: u8,
        /// Active protocol version.
        expected: u8,
    },
    /// Unknown `proposal_type` byte.
    UnsupportedProposalType(u8),
    /// `extraData` is shorter than the minimum required for SmartNet proposal metadata
    /// (32-byte vanity + 65-byte seal = 97 bytes). The version byte is not present —
    /// this is a truncated/malformed proposal, not an unsupported version.
    MalformedProposal {
        /// Actual length of `extraData`.
        extra_data_len: usize,
        /// Minimum required length.
        min_required: usize,
    },
    /// Header reports more gas used than the block gas limit allows — the body's declared gas
    /// limits were never capped during selection (or simulation diverged). Sealing this header
    /// would produce a block the node rejects (`HeaderGasUsedExceedsGasLimit`).
    GasUsedExceedsGasLimit {
        /// `header.gasUsed`.
        gas_used: u64,
        /// `header.gasLimit`.
        gas_limit: u64,
    },
}

impl core::fmt::Display for SmartNetValidationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnauthorizedSigner(a) => write!(f, "unauthorized SmartNet proposer: {a}"),
            Self::UnsupportedProposalVersion { got, expected } => {
                write!(
                    f,
                    "unsupported SmartNet proposal version: got {got}, expected {expected}"
                )
            }
            Self::UnsupportedProposalType(t) => {
                write!(f, "unsupported SmartNet proposal type: {t}")
            }
            Self::MalformedProposal {
                extra_data_len,
                min_required,
            } => {
                write!(
                    f,
                    "malformed SmartNet proposal: extraData too short ({extra_data_len} bytes, need >= {min_required})"
                )
            }
            Self::GasUsedExceedsGasLimit {
                gas_used,
                gas_limit,
            } => {
                write!(
                    f,
                    "header gasUsed {gas_used} exceeds block gasLimit {gas_limit} — refusing to seal an invalid block"
                )
            }
        }
    }
}

impl std::error::Error for SmartNetValidationError {}

/// Validate authorized signer + proposal metadata (no difficulty / in-turn semantics).
///
/// Era discrimination is baked into the block itself: SmartNet-era headers carry
/// [`SmartNetProposalMeta`] with the active protocol version in the 32-byte vanity; legacy
/// legacy sealed headers (the entire pre-SmartNet history) carry arbitrary vanity bytes that
/// decode as junk (e.g. version 216). Only headers presenting the ACTIVE version are held to
/// SmartNet proposal rules — everything else falls back to lite seal admission: a valid
/// seal from a genesis-authorized signer (already checked by `validate_static_signer` before
/// this runs). This is safe while signing authority is limited to the authorized set: no
/// unauthorized producer can pass either path.
pub fn validate_smartnet_proposal(
    profile: &SmartNetRuntimeProfile,
    header: &Header,
    signer: Address,
    authorized: &[Address],
) -> Result<(), SmartNetValidationError> {
    if !authorized.contains(&signer) {
        return Err(SmartNetValidationError::UnauthorizedSigner(signer));
    }

    // Hard block-gas-limit check: the sealed header must not report more gas used than its own
    // gas limit (node-side `validate_header_gas` rejects such a block on import). Selection caps
    // the body's Σ declared gas limits at this limit; this is the pre-seal belt against any
    // divergence between selection and simulation.
    if header.gas_used > header.gas_limit {
        return Err(SmartNetValidationError::GasUsedExceedsGasLimit {
            gas_used: header.gas_used,
            gas_limit: header.gas_limit,
        });
    }

    // SmartNet-era header? Only meta with the ACTIVE version counts; anything else
    // (undecodable, or junk version from legacy vanity) is a legacy sealed header.
    let extra_len = header.extra_data.len();
    let Some(meta) = SmartNetProposalMeta::decode_from_extra_data(&header.extra_data) else {
        // extraData too short for SmartNet proposal metadata (< 97 bytes).
        // This is a malformed/truncated proposal, not a legacy sealed header —
        // legacy sealed headers have at least 32+20+65=117 bytes (vanity + signer + seal).
        if extra_len < super::proposal_meta::MIN_SMARTNET_EXTRA_DATA_LEN {
            return Err(SmartNetValidationError::MalformedProposal {
                extra_data_len: extra_len,
                min_required: super::proposal_meta::MIN_SMARTNET_EXTRA_DATA_LEN,
            });
        }
        return Ok(()); // legacy sealed header — seal already validated.
    };
    if meta.version != profile.protocol_version {
        return Ok(()); // legacy vanity that happens to decode — treat as legacy.
    }

    if meta.proposal_type != super::proposal_meta::PROPOSAL_TYPE_NORMAL {
        return Err(SmartNetValidationError::UnsupportedProposalType(
            meta.proposal_type,
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_consensus::runtime_profile::SmartNetRuntimeProfile;
    use alloy_primitives::{Bytes, address};

    fn test_profile() -> SmartNetRuntimeProfile {
        SmartNetRuntimeProfile::for_sealed_chain(315)
    }

    fn smartnet_extra_data(version: u8, proposal_type: u8) -> Bytes {
        let mut extra = vec![0u8; 32 + 65];
        extra[0] = version;
        extra[1] = proposal_type;
        Bytes::from(extra)
    }

    fn legacy_extra_data() -> Bytes {
        Bytes::from(vec![0u8; 32 + 65]) // all zeros — looks like a legacy seal
    }

    #[test]
    fn unauthorized_signer_rejected() {
        let header = alloy_consensus::Header {
            extra_data: smartnet_extra_data(1, 0),
            ..Default::default()
        };
        let authorized = vec![address!("0x1111111111111111111111111111111111111111")];
        let result = validate_smartnet_proposal(
            &test_profile(),
            &header,
            address!("0x000000000000000000000000000000000000AAAA"),
            &authorized,
        );
        assert!(matches!(
            result,
            Err(SmartNetValidationError::UnauthorizedSigner(_))
        ));
    }

    #[test]
    fn valid_smartnet_proposal_accepted() {
        let header = alloy_consensus::Header {
            extra_data: smartnet_extra_data(1, 0),
            ..Default::default()
        };
        let signer = address!("0x1111111111111111111111111111111111111111");
        let result = validate_smartnet_proposal(&test_profile(), &header, signer, &[signer]);
        assert!(result.is_ok());
    }

    #[test]
    fn legacy_header_accepted() {
        let header = alloy_consensus::Header {
            extra_data: legacy_extra_data(),
            ..Default::default()
        };
        let signer = address!("0x1111111111111111111111111111111111111111");
        let result = validate_smartnet_proposal(&test_profile(), &header, signer, &[signer]);
        assert!(result.is_ok());
    }

    #[test]
    fn short_extra_data_rejected_as_malformed() {
        let header = alloy_consensus::Header {
            extra_data: Bytes::from(vec![0u8; 80]),
            ..Default::default()
        };
        let signer = address!("0x1111111111111111111111111111111111111111");
        let result = validate_smartnet_proposal(&test_profile(), &header, signer, &[signer]);
        assert!(matches!(
            result,
            Err(SmartNetValidationError::MalformedProposal { .. })
        ));
    }

    #[test]
    fn wrong_version_treated_as_legacy() {
        let header = alloy_consensus::Header {
            extra_data: smartnet_extra_data(99, 0),
            ..Default::default()
        };
        let signer = address!("0x1111111111111111111111111111111111111111");
        let result = validate_smartnet_proposal(&test_profile(), &header, signer, &[signer]);
        assert!(result.is_ok()); // treated as legacy
    }

    #[test]
    fn unsupported_proposal_type_rejected() {
        let header = alloy_consensus::Header {
            extra_data: smartnet_extra_data(1, 99),
            ..Default::default()
        };
        let signer = address!("0x1111111111111111111111111111111111111111");
        let result = validate_smartnet_proposal(&test_profile(), &header, signer, &[signer]);
        assert!(matches!(
            result,
            Err(SmartNetValidationError::UnsupportedProposalType(99))
        ));
    }

    #[test]
    fn profile_protocol_version_used_not_global_constant() {
        // Create a profile with a custom protocol version
        let mut profile = test_profile();
        profile.protocol_version = 42;
        // Header with version 42 should be accepted
        let header = alloy_consensus::Header {
            extra_data: smartnet_extra_data(42, 0),
            ..Default::default()
        };
        let signer = address!("0x1111111111111111111111111111111111111111");
        let result = validate_smartnet_proposal(&profile, &header, signer, &[signer]);
        assert!(result.is_ok());
    }

    #[test]
    fn gas_used_within_limit_accepted() {
        let signer = address!("0x1111111111111111111111111111111111111111");
        // Below the limit and exactly at the limit are both valid.
        for gas_used in [0u64, 21_000, 30_000_000] {
            let header = alloy_consensus::Header {
                extra_data: smartnet_extra_data(1, 0),
                gas_used,
                gas_limit: 30_000_000,
                ..Default::default()
            };
            let result = validate_smartnet_proposal(&test_profile(), &header, signer, &[signer]);
            assert!(
                result.is_ok(),
                "gas_used {gas_used} must be accepted: {result:?}"
            );
        }
    }

    #[test]
    fn gas_used_over_limit_rejected() {
        let signer = address!("0x1111111111111111111111111111111111111111");
        let header = alloy_consensus::Header {
            extra_data: smartnet_extra_data(1, 0),
            gas_used: 30_000_001,
            gas_limit: 30_000_000,
            ..Default::default()
        };
        let result = validate_smartnet_proposal(&test_profile(), &header, signer, &[signer]);
        assert_eq!(
            result,
            Err(SmartNetValidationError::GasUsedExceedsGasLimit {
                gas_used: 30_000_001,
                gas_limit: 30_000_000,
            })
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exceeds block gasLimit")
        );
    }
}
