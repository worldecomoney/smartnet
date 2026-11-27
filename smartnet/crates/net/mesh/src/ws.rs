//! Binary `WebSocket` transport for the mesh.
//!
//! The mesh is all-to-all and closed-list: every peer pair keeps **one persistent
//! `WebSocket`** (`ws://ip:port/mesh`) that carries every protocol message — rumor
//! exchanges, sync dialogs, and keepalive pings alike. Outbound links are owned by
//! [`crate::link::LinkPool`] (connect + identity handshake once with bounded backoff,
//! reuse for every rumor, reconnect when the connection drops); this module provides the
//! link session loop, the inbound server, and the one-shot helpers
//! (`send_rumor`/`ping`/`fetch_peers`) used by tests and bootstrap.
//!
//! Every connection opens with the Layer-2 identity handshake: the initiator's first
//! frame is `HELLO_PING` (signed, fresh nonce), the responder answers `HELLO_PONG`
//! (echoed nonce, own signature) only after the initiator verifies against the peer-file
//! allowlist and the nonce passes the bounded replay cache. An unauthenticated peer
//! never reaches the protocol loop.
//!
//! The server runs each accepted connection until the peer closes, sends `PONG_DROP`,
//! or the line goes silent past [`crate::protocol::Limits::silence`]; its read loop stays serial
//! (framing, seen-checks, one writer per message) while local injects run on bounded
//! concurrent tasks — each data `PING_OK` is sent when *its* inject completes. Process-wide
//! inbound and outbound byte semaphores charge every retained frame before inject and
//! every encoded frame before send, capping mesh payload buffers at 256 MiB (with the
//! 64 MiB object cache) across every connection; idle readers hold no budget, and the
//! slot semaphore is taken before spawn so waiters cannot buffer more.
//! A link client keeps up to [`LINK_WINDOW`] announces in flight on its one connection,
//! so a burst of rumors is pipelined instead of paying one round-trip per object.
//!
//! Only binary frames are accepted. Text frames, JSON, unknown opcodes, wrong versions,
//! oversized frames, and non-`/mesh` handshake paths are rejected.
//!
//! Block injects are strictly serialized node-wide ([`block_inject_gate`], protocol rule:
//! no block batches — only tx batches); tx injects stay bounded-concurrent.

use crate::{
    config_hash::ConfigHashes,
    identity::{verify_hello, HelloVerify, Identity, NonceCache},
    inject::Injector,
    link::{PendingRumor, RejectedPeers},
    peers::{allowlist_addresses, load_peers, PeerEntry},
    preauth::{self, PreAuthGuard},
    protocol::{self, Limits, Message, PeerRecord, ProtocolError},
    rumor::{Rumor, RumorKind},
    seen::SeenCache,
    store::ObjectCache,
    sync_gate::SyncGate,
};
use alloy_primitives::{Address, Bytes};
use futures::{SinkExt, StreamExt};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore},
    time::MissedTickBehavior,
};
use tokio_tungstenite::{
    accept_hdr_async_with_config, client_async_with_config,
    tungstenite::{
        handshake::server::{ErrorResponse, Request, Response},
        http::StatusCode,
        protocol::{Message as WsMessage, WebSocketConfig},
        Error as WsError,
    },
    WebSocketStream,
};
use tracing::{debug, info, warn};

/// The only accepted `WebSocket` path.
pub const MESH_PATH: &str = "/mesh";
/// Time allowed for the `WebSocket` upgrade and the first `HELLO_PING` frame —
/// one-time, on connect. The handshake signs a fresh HELLO, and signing decrypts the
/// scrypt keystore transiently; that decryption is the dominant cost and must fit here.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Default per-operation timeout: one rumor exchange, one pong wait, one-shot helpers.
/// A freshly queued rumor can wait for the link's connect + HELLO before its exchange
/// starts, so this budget must cover one transient keystore decryption too.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// First reconnect delay after a link drops; doubles up to [`RECONNECT_MAX`].
pub const RECONNECT_MIN: Duration = Duration::from_millis(50);
/// Grace for a link that has not completed its first connect: a rumor may be queued
/// while the transient HELLO decryption is still running. Kept separate from the
/// reconnect backoff — that backoff is short, but a handshake is not.
pub const CONNECT_GRACE: Duration = Duration::from_secs(10);
/// Ceiling for the reconnect backoff delay. Kept short so a broken link comes back fast
/// (the mesh is always on; a peer is expected to be reachable again shortly).
pub const RECONNECT_MAX: Duration = Duration::from_millis(600);
/// Rumors one persistent link may have in flight at once: the client announces up to
/// this many before waiting for acks, so a burst pipelines over the single connection
/// instead of serializing on one round-trip (+ inject) per rumor.
pub const LINK_WINDOW: usize = 64;
/// Bounded responder-side `HELLO` nonce replay cache capacity.
pub const NONCE_CACHE_CAP: usize = 4096;

/// Node-wide single-block gate: the mesh injects exactly **one block at a time** into the
/// engine (protocol rule — no block batches anywhere; only tx batches are supported).
/// Shared by every path that injects a block: the inbound server's ordered pull and
/// stray-pong arm, `bootstrap_sync`, and each link session's pushed-block handler.
fn block_inject_gate() -> &'static Semaphore {
    static GATE: OnceLock<Semaphore> = OnceLock::new();
    GATE.get_or_init(|| Semaphore::new(1))
}

/// Process-wide inbound byte budget. A received frame charges its real size before it is
/// retained and keeps the reservation through inject; together with the send pool and the
/// object cache this is the hard 256 MiB ceiling.
fn mesh_recv_byte_budget() -> &'static Arc<Semaphore> {
    static BUDGET: OnceLock<Arc<Semaphore>> = OnceLock::new();
    BUDGET.get_or_init(|| Arc::new(Semaphore::new(protocol::MAX_RECV_BYTES)))
}

/// Process-wide outbound byte budget. `send_protocol` reserves the encoded frame's real
/// size before sending, so backpressured sends cannot pile up unbounded frame buffers.
fn mesh_send_byte_budget() -> &'static Arc<Semaphore> {
    static BUDGET: OnceLock<Arc<Semaphore>> = OnceLock::new();
    BUDGET.get_or_init(|| Arc::new(Semaphore::new(protocol::MAX_SEND_BYTES)))
}

/// Reads one frame, then charges its actual size to the hard byte cap *before* the
/// payload is retained by the caller. The permit is returned so payload-bearing messages
/// can hold it through inject; control frames drop it at the end of their handler.
/// Waiting on the socket never consumes the budget — only buffered payloads do.
async fn recv_protocol_budgeted<S>(
    socket: &mut S,
    limit: usize,
) -> Result<(Message, OwnedSemaphorePermit), TransportError>
where
    S: futures::Stream<Item = Result<WsMessage, WsError>> + Unpin,
{
    let (message, frame_len) = recv_protocol(socket, limit).await?;
    let reserve = mesh_recv_byte_budget()
        .clone()
        .acquire_many_owned(u32::try_from(frame_len).expect("clamped mesh frame size"))
        .await
        .expect("mesh byte budget closed");
    Ok((message, reserve))
}

/// Inbound `WebSocket` server limits.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Operator-tunable mesh limits (max frame size, inject slots, link queue, and the
    /// keepalive/silence timings both ends share).
    pub limits: Limits,
    /// Time allowed for the `WebSocket` handshake.
    pub handshake_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }
}

/// Transport/protocol failure for an outbound mesh operation.
#[derive(Debug, Error)]
pub enum TransportError {
    /// The operation exceeded its timeout.
    #[error("mesh operation timed out")]
    Timeout,
    /// `WebSocket` transport failure.
    #[error("websocket: {0}")]
    WebSocket(#[from] WsError),
    /// Binary protocol failure.
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    /// The peer sent a text frame (JSON and every other text payload are invalid).
    #[error("mesh peer sent a text frame; mesh payloads must be binary")]
    TextFrame,
    /// The peer closed before answering.
    #[error("mesh peer closed the connection")]
    Closed,
    /// Bootstrap config files differ: the named file's hash did not match.
    #[error("bootstrap config mismatch in {0}")]
    ConfigMismatch(&'static str),
}

/// Per-connection sync mode (spec: `MODE`). `Sync` while the peer's tip is ahead by two
/// or more blocks (or a reorg regressed our view); `Rumor` once tips are aligned. A mode
/// change never reconnects — it is per-connection state only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkMode {
    /// Peer is ahead — status/fetch dialogs drive catch-up.
    Sync,
    /// Tips aligned — announce/counter-request gossip is the steady state.
    Rumor,
}

/// What a layer handler decides for the connection session loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    /// Message handled — read the next one.
    Next,
    /// Fatal for this connection (already logged in the handler) — exit the session;
    /// the socket drops on return.
    Close,
}

/// Clears the boot gate's pull belief for `link` when the connection ends. Every exit
/// path drops it — a dead session can never leave the gate waiting on a pull that no
/// longer exists.
struct PullGuard {
    gate: Arc<SyncGate>,
    link: String,
}

impl Drop for PullGuard {
    fn drop(&mut self) {
        self.gate.note_pull(&self.link, false);
    }
}

/// Inbound connection handler: protocol state plus the local injection path
/// ([`Injector`] — in-process on the node, loopback HTTP in tests).
#[derive(Debug)]
pub struct MeshServer {
    peers_file: PathBuf,
    /// `validators.jsonl` path — hashed (with the peer file) to answer `CONFIG_HASH_PING`.
    validators_file: PathBuf,
    seen: Arc<SeenCache>,
    objects: Arc<ObjectCache>,
    rpc: Arc<dyn Injector>,
    /// This node's identity: signs our `HELLO` reply; `network_id` must match the peer's.
    identity: Arc<Identity>,
    /// Recently accepted initiator `HELLO` nonces (bounded replay cache).
    nonces: Mutex<NonceCache>,
    /// Boot readiness accounting: peer tips, pulls, and local tip observed here.
    gate: Arc<SyncGate>,
}

impl MeshServer {
    /// Creates a server handler.
    pub fn new(
        peers_file: PathBuf,
        validators_file: PathBuf,
        seen: Arc<SeenCache>,
        objects: Arc<ObjectCache>,
        rpc: Arc<dyn Injector>,
        identity: Arc<Identity>,
        gate: Arc<SyncGate>,
    ) -> Self {
        Self {
            peers_file,
            validators_file,
            seen,
            objects,
            rpc,
            identity,
            nonces: Mutex::new(NonceCache::new(NONCE_CACHE_CAP)),
            gate,
        }
    }

