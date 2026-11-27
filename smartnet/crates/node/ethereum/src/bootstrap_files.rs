//! Bootstrap precondition: both node-level data files must exist and be loaded.
//!
//! `validators.jsonl` is the block gate's authority; `mainnet.jsonl` is the mesh membership
//! list. A node that starts without either one is not a node of this network, so the check
//! is fatal and runs before the rest of bootstrap. This is not a config-key exercise: the
//! registry is actually loaded into RAM and the peer file is actually read and parsed.

use alloy_primitives::Address;
use eyre::{bail, eyre, Context, Result};
use smartnet_mesh::peers::{parse_peers, PeerEntry};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::Path,
    sync::OnceLock,
};
use tracing::{info, warn};

use crate::validator_registry;

/// This node's keystore identity, pinned by the bootstrap guard so later registry loads can
/// re-apply the inert self-label after they replace the RAM array.
static SELF_ADDRESS: OnceLock<Address> = OnceLock::new();

/// Remember the identity used to label self entries. First call wins; boot calls this once.
pub fn set_self_address(address: Address) {
    let _ = SELF_ADDRESS.set(address);
}

/// The pinned identity, when the bootstrap guard has run.
pub fn self_address() -> Option<Address> {
    SELF_ADDRESS.get().copied()
}

/// Load both bootstrap files or refuse to start.
///
/// The `is_self` labels set here are inert bookkeeping: no gate, seal, or gossip decision
/// consults them.
pub fn ensure_bootstrap_files_loaded(
    val_file: &Path,
    peers_file: &Path,
    self_address: Address,
) -> Result<()> {
    set_self_address(self_address);
    if !val_file.is_file() {
        bail!(
            "validators.jsonl is missing at {} — cannot bootstrap without the validator registry",
            val_file.display()
        );
    }
    validator_registry::load_validators(val_file)
        .with_context(|| format!("load validators.jsonl {}", val_file.display()))?;
    calibri_lite_consensus::registry::mark_self(self_address);

    if !peers_file.is_file() {
        bail!(
            "mainnet.jsonl is missing at {} — cannot bootstrap without the mesh peer list",
            peers_file.display()
        );
    }
    let content = std::fs::read_to_string(peers_file)
        .with_context(|| format!("read mainnet.jsonl {}", peers_file.display()))?;
    let mut parsed = parse_peers(&content);
    PeerEntry::mark_self(&mut parsed.peers, None, self_address);
    for error in &parsed.errors {
        warn!(target: "smartnet::mesh", path = %peers_file.display(), %error, "mesh peer line skipped");
    }
    Ok(())
}

/// Whole-file logical validation of `mainnet.jsonl`, run before any identity matching.
///
/// Two records for one address, or two records on the same `ip:port` endpoint, are broken
/// membership data: no node may boot against such a file. Only parseable addresses are checked
/// for duplication; legacy label `key_id` values are still checked as endpoints.
fn validate_peer_file(peers: &[PeerEntry], path: &Path) -> Result<()> {
    let mut addresses: HashMap<Address, usize> = HashMap::new();
    let mut endpoints: HashMap<(&str, u16), usize> = HashMap::new();
    for (index, peer) in peers.iter().enumerate() {
        let line = index + 1;
        if let Some(address) = peer.address() &&
            let Some(first) = addresses.insert(address, line)
        {
            bail!(
                "mainnet.jsonl {} has duplicate address {address:#x} on lines {first} and {line}",
                path.display()
            );
        }
        let endpoint = (peer.ip.as_str(), peer.port_range);
        if let Some(first) = endpoints.insert(endpoint, line) {
            bail!(
                "mainnet.jsonl {} has duplicate endpoint {}:{} on lines {first} and {line}",
                path.display(),
                peer.ip,
                peer.port_range
            );
        }
    }
    Ok(())
}

