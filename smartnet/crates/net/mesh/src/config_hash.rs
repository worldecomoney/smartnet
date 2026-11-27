//! Bootstrap config hash — prove two nodes run the same `mainnet.jsonl` and
//! `validators.jsonl`.
//!
//! The digest is computed from the **file on disk on every call**: there is deliberately no
//! cached/`OnceLock` hash in RAM, and this is the only hash function for these files. A node
//! recomputes both digests each time it bootstraps a link, so a file changed after boot is
//! reflected on the next connection. No file is ever written or synced — a mismatch just
//! drops that one connection.

use alloy_primitives::keccak256;
use std::{io, path::Path};

/// One node's two bootstrap-file digests, exchanged once when a link comes up.
///
/// A transient wire value only: it is never stored as a node-wide cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigHashes {
    /// digest of `mainnet.jsonl`.
    pub mainnet: [u8; 32],
    /// digest of `validators.jsonl`.
    pub validators: [u8; 32],
}

/// The single hash function for the two bootstrap files. Reads the whole file and hashes
/// its raw bytes — recomputed on every call, never cached. Callers pass which file.
pub fn get_real_file_hash(path: &Path) -> io::Result<[u8; 32]> {
    let bytes = std::fs::read(path)?;
    Ok(keccak256(bytes).0)
}

impl ConfigHashes {
    /// Compute both digests fresh from disk (one `get_real_file_hash` call per file).
    pub fn read(mainnet: &Path, validators: &Path) -> io::Result<Self> {
        Ok(Self {
            mainnet: get_real_file_hash(mainnet)?,
            validators: get_real_file_hash(validators)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_recomputed_on_every_call_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.jsonl");
        std::fs::write(&path, b"first").unwrap();
        let first = get_real_file_hash(&path).unwrap();
        assert_eq!(first, get_real_file_hash(&path).unwrap());
        // Change the file: the very next call must see the new bytes.
        std::fs::write(&path, b"second").unwrap();
        let second = get_real_file_hash(&path).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn both_files_are_hashed_independently() {
        let dir = tempfile::tempdir().unwrap();
        let mainnet = dir.path().join("mainnet.jsonl");
        let validators = dir.path().join("validators.jsonl");
        std::fs::write(&mainnet, b"peers").unwrap();
        std::fs::write(&validators, b"vals").unwrap();
        let hashes = ConfigHashes::read(&mainnet, &validators).unwrap();
        assert_eq!(hashes.mainnet, get_real_file_hash(&mainnet).unwrap());
        assert_eq!(hashes.validators, get_real_file_hash(&validators).unwrap());
        assert_ne!(hashes.mainnet, hashes.validators);
    }

    #[test]
    fn missing_file_is_an_error_not_a_zero_hash() {
        let dir = tempfile::tempdir().unwrap();
        assert!(get_real_file_hash(&dir.path().join("absent.jsonl")).is_err());
    }
}
