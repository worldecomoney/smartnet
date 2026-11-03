use std::{io, path::PathBuf};

use alloy_rpc_types_admin::{PeerInfo, PeerNetworkInfo, PeerProtocolInfo};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use smartnet_network_api::{NetworkInfo, Peers};
use smartnet_transaction_pool::TransactionPool;
use revm_primitives::keccak256;
use smartnet_mesh::peers::{default_peers_file, load_peers};
use smartnet_rpc_api::{AdminApiServer, MeshNodeInfo, MeshPeerEntry};
use smartnet_rpc_server_types::{result::internal_rpc_err, ToRpcResult};

/// `admin` API implementation.
///
/// This type provides the functionality for handling `admin` related requests.
pub struct AdminApi<N, Pool> {
    /// An interface to interact with the network
    network: N,
    /// The transaction pool
    pool: Pool,
    /// The mesh peers file (`MESH_PEERS_FILE`); `None` falls back to the mesh default
    /// (`$HOME/.smartnet/mainnet.jsonl`).
    mesh_peers_file: Option<PathBuf>,
}

impl<N, Pool> AdminApi<N, Pool> {
    /// Creates a new instance of `AdminApi`.
    pub const fn new(network: N, pool: Pool, mesh_peers_file: Option<PathBuf>) -> Self {
        Self { network, pool, mesh_peers_file }
    }
}

#[async_trait]
impl<N, Pool> AdminApiServer for AdminApi<N, Pool>
where
    N: NetworkInfo + Peers + 'static,
    Pool: TransactionPool + 'static,
{
    /// Handler for `admin_peers`
    ///
    /// No devp2p sessions exist in the mesh, so there are never any connected peers here;
    /// mesh membership is reported by [`Self::node_info`].
    async fn peers(&self) -> RpcResult<Vec<PeerInfo>> {
        let peers = self.network.get_all_peers().await.to_rpc_result()?;
        let mut infos = Vec::with_capacity(peers.len());

        for peer in peers {
            infos.push(PeerInfo {
                id: alloy_primitives::hex::encode(keccak256(peer.remote_id.as_slice())),
                name: peer.client_version.to_string(),
                caps: peer.capabilities.capabilities().iter().map(|cap| cap.to_string()).collect(),
                // No devp2p handshake exists in the mesh, so enode/ENR stay empty.
                enode: String::new(),
                enr: None,
                network: PeerNetworkInfo {
                    remote_address: peer.remote_addr,
                    local_address: peer.local_addr.unwrap_or_else(|| self.network.local_addr()),
                    inbound: peer.direction.is_incoming(),
                    trusted: peer.kind.is_trusted(),
                    static_node: peer.kind.is_static(),
                },
                protocols: PeerProtocolInfo { eth: None, snap: None, other: Default::default() },
            })
        }

        Ok(infos)
    }

    /// Handler for `admin_nodeInfo`: the closed mesh roster from the peers file.
    ///
    /// A missing file is the normal pre-provisioning state (mesh outbound off) and
    /// reports an empty roster instead of an error.
    async fn node_info(&self) -> RpcResult<MeshNodeInfo> {
        mesh_node_info(self.mesh_peers_file.clone()).await
    }

    /// Handler for `admin_peerEvents`
    async fn subscribe_peer_events(
        &self,
        _pending: jsonrpsee::PendingSubscriptionSink,
    ) -> jsonrpsee::core::SubscriptionResult {
        Err("admin_peerEvents is not implemented yet".into())
    }

    /// Handler for `admin_clearTxpool`
    async fn clear_txpool(&self) -> RpcResult<u64> {
        let all_hashes = self.pool.all_transaction_hashes();
        let count = all_hashes.len() as u64;
        let _ = self.pool.remove_transactions(all_hashes);
        Ok(count)
    }
}

impl<N, Pool> std::fmt::Debug for AdminApi<N, Pool> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminApi").finish_non_exhaustive()
    }
}

/// Loads the closed mesh roster for `nodeInfo`-style handlers (`admin_nodeInfo`,
/// `web3_nodeInfo`).
///
/// A missing file is the normal pre-provisioning state (mesh outbound off) and reports an
/// empty roster instead of an error.
pub async fn mesh_node_info(peers_file: Option<PathBuf>) -> RpcResult<MeshNodeInfo> {
    let path = peers_file.unwrap_or_else(default_peers_file);
    let parsed = match load_peers(&path) {
        Ok(parsed) => parsed,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(MeshNodeInfo::default()),
        Err(err) => return Err(internal_rpc_err(err.to_string())),
    };
    Ok(MeshNodeInfo {
        peers: parsed
            .peers
            .into_iter()
            .map(|entry| MeshPeerEntry {
                key_id: entry.key_id,
                url: entry.url,
                ip: entry.ip,
                port_range: entry.port_range,
                active: entry.active,
                valid: entry.valid,
            })
            .collect(),
    })
}
