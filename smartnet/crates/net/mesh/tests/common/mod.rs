//! Shared fixtures for mesh integration tests: TLS provider install, test identities, a
//! binary `WebSocket` mesh server, a recording fake peer that speaks the identity
//! handshake, and a minimal recording HTTP mock for the JSON-RPC inject path.

#![allow(dead_code)] // shared helpers, not all used by every test binary
#![allow(unreachable_pub)] // `pub` is intra-test-binary visibility here

use alloy_primitives::Bytes;
use smartnet_mesh::{
    identity::Identity,
    inject::{Injector, LocalRpc},
    protocol::Message,
    seen::SeenCache,
    store::ObjectCache,
    sync_gate::SyncGate,
    ws::{serve, MeshServer, ServerConfig},
};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// Installs the workspace rustls provider (required before any reqwest client is built;
/// the node binary does this in `main`).
pub fn install_tls() {
    let _ = smartnet_tls_provider::install_default();
}

/// A deterministic test identity: key byte 31 = `seed`, network id 1.
pub fn test_identity(seed: u8) -> Identity {
    let mut bytes = [0u8; 32];
    bytes[31] = seed;
    let key = k256::ecdsa::SigningKey::from_slice(&bytes).expect("valid scalar");
    Identity::new(key, 1)
}

/// The client (initiator) identity used across mesh tests.
pub fn client_identity() -> Identity {
    test_identity(1)
}

/// The server (responder) identity used by [`mesh_server`].
pub fn server_identity() -> Identity {
    test_identity(2)
}

/// The allowlist a test client presents when dialing: the client itself plus the mesh
/// server identity (the responder must authenticate too).
pub fn client_allowlist() -> Vec<alloy_primitives::Address> {
    vec![client_identity().address(), server_identity().address()]
}

/// A peer-file line admitting the test client (its `key_id` is the client's address, the
/// mesh allowlist). `port` is the entry's mesh port (only matters for self-skip).
pub fn client_peer_line(port: u16) -> String {
    format!(
        r#"{{"key_id":"{:#x}","url":"","ip":"127.0.0.1","port_range":{port},"active":true,"valid":true}}"#,
        client_identity().address()
    )
}

/// An injector that never fails and never touches a socket — for spread/link tests that
/// never exercise serving or inject.
#[derive(Debug)]
pub struct NoopInjector;

#[async_trait::async_trait]
impl Injector for NoopInjector {
    async fn send_raw_transaction(&self, _raw: &Bytes) -> Result<(), String> {
        Ok(())
    }
    async fn new_payload(&self, _block_rlp: &Bytes) -> Result<(), String> {
        Ok(())
    }
    async fn last_block_nr(&self) -> Result<u64, String> {
        Ok(0)
    }
    async fn block_rlp(&self, _block_nr: u64) -> Result<Option<Bytes>, String> {
        Ok(None)
    }
}

/// A recorded HTTP request (method, path, raw body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Request method.
    pub method: String,
    /// Request target.
    pub path: String,
    /// Raw body bytes.
    pub body: Vec<u8>,
}

/// Every request a [`spawn_http_mock`] server has received, in arrival order.
#[derive(Clone, Default)]
pub struct Recorded(Arc<Mutex<Vec<HttpRequest>>>);

impl Recorded {
    /// Snapshot of the recorded requests.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.0.lock().expect("recorded lock").clone()
    }

    /// How many requests arrived so far.
    pub fn len(&self) -> usize {
        self.0.lock().expect("recorded lock").len()
    }

    /// Whether no requests arrived.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A minimal HTTP/1.1 server for the JSON-RPC inject client: records each request, answers
/// from `responses` (falling back to `200 {"jsonrpc":"2.0","id":1,"result":"0x0"}` — a
/// success-shaped default so chain reads like `eth_blockNumber` parse as tip 0), and
/// closes the connection. Returns its address and the recording handle.
pub async fn spawn_http_mock(responses: Vec<(u16, String)>) -> (SocketAddr, Recorded) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind http mock");
    let addr = listener.local_addr().expect("http mock addr");
    let recorded = Recorded::default();
    let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
    let record = recorded.0.clone();

    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(_) => continue,
            };
            match read_http_request(&mut socket).await {
                Some(request) => {
                    record.lock().expect("recorded lock").push(request);
                    let (status, body) =
                        queue.lock().expect("queue lock").pop_front().unwrap_or_else(|| {
                            (200, r#"{"jsonrpc":"2.0","id":1,"result":"0x0"}"#.to_string())
                        });
                    let _ = write_http_response(&mut socket, status, &body).await;
                }
                None => {
                    let _ = write_http_response(&mut socket, 400, "{}").await;
                }
            }
        }
    });

    (addr, recorded)
}

