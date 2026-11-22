//! Persistent per-peer links — one `WebSocket` per peer, reused for every protocol message.
//!
//! The pool spawns one link task per peer; the task owns the connection: it connects
//! (bounded backoff on failure), runs the identity handshake, pipelines the queued
//! rumors on that one connection (announced in order, up to [`crate::ws::LINK_WINDOW`]
//! in flight), answers `PING` with `PONG_OK`, serves the peer's counter-requests, sends
//! its own `PING` when idle, and reconnects when the connection drops. There is no
//! connect-and-disconnect per message, no UDP, no secondary transport — the same binary
//! envelope carries everything.

use crate::{
    identity::Identity,
    inject::Injector,
    protocol::Limits,
    rumor::Rumor,
    seen::SeenCache,
    store::ObjectCache,
    sync_gate::SyncGate,
    ws,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    sync::{mpsc, oneshot},
    time::sleep,
};
use tracing::debug;

/// Rumors waiting on a link, with the spread ack resolved when the peer has them.
pub(crate) struct PendingRumor {
    pub(crate) rumor: Rumor,
    pub(crate) ack: oneshot::Sender<()>,
}

/// Per-link connectivity + birth time (grace window for the very first connect).
#[derive(Debug)]
pub(crate) struct LinkState {
    connected: AtomicBool,
    started: std::time::Instant,
}

impl LinkState {
    fn new() -> Self {
        Self { connected: AtomicBool::new(false), started: std::time::Instant::now() }
    }

    /// Connected, or still inside the first-connect grace (a young link that has not
    /// failed yet is worth one rumor — the queue holds it until the connect + handshake
    /// lands).
    fn is_active(&self) -> bool {
        self.connected.load(Ordering::Acquire) || self.started.elapsed() < ws::CONNECT_GRACE
    }

    /// A **live** connection — no grace window. What the status line reports as a
    /// connected node.
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub(crate) struct PeerLink {
    tx: mpsc::Sender<PendingRumor>,
    state: Arc<LinkState>,
}

/// One persistent connection per peer. Spawns a link task on first use per peer and
/// drops (and thereby stops) links to peers the operator removes from the file.
#[derive(Debug)]
pub struct LinkPool {
    links: Mutex<HashMap<(String, u16), PeerLink>>,
    limits: Limits,
    identity: Arc<Identity>,
    peers_file: PathBuf,
    injector: Arc<dyn Injector>,
    seen: Arc<SeenCache>,
    objects: Arc<ObjectCache>,
    gate: Arc<SyncGate>,
}

impl LinkPool {
    /// Creates a pool: operator limits plus the session wiring every link needs
    /// (identity for the handshake, peer file for the allowlist, injector/caches for
    /// serving counter-requests and accepting pushed data, and the boot readiness gate
    /// each session reports its bootstrap/pull state into).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        limits: Limits,
        identity: Arc<Identity>,
        peers_file: PathBuf,
        injector: Arc<dyn Injector>,
        seen: Arc<SeenCache>,
        objects: Arc<ObjectCache>,
        gate: Arc<SyncGate>,
    ) -> Self {
        Self { links: Mutex::default(), limits, identity, peers_file, injector, seen, objects, gate }
    }

    /// Queue sender for `ip:port`, spawning the peer's link task on first use.
    pub(crate) fn ensure(&self, ip: &str, port: u16) -> mpsc::Sender<PendingRumor> {
        let mut links = self.links.lock().expect("link pool poisoned");
        if let Some(link) = links.get(&(ip.to_owned(), port)) {
            return link.tx.clone();
        }
        let (tx, rx) = mpsc::channel(self.limits.link_queue_capacity);
        let state = Arc::new(LinkState::new());
        tokio::spawn(link_task(
            ip.to_owned(),
            port,
            rx,
            state.clone(),
            self.limits,
            self.identity.clone(),
            self.peers_file.clone(),
            self.injector.clone(),
            self.seen.clone(),
            self.objects.clone(),
            self.gate.clone(),
        ));
        links.insert((ip.to_owned(), port), PeerLink { tx: tx.clone(), state });
        tx
    }

    /// Whether gossip may target this peer right now: connected, or young enough that
    /// its first connect has not failed yet (grace). A peer in backoff is skipped —
    /// its link keeps retrying on its own and will take rumors again once it lands.
    pub(crate) fn is_active(&self, ip: &str, port: u16) -> bool {
        self.links
            .lock()
            .expect("link pool poisoned")
            .get(&(ip.to_owned(), port))
            .is_some_and(|link| link.state.is_active())
    }

    /// Drop links to peers no longer in `keep` — their tasks exit when the last sender
    /// (this map entry) goes away.
    /// How many links hold a **live** connection right now — the status line's
    /// "X nodes connected". A link still inside its first-connect grace does not
    /// count: it has not proven a connection yet.
    pub(crate) fn connected_count(&self) -> usize {
        self.links
            .lock()
            .expect("link pool poisoned")
            .values()
            .filter(|link| link.state.is_connected())
            .count()
    }

    pub(crate) fn retain(&self, keep: &HashSet<(String, u16)>) {
        self.links.lock().expect("link pool poisoned").retain(|key, _| keep.contains(key));
    }
}

/// Owns one peer's connection for the life of the pool entry: connect, handshake,
/// session, drain, backoff, reconnect — forever, until the pool drops the queue sender.
#[allow(clippy::too_many_arguments)]
async fn link_task(
    ip: String,
    port: u16,
    mut rx: mpsc::Receiver<PendingRumor>,
    state: Arc<LinkState>,
    limits: Limits,
    identity: Arc<Identity>,
    peers_file: PathBuf,
    injector: Arc<dyn Injector>,
    seen: Arc<SeenCache>,
    objects: Arc<ObjectCache>,
    gate: Arc<SyncGate>,
) {
    let mut backoff = ws::RECONNECT_MIN;
    loop {
        let started = std::time::Instant::now();
        match ws::run_link_session(
            &ip,
            port,
            &mut rx,
            &state.connected,
            limits,
            identity.as_ref(),
            &peers_file,
            &injector,
            &seen,
            &objects,
            &gate,
        )
        .await
        {
            Ok(()) => return, // pool dropped the sender (peer removed / process exit)
            Err(err) => {
                // The listener is up; a listed peer simply is not online yet. That is a
                // normal waiting state in the closed list — retry quietly at debug level.
                // The status line already reports `0 nodes connected`; no WARN here.
                debug!(target: "smartnet::mesh", %ip, port, %err, "mesh peer not reachable yet — retrying");
                // One handshake try per boot: a failed dial/handshake counts as this
                // link's boot chance (`peer_tip` 0 raises no max), so the readiness
                // gate moves on instead of waiting out its timeout on an absent or
                // dead peer. Idempotent — a later successful bootstrap overwrites
                // with real tips.
                gate.note_bootstrap(&format!("{ip}:{port}"), 0, 0);
            }
        }
        // At-most-once while down: drop what queued during the gap (bounded memory);
        // the next spread offers fresh rumors once the link is active again.
        while rx.try_recv().is_ok() {}
        // A session that held up proves the peer reachable — start over instead of
        // ratcheting to the ceiling after one blip.
        if started.elapsed() > limits.keepalive {
            backoff = ws::RECONNECT_MIN;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(ws::RECONNECT_MAX);
    }
}
