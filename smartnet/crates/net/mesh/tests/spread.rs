//! `spread` fanout over binary `WebSocket`: active-valid targets only, self-skip, dead peers
//! tolerated, peer-file hot reload between spreads, and the persistent link (one connection
//! per peer carrying every rumor, reconnect after a dead peer comes up).

mod common;

use common::{
    client_identity, dead_addr, install_tls, spawn_ws_recorder, spawn_ws_recorder_on, NoopInjector,
    WsRecorded,
};
use smartnet_mesh::{
    identity::Identity,
    protocol::Message,
    rumor::Rumor,
    seen::SeenCache,
    spread::{SpreadConfig, Spreader},
    store::ObjectCache,
    sync_gate::SyncGate,
};
use std::{sync::Arc, time::Duration};
use tokio::net::TcpListener;

/// A peer-file line for a recorder: its `key_id` is the recorder's address (the dialer's
/// client-side allowlist requires the responder to authenticate).
fn peer_line(identity: &Identity, port: u16, active: bool, valid: bool) -> String {
    format!(
        r#"{{"address":"{:#x}","url":"","ip":"127.0.0.1","port_range":{port},"active":{active},"valid":{valid}}}"#,
        identity.address()
    )
}

/// Writes a peer file (temp dir keeps the file alive for the test).
fn write_peers(dir: &tempfile::TempDir, lines: &[String]) -> std::path::PathBuf {
    let path = dir.path().join("mainnet.jsonl");
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write peers");
    path
}

/// A stable validators.jsonl beside the peer file, for the bootstrap config-hash gate.
fn validators_file(path: &std::path::Path) -> std::path::PathBuf {
    let validators = path.with_file_name("validators.jsonl");
    if !validators.exists() {
        std::fs::write(
            &validators,
            b"{\"address\":\"0x0000000000000000000000000000000000000001\",\"status\":true}\n",
        )
        .expect("write validators");
    }
    validators
}

fn spreader(path: &std::path::Path, self_port: u16) -> Spreader {
    let validators = validators_file(path);
    Spreader::new(
        SpreadConfig::new(path, self_port, Arc::new(client_identity()))
            .with_validators_file(validators),
        Arc::new(NoopInjector),
        Arc::new(SeenCache::default()),
        Arc::new(ObjectCache::default()),
        SyncGate::new(),
    )
}

/// Announce messages the recorder saw (excludes handshake/keepalive frames).
fn announced(recorded: &WsRecorded) -> Vec<Message> {
    recorded
        .messages()
        .into_iter()
        .filter(|message| {
            matches!(message, Message::RumorTxPing { .. } | Message::RumorBlocksPing { .. })
        })
        .collect()
}

#[tokio::test]
async fn fanout_reaches_active_peers_only() {
    install_tls();
    let (live_a, id_a, _) = spawn_ws_recorder().await;
    let (live_b, id_b, _) = spawn_ws_recorder().await;
    let (inactive, id_inactive, inactive_rec) = spawn_ws_recorder().await;
    let (invalid, id_invalid, invalid_rec) = spawn_ws_recorder().await;
    let dead = dead_addr().await;
    let self_port = 41_811;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_peers(
        &dir,
        &[
            peer_line(&id_a, live_a.port(), true, true),
            peer_line(&id_b, live_b.port(), true, true),
            // The dead entry needs a distinct identity — self entries are skipped.
            peer_line(&common::server_identity(), dead.port(), true, true),
            peer_line(&id_inactive, inactive.port(), false, true),
            peer_line(&id_invalid, invalid.port(), true, false),
            peer_line(&client_identity(), self_port, true, true), // self
            "totally not json".to_string(),
        ],
    );

    let spreader = spreader(&path, self_port);
    let report = spreader.spread(&Rumor::tx(vec![0xca, 0xfe])).await;

    // 3 targets: live_a, live_b, dead (inactive/invalid/self/malformed excluded).
    assert_eq!(report.targeted, 3, "{report:?}");
    assert_eq!(report.delivered, 2, "{report:?}");
    assert_eq!(report.failed, 1, "{report:?}");
    assert!(inactive_rec.is_empty() && invalid_rec.is_empty());
}

#[tokio::test]
async fn live_peers_receive_the_block_announce() {
    install_tls();
    let (live_a, id_a, rec_a) = spawn_ws_recorder().await;
    let (live_b, id_b, rec_b) = spawn_ws_recorder().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_peers(
        &dir,
        &[peer_line(&id_a, live_a.port(), true, true), peer_line(&id_b, live_b.port(), true, true)],
    );

    let spreader = spreader(&path, 1);
    let rumor = Rumor::block(9, vec![0x01, 0x02, 0x03]);
    let report = spreader.spread(&rumor).await;
    assert_eq!((report.targeted, report.delivered, report.failed), (2, 2, 0), "{report:?}");

    for recorded in [&rec_a, &rec_b] {
        let announces = announced(recorded);
        assert_eq!(announces.len(), 1, "{announces:?}");
        assert_eq!(announces[0], Message::RumorBlocksPing { block_nr: 9 }, "{announces:?}");
    }
}