/// The JSON-RPC URL for an HTTP mock address.
pub fn http_mock_url(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

/// Builds a mesh server whose local inject posts to `rpc_addr` with the given recordable
/// seen/object caches.
pub fn mesh_server(
    peers_file: std::path::PathBuf,
    rpc_addr: SocketAddr,
    seen: Arc<SeenCache>,
    objects: Arc<ObjectCache>,
) -> Arc<MeshServer> {
    let rpc = Arc::new(LocalRpc::new(http_mock_url(rpc_addr)));
    Arc::new(MeshServer::new(
        peers_file,
        seen,
        objects,
        rpc,
        Arc::new(server_identity()),
        SyncGate::new(),
    ))
}

/// Spawns a real `WebSocket` mesh server and returns its address plus the caches wired in.
pub async fn spawn_mesh_server(
    peers_file: std::path::PathBuf,
) -> (std::net::SocketAddr, Arc<SeenCache>, Arc<ObjectCache>) {
    let (addr, seen, objects, _recorded) = spawn_mesh_server_record(peers_file).await;
    (addr, seen, objects)
}

/// [`spawn_mesh_server`] plus the JSON-RPC inject recording handle.
pub async fn spawn_mesh_server_record(
    peers_file: std::path::PathBuf,
) -> (std::net::SocketAddr, Arc<SeenCache>, Arc<ObjectCache>, Recorded) {
    let (rpc_addr, recorded) = spawn_http_mock(vec![]).await;
    let seen = Arc::new(SeenCache::default());
    let objects = Arc::new(ObjectCache::default());
    let server = mesh_server(peers_file, rpc_addr, seen.clone(), objects.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mesh");
    let addr = listener.local_addr().expect("mesh addr");
    tokio::spawn(async move {
        let _ = serve(listener, server, ServerConfig::default()).await;
    });
    (addr, seen, objects, recorded)
}

/// A TCP address with nothing listening (connect → refused), for dead-peer tests.
pub async fn dead_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener); // release immediately; nobody listens on `addr` anymore
    addr
}

/// Reads one HTTP/1.1 request (request line, headers, `Content-Length` body).
async fn read_http_request(socket: &mut TcpStream) -> Option<HttpRequest> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 32 * 1024 {
            return None;
        }
    }
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().ok()?;
        }
    }
    let body_start = header_end + 4;
    while buf.len() < body_start + content_length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    buf.truncate(body_start + content_length);
    Some(HttpRequest { method, path, body: buf.split_off(body_start) })
}

/// Writes a minimal HTTP/1.1 response with `Connection: close`.
async fn write_http_response(socket: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Unknown",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body.as_bytes()).await;
    let _ = socket.flush().await;
}

/// Every binary protocol message a [`spawn_ws_recorder`] server received, in order, plus
/// how many `WebSocket` connections it accepted.
#[derive(Clone, Default)]
pub struct WsRecorded {
    messages: Arc<Mutex<Vec<Message>>>,
    connections: Arc<std::sync::atomic::AtomicUsize>,
}

impl WsRecorded {
    /// Snapshot of the recorded messages.
    pub fn messages(&self) -> Vec<Message> {
        self.messages.lock().expect("ws recorded lock").clone()
    }

    /// How many messages arrived.
    pub fn len(&self) -> usize {
        self.messages.lock().expect("ws recorded lock").len()
    }

    /// Whether no messages arrived.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many `WebSocket` connections the recorder accepted — the persistent-link
    /// guarantee under test is "one per peer, reused for every message".
    pub fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A minimal `WebSocket` peer that speaks the identity handshake (responder side) and
/// always answers announces with `PONG_OK` and `PING` with `PONG_OK`. Used where tests
/// need an observable "live peer" that never counter-requests payload bytes. Returns the
/// recorder's identity — its address must be in the dialing node's peer-file allowlist.
pub async fn spawn_ws_recorder() -> (SocketAddr, Identity, WsRecorded) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ws recorder");
    let addr = listener.local_addr().expect("ws recorder addr");
    use std::sync::atomic::{AtomicU8, Ordering};
    static NEXT_SEED: AtomicU8 = AtomicU8::new(10);
    let seed = NEXT_SEED.fetch_add(1, Ordering::Relaxed);
    let (identity, recorded) = spawn_ws_recorder_on(listener, test_identity(seed)).await;
    (addr, identity, recorded)
}

/// [`spawn_ws_recorder`] on an already-bound listener with a chosen responder identity —
/// lets a test bring a recorder up on a port a dead peer once used (reconnect tests)
/// under the identity the peer file already lists.
pub async fn spawn_ws_recorder_on(listener: TcpListener, identity: Identity) -> (Identity, WsRecorded) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{accept_async, tungstenite::Message as WsMessage};

    let recorded = WsRecorded::default();
    let record = recorded.messages.clone();
    let connections = recorded.connections.clone();
    let responder = identity.clone();

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(_) => continue,
            };
            connections.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut socket = match accept_async(stream).await {
                Ok(socket) => socket,
                Err(_) => continue,
            };
            let record = record.clone();
            let responder = responder.clone();
            tokio::spawn(async move {
                while let Some(Ok(frame)) = socket.next().await {
                    let WsMessage::Binary(data) = frame else { continue };
                    let Ok(message) = Message::decode(&data) else { continue };
                    match &message {
                        Message::HelloPing(hello) => {
                            let reply = Message::HelloPong(responder.sign_hello_responder(
                                &hello.initiator,
                                &hello.initiator_nonce,
                            ))
                            .encode()
                            .expect("hello encodes");
                            let _ = socket.send(WsMessage::Binary(reply.into())).await;
                        }
                        Message::Ping | Message::RumorTxPing { .. } | Message::RumorBlocksPing { .. } => {
                            let _ = socket
                                .send(WsMessage::Binary(Message::PongOk.encode().unwrap().into()))
                                .await;
                        }
                        Message::SyncBlockPing { .. } => {
                            // The dialing node's status bootstrap: report tip 0, so it has
                            // nothing to pull (one round trip, then gossip starts).
                            let reply = Message::SyncBlockPong { last_block: 0 }
                                .encode()
                                .expect("status encodes");
                            let _ = socket.send(WsMessage::Binary(reply.into())).await;
                        }
                        _ => {}
                    }
                    record.lock().expect("ws recorded lock").push(message);
                }
            });
        }
    });

    (identity, recorded)
}