    /// Serves one accepted `WebSocket` until the peer closes, sends `PONG_DROP`, a frame
    /// is rejected, or the line stays silent past `limits.silence`.
    ///
    /// The read loop stays serial (framing, hash/seen checks, immediate short-circuit
    /// acks), but every new object injects on a spawned task bounded by
    /// [`ServerConfig::limits.concurrent_injects`]. The slot is taken **before** the task
    /// is spawned, so payload buffers waiting for a slot cannot pile up behind the
    /// semaphore. The exception is the ordered catch-up pull: its blocks inject inline
    /// (parent first, one at a time) and the chain head decides the next fetch. Writes
    /// (replies plus inject acks) share one sink behind a mutex; each data `PING_OK` is
    /// sent after *its* inject completes, so delivery reports stay truthful.
    pub async fn handle_connection<S>(
        &self,
        socket: WebSocketStream<S>,
        peer: SocketAddr,
        config: &ServerConfig,
        budget: &preauth::Budget,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sink, stream) = socket.split();
        let sink = Arc::new(tokio::sync::Mutex::new(sink));
        let slots = Arc::new(Semaphore::new(config.limits.concurrent_injects));
        let mut stream = stream;
        let limit = config.limits.max_message_bytes;

        // Layer-2 identity handshake: the first frame must be the peer's signed
        // `HELLO_PING`, verified against the allowlist (cheap gate) before the signature
        // is recovered, with the nonce checked against the replay cache. A failure here
        // closes the connection — an unauthenticated peer never reaches the protocol
        // loop. There is no blacklist: the peer may reconnect and be re-evaluated.
        //
        // Until this returns Ok the connection could read at most `preauth::PREAUTH_BUDGET`
        // bytes (the accept path wraps the raw stream in `PreAuthGuard`); a positive
        // verification lifts that cap for the rest of the session.
        if let Err(err) = self.handshake(&sink, &mut stream, peer, limit).await {
            warn!(target: "smartnet::mesh", %peer, %err, "mesh identity handshake failed");
            let _ = sink.lock().await.close().await;
            return;
        }
        preauth::arm(budget);