#[tokio::test]
async fn missing_peers_file_means_no_targets() {
    install_tls();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("absent.jsonl");
    let spreader = spreader(&path, 1);
    let report = spreader.spread(&Rumor::tx(vec![0x01])).await;
    assert_eq!(report.targeted, 0);
}

#[tokio::test]
async fn peer_file_is_reread_on_every_spread() {
    install_tls();
    let (first, id_first, rec_first) = spawn_ws_recorder().await;
    let (second, id_second, rec_second) = spawn_ws_recorder().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_peers(&dir, &[peer_line(&id_first, first.port(), true, true)]);

    let spreader = spreader(&path, 1);
    let report = spreader.spread(&Rumor::tx(vec![0x01])).await;
    assert_eq!(report.delivered, 1);

    // Operator swaps membership: drop `first`, add `second` — no restart needed.
    std::fs::write(&path, format!("{}\n", peer_line(&id_second, second.port(), true, true)))
        .expect("rewrite peers");
    let report = spreader.spread(&Rumor::tx(vec![0x02])).await;
    assert_eq!(report.delivered, 1);

    assert_eq!(announced(&rec_first).len(), 1);
    assert_eq!(announced(&rec_second).len(), 1);
}

#[tokio::test]
async fn one_peer_connection_carries_every_rumor() {
    install_tls();
    let (peer, id_peer, rec) = spawn_ws_recorder().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_peers(&dir, &[peer_line(&id_peer, peer.port(), true, true)]);

    let spreader = spreader(&path, 1);
    for i in 0..3u8 {
        let report = spreader.spread(&Rumor::tx(vec![0x10, i])).await;
        assert_eq!((report.targeted, report.delivered, report.failed), (1, 1, 0), "{report:?}");
    }

    // One peer = one WebSocket, reused for every message; all three announces arrived on it.
    assert_eq!(rec.connections(), 1, "peer must keep a single connection");
    assert_eq!(announced(&rec).len(), 3, "every rumor must arrive on that connection");
}

#[tokio::test]
async fn link_reconnects_when_a_dead_peer_comes_up() {
    install_tls();
    let dead = dead_addr().await;
    let dir = tempfile::tempdir().expect("tempdir");
    // The (future) recorder's identity is unknown before it binds — reuse a fixed test
    // identity for the dead entry; the client-side allowlist checks the responder's
    // address, so the re-upped recorder must use that same identity.
    let peer_id = common::test_identity(42);
    let path = write_peers(&dir, &[peer_line(&peer_id, dead.port(), true, true)]);
    let spreader = spreader(&path, 1);

    // Down peer: targeted but nothing delivered.
    let report = spreader.spread(&Rumor::tx(vec![0xde, 0xad])).await;
    assert_eq!((report.targeted, report.delivered, report.failed), (1, 0, 1), "{report:?}");

    // The peer comes up on the same port — the link's backoff reconnects on its own.
    // The recorder must authenticate as the identity already listed in the peer file.
    let listener = TcpListener::bind(("127.0.0.1", dead.port())).await.expect("rebind dead port");
    let (_, rec) = spawn_ws_recorder_on(listener, peer_id).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let report = spreader.spread(&Rumor::tx(vec![0xbe, 0xef])).await;
    assert_eq!((report.targeted, report.delivered, report.failed), (1, 1, 0), "{report:?}");
    assert_eq!(rec.connections(), 1, "reconnect must dial exactly once");
    assert_eq!(announced(&rec).len(), 1, "reconnected link must deliver exactly one rumor");
}

/// Boot must not block on peers that cannot be reached: the first failed handshake
/// counts as the link's boot chance and releases the gate at once — no waiting out
/// the boot timeout on a dead or absent peer. (Background reconnect still runs; it
/// just no longer holds the gate.)
#[tokio::test]
async fn dead_peer_releases_the_gate_on_its_first_handshake_try() {
    install_tls();
    let dead = dead_addr().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_peers(&dir, &[peer_line(&common::server_identity(), dead.port(), true, true)]);

    let gate = SyncGate::new();
    let validators = validators_file(&path);
    let spreader = Spreader::new(
        SpreadConfig::new(&path, 41_812, Arc::new(client_identity()))
            .with_validators_file(validators),
        Arc::new(NoopInjector),
        Arc::new(SeenCache::default()),
        Arc::new(ObjectCache::default()),
        gate.clone(),
    );
    let targets = spreader.targets();
    assert_eq!(targets.len(), 1, "{targets:?}");
    gate.set_expected(targets.iter().map(|peer| format!("{}:{}", peer.ip, peer.port_range)));
    spreader.preconnect(&targets);

    let ready =
        smartnet_mesh::sync_gate::wait_ready(gate.subscribe(), Duration::from_secs(2)).await;
    assert!(ready, "first failed handshake must release the gate, not the boot timeout");
}
