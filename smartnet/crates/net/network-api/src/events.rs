//! API related to listening for network events.
//!
//! The event streams themselves were removed together with the devp2p transport: there are no
//! more sessions or discovery events in the mesh build, only the associated message primitives
//! remain (used by the sync components' type bounds).

use smartnet_eth_wire_types::NetworkPrimitives;

/// Provides event subscription for the network.
#[auto_impl::auto_impl(&, Arc)]
pub trait NetworkEventListenerProvider: Send + Sync {
    /// The primitive types used by the network's message payloads.
    type Primitives: NetworkPrimitives;
}
