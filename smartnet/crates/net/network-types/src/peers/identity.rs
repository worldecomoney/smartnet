//! Peer identity types (merged from the former `smartnet-network-peers` crate).
//!
//! `PeerId` is the peer public-key identifier used throughout the network types. The
//! ENR / discv5 discovery record types (`AnyNode`, `Enr`, `WithPeerId`, `pk2id`, `id2pk`)
//! were removed along with the p2p stack — `SmartNet` has no discovery; peer membership
//! lives in the closed-list mesh peers file.

use alloy_primitives::B512;

/// Alias for a peer identifier.
pub type PeerId = B512;
