//! **Calibri Lite Consensus** — single SmartNet consensus crate.
//!
//! | Submodule | Role |
//! |-----------|------|
//! | [`pre_consensus`] | **Active:** runtime profile, proposal meta, seal, header policy |
//! | [`consensus`] | CALIBRI stubs + quorum resolution from genesis |
//!
//! Genesis JSON seal keys remain opaque; execution/wallet layers are unchanged.

/// The consensus identity of this node — single declaration point for bootstrap logs.
pub const CONSENSUS_NAME: &str = "Calibri-Lite";

pub mod consensus;
pub mod pre_consensus;
pub mod registry;
pub mod select;

pub use select::{
    check_seal, get_val_chair,
    Purpose as SelectPurpose, addr_key, chair_entropy, chair_for, committee_size, current_chair,
    current_slot, normalize_eth, parse_addr, score, score_rehash, select_chair_and_quorum,
    select_k, select_one,
};

pub use consensus::{
    BlockNr, CalibriError, INTERIM_QUORUM_THRESHOLD, calibri_schedule_available,
    genesis_wire_signers, get_chair, get_chair_address, get_chair_addresses, get_quorum,
    get_quorum_addresses, get_quorum_addresses_for_parent, get_quorum_for_parent, is_chair,
    is_quorum_member, quorum_threshold,
};

pub use pre_consensus::{
    CALIBRI_LITE_RUNTIME_PROTOCOL_VERSION, CalibriLiteSealError, ChildHeaderFields,
    ConsensusProfile, ExecutionCompatProfile, PROPOSAL_TYPE_NORMAL, SMARTNET_FIXED_GAS_PRICE_WEI,
    SMARTNET_FIXED_TRANSFER_FEE_WEI, SmartNetProposalMeta, SmartNetRuntimeProfile,
    SmartNetValidationError, TxCompatProfile, build_vanity_with_meta, calibri_lite_seal_hash,
    fixed_gas_price_wei, fixed_transfer_fee_wei,
    into_block_raw, parse_extra_data_validators, recover_calibri_lite_signer,
    secret_key_to_address, try_into_block_with_sidecar, validate_pre_consensus_header,
    validate_pre_consensus_timestamp_against_parent, validate_smartnet_proposal,
    validate_static_signer, validate_tier0_sparse_timestamp,
};
