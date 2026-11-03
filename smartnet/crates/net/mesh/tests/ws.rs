//! Binary `WebSocket` transport: announce/counter-request dialogs, have short-circuit,
//! keepalive ping, bootstrap, identity gating, and the rejections that keep the wire
//! JSON-free.

mod common;

use common::{
    client_allowlist, client_identity, install_tls, mesh_server, spawn_http_mock,
    spawn_mesh_server_record,
};
use futures::{SinkExt, StreamExt};
use smartnet_mesh::{
    identity::{verify_hello, HelloVerify},
    protocol::Message,
    rumor::Rumor,
    seen::SeenCache,
    store::ObjectCache,
    ws::{self, MESH_PATH},
};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::{
    connect_async,
    tungstenite::Message as WsMessage,
    WebSocketStream,
};

fn temp_peers() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mainnet.jsonl");
    // The client must be on the server's allowlist or the HELLO handshake fails.
    std::fs::write(&path, format!("{}\n", common::client_peer_line(1))).expect("write peers");
    (dir, path)
}

async fn spawn_server(
    peers_file: std::path::PathBuf,
) -> (std::net::SocketAddr, Arc<SeenCache>, Arc<ObjectCache>, common::Recorded) {
    let (rpc_addr, recorded) = spawn_http_mock(vec![]).await;
    let seen = Arc::new(SeenCache::default());
    let objects = Arc::new(ObjectCache::default());
    let server = mesh_server(peers_file, rpc_addr, seen.clone(), objects.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mesh");
    let addr = listener.local_addr().expect("mesh addr");
    tokio::spawn(async move {
        let _ = ws::serve(listener, server, ws::ServerConfig::default()).await;
    });
    (addr, seen, objects, recorded)
}

#[tokio::test]
async fn unknown_tx_is_fetched_and_injected() {
    install_tls();
    let (_dir, peers_file) = temp_peers();
    let (addr, _, _, recorded) = spawn_server(peers_file).await;

    let rumor = Rumor::tx(vec![0x01, 0x02]);
    ws::send_rumor(
        "127.0.0.1",
        addr.port(),
        &rumor,
        std::time::Duration::from_secs(5),
        &client_identity(),
        &client_allowlist(),
    )
    .await
    .expect("deliver rumor");

    let requests = recorded.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/");
    let value: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json rpc");
    assert_eq!(value["method"], "eth_sendRawTransaction");
    assert_eq!(value["params"], serde_json::json!(["0x0102"]));
}

#[tokio::test]
async fn already_seen_tx_is_short_circuited_with_pong_ok() {
    install_tls();
    let (_dir, peers_file) = temp_peers();
    let (addr, seen, _, recorded) = spawn_server(peers_file).await;

    let rumor = Rumor::tx(vec![0x09]);
    assert!(seen.check_and_mark(rumor.hash));

    ws::send_rumor(
        "127.0.0.1",
        addr.port(),
        &rumor,
        std::time::Duration::from_secs(5),
        &client_identity(),
        &client_allowlist(),
    )
    .await
    .expect("have short circuit");

    assert!(recorded.is_empty(), "have must never reach the inject path");
}

#[tokio::test]
async fn ping_is_answered_with_pong_ok() {
    install_tls();
    let (_dir, peers_file) = temp_peers();
    let (addr, _, _, _) = spawn_server(peers_file).await;

    assert!(
        ws::ping(
            "127.0.0.1",
            addr.port(),
            std::time::Duration::from_secs(5),
            &client_identity(),
            &client_allowlist(),
        )
        .await
    );
}

#[tokio::test]
async fn bootstrap_delivers_the_peer_list() {
    install_tls();
    let dir = tempfile::tempdir().expect("tempdir");
    let peers_file = dir.path().join("mainnet.jsonl");
    // Entry "a" carries the client's address so the allowlist admits the fetcher; the
    // list content itself is what the test asserts.
    std::fs::write(
        &peers_file,
        format!(
            "{}\n{}\n",
            common::client_peer_line(30811),
            r#"{"key_id":"b","url":"","ip":"10.0.0.9","port_range":30812,"active":true,"valid":false}"#,
        ),
    )
    .expect("write peers");
    let (addr, _, _, _) = spawn_server(peers_file).await;

    let peers = ws::fetch_peers(
        "127.0.0.1",
        addr.port(),
        std::time::Duration::from_secs(5),
        &client_identity(),
        &client_allowlist(),
    )
    .await
    .expect("fetch peers");
    assert_eq!(peers.len(), 2);
    assert!(peers[0].ip == "127.0.0.1" || peers[1].ip == "127.0.0.1");
    let b = peers.iter().find(|peer| peer.ip == "10.0.0.9").expect("entry b");
    assert!(!b.valid);
}

#[tokio::test]
async fn wrong_path_is_rejected_at_handshake() {
    install_tls();
    let (_dir, peers_file) = temp_peers();
    let (addr, _, _, _) = spawn_server(peers_file).await;

    let url = format!("ws://127.0.0.1:{}/not-mesh", addr.port());
    assert!(connect_async(url).await.is_err());
}

#[tokio::test]
async fn text_frames_are_rejected() {
    install_tls();
    let (_dir, peers_file) = temp_peers();
    let (addr, _, _, _) = spawn_server(peers_file).await;

    let (mut socket, _) =
        connect_async(format!("ws://127.0.0.1:{}{MESH_PATH}", addr.port())).await.expect("connect");
    socket.send(WsMessage::Text("not json, not binary".into())).await.expect("send text");

    // The server must drop the connection rather than parse text as a protocol message
    // (the first frame is still the identity-handshake slot — text is rejected there).
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("server reply")
        .expect("a frame");
    assert!(matches!(reply, Ok(WsMessage::Close(_))));
}

#[tokio::test]
async fn handshake_rejects_a_stranger_not_on_the_allowlist() {
    install_tls();
    let (_dir, peers_file) = temp_peers(); // admits only the client identity (seed 1)
    let (addr, _, _, _) = spawn_server(peers_file).await;

    let stranger = common::test_identity(99);
    let result = ws::ping(
        "127.0.0.1",
        addr.port(),
        std::time::Duration::from_secs(5),
        &stranger,
        // The stranger *claims* itself is allowlisted on its side; the server's own
        // peer file is what gates the inbound HELLO.
        &[stranger.address()],
    )
    .await;
    assert!(!result, "server must refuse a HELLO from an unlisted address");
}

#[tokio::test]
async fn block_announce_is_fetched_and_injected() {
    install_tls();
    let (_dir, peers_file) = temp_peers();
    let (addr, _, _, recorded) = spawn_server(peers_file).await;

    // Mock tip is 0 (the fallback result "0x0"), so block 1 is ahead → counter-fetch.
    let rumor = Rumor::block(1, vec![0xc0]);
    ws::send_rumor(
        "127.0.0.1",
        addr.port(),
        &rumor,
        std::time::Duration::from_secs(5),
        &client_identity(),
        &client_allowlist(),
    )
    .await
    .expect("deliver block");

    // Two RPC calls arrive: the tip probe (`eth_blockNumber`) and the inject
    // (`smartnet_newPayload`) — assert on the inject itself.
    let payload_calls: Vec<serde_json::Value> = recorded
        .requests()
        .iter()
        .filter_map(|request| {
            let value: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
            (value["method"] == "smartnet_newPayload").then_some(value)
        })
        .collect();
    assert_eq!(payload_calls.len(), 1, "{payload_calls:?}");
    assert_eq!(payload_calls[0]["params"], serde_json::json!(["0xc0", true, true]));
}

#[tokio::test]
async fn duplicate_tx_injects_only_once() {
    install_tls();
    let (_dir, peers_file) = temp_peers();
    let (addr, seen, _, recorded) = spawn_mesh_server_record(peers_file).await;

    let rumor = Rumor::tx(vec![0x11, 0x22]);
    let identity = client_identity();
    let allowlist = client_allowlist();
    ws::send_rumor(
        "127.0.0.1",
        addr.port(),
        &rumor,
        std::time::Duration::from_secs(5),
        &identity,
        &allowlist,
    )
    .await
    .expect("first");
    ws::send_rumor(
        "127.0.0.1",
        addr.port(),
        &rumor,
        std::time::Duration::from_secs(5),
        &identity,
        &allowlist,
    )
    .await
    .expect("second (have)");

    assert_eq!(recorded.requests().len(), 1);
    assert_eq!(seen.len(), 1);
}

/// Next binary protocol frame (skips WS-level pings/control frames).
async fn next_message<S>(socket: &mut WebSocketStream<S>) -> Message
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("frame timeout")
            .expect("stream ended")
            .expect("ws error");
        let WsMessage::Binary(data) = frame else { continue };
        return Message::decode(&data).expect("protocol message");
    }
}

