//! Fire-and-forget fanout of one rumor to every active peer over persistent links.
//!
//! The peer file is re-read on **every** spread (hot reload of membership) and links are
//! pruned to match: a peer the operator removes has its connection dropped. Each target
//! gets the rumor queued on its [`LinkPool`] link (one connection per peer, reused for
//! every message); a full link queue **applies backpressure** — the spread waits for the
//! session to drain it instead of dropping the rumor (a dropped rumor would leave a
//! permanent nonce hole at the receiver). The spread resolves when the peers acknowledge
//! or `request_timeout` elapses. Block relay awaits each spread, preserving per-peer
//! order; transactions detach each spread because pool order is not a mesh guarantee.

use crate::{
    identity::Identity,
    inject::Injector,
    link::{LinkPool, PendingRumor},
    peers::{self, PeerEntry},
    protocol::Limits,
    rumor::Rumor,
    seen::SeenCache,
    store::ObjectCache,
    sync_gate::SyncGate,
    ws::DEFAULT_REQUEST_TIMEOUT,
};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

/// Outbound gossip configuration.
#[derive(Debug, Clone)]
pub struct SpreadConfig {
    /// Path of the peer file (`mainnet.jsonl`), re-read per spread.
    pub peers_file: PathBuf,
    /// Our own mesh listen port — entries with this port are skipped as self.
    pub self_port: u16,
    /// How long one spread waits for the peers' delivery acks.
    pub request_timeout: Duration,
    /// Operator-tunable mesh limits (max frame size, inject slots, link queue).
    pub limits: Limits,
    /// This node's identity: signs every link's outbound `HELLO_PING`.
    pub identity: Arc<Identity>,
}

impl SpreadConfig {
    /// Creates a config with [`DEFAULT_REQUEST_TIMEOUT`] and default limits.
    pub fn new(peers_file: impl Into<PathBuf>, self_port: u16, identity: Arc<Identity>) -> Self {
        Self {
            peers_file: peers_file.into(),
            self_port,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            limits: Limits::default(),
            identity,
        }
    }
}

/// Outcome of one [`Spreader::spread`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpreadReport {
    /// Peers the rumor was offered to (`active && valid`, not self).
    pub targeted: usize,
    /// Peers that acknowledged delivery (`RUMOR_HAVE`, inject or short-circuit).
    pub delivered: usize,
    /// Peers that did not ack (link down/backing off, closed, timeout).
    pub failed: usize,
}

/// Spreads rumors to the peer file's active targets over the persistent link pool.
#[derive(Debug)]
pub struct Spreader {
    config: SpreadConfig,
    links: Arc<LinkPool>,
    missing_file_warned: AtomicBool,
}

impl Spreader {
    /// Creates a spreader with an empty link pool (links spawn on first use, or eagerly
    /// via [`Spreader::preconnect`]). The injector and caches wire every link session:
    /// serving counter-requests and accepting pushed data on our own outbound links.
    /// The gate is passed to every link session (bootstrap/pull notes).
    pub fn new(
        config: SpreadConfig,
        injector: Arc<dyn Injector>,
        seen: Arc<SeenCache>,
        objects: Arc<ObjectCache>,
        gate: Arc<SyncGate>,
    ) -> Self {
        let links = Arc::new(LinkPool::new(
            config.limits,
            config.identity.clone(),
            config.peers_file.clone(),
            injector,
            seen,
            objects,
            gate,
        ));
        Self {
            config,
            links,
            missing_file_warned: AtomicBool::new(false),
        }
    }

    /// The peer file this spreader re-reads on every spread.
    pub const fn peers_file(&self) -> &PathBuf {
        &self.config.peers_file
    }

    /// How many mesh links hold a live connection right now — the node status
    /// line's "X nodes connected".
    pub fn connected_count(&self) -> usize {
        self.links.connected_count()
    }

