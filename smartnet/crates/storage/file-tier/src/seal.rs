//! Sealing a sub-shard: compress the active `.jsonl` files into `.gz` (AMEND1 + A3).
//!
//! The active shard is plain JSONL — hot, read without decompression. There are only **two
//! persistent file states**: `{name}.jsonl` (active, or mid-seal) and `{name}.gz` (sealed).
//! The seal compresses each `.jsonl` into its `.gz`; the `.jsonl` stays the source until the
//! `.gz` is in place, then is deleted. The `.gz.tmp` name is transient (a partial gzip).
//!
//! The seal is atomic per file: gzip to `{name}.gz.tmp`, fsync, rename to `{name}.gz`, fsync
//! the shard directory, delete `{name}.jsonl`, fsync the shard directory again. So a `.gz` at
//! its final name is always complete, and a crash leaves either the `.jsonl` alone (before the
//! rename), a `.gz.tmp` beside the `.jsonl` (mid-compression), or both `.gz` and a stale
//! `.jsonl` (after the rename, before the delete) — never neither. The directory fsyncs are
//! what make that last guarantee: the file fsyncs protect the bytes, and only the directory
//! fsync protects the names. [`reconcile_seals`](crate::recovery::reconcile_seals) resolves
//! whatever a crash still leaves behind on the next boot.
//!
//! [`seal_shard`] is run on a background thread by the persistence service (off the critical
//! path); the reader keeps serving the shard from its `.jsonl` until the compression completes.

use crate::{
    error::{FileTierError, FileTierResult},
    shard::{shard_dir, ShardId},
    FILE_NAMES,
};
use std::path::{Path, PathBuf};

/// The active (unsealed) path for a file in a sub-shard: `{dir}/{name}.jsonl`.
pub fn active_file_path(base: impl AsRef<Path>, shard: ShardId, name: &str) -> PathBuf {
    shard_dir(base.as_ref(), shard).join(format!("{name}.jsonl"))
}

/// The sealed path for a file in a sub-shard: `{dir}/{name}.gz`.
pub fn sealed_file_path(base: impl AsRef<Path>, shard: ShardId, name: &str) -> PathBuf {
    shard_dir(base.as_ref(), shard).join(format!("{name}.gz"))
}

/// The transient path a seal gzip is written to before its atomic rename: `{dir}/{name}.gz.tmp`.
pub fn seal_tmp_path(base: impl AsRef<Path>, shard: ShardId, name: &str) -> PathBuf {
    shard_dir(base.as_ref(), shard).join(format!("{name}.gz.tmp"))
}

/// Test helper: create the shard directory and write `contents` to `{name}.jsonl`.
#[cfg(test)]
pub(crate) fn write_active(base: &Path, shard: ShardId, name: &str, contents: &[u8]) {
    smartnet_fs_util::create_dir_all(shard_dir(base, shard)).unwrap();
    smartnet_fs_util::write(active_file_path(base, shard, name), contents).unwrap();
}

/// Test helper: create the shard directory and write `contents` to `{name}.gz`.
#[cfg(test)]
pub(crate) fn write_sealed(base: &Path, shard: ShardId, name: &str, contents: &[u8]) {
    smartnet_fs_util::create_dir_all(shard_dir(base, shard)).unwrap();
    smartnet_fs_util::write(sealed_file_path(base, shard, name), contents).unwrap();
}

/// A sub-shard that has been sealed: the files that now carry their final `.gz` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedShard {
    /// The sealed sub-shard's coordinates.
    pub shard: ShardId,
    /// The final (`.gz`) paths that were sealed, in [`FILE_NAMES`] order.
    pub files: Vec<PathBuf>,
}

