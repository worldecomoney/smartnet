//! Pre-consensus layer — **active today** for SmartNet block production and import.
//!
//! Owns runtime profile, proposal metadata in `extraData`, ECDSA seal wire format,
//! genesis signer parsing, and header policy. Does **not** implement quorum/BT/minority report.

mod entrypoint;
mod genesis;
mod proposal_meta;
mod proposal_validation;
mod rlp_decode;
mod runtime_profile;
mod seal;
mod timestamp;

pub use entrypoint::{
    PreConsensusError, validate_pre_consensus_header,
    validate_pre_consensus_timestamp_against_parent,
};
pub use genesis::parse_extra_data_validators;
pub use proposal_meta::{
    META_LEN, PROPOSAL_TYPE_NORMAL, SmartNetProposalMeta, build_vanity_with_meta,
};
pub use proposal_validation::{SmartNetValidationError, validate_smartnet_proposal};
pub use rlp_decode::{into_block_raw, try_into_block_with_sidecar};
pub use runtime_profile::{
    CALIBRI_LITE_RUNTIME_PROTOCOL_VERSION, ChildHeaderFields, ConsensusProfile,
    ExecutionCompatProfile, SMARTNET_FIXED_GAS_PRICE_WEI, SMARTNET_FIXED_TRANSFER_FEE_WEI,
    SmartNetRuntimeProfile, TxCompatProfile,
};
pub use seal::{
    CalibriLiteSealError, calibri_lite_seal_hash, recover_calibri_lite_signer,
    secret_key_to_address, validate_static_signer,
};
pub use timestamp::validate_tier0_sparse_timestamp;
