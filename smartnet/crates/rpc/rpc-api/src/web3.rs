use alloy_primitives::{Bytes, B256};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use serde::{Deserialize, Serialize};

/// One entry of the closed mesh peer list, as reported by `web3_nodeInfo`.
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

/// The closed mesh roster returned by `web3_nodeInfo`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshNodeInfo {
    /// Every entry of the mesh peers file, in file order.
    pub peers: Vec<MeshPeerEntry>,
}

/// Web3 rpc interface.
#[cfg_attr(not(feature = "client"), rpc(server, namespace = "web3"))]
#[cfg_attr(feature = "client", rpc(server, client, namespace = "web3"))]
pub trait Web3Api {
    /// Returns current client version.
    #[method(name = "clientVersion")]
    async fn client_version(&self) -> RpcResult<String>;

    /// Returns sha3 of the given data.
    #[method(name = "sha3")]
    fn sha3(&self, input: Bytes) -> RpcResult<B256>;
}
