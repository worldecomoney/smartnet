use alloy_rpc_types_admin::PeerInfo;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use serde::{Deserialize, Serialize};

/// One entry of the closed mesh peer list, as reported by [`AdminApi::node_info`].
///
/// Mirrors the `mainnet.jsonl` schema (see the `smartnet-mesh` crate). The devp2p
/// `enode`/`ENR` identities do not exist in the mesh, so they are not reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshPeerEntry {
    /// Free-form label used in logs only.
    pub key_id: String,
    /// Reserved (always empty today), kept for file-schema parity.
    pub url: String,
    /// Host/IP of the peer's mesh listener.
    pub ip: String,
    /// Port of the peer's mesh listener.
    pub port_range: u16,
    /// Whether gossip to this peer is enabled.
    pub active: bool,
    /// Whether this peer is still a valid mesh member.
    pub valid: bool,
}

/// The closed mesh roster returned by `admin_nodeInfo`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshNodeInfo {
    /// Every entry of the mesh peers file, in file order.
    pub peers: Vec<MeshPeerEntry>,
}

/// Admin namespace rpc interface that gives access to several non-standard RPC methods.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "admin"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "admin"))]
pub trait AdminApi {
    /// The peers administrative property lists the remote nodes connected to this node at
    /// the networking granularity. With the closed-list mesh transport there are no
    /// devp2p sessions, so this is always empty — mesh membership (the peers file) is
    /// reported by [`AdminApi::node_info`].
    #[method(name = "peers")]
    async fn peers(&self) -> RpcResult<Vec<PeerInfo>>;

    /// Creates an RPC subscription which serves events received from the network.
    #[subscription(
        name = "peerEvents",
        unsubscribe = "peerEvents_unsubscribe",
        item = String
    )]
    async fn subscribe_peer_events(&self) -> jsonrpsee::core::SubscriptionResult;

    /// Returns this node's closed mesh roster (the `mainnet.jsonl` peer file).
    ///
    /// There is no enode/ENR — the devp2p overlay was replaced by the all-to-all mesh.
    #[method(name = "nodeInfo")]
    async fn node_info(&self) -> RpcResult<MeshNodeInfo>;

    /// Clears all transactions from the transaction pool.
    /// Returns the number of transactions that were removed from the pool.
    #[method(name = "clearTxpool")]
    async fn clear_txpool(&self) -> RpcResult<u64>;
}
