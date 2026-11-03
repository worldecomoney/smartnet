//! Stage 6C: runtime bytecode in content-addressed files (`steps.md` step 7; spec
//! `fjall4a.md` §3.5's format and §7's "Fjall does not hold runtime bytecode").
//!
//! Layout: `bytecodes/{h[0..2]}/{h[2..4]}/{hash}` under the file-tier base, where `{hash}`
//! is the 64-char lowercase hex of `keccak256(code)` — the `code_hash` accounts reference.
//!
//! Content: the raw executable bytecode exactly as deployed — **binary, uncompressed,
//! headerless** (operator ruling: no transcoding, direct execution; EVM bytecode is
//! near-random, so spec §3.5 mandates no gzip).
//!
//! Write-once: code is addressed by its own hash, so identical bytecode is one file and
//! never changes after creation — readers need no invalidation, and a crash between
//! writes leaves at most an orphan file. Writes are temp-file + rename, so a final path
//! never holds a partial write. The writer path is single-writer (persistence), which
//! makes the shared temp name for concurrent identical code harmless.

use crate::error::FileTierResult;
use alloy_primitives::B256;
use smartnet_fs_util::FsPathError;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Root of the bytecode tree, a sibling of `blocks/` under the file-tier base.
pub const BYTECODES_DIR: &str = "bytecodes";

/// The file holding `code_hash`'s runtime bytecode under `base`:
/// `bytecodes/{h[0..2]}/{h[2..4]}/{hash}`.
pub fn bytecode_path(base: &Path, code_hash: &B256) -> PathBuf {
    let hash = alloy_primitives::hex::encode(code_hash);
    base.join(BYTECODES_DIR).join(&hash[0..2]).join(&hash[2..4]).join(&hash)
}

/// Writes the raw runtime bytecode for `code_hash`, write-once (spec §4: files are
/// immutable once written). A crash before the rename leaves only a `{hash}.tmp` sibling,
/// which readers never see; the next write of the same code truncates and retries it.
pub fn write_bytecode(base: &Path, code_hash: &B256, code: &[u8]) -> FileTierResult<()> {
    let path = bytecode_path(base, code_hash);
    if path.exists() {
        return Ok(());
    }
    let dir = path.parent().expect("bytecode_path always has a parent");
    fs::create_dir_all(dir).map_err(|err| FsPathError::create_dir(err, dir))?;

    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp).map_err(|err| FsPathError::create_file(err, &tmp))?;
    file.write_all(code).map_err(|err| FsPathError::write(err, &tmp))?;
    file.sync_all().map_err(|err| FsPathError::fsync(err, &tmp))?;
    drop(file);
    fs::rename(&tmp, &path).map_err(|err| FsPathError::rename(err, &tmp, &path))?;
    // The file's *contents* are on the device, but its *name* is not: a rename only becomes
    // durable when the directory holding both entries is fsynced. Without this a crash can
    // undo the rename, leaving the `.tmp` sibling (which readers ignore) and no final path —
    // a bytecode file that was written, fsynced and then silently lost.
    smartnet_fs_util::fsync_dir(dir)?;
    Ok(())
}

/// Reads the raw runtime bytecode for `code_hash`. `Ok(None)` when no file exists — the
/// code predates the file tier (the `Bytecodes` table still holds it) or is unknown, and
/// the caller falls back. Any other I/O failure is an error, never a silent miss.
pub fn read_bytecode(base: &Path, code_hash: &B256) -> FileTierResult<Option<Vec<u8>>> {
    let path = bytecode_path(base, code_hash);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(FsPathError::read(err, &path).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{hex, keccak256};

    const CODE: &[u8] = &[0x60, 0x80, 0x60, 0x40, 0x52, 0x60, 0x00, 0x55];

    #[test]
    fn bytecode_path_layout() {
        let hash = keccak256(CODE);
        let hex_hash = hex::encode(hash);
        let path = bytecode_path(Path::new("/data"), &hash);
        assert_eq!(
            path,
            PathBuf::from(format!("/data/{BYTECODES_DIR}/{}/{}", &hex_hash[0..2], &hex_hash[2..4]))
                .join(&hex_hash)
        );
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), hex_hash);
    }

    #[test]
    fn write_then_read_round_trips_raw_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let hash = keccak256(CODE);
        assert!(read_bytecode(dir.path(), &hash).unwrap().is_none());

        write_bytecode(dir.path(), &hash, CODE).unwrap();

        let path = bytecode_path(dir.path(), &hash);
        assert_eq!(fs::read(&path).unwrap(), CODE, "the file is the raw bytecode, no encoding");
        assert_eq!(keccak256(fs::read(&path).unwrap()).0, hash.0, "path hash is keccak of content");
        assert_eq!(read_bytecode(dir.path(), &hash).unwrap().unwrap(), CODE);
        assert!(!path.with_extension("tmp").exists(), "the temp file is renamed away");
    }

    #[test]
    fn write_is_write_once() {
        let dir = tempfile::tempdir().unwrap();
        let hash = keccak256(CODE);
        write_bytecode(dir.path(), &hash, CODE).unwrap();

        let other: &[u8] = &[0x00];
        write_bytecode(dir.path(), &hash, other).unwrap();

        assert_eq!(fs::read(bytecode_path(dir.path(), &hash)).unwrap(), CODE);
    }

    #[test]
    fn read_ignores_unrenamed_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let hash = keccak256(CODE);
        let path = bytecode_path(dir.path(), &hash);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path.with_extension("tmp"), CODE).unwrap();

        assert!(
            read_bytecode(dir.path(), &hash).unwrap().is_none(),
            "a crash's tmp must not serve"
        );
    }

    #[test]
    fn read_missing_is_none_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let unknown = keccak256(b"never written");
        assert_eq!(read_bytecode(dir.path(), &unknown).unwrap(), None);
    }
}