        let mut mode = LinkMode::Rumor;
        // Ordered catch-up on this connection: `(next_to_fetch, target)` — the range
        // `[local_tip + 1 ..= target]` pulled one block at a time (one fetch in flight,
        // the chain head deciding each advance). Armed by a block announce ahead of our
        // tip; `None` is the steady state. Per-connection only — a mode change or a
        // completed/aborted pull never reconnects.
        let mut pull: Option<(u64, u64)> = None;
        // Boot-gate accounting: report the pull state at the top of every turn (covers
        // every mutation path, `continue` included) and clear it via the guard on any
        // exit — a dead session can never leave the gate believing catch-up still runs.
        let link = peer.to_string();
        let _pull_guard = PullGuard { gate: self.gate.clone(), link: link.clone() };
        loop {
            self.gate.note_pull(&link, pull.is_some());
            // The received frame is charged to the byte cap before its payload is
            // retained; idle readers waiting on the socket hold no budget.
            let (message, reservation) = match tokio::time::timeout(
                config.limits.silence,
                recv_protocol_budgeted(&mut stream, limit),
            )
            .await
            {
                Ok(Ok(pair)) => pair,
                Ok(Err(TransportError::Closed)) => {
                    debug!(target: "smartnet::mesh", %peer, "mesh peer closed");
                    return;
                }
                Ok(Err(err)) => {
                    debug!(target: "smartnet::mesh", %peer, %err, "mesh frame rejected");
                    let _ = sink.lock().await.close().await;
                    return;
                }
                Err(_) => {
                    debug!(target: "smartnet::mesh", %peer, "mesh connection idle — closing");
                    let _ = sink.lock().await.close().await;
                    return;
                }
            };

            // Layer dispatch: each layer lives in its own `serve_*` handler (Tier-B
            // split); handlers return `Turn::Next` (keep reading) or `Turn::Close`
            // (session over — the socket drops). State-mutating handlers return the
            // updated `(mode, pull)` alongside the turn.
            let turn = match message {
                // ── Layer 1: keepalive + generic acks ───────────────────
                Message::Ping => self.serve_keepalive(&sink, limit, peer, pull).await,
                // ── Layer 3: bootstrap ──────────────────────────────────
                Message::BootstrapPeersPing => self.serve_bootstrap(&sink, limit, peer).await,
                // ── Layer 3b: bootstrap config gate (right after HELLO) ─
                Message::ConfigHashPing { .. } => {
                    self.serve_config_hash(&sink, limit, peer).await
                }
                // ── Layer 4: sync status + block fetch ──────────────────
                Message::SyncBlockPing { last_block } => {
                    let (turn, m, p) =
                        self.serve_sync_status(&sink, limit, peer, last_block, mode, pull).await;
                    mode = m;
                    pull = p;
                    turn
                }
                Message::SyncBlockRlpPing { block_nr } => {
                    self.serve_block_fetch(&sink, limit, peer, block_nr).await
                }
                Message::SyncBlockRlpPong { block_nr, block } => {
                    let (turn, m, p) = self
                        .serve_pulled_block(
                            &sink,
                            limit,
                            &slots,
                            reservation,
                            peer,
                            block_nr,
                            block,
                            mode,
                            pull,
                        )
                        .await;
                    mode = m;
                    pull = p;
                    turn
                }
                // ── Layer 5: block announce ─────────────────────────────
                Message::RumorBlocksPing { block_nr } => {
                    let (turn, m, p) =
                        self.serve_block_announce(&sink, limit, peer, block_nr, mode, pull).await;
                    mode = m;
                    pull = p;
                    turn
                }
                // ── Layer 6: tx announce + counter-request dialog ───────
                Message::RumorTxPing { tx_hash } => {
                    self.serve_tx_announce(&sink, limit, peer, tx_hash).await
                }
                Message::RumorTxRequestPing { tx_hash } => {
                    self.serve_tx_counter_request(&sink, limit, peer, tx_hash).await
                }
                Message::RumorTxRequestPong { tx_hash, tx } => {
                    self.serve_tx_counter_reply(&sink, limit, &slots, reservation, peer, tx_hash, tx)
                        .await
                }
                // Strays: the server never announces on an inbound connection, so
                // keepalive echoes, Layer-2 identity frames (stale after the
                // handshake), Layer-3 replies to dialogs this side does not own, and
                // SYNC_BLOCK_PONG (the requester's client dialog lives on its own
                // link) all pass without action.
                Message::PingOk
                | Message::PongOk
                | Message::HelloPing(_)
                | Message::HelloPong(_)
                | Message::BootstrapPeersPong { .. }
                | Message::ConfigHashPong { .. }
                | Message::SyncBlockPong { .. } => Turn::Next,
                Message::PongDrop => {
                    debug!(target: "smartnet::mesh", %peer, "mesh peer dropped the connection");
                    Turn::Close
                }
            };
            match turn {
                Turn::Next => {}
                Turn::Close => return,
            }
        }
    }

    // ── Layer handlers (Tier-B split of `handle_connection`) ────────────────
    //
    // One handler per protocol layer. Each returns [`Turn`]; the state-mutating
    // handlers additionally return the updated `(LinkMode, pull)` so the session
    // loop keeps its per-connection state. Behavior is identical to the inline
    // arms this split extracted from (log-then-close on write failure, no receipt
    // on failed injects, etc.).

    /// Layer 1 — keepalive: answer `PING` with `PONG_OK`, then use the heartbeat as a
    /// second chance for a pending pull fetch whose answer failed transiently (the next
    /// announce may be a long time coming).
    async fn serve_keepalive<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        limit: usize,
        peer: SocketAddr,
        pull: Option<(u64, u64)>,
    ) -> Turn
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
    {
        if let Err(err) = send_via(sink, &Message::PongOk, limit).await {
            debug!(target: "smartnet::mesh", %peer, %err, "mesh pong write failed");
            return Turn::Close;
        }
        if let Some((next, _)) = pull
            && let Err(err) =
                send_via(sink, &Message::SyncBlockRlpPing { block_nr: next }, limit).await
        {
            debug!(target: "smartnet::mesh", %peer, %err, "mesh fetch write failed");
            return Turn::Close;
        }
        Turn::Next
    }

    /// Layer 3 — bootstrap: answer the peer list request from the peer file.
    async fn serve_bootstrap<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        limit: usize,
        peer: SocketAddr,
    ) -> Turn
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
    {
        match bootstrap_message(&self.peers_file) {
            Ok(reply) => {
                if let Err(err) = send_via(sink, &reply, limit).await {
                    debug!(target: "smartnet::mesh", %peer, %err, "mesh bootstrap response failed");
                    return Turn::Close;
                }
            }
            Err(err) => {
                debug!(target: "smartnet::mesh", %peer, %err, "mesh bootstrap response failed");
                return Turn::Close;
            }
        }
        Turn::Next
    }

    /// Layer 4 — sync status: report our tip (boot-gate evidence), track the peer's
    /// reported tip, flip `mode`, and arm/extend the ordered pull when the peer is
    /// ahead — the status exchange is what lets a behind node finish catch-up at boot
    /// without waiting for an announce that may never come on a quiet chain.
    async fn serve_sync_status<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        limit: usize,
        peer: SocketAddr,
        last_block: u64,
        mut mode: LinkMode,
        mut pull: Option<(u64, u64)>,
    ) -> (Turn, LinkMode, Option<(u64, u64)>)
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
    {
        let Ok(our_tip) = self.rpc.last_block_nr().await else {
            debug!(target: "smartnet::mesh", %peer, "mesh tip unavailable — status unanswered");
            return (Turn::Next, mode, pull);
        };
        // Boot-gate evidence: the peer's reported tip and our own head.
        self.gate.note_peer_tip(last_block);
        self.gate.note_local_tip(our_tip);
        let previous = mode;
        if last_block > our_tip + 1 {
            mode = LinkMode::Sync;
        } else if last_block <= our_tip && pull.is_none() {
            mode = LinkMode::Rumor;
        }
        if mode != previous {
            debug!(target: "smartnet::mesh", %peer, ?mode, last_block, our_tip, "mesh mode change");
        }
        if let Err(err) =
            send_via(sink, &Message::SyncBlockPong { last_block: our_tip }, limit).await
        {
            debug!(target: "smartnet::mesh", %peer, %err, "mesh status write failed");
            return (Turn::Close, mode, pull);
        }
        // Retrieve now (mirrors the announce path: first exchange reports height,
        // second retrieves) instead of waiting for an announce that may never come.
        if last_block > our_tip {
            let next = match pull {
                Some((next, target)) => {
                    pull = Some((next, target.max(last_block)));
                    next
                }
                None => {
                    let next = our_tip + 1;
                    pull = Some((next, last_block));
                    next
                }
            };
            if let Err(err) =
                send_via(sink, &Message::SyncBlockRlpPing { block_nr: next }, limit).await
            {
                debug!(target: "smartnet::mesh", %peer, %err, "mesh fetch write failed");
                return (Turn::Close, mode, pull);
            }
        }
        (Turn::Next, mode, pull)
    }

    /// Layer 4 — block fetch (serving side): answer a `SYNC_BLOCK_RLP_PING` from the
    /// chain. A miss is a no-op — the peer's heartbeat re-asks.
    async fn serve_block_fetch<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        limit: usize,
        peer: SocketAddr,
        block_nr: u64,
    ) -> Turn
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
    {
        match self.rpc.block_rlp(block_nr).await {
            Ok(Some(block)) => {
                if let Err(err) =
                    send_via(sink, &Message::SyncBlockRlpPong { block_nr, block }, limit).await
                {
                    debug!(target: "smartnet::mesh", %peer, %err, "mesh block write failed");
                    return Turn::Close;
                }
            }
            Ok(None) => {
                debug!(target: "smartnet::mesh", %peer, block_nr, "mesh block not local — fetch unanswered");
            }
            Err(err) => {
                debug!(target: "smartnet::mesh", %peer, block_nr, %err, "mesh block read failed");
            }
        }
        Turn::Next
    }

    /// Layer 4 — pulled block (ordered catch-up + stray): the block the ordered pull is
    /// waiting for injects inline (parent first, one at a time) and advances the range
    /// from the injected nr itself; strays inject bounded-concurrent (the global
    /// single-block gate serializes block injects node-wide). The byte reservation is
    /// held through inject, including the inline pull path.
    #[allow(clippy::too_many_arguments)] // layer plumbing: mirrors the session loop
    async fn serve_pulled_block<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        limit: usize,
        slots: &Arc<Semaphore>,
        byte_permit: OwnedSemaphorePermit,
        peer: SocketAddr,
        block_nr: u64,
        block: alloy_primitives::Bytes,
        mut mode: LinkMode,
        mut pull: Option<(u64, u64)>,
    ) -> (Turn, LinkMode, Option<(u64, u64)>)
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + Send + 'static,
    {
        let rumor = Rumor::block(block_nr, block);
        if !rumor.hash_matches() {
            // Unreachable by construction (hash = keccak of bytes).
            return (Turn::Next, mode, pull);
        }
        let fresh = self.seen.check_and_mark(rumor.hash);
        match pull {
            // The block the ordered pull is waiting for: inject inline
            // (parent first, one at a time), then advance the range from
            // the injected nr itself — the head read only skips ahead,
            // never gates progress (re-injecting an already-known block
            // is idempotent, so a duplicate fetch racing a parallel
            // link still converges).
            Some((next, target)) if next == block_nr => {
                self.objects.insert(rumor.clone());
                if let Err(err) = Self::inject(self.rpc.as_ref(), &rumor).await {
                    warn!(target: "smartnet::mesh", %peer, kind = "block", %err, "local inject failed");
                    // No receipt on a failed inject (spec); the next announce
                    // or a reconnect's status bootstrap restarts the range.
                    pull = None;
                    return (Turn::Next, mode, pull);
                }
                if let Err(err) = send_via(sink, &Message::PingOk, limit).await {
                    debug!(target: "smartnet::mesh", %peer, %err, "mesh ping_ok write failed");
                    return (Turn::Close, mode, pull);
                }
                match self.rpc.last_block_nr().await {
                    // Head reached (or passed — a parallel sync may have
                    // jumped it) the announce that armed the pull: catch-up
                    // done, the connection stitches back into rumor.
                    Ok(tip) if tip >= target => {
                        self.gate.note_local_tip(tip);
                        pull = None;
                        let previous = mode;
                        mode = LinkMode::Rumor;
                        if mode != previous {
                            debug!(target: "smartnet::mesh", %peer, ?mode, tip, target, "mesh mode change");
                        }
                    }
                    // Advance from the best evidence of progress: a head
                    // that raced ahead skips to head+1, otherwise the block
                    // just injected decides. The head read can transiently
                    // lag the canonical event by µs — that is not a stall,
                    // so never abort on it (a quiet chain would strand the
                    // range with nothing left to re-arm it).
                    Ok(tip) => {
                        self.gate.note_local_tip(tip);
                        let next_nr = tip.max(block_nr) + 1;
                        if next_nr > target {
                            pull = None;
                            debug!(target: "smartnet::mesh", %peer, tip, target, "mesh range complete — head settling");
                        } else {
                            pull = Some((next_nr, target));
                            if let Err(err) = send_via(
                                sink,
                                &Message::SyncBlockRlpPing { block_nr: next_nr },
                                limit,
                            )
                            .await
                            {
                                debug!(target: "smartnet::mesh", %peer, %err, "mesh fetch write failed");
                                return (Turn::Close, mode, pull);
                            }
                        }
                    }
                    Err(err) => {
                        debug!(target: "smartnet::mesh", %peer, %err, "mesh tip unavailable — aborting pull");
                        pull = None;
                    }
                }
            }
            // Stray or the aligned announce reply racing the ordered range:
            // fresh objects inject bounded-concurrent (receipt when that
            // inject completes), already-known ones only get the receipt.
            _ => {
                if !fresh {
                    let _ = send_via(sink, &Message::PingOk, limit).await;
                    return (Turn::Next, mode, pull);
                }
                self.objects.insert(rumor.clone());
                // Take the slot in the read loop, before the payload is handed to a task:
                // a task waiting on the semaphore would still hold its 32 MiB buffer.
                let Ok(permit) = Arc::clone(slots).acquire_owned().await else {
                    return (Turn::Next, mode, pull);
                };
                let sink = sink.clone();
                let rpc = self.rpc.clone();
                let gate = self.gate.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _byte_permit = byte_permit;
                    match Self::inject(rpc.as_ref(), &rumor).await {
                        Err(err) => {
                            warn!(target: "smartnet::mesh", %peer, kind = "block", %err, "local inject failed");
                        }
                        Ok(()) => {
                            if let Ok(tip) = rpc.last_block_nr().await {
                                gate.note_local_tip(tip);
                            }
                        }
                    }
                    if let Err(err) = send_via(&sink, &Message::PingOk, limit).await {
                        debug!(target: "smartnet::mesh", %peer, %err, "mesh ping_ok write failed");
                    }
                });
            }
        }
        (Turn::Next, mode, pull)
    }

    /// Layer 5 — block announce: boot-gate evidence, mode flip, the aligned fetch for
    /// the announced block itself, and arming/extending the ordered catch-up range (the
    /// re-send is what keeps a pull alive after a transient miss).
    async fn serve_block_announce<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        limit: usize,
        peer: SocketAddr,
        block_nr: u64,
        mut mode: LinkMode,
        mut pull: Option<(u64, u64)>,
    ) -> (Turn, LinkMode, Option<(u64, u64)>)
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
    {
        let Ok(our_tip) = self.rpc.last_block_nr().await else {
            debug!(target: "smartnet::mesh", %peer, "mesh tip unavailable — announce unanswered");
            return (Turn::Next, mode, pull);
        };
        // Boot-gate evidence: the announced block is a peer-tip floor and
        // our head a local floor (max-merged, so a stale announce is a no-op).
        self.gate.note_peer_tip(block_nr);
        self.gate.note_local_tip(our_tip);
        if block_nr > our_tip {
            if block_nr >= our_tip + 2 {
                let previous = mode;
                mode = LinkMode::Sync;
                if mode != previous {
                    debug!(target: "smartnet::mesh", %peer, ?mode, block_nr, our_tip, "mesh mode change");
                }
            }
            // Aligned Layer-5 reply: the announced block itself (we do not
            // have it — that is why the announce is a fetch).
            if let Err(err) =
                send_via(sink, &Message::SyncBlockRlpPing { block_nr }, limit).await
            {
                debug!(target: "smartnet::mesh", %peer, %err, "mesh fetch write failed");
                return (Turn::Close, mode, pull);
            }
            // The ordered catch-up range `[our_tip + 1 ..= block_nr]`: arm it
            // (or extend the target), then (re)send its first fetch.
            let next = match pull {
                Some((next, target)) => {
                    pull = Some((next, target.max(block_nr)));
                    next
                }
                None => {
                    let next = our_tip + 1;
                    pull = Some((next, block_nr));
                    next
                }
            };
            if let Err(err) =
                send_via(sink, &Message::SyncBlockRlpPing { block_nr: next }, limit).await
            {
                debug!(target: "smartnet::mesh", %peer, %err, "mesh fetch write failed");
                return (Turn::Close, mode, pull);
            }
        } else {
            let previous = mode;
            if pull.is_none() {
                mode = LinkMode::Rumor;
            }
            if mode != previous {
                debug!(target: "smartnet::mesh", %peer, ?mode, block_nr, our_tip, "mesh mode change");
            }
            if let Err(err) = send_via(sink, &Message::PongOk, limit).await {
                debug!(target: "smartnet::mesh", %peer, %err, "mesh have write failed");
                return (Turn::Close, mode, pull);
            }
        }
        (Turn::Next, mode, pull)
    }

    /// Layer 6 — tx announce: answer with `PONG_OK` (have) or a counter-request (need).
    async fn serve_tx_announce<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        limit: usize,
        peer: SocketAddr,
        tx_hash: [u8; 32],
    ) -> Turn
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
    {
        let have = self.seen.contains(&tx_hash) || self.objects.contains(&tx_hash);
        let response = if have {
            Message::PongOk
        } else {
            Message::RumorTxRequestPing { tx_hash }
        };
        if let Err(err) = send_via(sink, &response, limit).await {
            debug!(target: "smartnet::mesh", %peer, %err, "mesh announce response failed");
            return Turn::Close;
        }
        Turn::Next
    }

    /// Layer 6 — tx counter-request: serve the bytes from the object cache if we hold them.
    async fn serve_tx_counter_request<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        limit: usize,
        peer: SocketAddr,
        tx_hash: [u8; 32],
    ) -> Turn
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
    {
        if let Some(rumor) = self.objects.get(&tx_hash)
            && let Err(err) = send_via(
                sink,
                &Message::RumorTxRequestPong { tx_hash, tx: rumor.data.clone() },
                limit,
            )
            .await
            {
                debug!(target: "smartnet::mesh", %peer, %err, "mesh tx write failed");
                return Turn::Close;
            }
        Turn::Next
    }

    /// Layer 6 — tx counter-reply: seen-check, then inject bounded-concurrent (tx injects
    /// are not gated by the single-block rule) with a truthful receipt after the inject.
    /// Both the slot and the byte reservation are taken by the read loop before spawn.
    #[allow(clippy::too_many_arguments)] // layer plumbing: mirrors the session loop
    async fn serve_tx_counter_reply<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        limit: usize,
        slots: &Arc<Semaphore>,
        byte_permit: OwnedSemaphorePermit,
        peer: SocketAddr,
        tx_hash: [u8; 32],
        tx: alloy_primitives::Bytes,
    ) -> Turn
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + Send + 'static,
    {
        let rumor = Rumor::tx(tx);
        if rumor.hash != tx_hash {
            warn!(target: "smartnet::mesh", %peer, "mesh tx hash mismatch");
            return Turn::Next;
        }
        if !self.seen.check_and_mark(tx_hash) {
            let _ = send_via(sink, &Message::PingOk, limit).await;
            return Turn::Next;
        }
        self.objects.insert(rumor.clone());
        let Ok(permit) = Arc::clone(slots).acquire_owned().await else {
            return Turn::Next;
        };
        let sink = sink.clone();
        let rpc = self.rpc.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _byte_permit = byte_permit;
            if let Err(err) = Self::inject(rpc.as_ref(), &rumor).await {
                warn!(target: "smartnet::mesh", %peer, kind = "tx", %err, "local inject failed");
            }
            if let Err(err) = send_via(&sink, &Message::PingOk, limit).await {
                debug!(target: "smartnet::mesh", %peer, %err, "mesh ping_ok write failed");
            }
        });
        Turn::Next
    }


    /// Verifies the peer's first-frame `HELLO_PING` and replies with our signed
    /// `HELLO_PONG` (the responder side).
    ///
    /// Order matches the spec: parse → version/network → allowlist (no recovery) →
    /// nonce replay check → signature recovery + address match. Only a verified fresh
    /// `HELLO_PING` gets our signed reply; anything else is an error the caller turns
    /// into a close.
    async fn handshake<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        stream: &mut (impl futures::Stream<Item = Result<WsMessage, WsError>> + Unpin),
        peer: SocketAddr,
        limit: usize,
    ) -> Result<(), TransportError>
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
    {
        // The inbound allowlist is the peer file's `address` identities, re-read so
        // membership changes apply without a restart.
        let allowlist = match load_peers(&self.peers_file) {
            Ok(mut parsed) => {
                // Label our own entry in the in-RAM peer set; the allowlist logic below is
                // deliberately unchanged.
                PeerEntry::mark_self(&mut parsed.peers, None, self.identity.address());
                allowlist_addresses(&parsed)
            }
            Err(err) => {
                warn!(
                    target: "smartnet::mesh",
                    path = %self.peers_file.display(),
                    %err, "mesh peer file unreadable — inbound handshake has an empty allowlist"
                );
                Vec::new()
            }
        };

        let (message, _budget) =
            tokio::time::timeout(DEFAULT_HANDSHAKE_TIMEOUT, recv_protocol_budgeted(stream, limit))
                .await
                .map_err(|_| TransportError::Timeout)??;

        let Message::HelloPing(hello) = message else {
            return Err(TransportError::Protocol(ProtocolError::Malformed(
                "first mesh frame must be HELLO_PING",
            )));
        };

        // Replay gate first — cheap membership probe BEFORE signature recovery (spec
        // order: allowlist → replay → signature). Insert only after a successful
        // verify (below) so a bad-signature frame can never poison the cache with
        // attacker-chosen nonces; the post-verify insert still catches a parallel
        // race on the same nonce.
        if self.nonces.lock().expect("nonce cache poisoned").contains(&hello.initiator_nonce) {
            warn!(target: "smartnet::mesh", %peer, "mesh HELLO nonce replay — dropping");
            return Err(TransportError::Protocol(ProtocolError::Malformed("HELLO nonce replay")));
        }

        match verify_hello(&hello, self.identity.network_id(), &allowlist, true) {
            HelloVerify::Authenticated(address) => {
                debug!(
                    target: "smartnet::mesh",
                    %peer,
                    initiator = %format!("{address:#x}"),
                    "mesh inbound HELLO authenticated"
                );
            }
            HelloVerify::NotAllowlisted => {
                return Err(TransportError::Protocol(ProtocolError::Malformed(
                    "HELLO initiator not in allowlist",
                )));
            }
            HelloVerify::BadSignature => {
                return Err(TransportError::Protocol(ProtocolError::Malformed(
                    "HELLO signature invalid",
                )));
            }
            HelloVerify::Mismatched { field } => {
                return Err(TransportError::Protocol(ProtocolError::Malformed(
                    match field {
                        "version" => "HELLO version mismatch",
                        _ => "HELLO network_id mismatch",
                    },
                )));
            }
        }

        // Record the fresh nonce (post-verify, bounded cache — no unbounded memory, no
        // per-peer state that outlives the connection).
        {
            let mut nonces = self.nonces.lock().expect("nonce cache poisoned");
            if !nonces.insert(hello.initiator_nonce) {
                warn!(target: "smartnet::mesh", %peer, "mesh HELLO nonce replay — dropping");
                return Err(TransportError::Protocol(ProtocolError::Malformed(
                    "HELLO nonce replay",
                )));
            }
        }

        // Reply with our signed `HELLO_PONG`, echoing the initiator's nonce and identity.
        let reply = self.identity.sign_hello_responder(&hello.initiator, &hello.initiator_nonce);
        send_via(sink, &Message::HelloPong(reply), limit).await
    }

    /// Answers the initiator's bootstrap config gate with OUR two file digests, computed
    /// fresh from disk on every request (never cached). If either file can't be read the
    /// connection is closed — a node that cannot prove its files is not a bootstrap peer.
    async fn serve_config_hash<S>(
        &self,
        sink: &Arc<tokio::sync::Mutex<S>>,
        limit: usize,
        peer: SocketAddr,
    ) -> Turn
    where
        S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
    {
        let hashes = match ConfigHashes::read(&self.peers_file, &self.validators_file) {
            Ok(hashes) => hashes,
            Err(err) => {
                warn!(
                    target: "smartnet::mesh",
                    %peer,
                    mainnet = %self.peers_file.display(),
                    validators = %self.validators_file.display(),
                    %err,
                    "mesh config hash unavailable — dropping bootstrap peer"
                );
                return Turn::Close;
            }
        };
        let reply = Message::ConfigHashPong { mainnet: hashes.mainnet, validators: hashes.validators };
        match send_via(sink, &reply, limit).await {
            Ok(()) => Turn::Next,
            Err(err) => {
                debug!(target: "smartnet::mesh", %peer, %err, "mesh config hash reply failed");
                Turn::Close
            }
        }
    }

    async fn inject(rpc: &dyn Injector, rumor: &Rumor) -> Result<(), String> {
        match rumor.kind {
            RumorKind::Tx => rpc.send_raw_transaction(&rumor.data).await,
            RumorKind::Block => {
                // Protocol rule: one block at a time into the engine — the node-wide
                // gate serializes every block inject regardless of which connection
                // (server pull, stray pong, bootstrap, link session) delivered it.
                let _permit = block_inject_gate()
                    .acquire()
                    .await
                    .map_err(|_| "block inject gate closed".to_string())?;
                rpc.new_payload(&rumor.data).await
            }
        }
    }
}