    /// The current target set: file entries with `active && valid`, excluding our own
    /// port.
    ///
    /// A missing file logs once at `info` and yields no targets (mesh outbound off); an
    /// unreadable file logs at `warn` each time. Neither is fatal — the inbound
    /// [`ws::serve`](crate::ws::serve) keeps running.
    pub fn targets(&self) -> Vec<PeerEntry> {
        let content = match std::fs::read_to_string(&self.config.peers_file) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if !self.missing_file_warned.swap(true, Ordering::Relaxed) {
                    info!(
                        target: "smartnet::mesh",
                        path = %self.config.peers_file.display(),
                        "mesh peers file not found — outbound gossip disabled"
                    );
                }
                return Vec::new();
            }
            Err(err) => {
                warn!(
                    target: "smartnet::mesh",
                    path = %self.config.peers_file.display(),
                    %err, "mesh peers file unreadable — outbound gossip skipped"
                );
                return Vec::new();
            }
        };

        let parsed = peers::parse_peers(&content);
        for error in &parsed.errors {
            warn!(target: "smartnet::mesh", path = %self.config.peers_file.display(), %error, "mesh peer line skipped");
        }
        parsed.peers.into_iter().filter(|peer| peer.is_target(self.config.self_port)).collect()
    }

    /// Dials every passed target **now** instead of lazily on first spread. Boot needs
    /// this: links used to spawn only when a rumor was first spread, so on a quiet chain
    /// a fresh node never connected out and never ran its bootstrap catch-up.
    ///
    /// Takes the [`Spreader::targets`] snapshot the caller already used to seed the
    /// readiness gate, so gate expectations and dialing come from one peer-file read.
    pub fn preconnect(&self, targets: &[PeerEntry]) {
        for peer in targets {
            self.links.ensure(&peer.ip, peer.port_range);
        }
    }

    /// Queues the rumor on every current target's link and waits for their acks.
    ///
    /// Membership is pruned first: links to peers no longer in the file are dropped, so
    /// an operator removing a peer tears its connection down on the next spread.
    pub async fn spread(&self, rumor: &Rumor) -> SpreadReport {
        let targets = self.targets();
        let keep: HashSet<(String, u16)> =
            targets.iter().map(|peer| (peer.ip.clone(), peer.port_range)).collect();
        self.links.retain(&keep);
        if targets.is_empty() {
            return SpreadReport::default();
        }

        let mut pending = Vec::with_capacity(targets.len());
        for peer in &targets {
            let tx = self.links.ensure(&peer.ip, peer.port_range);
            if !self.links.is_active(&peer.ip, peer.port_range) {
                debug!(
                    target: "smartnet::mesh",
                    ip = %peer.ip, port = peer.port_range,
                    "mesh link backing off — rumor skipped"
                );
                continue; // no ack queued: counted as failed via `targeted - delivered`
            }
            let (ack_tx, ack_rx) = oneshot::channel();
            pending.push((tx, peer.ip.clone(), peer.port_range, ack_tx, ack_rx));
        }

        // Queue on every live link concurrently: a full queue waits for the session to
        // drain it (backpressure) rather than dropping the rumor on the floor.
        let rumor = rumor.clone();
        let acks = futures::future::join_all(pending.into_iter().map(
            |(tx, ip, port, ack_tx, ack_rx)| {
                let rumor = rumor.clone();
                async move {
                    match tx.reserve().await {
                        Ok(permit) => {
                            permit.send(PendingRumor { rumor, ack: ack_tx });
                            Some(ack_rx)
                        }
                        Err(_) => {
                            debug!(
                                target: "smartnet::mesh",
                                %ip, port,
                                "mesh link closed — rumor dropped"
                            );
                            None // peer removed / link task gone: failed via targeted - delivered
                        }
                    }
                }
            },
        ))
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

        let timeout = self.config.request_timeout;
        let outcomes = futures::future::join_all(acks.into_iter().map(|ack| async move {
            tokio::time::timeout(timeout, ack).await.is_ok_and(|result| result.is_ok())
        }))
        .await;
        let delivered = outcomes.iter().filter(|ok| **ok).count();
        SpreadReport {
            targeted: targets.len(),
            delivered,
            failed: targets.len().saturating_sub(delivered),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use k256::ecdsa::SigningKey;

    fn test_identity() -> Arc<Identity> {
        let mut bytes = [0u8; 32];
        bytes[31] = 1;
        let key = SigningKey::from_slice(&bytes).expect("valid scalar");
        Arc::new(Identity::new(key, 1))
    }

    #[test]
    fn spread_config_carries_the_default_timeout() {
        let config = SpreadConfig::new("/tmp/peers", 30811, test_identity());
        assert_eq!(config.request_timeout, DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(config.self_port, 30811);
    }
}
