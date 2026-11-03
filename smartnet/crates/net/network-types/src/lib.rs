//! Commonly used networking types.
//!
//! Peer identity types (`PeerId`, `NodeRecord`) were merged in from the former
//! `smartnet-network-peers` crate. The ENR / discv5 discovery record types were removed
//! with the p2p stack.
//!
//! ## Feature Flags
//!
//! - `serde` (default): Enable serde support

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

/// Types related to peering.
pub mod peers;

pub use peers::{
    addr::PeerAddr,
    identity::PeerId,
    node_record::{NodeRecord, NodeRecordParseError},
    kind::PeerKind,
    reputation::{
        is_banned_reputation, is_connection_failed_reputation, Reputation, ReputationChangeKind,
        ReputationChangeOutcome, ReputationChangeWeights, DEFAULT_REPUTATION,
    },
    state::PeerConnectionState,
    Peer,
};