/// Writes one protocol message through a shared sink half.
async fn send_via<S>(
    sink: &Arc<tokio::sync::Mutex<S>>,
    message: &Message,
    limit: usize,
) -> Result<(), TransportError>
where
    S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
{
    let mut guard = sink.lock().await;
    send_protocol(&mut *guard, message, limit).await
}

/// The bootstrap answer built from the peer file: canonical list hash + records.
fn bootstrap_message(peers_file: &Path) -> Result<Message, TransportError> {
    let parsed = match load_peers(peers_file) {
        Ok(parsed) => parsed,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(err) => {
            warn!(
                target: "smartnet::mesh",
                path = %peers_file.display(),
                %err,
                "mesh peer file unreadable for bootstrap"
            );
            Default::default()
        }
    };
    for error in &parsed.errors {
        warn!(target: "smartnet::mesh", %error, "mesh peer line skipped");
    }
    let peers: Vec<PeerRecord> = parsed.peers.into_iter().map(PeerRecord::from).collect();
    let list_hash = protocol::peer_list_hash(&peers)?;
    Ok(Message::BootstrapPeersPong { list_hash, peers })
}

/// The inbound-`HELLO` allowlist from a peer file (re-read at each use so membership
/// changes need no restart). An unreadable file yields an empty allowlist (everyone
/// fails the handshake) with a warning — fail-closed, never fail-open.
fn load_allowlist(peers_file: &Path) -> Vec<Address> {
    match load_peers(peers_file) {
        Ok(parsed) => allowlist_addresses(&parsed),
        Err(err) => {
            warn!(
                target: "smartnet::mesh",
                path = %peers_file.display(),
                %err, "mesh peer file unreadable — outbound handshake has an empty allowlist"
            );
            Vec::new()
        }
    }
}

impl From<PeerEntry> for PeerRecord {
    fn from(peer: PeerEntry) -> Self {
        Self {
            key_id: peer.key_id,
            ip: peer.ip,
            port: peer.port_range,
            active: peer.active,
            valid: peer.valid,
        }
    }
}