/// Seals a sub-shard by compressing each `.jsonl` into its `.gz` name (and deleting the source).
///
/// Atomic per file (gzip to `.gz.tmp`, fsync, rename + dir fsync, delete `.jsonl` + dir fsync),
/// so a `.gz` at its final name is always complete and the name itself survives a crash.
/// Idempotent: a file already sealed is left as-is (a stale `.jsonl` is deleted); a file with
/// neither `.jsonl` nor `.gz` (e.g. `4626` with no events) is skipped.
///
/// Run on a background thread by the persistence service; the reader keeps reading the
/// `.jsonl` until this completes.
pub fn seal_shard(base: impl AsRef<Path>, shard: ShardId) -> FileTierResult<SealedShard> {
    let base = base.as_ref();
    // Every branch below changes this directory's entry list (rename, removal, removal), and a
    // directory's entries are only durable once the directory itself is fsynced — syncing the
    // files protects their bytes, never their names.
    let dir = shard_dir(base, shard);
    let mut files = Vec::with_capacity(FILE_NAMES.len());
    for name in FILE_NAMES {
        let active = active_file_path(base, shard, name);
        let sealed = sealed_file_path(base, shard, name);
        if sealed.exists() {
            // Already sealed (possibly a re-run): the `.gz` is authoritative; drop a stale
            // `.jsonl` left by a crash between the rename and the delete.
            smartnet_fs_util::remove_file_if_exists(&active)?;
            smartnet_fs_util::fsync_dir(&dir)?;
            files.push(sealed);
            continue;
        }
        if !active.exists() {
            // Never written (e.g. `4626` in a sub-shard with no ERC-4626 events).
            continue;
        }
        // Compress to a transient name, fsync, then rename — the final `.gz` is atomic.
        let tmp = seal_tmp_path(base, shard, name);
        smartnet_fs_util::gzip_file(&active, &tmp)?;
        smartnet_fs_util::rename(&tmp, &sealed)?;
        // The rename must be durable *before* the source is dropped: otherwise a crash after
        // the delete could take the `.jsonl` with it and leave neither file.
        smartnet_fs_util::fsync_dir(&dir)?;
        // Per file: drop the source `.jsonl` only once the sealed `.gz` is confirmed present
        // and non-empty. A missing/empty sealed file aborts before the delete, so the `.jsonl`
        // stays the source (recovery re-runs the seal).
        let sealed_ok = smartnet_fs_util::metadata(&sealed).map(|m| m.len() > 0).unwrap_or(false);
        if !sealed_ok {
            return Err(FileTierError::RecordField(format!(
                "sealed file missing or empty after compression: {sealed:?}"
            )));
        }
        smartnet_fs_util::remove_file_if_exists(&active)?;
        smartnet_fs_util::fsync_dir(&dir)?;
        files.push(sealed);
    }
    Ok(SealedShard { shard, files })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `seal_shard` compresses every present `.jsonl` into `.gz` and removes the `.jsonl`.
    #[test]
    fn seal_compresses_present_active_files() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        write_active(dir.path(), shard, "b", b"{\"b\":0}\n");
        write_active(dir.path(), shard, "txs", b"{\"b\":0}\n");

        let sealed = seal_shard(dir.path(), shard).unwrap();
        assert_eq!(sealed.files.len(), 2);
        assert!(sealed_file_path(dir.path(), shard, "b").exists());
        assert!(sealed_file_path(dir.path(), shard, "txs").exists());
        assert!(!active_file_path(dir.path(), shard, "b").exists());
        assert!(!sealed_file_path(dir.path(), shard, "4626").exists());
        assert!(!seal_tmp_path(dir.path(), shard, "b").exists());
    }

    /// Re-running seal on an already-sealed shard is a no-op (idempotent).
    #[test]
    fn seal_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        write_active(dir.path(), shard, "b", b"{\"b\":0}\n");

        let first = seal_shard(dir.path(), shard).unwrap();
        assert_eq!(first.files.len(), 1);
        let second = seal_shard(dir.path(), shard).unwrap();
        assert_eq!(second.files.len(), 1, "already-sealed file is still reported");
        assert!(sealed_file_path(dir.path(), shard, "b").exists());
    }

    /// A crash after the rename but before the delete (`.gz` + stale `.jsonl`) resolves to the
    /// sealed `.gz`; the stale `.jsonl` is removed.
    #[test]
    fn seal_prefers_existing_gz_over_stale_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        write_active(dir.path(), shard, "b", b"{\"b\":0}\n");
        write_sealed(dir.path(), shard, "b", b"sealed");

        seal_shard(dir.path(), shard).unwrap();
        assert!(!active_file_path(dir.path(), shard, "b").exists());
        assert_eq!(std::fs::read(sealed_file_path(dir.path(), shard, "b")).unwrap(), b"sealed");
    }
}
