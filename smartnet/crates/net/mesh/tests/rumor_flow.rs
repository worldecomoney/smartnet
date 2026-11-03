//! Loop suppression: a seen object hash is never re-injected, and the origin's own echo
//! (A→B→A) is dropped.

mod common;

use common::{client_allowlist, client_identity, install_tls, spawn_mesh_server_record};
use smartnet_mesh::{rumor::Rumor, ws};
use std::time::Duration;

#[tokio::test]
async fn duplicate_delivery_injects_only_once() {
    install_tls();
    let dir = tempfile::tempdir().expect("tempdir");
    let peers_file = dir.path().join("mainnet.jsonl");
    std::fs::write(&peers_file, format!("{}\n", common::client_peer_line(1))).expect("write peers");

    let (addr, seen, _, recorded) = spawn_mesh_server_record(peers_file).await;
    let rumor = Rumor::tx(vec![0x11, 0x22]);
    let identity = client_identity();
    let allowlist = client_allowlist();

    ws::send_rumor(
        "127.0.0.1",
        addr.port(),
        &rumor,
        Duration::from_secs(5),
        &identity,
        &allowlist,
    )
    .await
    .expect("first");
    ws::send_rumor(
        "127.0.0.1",
        addr.port(),
        &rumor,
        Duration::from_secs(5),
        &identity,
        &allowlist,
    )
    .await
    .expect("second");

    assert_eq!(recorded.requests().len(), 1);
    assert_eq!(seen.len(), 1);
}

#[tokio::test]
async fn origin_marked_rumor_is_not_reinjected_on_echo() {
    install_tls();
    let dir = tempfile::tempdir().expect("tempdir");
    let peers_file = dir.path().join("mainnet.jsonl");
    std::fs::write(&peers_file, format!("{}\n", common::client_peer_line(1))).expect("write peers");

    let (addr, seen, _, recorded) = spawn_mesh_server_record(peers_file).await;

    // Origin marks its own rumor before spreading (the node glue does this too).
    let rumor = Rumor::block(3, vec![0xaa, 0xbb]);
    assert!(seen.check_and_mark(rumor.hash));

    // A peer echoes the same object back (A→B→A path) — it must not be injected again.
    ws::send_rumor(
        "127.0.0.1",
        addr.port(),
        &rumor,
        Duration::from_secs(5),
        &client_identity(),
        &client_allowlist(),
    )
    .await
    .expect("echo handled as have");

    // The status probe (`eth_blockNumber`) is not an inject; only inject methods count.
    let injects: Vec<String> = recorded
        .requests()
        .iter()
        .filter_map(|request| {
            let value: serde_json::Value = serde_json::from_slice(&request.body).ok()?;
            let method = value["method"].as_str().unwrap_or_default().to_string();
            (method == "eth_sendRawTransaction" || method == "smartnet_newPayload")
                .then_some(method)
        })
        .collect();
    assert!(injects.is_empty(), "echo of our own rumor must not be injected again: {injects:?}");
    assert_eq!(seen.len(), 1);
}