/// Accepts inbound `WebSocket` mesh connections until the listener fails.
///
/// Each accepted connection runs in its own task: one peer's exchange (or a stalled
/// peer) never blocks another's. Within a connection the read loop of
/// [`MeshServer::handle_connection`] stays serial while injects run bounded-concurrent.
pub async fn serve(
    listener: TcpListener,
    server: Arc<MeshServer>,
    config: ServerConfig,
) -> std::io::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                warn!(target: "smartnet::mesh", %err, "mesh accept failed");
                continue;
            }
        };

        let server = server.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let callback = |request: &Request, response: Response| {
                if request.uri().path() == MESH_PATH {
                    Ok(response)
                } else {
                    let mut error = ErrorResponse::new(Some("not a mesh endpoint".to_string()));
                    *error.status_mut() = StatusCode::NOT_FOUND;
                    Err(error)
                }
            };
            // Pre-auth byte cap: the raw stream is wrapped so a not-yet-verified peer
            // can read at most `preauth::PREAUTH_BUDGET` bytes (upgrade + HELLO) until
            // `handle_connection` arms the shared budget after a positive HELLO.
            let budget = preauth::restricted();
            let guarded = PreAuthGuard::new(stream, budget.clone());
            let websocket_config = websocket_config(config.limits.max_message_bytes);
            let handshake = tokio::time::timeout(
                config.handshake_timeout,
                accept_hdr_async_with_config(guarded, callback, Some(websocket_config)),
            )
            .await;
            let socket = match handshake {
                Ok(Ok(socket)) => socket,
                Ok(Err(err)) => {
                    debug!(target: "smartnet::mesh", %peer, %err, "mesh handshake rejected");
                    return;
                }
                Err(_) => {
                    debug!(target: "smartnet::mesh", %peer, "mesh handshake timed out");
                    return;
                }
            };

            server.handle_connection(socket, peer, &config, &budget).await;
        });
    }
}

/// Announces `rumor` to one peer over a fresh connection (identity handshake first) and
/// serves the counter-request with the bytes. One-shot helper for tests/bootstrap —
/// gossip uses the persistent link session instead.
///
/// Returns after the peer says it already has the object (`PONG_OK` to the announce) or
/// acknowledges injected bytes (`PING_OK` after our data `PONG`; a close after the write
/// also counts — the receiver injects before acking/closing).
pub async fn send_rumor(
    ip: &str,
    port: u16,
    rumor: &Rumor,
    timeout: Duration,
    identity: &Identity,
    allowlist: &[Address],
) -> Result<(), TransportError> {
    tokio::time::timeout(timeout, send_rumor_inner(ip, port, rumor, identity, allowlist))
        .await
        .map_err(|_| TransportError::Timeout)?
}

async fn send_rumor_inner(
    ip: &str,
    port: u16,
    rumor: &Rumor,
    identity: &Identity,
    allowlist: &[Address],
) -> Result<(), TransportError> {
    let limit = Limits::default().max_message_bytes;
    let mut socket = connect(ip, port, limit).await?;
    handshake_client(&mut socket, identity, allowlist, limit).await?;
    socket.get_mut().arm();
    exchange(&mut socket, rumor, limit).await?;
    let _ = socket.close(None).await;
    Ok(())
}

/// One announce/counter-request/data/`PING_OK` exchange for `rumor` on an
/// already-connected (handshaken) socket.
///
/// Completion = the peer confirmed the object: `PONG_OK` to the announce (already had
/// it), or `PING_OK` after receiving our data `PONG` (injected). A close after the data
/// write also counts — the receiver injects before it closes.
async fn exchange<S>(
    socket: &mut WebSocketStream<S>,
    rumor: &Rumor,
    limit: usize,
) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, exchange_inner(socket, rumor, limit))
        .await
        .map_err(|_| TransportError::Timeout)?
}

async fn exchange_inner<S>(
    socket: &mut WebSocketStream<S>,
    rumor: &Rumor,
    limit: usize,
) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match rumor.kind {
        RumorKind::Tx => {
            send_protocol(socket, &Message::RumorTxPing { tx_hash: rumor.hash }, limit).await?;
            loop {
                let (reply, _budget) = recv_protocol_budgeted(socket, limit).await?;
                match reply {
                    Message::PongOk => return Ok(()),
                    Message::RumorTxRequestPing { tx_hash } if tx_hash == rumor.hash => {
                        send_protocol(
                            socket,
                            &Message::RumorTxRequestPong { tx_hash, tx: rumor.data.clone() },
                            limit,
                        )
                        .await?;
                        return wait_for_delivery(socket, limit).await;
                    }
                    Message::Ping => send_protocol(socket, &Message::PongOk, limit).await?,
                    _ => {}
                }
            }
        }
        RumorKind::Block => {
            let block_nr = rumor.block_nr.expect("block rumors carry a block number");
            send_protocol(socket, &Message::RumorBlocksPing { block_nr }, limit).await?;
            loop {
                let (reply, _budget) = recv_protocol_budgeted(socket, limit).await?;
                match reply {
                    Message::PongOk => return Ok(()),
                    Message::SyncBlockRlpPing { block_nr: requested }
                        if requested == block_nr =>
                    {
                        send_protocol(
                            socket,
                            &Message::SyncBlockRlpPong { block_nr, block: rumor.data.clone() },
                            limit,
                        )
                        .await?;
                        return wait_for_delivery(socket, limit).await;
                    }
                    Message::Ping => send_protocol(socket, &Message::PongOk, limit).await?,
                    _ => {}
                }
            }
        }
    }
}

/// After our data `PONG`: the truthful close is `PING_OK` (sent post-inject); a close
/// also counts because the receiver injects before closing.
async fn wait_for_delivery<S>(
    socket: &mut WebSocketStream<S>,
    limit: usize,
) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match recv_protocol_budgeted(socket, limit).await {
            Ok((Message::PingOk, _)) | Err(TransportError::Closed) => return Ok(()),
            Ok((Message::Ping, _)) => send_protocol(socket, &Message::PongOk, limit).await?,
            Ok(_) => {}
            Err(err) => return Err(err),
        }
    }
}

/// Initiator side of the Layer-2 handshake: sends our signed `HELLO_PING`, verifies the
/// peer's `HELLO_PONG` against the allowlist (authenticating the responder) **and**
/// against our outstanding nonce (the replay defense — a captured `HELLO_PONG` is
/// useless on any other connection). Runs on every outbound connection before any
/// other frame — the spec's first-frame rule.
async fn handshake_client<S>(
    socket: &mut S,
    identity: &Identity,
    allowlist: &[Address],
    limit: usize,
) -> Result<(), TransportError>
where
    S: futures::Sink<WsMessage, Error = WsError>
        + futures::Stream<Item = Result<WsMessage, WsError>>
        + Unpin,
{
    let hello = identity.sign_hello_initiator();
    // The outstanding nonce: the reply must echo it verbatim. Stored before the send so
    // a fast (or replayed) reply can never race an unset value.
    let outstanding_nonce = hello.initiator_nonce;
    let our_initiator = hello.initiator;
    send_protocol(socket, &Message::HelloPing(hello), limit).await?;
    let (reply, _budget) =
        tokio::time::timeout(DEFAULT_HANDSHAKE_TIMEOUT, recv_protocol_budgeted(socket, limit))
            .await
            .map_err(|_| TransportError::Timeout)??;
    match reply {
        Message::HelloPong(reply) => {
            // Replay defense first (cheap): the echoed initiator nonce must be the one
            // we generated for THIS connection, the initiator claim must be our own
            // address, and the responder nonce must be fresh (non-zero).
            if reply.initiator_nonce != outstanding_nonce {
                return Err(TransportError::Protocol(ProtocolError::Malformed(
                    "HELLO_PONG initiator nonce mismatch",
                )));
            }
            if reply.initiator != our_initiator {
                return Err(TransportError::Protocol(ProtocolError::Malformed(
                    "HELLO_PONG initiator mismatch",
                )));
            }
            if reply.responder_nonce == [0u8; 32] {
                return Err(TransportError::Protocol(ProtocolError::Malformed(
                    "HELLO_PONG responder nonce is zero",
                )));
            }
            match verify_hello(&reply, identity.network_id(), allowlist, false) {
                HelloVerify::Authenticated(address) => {
                    debug!(
                        target: "smartnet::mesh",
                        responder = %format!("{address:#x}"),
                        "mesh outbound HELLO authenticated"
                    );
                    Ok(())
                }
                HelloVerify::NotAllowlisted => Err(TransportError::Protocol(
                    ProtocolError::Malformed("HELLO responder not in allowlist"),
                )),
                HelloVerify::BadSignature => Err(TransportError::Protocol(
                    ProtocolError::Malformed("HELLO responder signature invalid"),
                )),
                HelloVerify::Mismatched { .. } => Err(TransportError::Protocol(
                    ProtocolError::Malformed("HELLO responder mismatched"),
                )),
            }
        }
        _ => Err(TransportError::Protocol(ProtocolError::Malformed(
            "first reply must be HELLO_PONG",
        ))),
    }
}

/// Client side of the bootstrap config gate. Recomputes our two file digests from disk on
/// every call, sends them, and requires the peer's freshly computed digests to match BOTH.
/// A mismatch is [`TransportError::ConfigMismatch`] — the caller drops the link and marks the
/// peer false in memory for this process. Runs once per link bootstrap, right after HELLO.
async fn verify_config_hashes<S>(
    socket: &mut S,
    peers_file: &Path,
    validators_file: &Path,
    limit: usize,
) -> Result<(), TransportError>
where
    S: futures::Sink<WsMessage, Error = WsError>
        + futures::Stream<Item = Result<WsMessage, WsError>>
        + Unpin,
{
    let local = ConfigHashes::read(peers_file, validators_file).map_err(|err| {
        warn!(
            target: "smartnet::mesh",
            mainnet = %peers_file.display(),
            validators = %validators_file.display(),
            %err,
            "local bootstrap config files unreadable — cannot verify peer"
        );
        TransportError::Protocol(ProtocolError::Malformed("local bootstrap config files unreadable"))
    })?;
    send_protocol(
        socket,
        &Message::ConfigHashPing { mainnet: local.mainnet, validators: local.validators },
        limit,
    )
    .await?;
    let (reply, _budget) =
        tokio::time::timeout(DEFAULT_HANDSHAKE_TIMEOUT, recv_protocol_budgeted(socket, limit))
            .await
            .map_err(|_| TransportError::Timeout)??;
    match reply {
        Message::ConfigHashPong { mainnet, validators } => {
            if mainnet != local.mainnet {
                return Err(TransportError::ConfigMismatch("mainnet.jsonl"));
            }
            if validators != local.validators {
                return Err(TransportError::ConfigMismatch("validators.jsonl"));
            }
            Ok(())
        }
        _ => Err(TransportError::Protocol(ProtocolError::Malformed(
            "reply after HELLO must be CONFIG_HASH_PONG",
        ))),
    }
}

