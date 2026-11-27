use std::{io, path::PathBuf};

use alloy_primitives::{keccak256, Bytes, B256};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use smartnet_mesh::peers::{default_peers_file, load_peers};
use smartnet_network_api::NetworkInfo;
use smartnet_rpc_api::{MeshNodeInfo, MeshPeerEntry, Web3ApiServer};
use smartnet_rpc_server_types::{result::internal_rpc_err, ToRpcResult};

/// `web3` API implementation.
///
/// This type provides the functionality for handling `web3` related requests.
pub struct Web3Api<N> {
    /// An interface to interact with the network
    network: N,
}

impl<N> Web3Api<N> {
    /// Creates a new instance of `Web3Api`.
    pub const fn new(network: N) -> Self {
        Self { network }
    }
}

#[async_trait]
impl<N> Web3ApiServer for Web3Api<N>
where
    N: NetworkInfo + 'static,
{
    /// Handler for `web3_clientVersion`
    async fn client_version(&self) -> RpcResult<String> {
        let status = self.network.network_status().await.to_rpc_result()?;
        Ok(status.client_version)
    }

    /// Handler for `web3_sha3`
    fn sha3(&self, input: Bytes) -> RpcResult<B256> {
        Ok(keccak256(input))
    }
}

impl<N> std::fmt::Debug for Web3Api<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Web3Api").finish_non_exhaustive()
    }
}

/// Loads the closed mesh roster for `web3_nodeInfo`.
///
/// A missing file is the normal pre-provisioning state (mesh outbound off) and reports an empty
/// roster instead of an error.
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
