//! Single pre-consensus validation entry for node import and validator self-checks.

use super::proposal_validation::{SmartNetValidationError, validate_smartnet_proposal};
use super::runtime_profile::SmartNetRuntimeProfile;
use super::seal::{CalibriLiteSealError, validate_static_signer};
use super::timestamp::validate_tier0_sparse_timestamp;
use alloy_consensus::Header;
use alloy_primitives::Address;

/// Combined error surface for pre-consensus header admission.
#[derive(Debug)]
pub enum PreConsensusError {
    /// Seal recovery or authorized signer check failed.
    Seal(CalibriLiteSealError),
    /// SmartNet proposal metadata validation failed.
    Proposal(SmartNetValidationError),
    /// Sparse timestamp rule failed.
    Timestamp(CalibriLiteSealError),
}

impl core::fmt::Display for PreConsensusError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Seal(e) | Self::Timestamp(e) => write!(f, "{e}"),
            Self::Proposal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PreConsensusError {}

/// Validates seal recovery, authorized signer, and SmartNet proposal metadata.
pub fn validate_pre_consensus_header(
    profile: &SmartNetRuntimeProfile,
    header: &Header,
    authorized: &[Address],
) -> Result<Address, PreConsensusError> {
    let signer = validate_static_signer(header, authorized).map_err(PreConsensusError::Seal)?;
    validate_smartnet_proposal(profile, header, signer, authorized)
        .map_err(PreConsensusError::Proposal)?;
    Ok(signer)
}

/// Parent/child timestamp check (Tier 0 sparse time).
pub fn validate_pre_consensus_timestamp_against_parent(
    parent_timestamp: u64,
    header_timestamp: u64,
    max_future_drift_secs: Option<u64>,
) -> Result<(), PreConsensusError> {
    validate_tier0_sparse_timestamp(parent_timestamp, header_timestamp, max_future_drift_secs)
        .map_err(PreConsensusError::Timestamp)
}