async fn send_message<S>(socket: &mut WebSocketStream<S>, message: Message)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let frame = message.encode().expect("message encodes");
    socket.send(WsMessage::Binary(frame.into())).await.expect("frame write");
}

/// A gap-of-two announce arms the ordered pull: the reply carries the announced block
/// and the range start goes out as a second fetch, so the missing parent lands too —
/// the previous behavior fetched only the announced block and the gap never healed.
#[tokio::test]
async fn gap_announce_pulls_the_missing_range() {
    install_tls();
    let (_dir, peers_file) = temp_peers();
    let (addr, _, _, recorded) = spawn_server(peers_file).await;

    let (mut socket, _) = connect_async(format!("ws://127.0.0.1:{}{MESH_PATH}", addr.port()))
        .await
        .expect("connect");
    // Layer-2 handshake: our signed HELLO_PING first, then verify the server's reply.
    let hello = client_identity().sign_hello_initiator();
    send_message(&mut socket, Message::HelloPing(hello)).await;
    let Message::HelloPong(reply) = next_message(&mut socket).await else {
        panic!("first reply must be HELLO_PONG");
    };
    match verify_hello(&reply, 1, &client_allowlist(), false) {
        HelloVerify::Authenticated(_) => {}
        _ => panic!("server must authenticate against the allowlist"),
    }

    // Mock tip is 0, so announcing block 2 is ahead by two: the aligned reply carries
    // block 2, the ordered range starts at block 1.
    send_message(&mut socket, Message::RumorBlocksPing { block_nr: 2 }).await;
    assert!(
        matches!(next_message(&mut socket).await, Message::SyncBlockRlpPing { block_nr: 2 }),
        "aligned reply must fetch the announced block"
    );
    assert!(
        matches!(next_message(&mut socket).await, Message::SyncBlockRlpPing { block_nr: 1 }),
        "the gap must arm the ordered range from local_tip + 1"
    );

    // Serve the range: the in-order block injects inline (truthful receipt), the racing
    // aligned delivery injects bounded-concurrent (receipt when it completes).
    send_message(&mut socket, Message::SyncBlockRlpPong { block_nr: 1, block: vec![0xc1].into() }).await;
    assert!(
        matches!(next_message(&mut socket).await, Message::PingOk),
        "the ordered block must get a receipt after its inject"
    );
    send_message(&mut socket, Message::SyncBlockRlpPong { block_nr: 2, block: vec![0xc2].into() }).await;
    // The continuation may re-fetch block 2 before this pong lands (the aligned fetch
    // for it is still outstanding while the range advances on a stale mock tip) — the
    // protocol is duplicate-tolerant, so skip the refetch and wait for the receipt.
    loop {
        match next_message(&mut socket).await {
            Message::PingOk => break,
            Message::SyncBlockRlpPing { block_nr: 2 } => {}
            other => panic!("unexpected message while awaiting receipt: {other:?}"),
        }
    }

    let mut injected: Vec<String> = recorded
        .requests()
        .iter()
        .filter_map(|request| {
            let value: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
            (value["method"] == "smartnet_newPayload").then(|| {
                value["params"][0].as_str().expect("rlp param").to_string()
            })
        })
        .collect();
    injected.sort();
    assert_eq!(injected, vec!["0xc1".to_string(), "0xc2".to_string()], "both blocks must inject");
}