/// Node bootstrap self-check, in this order:
///
/// 1. validate the whole peer file (duplicate addresses / duplicate `ip:port` are hard errors);
/// 2. check the optional `COINBASE` selector against the key-derived identity (already done by
///    `ensure_keystore_ready`, which decrypts the selected keystore and derives the address);
/// 3. find this node's single record in `mainnet.jsonl` by that derived address — both files are
///    checked against the identity, neither file is authoritative over the other;
/// 4. require the `ip:port` from config and from the record to be equal. A mismatch is a hard
///    error; a match is an INFO note and boot proceeds.
///
/// No defaults and no hardcoded endpoint: both `MESH_ADDR` and `MESH_PORT` must be specified, and
/// production uses real routable IPs, not loopback. `url`, `active`, `valid` (and the inert
/// `is_self` label) are deliberately ignored. Nothing is written.
pub fn ensure_mesh_self_consistent(
    peers_file: &Path,
    mesh_addr: Option<IpAddr>,
    mesh_port: Option<u16>,
    coinbase: Option<Address>,
    self_address: Address,
) -> Result<()> {
    let path = peers_file.display();

    // 1. Whole-file logical errors first — a broken file is a file problem, not a key problem.
    let content = std::fs::read_to_string(peers_file)
        .with_context(|| format!("read mainnet.jsonl {path} for self-check"))?;
    let parsed = parse_peers(&content);
    validate_peer_file(&parsed.peers, peers_file)?;

    // 2. COINBASE is only a keystore-file selector. When present it must still name the SAME
    // identity as the key that was actually decrypted: a renamed/swapped key is a key mismatch,
    // not a silent fallback to whatever the filename claimed.
    if let Some(coinbase) = coinbase &&
        coinbase != self_address
    {
        bail!(
            "COINBASE {coinbase:#x} disagrees with the address derived from the keystore \
             {self_address:#x} — keys mismatch"
        );
    }

    // 3. Locate this node's record by the identity derived from its key.
    let Some(entry) = parsed.peers.iter().find(|peer| peer.address() == Some(self_address)) else {
        bail!(
            "no record for this node's address {self_address:#x} in mainnet.jsonl {path} — add one \
             with this address and the ip/port from config.toml"
        );
    };

    // 4. Match the two records against each other, not one against the other. Neither file is
    // authoritative: we only need to know whether they describe the same endpoint for this key.
    let mesh_addr = mesh_addr
        .ok_or_else(|| eyre!("MESH_ADDR is not set in config.toml — no default, specify a real IP"))?;
    let mesh_port = mesh_port
        .ok_or_else(|| eyre!("MESH_PORT is not set in config.toml — no default, specify a port"))?;
    if entry.ip != mesh_addr.to_string() || entry.port_range != mesh_port {
        bail!(
            "mesh endpoint mismatch for node {self_address:#x}: config.toml MESH_ADDR/MESH_PORT \
             is {mesh_addr}:{mesh_port}, mainnet.jsonl {path} ip/port_range is {}:{} — \
             the two files must agree",
            entry.ip,
            entry.port_range
        );
    }
    info!(
        target: "smartnet::mesh",
        address = %self_address,
        endpoint = %format!("{mesh_addr}:{mesh_port}"),
        "config.toml and mainnet.jsonl agree on this node's mesh endpoint"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deliberately not 41711: nodes may listen on distinct localhost ports.
    const TEST_IP: &str = "127.0.0.1";
    const TEST_PORT: u16 = 41_722;

    fn address(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    fn config_ip() -> IpAddr {
        TEST_IP.parse().expect("valid test ip")
    }

    #[test]
    fn refuses_when_validator_registry_is_absent() {
        let root = tempfile::tempdir().unwrap();
        let peers = root.path().join("mainnet.jsonl");
        std::fs::write(&peers, "{}").unwrap();
        let err = ensure_bootstrap_files_loaded(
            &root.path().join("validators.jsonl"),
            &peers,
            address(1),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("validators.jsonl"));
    }

    #[test]
    fn refuses_when_peer_file_is_absent() {
        let root = tempfile::tempdir().unwrap();
        let vals = root.path().join("validators.jsonl");
        std::fs::write(&vals, format!("{{\"address\":\"{:?}\",\"status\":true}}\n", address(2)))
            .unwrap();
        let err = ensure_bootstrap_files_loaded(
            &vals,
            &root.path().join("mainnet.jsonl"),
            address(2),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("mainnet.jsonl"));
    }

    fn write_peers(
        root: &std::path::Path,
        addr: Address,
        ip: &str,
        port: u16,
        active: bool,
        valid: bool,
    ) -> std::path::PathBuf {
        let path = root.join("mainnet.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"address\":\"{addr:#x}\",\"url\":\"irrelevant\",\"ip\":\"{ip}\",\
                 \"port_range\":{port},\"active\":{active},\"valid\":{valid}}}\n"
            ),
        )
        .unwrap();
        path
    }

    fn check(path: &std::path::Path, coinbase: Option<Address>, addr: Address) -> Result<()> {
        ensure_mesh_self_consistent(path, Some(config_ip()), Some(TEST_PORT), coinbase, addr)
    }

    #[test]
    fn self_check_accepts_matching_triplet_and_ignores_url_active_valid() {
        let root = tempfile::tempdir().unwrap();
        let addr = address(3);
        let peers = write_peers(root.path(), addr, TEST_IP, TEST_PORT, false, false);
        // url / active / valid are bookkeeping, explicitly not part of the comparison.
        check(&peers, None, addr).expect("matching triplet");
        check(&peers, Some(addr), addr).expect("COINBASE agreeing with the key");
    }

    #[test]
    fn self_check_accepts_non_default_port_when_record_matches() {
        let root = tempfile::tempdir().unwrap();
        let addr = address(12);
        let port = 41_999;
        let peers = write_peers(root.path(), addr, TEST_IP, port, true, true);
        ensure_mesh_self_consistent(&peers, Some(config_ip()), Some(port), None, addr)
            .expect("port matches the self record; there is no hardcoded port");
        let err =
            ensure_mesh_self_consistent(&peers, Some(config_ip()), Some(TEST_PORT), None, addr)
                .unwrap_err();
        assert!(format!("{err:#}").contains("MESH_PORT"), "got: {err:#}");
    }

    #[test]
    fn self_check_accepts_two_node_file_with_optional_header() {
        // The operator's shape: a header with string-valued fields, node1 on 41711 and
        // node2 on 41722. Booting node2 must pass now that the port is a config↔record match.
        let root = tempfile::tempdir().unwrap();
        let self_addr = address(15);
        let other = address(16);
        let path = root.path().join("mainnet.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"mesh_protocol_ver\":\"0.1.1\",\"signed\":\"\",\"ts\":\"\"}}\n\
                 {{\"address\":\"{other:#x}\",\"url\":\"\",\"ip\":\"{TEST_IP}\",\"port_range\":41711,\"active\":true,\"valid\":true}}\n\
                 {{\"address\":\"{self_addr:#x}\",\"url\":\"\",\"ip\":\"{TEST_IP}\",\"port_range\":41722,\"active\":true,\"valid\":true}}\n"
            ),
        )
        .unwrap();
        ensure_mesh_self_consistent(&path, Some(config_ip()), Some(41_722), None, self_addr)
            .expect("second localhost node boots on its own port");
    }

    #[test]
    fn self_check_accepts_routable_ip_from_both_files() {
        // Production uses real IPs: no loopback default, no fallback. Both sources name the
        // same public endpoint, so the symmetric match succeeds.
        let root = tempfile::tempdir().unwrap();
        let addr = address(17);
        let ip = "203.0.113.10";
        let port = 41_333;
        let peers = write_peers(root.path(), addr, ip, port, true, true);
        ensure_mesh_self_consistent(
            &peers,
            Some(ip.parse::<IpAddr>().unwrap()),
            Some(port),
            None,
            addr,
        )
        .expect("routable IP and port specified in both files");
    }

    #[test]
    fn self_check_rejects_missing_or_wrong_mesh_addr() {
        let root = tempfile::tempdir().unwrap();
        let addr = address(4);
        let peers = write_peers(root.path(), addr, TEST_IP, TEST_PORT, true, true);
        for mesh_addr in [None, Some("0.0.0.0".parse::<IpAddr>().unwrap())] {
            let err = ensure_mesh_self_consistent(
                &peers,
                mesh_addr,
                Some(TEST_PORT),
                None,
                addr,
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("MESH_ADDR"), "got: {err:#}");
        }
    }

    #[test]
    fn self_check_rejects_missing_or_wrong_mesh_port() {
        let root = tempfile::tempdir().unwrap();
        let addr = address(5);
        let peers = write_peers(root.path(), addr, TEST_IP, TEST_PORT, true, true);
        for mesh_port in [None, Some(41_999)] {
            let err = ensure_mesh_self_consistent(
                &peers,
                Some(config_ip()),
                mesh_port,
                None,
                addr,
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("MESH_PORT"), "got: {err:#}");
        }
    }

    #[test]
    fn self_check_rejects_coinbase_key_mismatch() {
        let root = tempfile::tempdir().unwrap();
        let addr = address(6);
        let peers = write_peers(root.path(), addr, TEST_IP, TEST_PORT, true, true);
        let err = check(&peers, Some(address(7)), addr).unwrap_err();
        let report = format!("{err:#}");
        assert!(report.contains("COINBASE") && report.contains("keys mismatch"), "got: {report}");
    }

    #[test]
    fn self_check_rejects_missing_or_ambiguous_self_record() {
        let root = tempfile::tempdir().unwrap();
        let addr = address(8);
        let missing =
            write_peers(root.path(), address(9), TEST_IP, TEST_PORT, true, true);
        let err = check(&missing, None, addr).unwrap_err();
        assert!(format!("{err:#}").contains("no record"), "got: {err:#}");

        // A duplicate address is a whole-file logical error and is reported before identity.
        let duplicate = format!(
            "{{\"address\":\"{addr:#x}\",\"ip\":\"{TEST_IP}\",\"port_range\":{TEST_PORT}}}\n"
        );
        std::fs::write(&missing, format!("{duplicate}{duplicate}")).unwrap();
        let err = check(&missing, None, addr).unwrap_err();
        assert!(format!("{err:#}").contains("duplicate address"), "got: {err:#}");
    }

    #[test]
    fn self_check_rejects_duplicate_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("mainnet.jsonl");
        let first = address(13);
        let second = address(14);
        std::fs::write(
            &path,
            format!(
                "{{\"address\":\"{first:#x}\",\"ip\":\"{TEST_IP}\",\"port_range\":{TEST_PORT}}}\n\
                 {{\"address\":\"{second:#x}\",\"ip\":\"{TEST_IP}\",\"port_range\":{TEST_PORT}}}\n"
            ),
        )
        .unwrap();
        let err = check(&path, None, first).unwrap_err();
        assert!(format!("{err:#}").contains("duplicate endpoint"), "got: {err:#}");
    }

    #[test]
    fn self_check_rejects_wrong_self_ip_or_port() {
        let root = tempfile::tempdir().unwrap();
        let addr = address(10);
        let wrong_ip = write_peers(root.path(), addr, "127.0.0.2", TEST_PORT, true, true);
        let err = check(&wrong_ip, None, addr).unwrap_err();
        assert!(format!("{err:#}").contains("mismatch"), "got: {err:#}");

        let wrong_port = write_peers(root.path(), addr, TEST_IP, 41_999, true, true);
        let err = check(&wrong_port, None, addr).unwrap_err();
        assert!(format!("{err:#}").contains("mismatch"), "got: {err:#}");
    }

    #[test]
    fn self_check_reports_unreadable_peer_file() {
        let root = tempfile::tempdir().unwrap();
        let err = check(&root.path().join("absent.jsonl"), None, address(11)).unwrap_err();
        assert!(format!("{err:#}").contains("mainnet.jsonl"), "got: {err:#}");
    }
}
