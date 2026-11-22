//! Peer-file (`mainnet.jsonl`) parsing: one JSON object per line, tolerant of junk.
//!
//! Schema (kept compatible with `go-mesh-real`'s `mainnet.jsonl`):
//!
//! ```json
//! {"address":"0x...","url":"","ip":"127.0.0.1","port_range":30888,"active":true,"valid":true}
//! ```
//!
//! - `ip` + `port_range` is where we open the `WebSocket` mesh connection
//!   (`ws://ip:port_range/mesh`).
//! - `address` is the verified Ethereum address (`0x…`, 20 bytes) — the identity allowlist a peer
//!   must back with a signed `HELLO`; `key_id` is accepted as a legacy alias. `url` is unused
//!   (defaults to empty).
//! - `active && valid` gates sending; our own entry (same listen port) is skipped as self.
//! - Blank lines and `#` comments are ignored; a malformed line is skipped with a warning (reported
//!   by [`parse_peers`]) — one bad line must not disable the whole mesh.
//! - The file is re-read on every spread, so membership changes need no restart.

use alloy_primitives::Address;
use serde::Deserialize;
use smartnet_fs_util::runtime_paths::resolve_data_file;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use tracing::warn;

/// One entry of the peer file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PeerEntry {
    /// The peer's verified Ethereum address (`0x…`) — the identity allowlist a peer
    /// must back with a signed `HELLO`. `key_id` is the legacy field name; both names
    /// feed this one value. A legacy/label value parses to no address and cannot be
    /// authenticated.
    #[serde(default, alias = "address")]
    pub key_id: String,
    /// Unused (reserved), defaults to empty.
    #[serde(default)]
    pub url: String,
    /// Host/IP we connect to.
    pub ip: String,
    /// `WebSocket` mesh port (`ws://ip:port_range/mesh`).
    pub port_range: u16,
    /// Whether gossip to this peer is enabled.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Whether this peer is still considered a valid mesh member.
    #[serde(default = "default_true")]
    pub valid: bool,
}

const fn default_true() -> bool {
    true
}

impl PeerEntry {
    /// The absolute URL of this peer's `WebSocket` mesh endpoint.
    pub fn mesh_url(&self) -> String {
        format!("ws://{}:{}/mesh", self.ip, self.port_range)
    }

    /// Whether rumors should be sent to this entry: `active && valid`, not ourselves.
    ///
    /// Self-identity is both the listen port and the node identity address. The port catches
    /// a normal `MESH_PORT` mismatch; the address also prevents a node with a misconfigured
    /// port from dialing its own keystore identity.
    pub fn is_target(&self, self_port: u16, self_address: Address) -> bool {
        self.active
            && self.valid
            && self.port_range != self_port
            && self.address() != Some(self_address)
    }

    /// This entry's `key_id` parsed as the verified Ethereum address it now carries.
    ///
    /// `key_id` holds a `0x`-prefixed 20-byte address (the identity allowlist). It is
    /// **not** self-authenticating — it is the *claim* a peer must back with a signed
    /// `HELLO`. Returns `None` for a legacy/label `key_id` that is not an address.
    pub fn address(&self) -> Option<Address> {
        self.key_id.trim().parse::<Address>().ok()
    }
}

/// Bare-mesh fallback when `MESH_PEERS_FILE` is unset: `./mainnet.jsonl`, then
/// `~/.smartnet/mainnet.jsonl`.
///
/// Configured nodes resolve the key against `HOME_DIR_PATH` in `load_operator_overlay`, so this
/// is only for direct mesh users with no node config. If neither location exists, the error is
/// logged and an empty path is returned — outbound gossip stays off, inbound still works.
pub fn default_peers_file() -> PathBuf {
    let Some(home) = std::env::var_os("HOME") else {
        warn!(
            target: "smartnet::mesh",
            "HOME is not set and MESH_PEERS_FILE is unset — outbound mesh gossip disabled"
        );
        return PathBuf::new();
    };
    let base = PathBuf::from(home).join(".smartnet");
    match resolve_data_file("mainnet.jsonl", &base, None) {
        Ok(path) => path,
        Err(err) => {
            warn!(
                target: "smartnet::mesh",
                %err,
                "no MESH_PEERS_FILE and mainnet.jsonl is not in ./ or ~/.smartnet — outbound mesh gossip disabled"
            );
            PathBuf::new()
        }
    }
}

/// Result of [`parse_peers`]: the entries that parsed plus human-readable line errors.
#[derive(Debug, Default)]
pub struct ParsedPeers {
    /// Successfully parsed entries, in file order.
    pub peers: Vec<PeerEntry>,
    /// One message per skipped line (caller logs them at `warn`).
    pub errors: Vec<String>,
}

