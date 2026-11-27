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
//! - An optional first-line header `{"mesh_protocol_ver": 11, "signed": "", "ts": 0,
//!   "hash": "", "block": 0}` is read and kept in RAM for later use, never checked yet.
//!   Absent means every field defaults to `0` / empty string (no error).

use alloy_primitives::Address;
use serde::Deserialize;
use smartnet_fs_util::runtime_paths::resolve_data_file;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::RwLock,
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
    /// Inert label: `true` when this entry is this node (same listen port or identity address).
    /// Set after parsing for later callers; gossip still uses `is_target` unchanged.
    #[serde(default)]
    pub is_self: bool,
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

    /// Label loaded peer entries whose port or identity address is ours. Pure bookkeeping:
    /// `is_target` and the allowlist still make their own decisions, so gossip behavior is
    /// unchanged.
    pub fn mark_self(peers: &mut [Self], self_port: Option<u16>, self_address: Address) {
        for peer in peers {
            peer.is_self = self_port.is_some_and(|port| peer.port_range == port) ||
                peer.address() == Some(self_address);
        }
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

/// The optional first-line header of `mainnet.jsonl`. Purely bookkeeping for now: it is read,
/// kept in RAM, and never checked — no handshake or gossip decision consults it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshFileHeader {
    /// `mesh_protocol_ver` — an integer; `0` when the header is absent.
    pub mesh_protocol_ver: u64,
    /// `signed` — empty string when absent.
    pub signed: String,
    /// `ts` — `0` when absent.
    pub ts: u64,
    /// `hash` — empty string when absent.
    pub hash: String,
    /// `block` — `0` when absent.
    pub block: u64,
}

impl MeshFileHeader {
    /// Const empty header for the static RAM slot (`Default` is not const for `String`).
    pub const fn empty() -> Self {
        Self {
            mesh_protocol_ver: 0,
            signed: String::new(),
            ts: 0,
            hash: String::new(),
            block: 0,
        }
    }

    /// Read the known fields leniently: missing or wrong-typed values become the same
    /// defaults an absent header gets. Nothing is validated at this stage.
    fn from_value(value: &serde_json::Value) -> Self {
        let field = |name: &str| value.get(name);
        Self {
            mesh_protocol_ver: field("mesh_protocol_ver").and_then(|v| v.as_u64()).unwrap_or(0),
            signed: field("signed").and_then(|v| v.as_str()).unwrap_or("").to_owned(),
            ts: field("ts").and_then(|v| v.as_u64()).unwrap_or(0),
            hash: field("hash").and_then(|v| v.as_str()).unwrap_or("").to_owned(),
            block: field("block").and_then(|v| v.as_u64()).unwrap_or(0),
        }
    }
}

impl Default for MeshFileHeader {
    fn default() -> Self {
        Self::empty()
    }
}

/// The last header this process read from the peer file. Starts at all-defaults; refreshed
/// on every [`parse_peers`] call (the peer file is re-read per spread), so a header added or
/// removed between reads is reflected here without a restart.
static MESH_FILE_HEADER: RwLock<MeshFileHeader> = RwLock::new(MeshFileHeader::empty());

/// The in-RAM `mainnet.jsonl` header — all defaults when the file has no header.
pub fn current_mesh_file_header() -> MeshFileHeader {
    MESH_FILE_HEADER.read().expect("mesh file header lock poisoned").clone()
}

/// Detect the optional header on the first non-blank, non-comment line. Returns the header
/// and that line's index (which the record parser must skip), or defaults and `None`.
pub fn parse_mesh_header(content: &str) -> (MeshFileHeader, Option<usize>) {
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) &&
            value.get("mesh_protocol_ver").is_some()
        {
            return (MeshFileHeader::from_value(&value), Some(index));
        }
        break;
    }
    (MeshFileHeader::default(), None)
}

/// Result of [`parse_peers`]: the file header, the entries that parsed, and human-readable
/// line errors.
#[derive(Debug, Default)]
pub struct ParsedPeers {
    /// The optional first-line header, or all defaults when absent.
    pub header: MeshFileHeader,
    /// Successfully parsed entries, in file order.
    pub peers: Vec<PeerEntry>,
    /// One message per skipped line (caller logs them at `warn`).
    pub errors: Vec<String>,
}

/// Parses peer-file content. Blank lines and `#` comments are ignored; malformed lines
/// are collected in [`ParsedPeers::errors`] instead of failing the whole parse.
pub fn parse_peers(content: &str) -> ParsedPeers {
    let (header, header_line) = parse_mesh_header(content);
    *MESH_FILE_HEADER.write().expect("mesh file header lock poisoned") = header.clone();
    let mut parsed = ParsedPeers { header, peers: Vec::new(), errors: Vec::new() };
    for (index, line) in content.lines().enumerate() {
        if Some(index) == header_line {
            continue;
        }
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
    fn mark_self_labels_own_entry_without_changing_targeting() {
        let content = r#"{"key_id":"0x1111111111111111111111111111111111111111","ip":"127.0.0.1","port_range":30888}
{"key_id":"0x2222222222222222222222222222222222222222","ip":"127.0.0.1","port_range":30811}
"#;
        let mut parsed = parse_peers(content);
        let self_address = Address::repeat_byte(0x11);
        PeerEntry::mark_self(&mut parsed.peers, Some(30811), self_address);
        assert!(parsed.peers[0].is_self);
        assert!(parsed.peers[1].is_self);
        // Inert flag: the existing port/address filter still decides targeting.
        assert!(!parsed.peers[0].is_target(30888, self_address));
        assert!(!parsed.peers[1].is_target(30811, self_address));
        assert!(parsed.peers[0].is_target(30999, Address::repeat_byte(0x33)));
    }

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

    #[test]
    fn optional_header_is_read_kept_and_skipped_as_a_record() {
        let content = "# a comment first\n{\"mesh_protocol_ver\":11,\"signed\":\"sig\",\"ts\":123,\"hash\":\"0xabc\",\"block\":42}\n{\"ip\":\"127.0.0.1\",\"port_range\":1}\n";
        let (header, line) = parse_mesh_header(content);
        assert_eq!(line, Some(1), "header may follow blanks/comments");
        assert_eq!(
            header,
            MeshFileHeader {
                mesh_protocol_ver: 11,
                signed: "sig".to_owned(),
                ts: 123,
                hash: "0xabc".to_owned(),
                block: 42,
            }
        );
        let parsed = parse_peers(content);
        assert_eq!(parsed.header, header);
        assert_eq!(parsed.peers.len(), 1);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    }

    #[test]
    fn absent_header_defaults_to_zeros_without_error() {
        let (header, line) = parse_mesh_header(REFERENCE_FILE);
        assert_eq!(line, None);
        assert_eq!(header, MeshFileHeader::default());
        assert_eq!(header.mesh_protocol_ver, 0);
        assert!(header.signed.is_empty() && header.hash.is_empty());
        assert_eq!((header.ts, header.block), (0, 0));
        let parsed = parse_peers(REFERENCE_FILE);
        assert_eq!(parsed.header, header);
        assert_eq!(parsed.peers.len(), 8);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    }

    #[test]
    fn partial_or_wrong_typed_header_fields_take_defaults() {
        let (header, line) =
            parse_mesh_header("{\"mesh_protocol_ver\":\"not-an-int\",\"block\":7}\n");
        assert_eq!(line, Some(0));
        assert_eq!(header.mesh_protocol_ver, 0);
        assert_eq!(header.block, 7);
        assert!(header.signed.is_empty() && header.hash.is_empty());
        assert_eq!((header.ts, header.block), (0, 7));
    }
}
