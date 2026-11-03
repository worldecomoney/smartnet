//! A network implementation that does nothing.
//!
//! This is useful for wiring components together that don't require network but still need to be
//! generic over it.

use core::marker::PhantomData;
use std::net::{IpAddr, SocketAddr};

use crate::{
    test_utils::{PeersHandle, PeersHandleProvider},
    NetworkError, NetworkEventListenerProvider, NetworkInfo,
    NetworkStatus, PeerId, PeerInfo, Peers, PeersInfo,
};
use smartnet_eth_wire_types::{DisconnectReason, EthNetworkPrimitives, EthVersion, NetworkPrimitives};
use smartnet_network_types::{PeerKind, Reputation, ReputationChangeKind};
use tokio::sync::mpsc;

/// A type that implements all network trait that does nothing.
///
/// Intended for testing purposes where network is not used.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NoopNetwork<Net = EthNetworkPrimitives> {
    chain_id: u64,
    peers_handle: PeersHandle,
    _marker: PhantomData<Net>,
}

impl<Net> NoopNetwork<Net> {
    /// Creates a new [`NoopNetwork`].
    pub fn new() -> Self {
        let (tx, _) = mpsc::unbounded_channel();

        Self { chain_id: 0, peers_handle: PeersHandle::new(tx), _marker: PhantomData }
    }

    /// Creates a new [`NoopNetwork`] from an existing one but with a new chain id.
    pub const fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = chain_id;
        self
    }
}

impl Default for NoopNetwork<EthNetworkPrimitives> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Net> NetworkInfo for NoopNetwork<Net>
where
    Net: Send + Sync,
{
    fn local_addr(&self) -> SocketAddr {
        (IpAddr::from(std::net::Ipv4Addr::UNSPECIFIED), 0).into()
    }

    async fn network_status(&self) -> Result<NetworkStatus, NetworkError> {
        Ok(NetworkStatus {
            client_version: format!("smartnet/{}", env!("CARGO_PKG_VERSION")),
            protocol_version: EthVersion::LATEST as u64,
            capabilities: vec![],
        })
    }

    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn is_syncing(&self) -> bool {
        false
    }

    fn is_initially_syncing(&self) -> bool {
        false
    }
}

impl<Net> PeersInfo for NoopNetwork<Net>
where
    Net: Send + Sync,
{
    fn num_connected_peers(&self) -> usize {
        0
    }
}

impl<Net> Peers for NoopNetwork<Net>
where
    Net: Send + Sync,
{
    fn add_trusted_peer_id(&self, _peer: PeerId) {}

    fn add_peer_kind(
        &self,
        _peer: PeerId,
        _kind: Option<PeerKind>,
        _tcp_addr: SocketAddr,
        _udp_addr: Option<SocketAddr>,
    ) {
    }

    async fn get_peers_by_kind(&self, _kind: PeerKind) -> Result<Vec<PeerInfo>, NetworkError> {
        Ok(vec![])
    }

    async fn get_all_peers(&self) -> Result<Vec<PeerInfo>, NetworkError> {
        Ok(vec![])
    }

    async fn get_peer_by_id(&self, _peer_id: PeerId) -> Result<Option<PeerInfo>, NetworkError> {
        Ok(None)
    }

    async fn get_peers_by_id(&self, _peer_id: Vec<PeerId>) -> Result<Vec<PeerInfo>, NetworkError> {
        Ok(vec![])
    }

    fn remove_peer(&self, _peer: PeerId, _kind: PeerKind) {}

    fn disconnect_peer(&self, _peer: PeerId) {}

    fn disconnect_peer_with_reason(&self, _peer: PeerId, _reason: DisconnectReason) {}

    fn connect_peer_kind(
        &self,
        _peer: PeerId,
        _kind: PeerKind,
        _tcp_addr: SocketAddr,
        _udp_addr: Option<SocketAddr>,
    ) {
    }

    fn reputation_change(&self, _peer_id: PeerId, _kind: ReputationChangeKind) {}

    async fn reputation_by_id(&self, _peer_id: PeerId) -> Result<Option<Reputation>, NetworkError> {
        Ok(None)
    }
}

impl<Net> NetworkEventListenerProvider for NoopNetwork<Net>
where
    Net: NetworkPrimitives,
{
    type Primitives = Net;
}

impl<Net> PeersHandleProvider for NoopNetwork<Net>
where
    Net: NetworkPrimitives,
{
    fn peers_handle(&self) -> &PeersHandle {
        &self.peers_handle
    }
}