/// One in-flight announce on a persistent link: bytes awaiting the peer's answer
/// (`PONG_OK` have-ack or a counter-request that resolves the spread ack at data-send),
/// the spread ack resolved when it arrives (dropped on timeout or session exit = the
/// spread counts it failed).
struct Inflight {
    rumor: Rumor,
    ack: Option<oneshot::Sender<()>>,
    started: Instant,
}

/// Layer-4 link-up on an outbound link: report our tip, take the peer's, then pull every
/// block we lack — start at `local_tip + 1`, bounded by the peer's reported height, one
/// block at a time with a truthful `PING_OK` receipt after each inject, the chain head
/// deciding each advance (the spec's sync flow).
///
/// Availability-first: a status/`fetch` timeout, a tip read failure, or an inject failure
/// ends the bootstrap and the session proceeds to gossip anyway (the announce-driven
/// pull on our inbound side catches anything this missed); transport failures propagate
/// so the caller reconnects with backoff.
/// What [`bootstrap_sync`] achieved, for the boot readiness gate: the peer's status tip
/// (`0` when the status exchange never completed), the local tip it stopped at, and
/// whether the loop actually ran to `our_tip >= peer_tip` (every skip path returns
/// `caught_up = false` — the link had its chance but never verified catch-up).
#[derive(Debug, Clone, Copy)]
pub(crate) struct BootReport {
    /// The peer's tip from the status exchange; 0 = no status was obtained.
    pub(crate) peer_tip: u64,
    /// Our local tip when bootstrap finished or stopped (best-effort).
    pub(crate) local_tip: u64,
    /// True only when the pull loop reached the peer's tip.
    pub(crate) caught_up: bool,
}

async fn bootstrap_sync<S>(
    socket: &mut WebSocketStream<S>,
    ip: &str,
    port: u16,
    injector: &Arc<dyn Injector>,
    seen: &Arc<SeenCache>,
    objects: &Arc<ObjectCache>,
    limit: usize,
) -> Result<BootReport, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut our_tip = match injector.last_block_nr().await {
        Ok(tip) => tip,
        Err(err) => {
            debug!(target: "smartnet::mesh", %ip, port, %err, "mesh tip unavailable — bootstrap sync skipped");
            return Ok(BootReport { peer_tip: 0, local_tip: 0, caught_up: false });
        }
    };
    send_protocol(socket, &Message::SyncBlockPing { last_block: our_tip }, limit).await?;
    let peer_tip = match await_tip(socket, limit).await {
        Ok(tip) => tip,
        Err(TransportError::Timeout) => {
            debug!(target: "smartnet::mesh", %ip, port, "mesh status timed out — bootstrap sync skipped");
            return Ok(BootReport { peer_tip: 0, local_tip: our_tip, caught_up: false });
        }
        Err(err) => return Err(err),
    };
    while our_tip < peer_tip {
        let nr = our_tip + 1;
        send_protocol(socket, &Message::SyncBlockRlpPing { block_nr: nr }, limit).await?;
        let (block, block_budget) = match await_block(socket, limit, nr).await {
            Ok(pair) => pair,
            Err(TransportError::Timeout) => {
                debug!(target: "smartnet::mesh", %ip, port, nr, "mesh fetch timed out — bootstrap sync stopping");
                return Ok(BootReport { peer_tip, local_tip: our_tip, caught_up: false });
            }
            Err(err) => return Err(err),
        };
        let rumor = Rumor::block(nr, block);
        if !rumor.hash_matches() {
            // Unreachable by construction (hash = keccak of bytes).
            return Ok(BootReport { peer_tip, local_tip: our_tip, caught_up: false });
        }
        if seen.check_and_mark(rumor.hash) {
            objects.insert(rumor.clone());
        }
        // Inject regardless of the seen check: a parallel link may have raced the same
        // block here, and the engine's duplicate handling is idempotent.
        if let Err(err) = MeshServer::inject(injector.as_ref(), &rumor).await {
            debug!(target: "smartnet::mesh", %ip, port, nr, %err, "mesh bootstrap inject failed — stopping");
            return Ok(BootReport { peer_tip, local_tip: our_tip, caught_up: false });
        }
        // The bytes are injected; release the hard-cap reservation before the head read.
        drop(block_budget);
        send_protocol(socket, &Message::PingOk, limit).await?;
        our_tip = match injector.last_block_nr().await {
            // The head read can transiently lag the canonical event (µs): trust the
            // injected nr as a floor rather than stopping on a stale read — the
            // loop is bounded by the peer's tip either way.
            Ok(tip) => tip.max(nr),
            Err(err) => {
                debug!(target: "smartnet::mesh", %ip, port, %err, "mesh tip unavailable — bootstrap sync stopping");
                return Ok(BootReport { peer_tip, local_tip: our_tip, caught_up: false });
            }
        };
    }
    Ok(BootReport { peer_tip, local_tip: our_tip, caught_up: true })
}

