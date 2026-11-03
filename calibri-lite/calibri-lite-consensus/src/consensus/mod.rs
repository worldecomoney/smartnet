//! Future Calibri consensus (quorum, chair delegation, BT agreement, minority reports).

pub mod calibri;

pub use calibri::{
    BlockNr, CalibriError, INTERIM_QUORUM_THRESHOLD, calibri_schedule_available,
    genesis_wire_signers, get_chair, get_chair_address, get_chair_addresses, get_quorum,
    get_quorum_addresses, get_quorum_addresses_for_parent, get_quorum_for_parent, is_chair,
    is_quorum_member, quorum_threshold,
};