/// Replay defense (initiator side): a captured `HELLO_PONG` is useless on any other
/// connection. Connection 1 completes a valid handshake and its `HELLO_PONG` is
/// captured; connection 2 presents a fresh `HELLO_PING` (new nonce) but the fake
/// server replays the captured pong — the initiator's nonce-echo check must reject it.
#[tokio::test]
async fn replayed_hello_pong_is_rejected() {
    install_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake server");
    let addr = listener.local_addr().expect("fake server addr");
    let url = format!("ws://{addr}{MESH_PATH}");
    let responder = common::server_identity();

    // Connection 1: accept, answer with a real signed HELLO_PONG, then drop. The test
    // client captures that pong below.
    let capture = tokio::spawn({
        let responder = responder.clone();
        async move {
            let (stream, _) = listener.accept().await.expect("accept 1");
            let mut socket = tokio_tungstenite::accept_async(stream).await.expect("ws accept 1");
            let Message::HelloPing(hello) = next_message(&mut socket).await else {
                panic!("first frame must be HELLO_PING");
            };
            let pong = responder.sign_hello_responder(&hello.initiator, &hello.initiator_nonce);
            send_message(&mut socket, Message::HelloPong(pong)).await;
            (listener, pong)
        }
    });

    // Client side of connection 1: capture the valid pong.
    let (mut socket1, _) = connect_async(&url).await.expect("connect 1");
    let hello1 = client_identity().sign_hello_initiator();
    let expected_nonce = hello1.initiator_nonce;
    send_message(&mut socket1, Message::HelloPing(hello1)).await;
    let Message::HelloPong(captured_pong) = next_message(&mut socket1).await else {
        panic!("first reply must be HELLO_PONG");
    };
    assert_eq!(captured_pong.initiator_nonce, expected_nonce, "pong echoes our nonce");
    drop(socket1);
    let (listener, captured_pong) = capture.await.expect("capture task");

    // Connection 2: consume the fresh HELLO_PING (nonce N2) and replay the captured
    // pong (echoing N1). The client's handshake_client must fail the nonce-echo check.
    let replay = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept 2");
        let mut socket = tokio_tungstenite::accept_async(stream).await.expect("ws accept 2");
        let fresh = next_message(&mut socket).await;
        assert!(matches!(fresh, Message::HelloPing(_)), "second frame must be HELLO_PING");
        send_message(&mut socket, Message::HelloPong(captured_pong)).await;
        // Keep the socket alive long enough for the client to read the replayed frame.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    });

    let accepted = ws::ping(
        "127.0.0.1",
        addr.port(),
        std::time::Duration::from_secs(5),
        &client_identity(),
        &client_allowlist(),
    )
    .await;
    assert!(!accepted, "a replayed HELLO_PONG must be rejected (initiator nonce mismatch)");
    let _ = replay.await;
}