/// The status dialog half of [`bootstrap_sync`]: waits for the peer's `SYNC_BLOCK_PONG`
/// to our request. The line stays alive (`PING` answered) and pre-session strays are
/// ignored — every message is self-describing, so nothing here needs a remembered state.
async fn await_tip<S>(
    socket: &mut WebSocketStream<S>,
    limit: usize,
) -> Result<u64, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, async {
        loop {
            let (reply, _budget) = recv_protocol_budgeted(socket, limit).await?;
            match reply {
                Message::SyncBlockPong { last_block } => return Ok(last_block),
                Message::Ping => send_protocol(socket, &Message::PongOk, limit).await?,
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| TransportError::Timeout)?
}

/// The fetch dialog half of [`bootstrap_sync`]: waits for the `SYNC_BLOCK_RLP_PONG`
/// matching `block_nr` (responses are matched by block number — one fetch at a time).
async fn await_block<S>(
    socket: &mut WebSocketStream<S>,
    limit: usize,
    block_nr: u64,
) -> Result<(Bytes, OwnedSemaphorePermit), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, async {
        loop {
            let (reply, budget) = recv_protocol_budgeted(socket, limit).await?;
            match reply {
                Message::SyncBlockRlpPong { block_nr: nr, block } if nr == block_nr => {
                    return Ok((block, budget))
                }
                Message::Ping => send_protocol(socket, &Message::PongOk, limit).await?,
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| TransportError::Timeout)?
}

/// Runs one persistent link session: identity handshake, the Layer-4 status bootstrap
/// (report our tip, pull whatever the peer has that we lack), then pipelines up to
/// [`LINK_WINDOW`] rumors from `rx` on one connection (announce each, serve the peer's
/// counter-requests with the bytes, resolve spread acks as `PONG_OK`/data-send arrive),
/// expires announces the never confirms, and keeps the line alive with `PING` while
/// idle (never while an announce is in flight — the keepalive answer shares `PONG_OK`
/// with the have-ack).
///
/// Returns `Ok` when `rx` closes (the [`crate::link::LinkPool`] dropped the peer) or
/// `Err` when the connection fails — the caller reconnects with bounded backoff.
#[allow(clippy::too_many_arguments)] // link plumbing, mirrors `link.rs` sessions
pub(crate) async fn run_link_session(
    ip: &str,
    port: u16,
    rx: &mut mpsc::Receiver<PendingRumor>,
    connected: &AtomicBool,
    limits: Limits,
    identity: &Identity,
    peers_file: &Path,
    validators_file: &Path,
    rejected: &RejectedPeers,
    injector: &Arc<dyn Injector>,
    seen: &Arc<SeenCache>,
    objects: &Arc<ObjectCache>,
    gate: &Arc<SyncGate>,
) -> Result<(), TransportError> {
    let mut socket = connect(ip, port, limits.max_message_bytes).await?;
    let allowlist = load_allowlist(peers_file);
    if let Err(err) =
        handshake_client(&mut socket, identity, &allowlist, limits.max_message_bytes).await
    {
        let _ = socket.close(None).await;
        return Err(err);
    }
    // Bootstrap config gate, right after HELLO: both files must be present and have equal
    // hashes on both ends. Mismatch drops THIS connection and marks the peer false in RAM
    // for the rest of this process; no file is written or synced.
    if let Err(err) =
        verify_config_hashes(&mut socket, peers_file, validators_file, limits.max_message_bytes)
            .await
    {
        if let TransportError::ConfigMismatch(file) = &err {
            warn!(
                target: "smartnet::mesh",
                %ip, port, file,
                "mesh bootstrap config hash mismatch — peer rejected for this process"
            );
            rejected.mark(ip, port);
        }
        let _ = socket.close(None).await;
        return Err(err);
    }
    // Responder authenticated (HELLO) and config verified: lift the pre-auth cap.
    socket.get_mut().arm();
    connected.store(true, Ordering::Release);
    let result = match bootstrap_sync(
        &mut socket,
        ip,
        port,
        injector,
        seen,
        objects,
        limits.max_message_bytes,
    )
    .await
    {
        Ok(report) => {
            // Every completed exchange counts as the link's one boot chance; its status
            // tip enters the gate max and the local tip it left behind is a floor. A
            // skip (timeout/fetch failure) reports peer_tip 0 and keeps the session —
            // a later status/announce on either direction can still finish catch-up.
            if !report.caught_up {
                debug!(
                    target: "smartnet::mesh",
                    %ip, port, peer_tip = report.peer_tip, local_tip = report.local_tip,
                    "mesh bootstrap skipped — link continues in session"
                );
            }
            gate.note_bootstrap(&format!("{ip}:{port}"), report.peer_tip, report.local_tip);
            session_loop(
                &mut socket,
                ip,
                port,
                rx,
                limits,
                peers_file,
                injector,
                seen,
                objects,
                gate,
            )
            .await
        }
        Err(err) => Err(err),
    };
    connected.store(false, Ordering::Release);
    let _ = socket.close(None).await;
    result
}

#[allow(clippy::too_many_arguments)] // link plumbing, mirrors `run_link_session`
async fn session_loop<S>(
    socket: &mut WebSocketStream<S>,
    ip: &str,
    port: u16,
    rx: &mut mpsc::Receiver<PendingRumor>,
    limits: Limits,
    peers_file: &Path,
    injector: &Arc<dyn Injector>,
    seen: &Arc<SeenCache>,
    objects: &Arc<ObjectCache>,
    gate: &Arc<SyncGate>,
) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let limit = limits.max_message_bytes;
    let mut inflight: VecDeque<Inflight> = VecDeque::new();
    let mut last_recv = Instant::now();
    let mut last_send = Instant::now();
    let mut keepalive = tokio::time::interval(limits.keepalive);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    keepalive.tick().await; // first tick fires immediately — skip it, we just handshook

    loop {
        let next_expiry =
            inflight.front().map(|entry| entry.started + DEFAULT_REQUEST_TIMEOUT);

        tokio::select! {
            item = rx.recv(), if inflight.len() < LINK_WINDOW => {
                let Some(pending) = item else { return Ok(()); }; // pool dropped the peer
                let rumor = pending.rumor;
                let announce = match rumor.kind {
                    RumorKind::Tx => Message::RumorTxPing { tx_hash: rumor.hash },
                    RumorKind::Block => Message::RumorBlocksPing {
                        block_nr: rumor.block_nr.expect("block rumors carry a block number"),
                    },
                };
                send_protocol(socket, &announce, limit).await?;
                last_send = Instant::now();
                inflight.push_back(Inflight { rumor, ack: Some(pending.ack), started: last_send });
            }
            incoming = recv_protocol_budgeted(socket, limit) => {
                // Held to the end of this arm: pushed payloads stay budgeted through inject.
                let (message, _budget) = incoming?;
                match message {
                    Message::Ping => {
                        send_protocol(socket, &Message::PongOk, limit).await?;
                        last_send = Instant::now();
                    }
                    // Have-ack for the oldest outstanding announce: the server replies
                    // serially, so `PONG_OK` order matches announce order.
                    Message::PongOk => {
                        if let Some(entry) = inflight.pop_front()
                            && let Some(ack) = entry.ack {
                                let _ = ack.send(());
                            }
                    }
                    Message::PongDrop => {
                        debug!(target: "smartnet::mesh", %ip, port, "mesh peer dropped the link");
                        return Err(TransportError::Closed);
                    }
                    // The peer counter-requested an object we announced: serve it from
                    // the in-flight bytes and resolve the spread ack at data-send.
                    Message::RumorTxRequestPing { tx_hash } => {
                        let position = inflight
                            .iter()
                            .position(|entry| entry.rumor.kind == RumorKind::Tx && entry.rumor.hash == tx_hash);
                        if let Some(position) = position {
                            let entry = inflight.remove(position).expect("position is valid");
                            send_protocol(
                                socket,
                                &Message::RumorTxRequestPong { tx_hash, tx: entry.rumor.data.clone() },
                                limit,
                            )
                            .await?;
                            last_send = Instant::now();
                            if let Some(ack) = entry.ack {
                                let _ = ack.send(());
                            }
                        }
                    }
                    Message::SyncBlockRlpPing { block_nr } => {
                        let position = inflight.iter().position(|entry| {
                            entry.rumor.kind == RumorKind::Block && entry.rumor.block_nr == Some(block_nr)
                        });
                        if let Some(position) = position {
                            let entry = inflight.remove(position).expect("position is valid");
                            send_protocol(
                                socket,
                                &Message::SyncBlockRlpPong { block_nr, block: entry.rumor.data.clone() },
                                limit,
                            )
                            .await?;
                            last_send = Instant::now();
                            if let Some(ack) = entry.ack {
                                let _ = ack.send(());
                            }
                        } else {
                            // The peer's ordered pull asks for a block we did not just
                            // announce — serve it from the chain (not local → left
                            // unanswered; the peer's heartbeat re-asks).
                            match injector.block_rlp(block_nr).await {
                                Ok(Some(block)) => {
                                    send_protocol(
                                        socket,
                                        &Message::SyncBlockRlpPong { block_nr, block },
                                        limit,
                                    )
                                    .await?;
                                    last_send = Instant::now();
                                }
                                Ok(None) => {
                                    debug!(target: "smartnet::mesh", %ip, port, block_nr, "mesh block not local — fetch unanswered");
                                }
                                Err(err) => {
                                    debug!(target: "smartnet::mesh", %ip, port, block_nr, %err, "mesh block read failed");
                                }
                            }
                        }
                    }
                    // Pushed data on our link (sync pulls, rare counter-flow): accept +
                    // inject + truthful `PING_OK`, same as the server side.
                    Message::RumorTxRequestPong { tx_hash, tx } => {
                        let rumor = Rumor::tx(tx);
                        if rumor.hash != tx_hash {
                            warn!(target: "smartnet::mesh", %ip, port, "mesh tx hash mismatch");
                            continue;
                        }
                        if seen.check_and_mark(tx_hash) {
                            objects.insert(rumor.clone());
                            if let Err(err) = MeshServer::inject(injector.as_ref(), &rumor).await {
                                warn!(target: "smartnet::mesh", %ip, port, %err, "local inject failed");
                            }
                        }
                        send_protocol(socket, &Message::PingOk, limit).await?;
                        last_send = Instant::now();
                    }
                    Message::SyncBlockRlpPong { block_nr, block } => {
                        let rumor = Rumor::block(block_nr, block);
                        if seen.check_and_mark(rumor.hash) {
                            objects.insert(rumor.clone());
                            match MeshServer::inject(injector.as_ref(), &rumor).await {
                                Err(err) => {
                                    warn!(target: "smartnet::mesh", %ip, port, %err, "local inject failed");
                                }
                                Ok(()) => {
                                    if let Ok(tip) = injector.last_block_nr().await {
                                        gate.note_local_tip(tip);
                                    }
                                }
                            }
                        }
                        send_protocol(socket, &Message::PingOk, limit).await?;
                        last_send = Instant::now();
                    }
                    // Bootstrap/status asked on our link: answer like the server does.
                    Message::BootstrapPeersPing => {
                        let reply = bootstrap_message(peers_file)?;
                        send_protocol(socket, &reply, limit).await?;
                        last_send = Instant::now();
                    }
                    // Strays for dialogs this side does not own: peers never announce on
                    // *our* connection (each direction owns its own outbound link), and
                    // status/identity frames are handled elsewhere. `PING_OK` is the
                    // receipt after a data-delivery `PONG` — no pending state (the
                    // spread ack resolved at data-send).
                    Message::PingOk |
                    Message::HelloPing(_) |
                    Message::HelloPong(_) |
                    Message::BootstrapPeersPong { .. } |
                    Message::ConfigHashPing { .. } |
                    Message::ConfigHashPong { .. } |
                    Message::SyncBlockPing { .. } |
                    Message::SyncBlockPong { .. } |
                    Message::RumorTxPing { .. } |
                    Message::RumorBlocksPing { .. } => {}
                }
                last_recv = Instant::now();
            }
            _ = keepalive.tick() => {
                // Silence past the shared bound = dead peer: exit and let the caller
                // reconnect with backoff. A live-but-idle peer answers pings well inside
                // this window (keepalive + SILENCE_MARGIN covers a full ping period).
                if last_recv.elapsed() >= limits.silence {
                    return Err(TransportError::Timeout);
                }
                // Ping only on a fully idle line: the keepalive answer shares `PONG_OK`
                // with the announce have-ack, so pinging with announces outstanding
                // would make the ack ambiguous. Outstanding announces already keep the
                // line busy (their answers update `last_recv`).
                if inflight.is_empty() && last_send.elapsed() >= limits.keepalive / 2 {
                    send_protocol(socket, &Message::Ping, limit).await?;
                    last_send = Instant::now();
                }
            }
            _ = tokio::time::sleep_until(
                tokio::time::Instant::from_std(
                    next_expiry.unwrap_or_else(|| Instant::now() + DEFAULT_REQUEST_TIMEOUT),
                ),
            ), if next_expiry.is_some() => {
                let now = Instant::now();
                while let Some(entry) = inflight.front() {
                    if now.duration_since(entry.started) < DEFAULT_REQUEST_TIMEOUT {
                        break;
                    }
                    // Peer never answered — drop the ack (spread counts it failed).
                    // A late delivery is harmless: hash-keyed, seen-checked, at-most-once.
                    let entry = inflight.pop_front().expect("front exists");
                    drop(entry.ack);
                }
            }
        }
    }
}

/// Pings one peer over a fresh `WebSocket` (identity handshake first); `true` only when
/// a binary `PONG_OK` arrives.
pub async fn ping(
    ip: &str,
    port: u16,
    timeout: Duration,
    identity: &Identity,
    allowlist: &[Address],
) -> bool {
    tokio::time::timeout(timeout, ping_inner(ip, port, identity, allowlist))
        .await
        .is_ok_and(|result| result.is_ok())
}

async fn ping_inner(
    ip: &str,
    port: u16,
    identity: &Identity,
    allowlist: &[Address],
) -> Result<(), TransportError> {
    let limit = Limits::default().max_message_bytes;
    let mut socket = connect(ip, port, limit).await?;
    handshake_client(&mut socket, identity, allowlist, limit).await?;
    socket.get_mut().arm();
    send_protocol(&mut socket, &Message::Ping, limit).await?;
    loop {
        let (reply, _budget) = recv_protocol_budgeted(&mut socket, limit).await?;
        match reply {
            Message::PongOk => {
                let _ = socket.close(None).await;
                return Ok(());
            }
            Message::Ping => send_protocol(&mut socket, &Message::PongOk, limit).await?,
            _ => {}
        }
    }
}

/// Fetches the bootstrap peer list from one peer (identity handshake first). The list is
/// accepted only when its canonical hash matches the enclosed records.
pub async fn fetch_peers(
    ip: &str,
    port: u16,
    timeout: Duration,
    identity: &Identity,
    allowlist: &[Address],
) -> Result<Vec<PeerRecord>, TransportError> {
    tokio::time::timeout(timeout, fetch_peers_inner(ip, port, identity, allowlist))
        .await
        .map_err(|_| TransportError::Timeout)?
}

async fn fetch_peers_inner(
    ip: &str,
    port: u16,
    identity: &Identity,
    allowlist: &[Address],
) -> Result<Vec<PeerRecord>, TransportError> {
    let limit = Limits::default().max_message_bytes;
    let mut socket = connect(ip, port, limit).await?;
    handshake_client(&mut socket, identity, allowlist, limit).await?;
    socket.get_mut().arm();
    send_protocol(&mut socket, &Message::BootstrapPeersPing, limit).await?;
    loop {
        let (reply, _budget) = recv_protocol_budgeted(&mut socket, limit).await?;
        match reply {
            Message::BootstrapPeersPong { list_hash, peers } => {
                if protocol::peer_list_hash(&peers)? != list_hash {
                    return Err(TransportError::Protocol(ProtocolError::Malformed(
                        "bootstrap list hash mismatch",
                    )));
                }
                let _ = socket.close(None).await;
                return Ok(peers);
            }
            Message::Ping => send_protocol(&mut socket, &Message::PongOk, limit).await?,
            _ => {}
        }
    }
}

async fn connect(
    ip: &str,
    port: u16,
    max_message_bytes: usize,
) -> Result<WebSocketStream<PreAuthGuard<TcpStream>>, TransportError> {
    let url = format!("ws://{ip}:{port}{MESH_PATH}");
    // Dial manually so the raw stream carries the pre-auth byte cap: a peer we have
    // not yet verified with `HELLO_PONG` can push at most `preauth::PREAUTH_BUDGET`
    // bytes before `handshake_client` arms the guard (same rule as the accept path).
    let stream = TcpStream::connect((ip, port))
        .await
        .map_err(|err| TransportError::WebSocket(WsError::Io(err)))?;
    stream
        .set_nodelay(true)
        .map_err(|err| TransportError::WebSocket(WsError::Io(err)))?;
    let guarded = PreAuthGuard::new(stream, preauth::restricted());
    let (socket, _) =
        client_async_with_config(url, guarded, Some(websocket_config(max_message_bytes))).await?;
    Ok(socket)
}

fn websocket_config(max_message_bytes: usize) -> WebSocketConfig {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(max_message_bytes);
    config.max_frame_size = Some(max_message_bytes);
    config
}

async fn send_protocol<S>(
    socket: &mut S,
    message: &Message,
    limit: usize,
) -> Result<(), TransportError>
where
    S: futures::Sink<WsMessage, Error = WsError> + Unpin + ?Sized,
{
    let frame = message.encode_with_limit(limit)?;
    // Reserve the real encoded size (never more than the frame cap) before handing the
    // buffer to the socket: a slow peer must not let many full-size encodes accumulate.
    let reserve = mesh_send_byte_budget()
        .clone()
        .acquire_many_owned(u32::try_from(frame.len()).expect("clamped mesh frame size"))
        .await
        .expect("mesh send budget closed");
    socket.send(WsMessage::Binary(frame.into())).await?;
    drop(reserve);
    Ok(())
}

/// Reads one protocol message from any `WebSocket` stream half (whole socket or a
/// split read half), with the raw frame length for byte accounting. `WS`-level `PONG`
/// maps to [`Message::PongOk`]; `WS`-level `PING` is swallowed — tungstenite queues its
/// own `PONG`, flushed on the next write.
async fn recv_protocol<S>(socket: &mut S, limit: usize) -> Result<(Message, usize), TransportError>
where
    S: futures::Stream<Item = Result<WsMessage, WsError>> + Unpin,
{
    loop {
        match socket.next().await {
            Some(Ok(WsMessage::Binary(frame))) => {
                let len = frame.len();
                return Ok((Message::decode_with_limit(&frame, limit)?, len))
            }
            Some(Ok(WsMessage::Text(_))) => return Err(TransportError::TextFrame),
            Some(Ok(WsMessage::Ping(_) | WsMessage::Frame(_))) => {}
            Some(Ok(WsMessage::Pong(payload))) => return Ok((Message::PongOk, payload.len())),
            Some(Ok(WsMessage::Close(_))) | None => return Err(TransportError::Closed),
            Some(Err(err)) => return Err(TransportError::WebSocket(err)),
        }
    }
}

/// Logs the active mesh transport once at startup.
pub fn log_listening(bind_addr: std::net::IpAddr, port: u16, peers_file: &std::path::Path, rpc: &str) {
    info!(
        target: "smartnet::mesh",
        %bind_addr,
        port,
        peers_file = %peers_file.display(),
        rpc,
        "mesh listening (binary WebSocket, closed list, /mesh)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Records the peak number of concurrent `new_payload` calls.
    #[derive(Debug, Default)]
    struct SerialProbe {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        blocks: AtomicUsize,
    }

    impl SerialProbe {
        fn observe(&self, in_flight: usize) {
            self.blocks.fetch_add(1, Ordering::SeqCst);
            self.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl Injector for SerialProbe {
        async fn send_raw_transaction(&self, _raw: &Bytes) -> Result<(), String> { Ok(()) }
        async fn new_payload(&self, _block_rlp: &Bytes) -> Result<(), String> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.observe(now);
            tokio::time::sleep(Duration::from_millis(5)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
        async fn last_block_nr(&self) -> Result<u64, String> { Ok(0) }
        async fn block_rlp(&self, _block_nr: u64) -> Result<Option<Bytes>, String> { Ok(None) }
    }

    /// Protocol rule: concurrent block injects from any mix of paths serialize through
    /// the node-wide gate — the engine never sees two `new_payload` calls at once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn block_injects_serialize_across_concurrent_callers() {
        let probe = Arc::new(SerialProbe::default());
        let mut handles = Vec::new();
        for nr in 0..8u64 {
            let probe = probe.clone();
            handles.push(tokio::spawn(async move {
                let rumor = Rumor::block(nr, Bytes::from(vec![nr as u8; 32]));
                MeshServer::inject(probe.as_ref(), &rumor).await
            }));
        }
        for handle in handles {
            handle.await.expect("inject task panicked").expect("inject failed");
        }
        assert_eq!(probe.blocks.load(Ordering::SeqCst), 8, "all injects ran");
        assert_eq!(
            probe.max_in_flight.load(Ordering::SeqCst),
            1,
            "block injects must never overlap (no block batches)"
        );
    }

    /// Tx injects are not gated by the block rule (tx batches stay supported).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tx_injects_are_not_serialized_by_the_block_gate() {
        let probe = Arc::new(SerialProbe::default());
        let mut handles = Vec::new();
        for i in 0..4u8 {
            let probe = probe.clone();
            handles.push(tokio::spawn(async move {
                let rumor = Rumor::tx(Bytes::from(vec![i; 8]));
                MeshServer::inject(probe.as_ref(), &rumor).await
            }));
        }
        for handle in handles {
            handle.await.expect("inject task panicked").expect("inject failed");
        }
        // The probe only counts new_payload; zero block injects happened here.
        assert_eq!(probe.blocks.load(Ordering::SeqCst), 0);
    }

    /// Minimal injector: this test's server only answers HELLO + `CONFIG_HASH_PING`, never
    /// injects an object.
    #[derive(Debug)]
    struct Noop;

    #[async_trait::async_trait]
    impl Injector for Noop {
        async fn send_raw_transaction(&self, _raw: &Bytes) -> Result<(), String> { Ok(()) }
        async fn new_payload(&self, _block_rlp: &Bytes) -> Result<(), String> { Ok(()) }
        async fn last_block_nr(&self) -> Result<u64, String> { Ok(0) }
        async fn block_rlp(&self, _block_nr: u64) -> Result<Option<Bytes>, String> { Ok(None) }
    }

    /// The bootstrap gate accepts a peer whose two files hash equal to ours and rejects one
    /// where either file differs (no file is written or synced by the gate).
    #[tokio::test]
    async fn config_hash_gate_accepts_equal_and_rejects_changed_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let peers = dir.path().join("mainnet.jsonl");
        let server_validators = dir.path().join("server-validators.jsonl");
        let client_validators = dir.path().join("client-validators.jsonl");
        let server_id = crate::identity::test_identity(9);
        let client_id = crate::identity::test_identity(10);
        // Both identities must be allowlisted: the server checks the initiator, the client
        // checks the responder, against this same peer file.
        std::fs::write(
            &peers,
            format!(
                "{{\"address\":\"{:#x}\",\"ip\":\"127.0.0.1\",\"port_range\":1,\"active\":true,\"valid\":true}}\n\
                 {{\"address\":\"{:#x}\",\"ip\":\"127.0.0.1\",\"port_range\":2,\"active\":true,\"valid\":true}}\n",
                server_id.address(),
                client_id.address()
            ),
        )
        .expect("write peers");
        std::fs::write(&server_validators, "validators-v1").expect("write server validators");
        std::fs::write(&client_validators, "validators-v1").expect("write client validators");

        let server = Arc::new(MeshServer::new(
            peers.clone(),
            server_validators.clone(),
            Arc::new(SeenCache::default()),
            Arc::new(ObjectCache::default()),
            Arc::new(Noop),
            Arc::new(server_id.clone()),
            SyncGate::new(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = serve(listener, server, ServerConfig::default()).await;
        });

        let allowlist = vec![server_id.address()];
        let limit = Limits::default().max_message_bytes;

        // Equal files: the gate passes.
        let mut socket = connect(&addr.ip().to_string(), addr.port(), limit).await.expect("connect");
        handshake_client(&mut socket, &client_id, &allowlist, limit).await.expect("hello");
        verify_config_hashes(&mut socket, &peers, &client_validators, limit)
            .await
            .expect("equal hashes must pass");

        // Our validators file changes: the server's copy stays at v1, so the very next
        // bootstrap must reject. Nothing is written back to the server's file.
        std::fs::write(&client_validators, "validators-v2").expect("rewrite validators");
        assert_eq!(std::fs::read(&server_validators).unwrap(), b"validators-v1");
        let mut socket = connect(&addr.ip().to_string(), addr.port(), limit).await.expect("connect");
        handshake_client(&mut socket, &client_id, &allowlist, limit).await.expect("hello");
        let err = verify_config_hashes(&mut socket, &peers, &client_validators, limit)
            .await
            .expect_err("changed validators must fail");
        assert!(matches!(err, TransportError::ConfigMismatch("validators.jsonl")), "got: {err:?}");
    }
}