/// Parses peer-file content. Blank lines and `#` comments are ignored; malformed lines
/// are collected in [`ParsedPeers::errors`] instead of failing the whole parse.
pub fn parse_peers(content: &str) -> ParsedPeers {
    let mut parsed = ParsedPeers::default();
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match serde_json::from_str::<PeerEntry>(line) {
            Ok(entry) => parsed.peers.push(entry),
            Err(err) => {
                parsed.errors.push(format!("line {}: {err}", index + 1));
            }
        }
    }
    parsed
}

/// Reads and parses a peer file.
///
/// [`io::ErrorKind::NotFound`] is passed through (callers treat it as "mesh outbound
/// disabled", logged once) — a missing file is a normal pre-provisioning state.
pub fn load_peers(path: &Path) -> Result<ParsedPeers, io::Error> {
    Ok(parse_peers(&fs::read_to_string(path)?))
}

/// Builds the inbound-`HELLO` allowlist from a peer file: the addresses of entries whose
/// identity field parses as a `0x` address **and** that are still members (`active && valid`
/// — the same gate as [`PeerEntry::is_target`]).
///
/// This is the cheap `DoS` gate order: an inbound `HELLO` whose claimed address is absent
/// from this list is dropped before any signature recovery. Re-read with the peer file so
/// membership changes take effect without a restart: marking a peer `valid: false`
/// (revoked) or `active: false` (paused) removes it from the inbound allowlist on the
/// next handshake, not just from outbound gossip. Entries with a legacy/label identity
/// contribute no address and are warned once per allowlist read.
pub fn allowlist_addresses(parsed: &ParsedPeers) -> Vec<Address> {
    parsed
        .peers
        .iter()
        .filter(|peer| peer.active && peer.valid)
        .filter_map(|peer| match peer.address() {
            Some(address) => Some(address),
            None => {
                warn!(
                    target: "smartnet::mesh",
                    ip = %peer.ip, port = peer.port_range,
                    "mesh peer has no 0x identity address — excluded from HELLO allowlist"
                );
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The 8-entry reference shape used across mesh tests.
    pub(crate) const REFERENCE_FILE: &str = r#"{"key_id": "1234abcd", "url": "", "ip": "127.0.0.1", "port_range": 30888, "active": true, "valid": true}
{"key_id": "dcba4321", "url": "", "ip": "127.0.0.1", "port_range": 30811, "active": true, "valid": true}
{"key_id": "node-c", "url": "", "ip": "127.0.0.1", "port_range": 30822, "active": true, "valid": true}
{"key_id": "node-d", "url": "", "ip": "127.0.0.1", "port_range": 30833, "active": false, "valid": true}
{"key_id": "node-e", "url": "", "ip": "127.0.0.1", "port_range": 30844, "active": true, "valid": false}
{"key_id": "node-f", "url": "", "ip": "127.0.0.1", "port_range": 30855}
{"key_id": "node-g", "url": "", "ip": "10.0.0.9", "port_range": 30866, "active": true, "valid": true}
{"key_id": "node-h", "url": "", "ip": "127.0.0.1", "port_range": 30877, "active": true, "valid": true}
"#;

    #[test]
    fn parses_reference_eight_entry_file() {
        let parsed = parse_peers(REFERENCE_FILE);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert_eq!(parsed.peers.len(), 8);
        assert_eq!(parsed.peers[0].key_id, "1234abcd");
        assert_eq!(parsed.peers[0].port_range, 30888);
        assert_eq!(parsed.peers[6].ip, "10.0.0.9");
        // missing active/valid default to true
        assert!(parsed.peers[5].active && parsed.peers[5].valid);
        // explicit false is preserved
        assert!(!parsed.peers[3].active);
        assert!(!parsed.peers[4].valid);
    }

    #[test]
    fn skips_blanks_comments_and_bad_lines() {
        let content = "\n# comment\nnot json\n{\"ip\":\"1.2.3.4\",\"port_range\":1}\n\n";
        let parsed = parse_peers(content);
        assert_eq!(parsed.peers.len(), 1);
        assert_eq!(parsed.peers[0].ip, "1.2.3.4");
        assert_eq!(parsed.errors.len(), 1, "{:?}", parsed.errors);
        assert!(parsed.errors[0].starts_with("line 3:"), "{}", parsed.errors[0]);
    }

    #[test]
    fn missing_ip_is_an_error_not_a_default() {
        let parsed = parse_peers(r#"{"port_range":1}"#);
        assert!(parsed.peers.is_empty());
        assert_eq!(parsed.errors.len(), 1);
    }

    #[test]
    fn mesh_url_shape() {
        let peer: PeerEntry =
            serde_json::from_str(r#"{"ip":"127.0.0.1","port_range":30888}"#).unwrap();
        assert_eq!(peer.mesh_url(), "ws://127.0.0.1:30888/mesh");
    }

    #[test]
    fn address_parses_key_id_and_skips_labels() {
        let addr = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
        let peer: PeerEntry = serde_json::from_str(&format!(
            r#"{{"key_id":"{addr}","ip":"127.0.0.1","port_range":30888}}"#
        ))
        .unwrap();
        assert_eq!(peer.address(), Some(addr.parse::<Address>().unwrap()));

        // The live operator file uses `address`; it is the same identity field.
        let peer: PeerEntry = serde_json::from_str(&format!(
            r#"{{"address":"{addr}","ip":"127.0.0.1","port_range":30888}}"#
        ))
        .unwrap();
        assert_eq!(peer.address(), Some(addr.parse::<Address>().unwrap()));

        // A legacy label / free-form `key_id` is not an address — no allowlist entry.
        let label: PeerEntry =
            serde_json::from_str(r#"{"key_id":"node-c","ip":"127.0.0.1","port_range":1}"#).unwrap();
        assert_eq!(label.address(), None);
    }

    #[test]
    fn self_skip_uses_port_and_identity() {
        let addr: Address = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8".parse().unwrap();
        let other: Address = "0x1111111111111111111111111111111111111111".parse().unwrap();
        let peer: PeerEntry = serde_json::from_str(&format!(
            r#"{{"address":"{addr}","ip":"127.0.0.1","port_range":41711}}"#
        ))
        .unwrap();

        assert!(!peer.is_target(41711, other), "same port is self");
        assert!(!peer.is_target(65534, addr), "same identity is self");
        assert!(peer.is_target(65534, other), "different port and identity is a target");
    }

    #[test]
    fn allowlist_addresses_collects_only_verifiable_entries() {
        let content = r#"{"key_id":"0x70997970c51812dc3a010c7d01b50e0d17dc79c8","ip":"127.0.0.1","port_range":30888}
{"key_id":"node-c","ip":"127.0.0.1","port_range":30822}
{"key_id":"0x1111111111111111111111111111111111111111","ip":"127.0.0.1","port_range":30811}
"#;
        let parsed = parse_peers(content);
        let allowlist = allowlist_addresses(&parsed);
        // Only the two 0x-address entries contribute; the label entry is skipped.
        assert_eq!(allowlist.len(), 2);
        assert!(allowlist.contains(&"0x70997970c51812dc3a010c7d01b50e0d17dc79c8".parse().unwrap()));
        assert!(allowlist.contains(&"0x1111111111111111111111111111111111111111".parse().unwrap()));
    }

    /// Revocation must revoke: a `valid: false` (or `active: false`) peer is excluded
    /// from the inbound-`HELLO` allowlist, not only from outbound gossip — otherwise a
    /// revoked peer keeps full inbound access on its next connection.
    #[test]
    fn allowlist_excludes_revoked_and_paused_peers() {
        let content = r#"{"key_id":"0x1111111111111111111111111111111111111111","ip":"1.1.1.1","port_range":1,"active":true,"valid":true}
{"key_id":"0x2222222222222222222222222222222222222222","ip":"1.1.1.2","port_range":2,"active":true,"valid":false}
{"key_id":"0x3333333333333333333333333333333333333333","ip":"1.1.1.3","port_range":3,"active":false,"valid":true}
"#;
        let parsed = parse_peers(content);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let allowlist = allowlist_addresses(&parsed);
        let member: Address = "0x1111111111111111111111111111111111111111".parse().unwrap();
        let revoked: Address = "0x2222222222222222222222222222222222222222".parse().unwrap();
        let paused: Address = "0x3333333333333333333333333333333333333333".parse().unwrap();
        assert_eq!(allowlist, vec![member], "only the active+valid member is admitted");
        assert!(!allowlist.contains(&revoked), "revoked peer must lose inbound access");
        assert!(!allowlist.contains(&paused), "paused peer must lose inbound access");
    }

    #[test]
    fn load_reads_from_disk_and_propagates_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mainnet.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "{REFERENCE_FILE}").unwrap();

        let parsed = load_peers(&path).unwrap();
        assert_eq!(parsed.peers.len(), 8);

        let missing = dir.path().join("nope.jsonl");
        let err = load_peers(&missing).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
