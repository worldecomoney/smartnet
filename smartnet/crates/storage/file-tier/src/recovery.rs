//! Crash recovery for the file tier (RECOVERY.md + AMEND1).
//!
//! Protocol invariant: **one block per save** — no block batches anywhere (only tx
//! batches are supported). Each save appends at most one block line (plus a possible
//! partial tail from a crash mid-append), so the drop-2 anchor below is always exact.
//!
//! Active shards are plain JSONL, so recovery is line-based (no gzip members, no `{` scan).
//! When the clean-shutdown flag is absent:
//!
//! 1. **Anchor:** drop the last 2 lines of `b.jsonl` — the one block line + a possible
//!    partial tail — but never the block-0 (genesis) line.
//! 2. Re-read the anchor; the last line's block number is `BX`.
//! 3. **Align:** `rm_above_block(file, BX)` for `txs`/`logs`/`4626`.
//! 4. (Caller) truncate the DB to `BX`, then re-sync from `BX + 1`.
//!
//! Sealed `.gz` files are read-only and never touched. A partial line (a crash mid-append) is
//! removed by the same line scan. This module also reconciles a half-finished seal (deletes a
//! leftover `.gz.tmp`, and a stale `.jsonl` when the `.gz` already exists) and provides
//! [`flush`], used by the clean-shutdown handler.

use crate::{
    error::{FileTierError, FileTierResult},
    seal::{active_file_path, seal_tmp_path, sealed_file_path},
    shard::ShardId,
    FILE_NAMES,
};
use smartnet_fs_util::FsPathError;
use std::{io, path::Path};

/// The highest sub-shard holding a `.jsonl` (the active one) and the highest holding a sealed
/// `.gz`, from one directory walk.
fn scan_shards(base: &Path) -> (Option<ShardId>, Option<ShardId>) {
    let mut best_active: Option<ShardId> = None;
    let mut best_sealed: Option<ShardId> = None;
    let Ok(s_dirs) = std::fs::read_dir(base.join("blocks")) else { return (None, None) };
    for s_entry in s_dirs.flatten() {
        let Some(s) = s_entry.file_name().to_str().and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let Ok(y_dirs) = std::fs::read_dir(s_entry.path()) else { continue };
        for y_entry in y_dirs.flatten() {
            let Some(y) = y_entry.file_name().to_str().and_then(|n| n.parse::<u64>().ok()) else {
                continue;
            };
            let shard = ShardId { s, y };
            if FILE_NAMES.iter().any(|name| active_file_path(base, shard, name).exists()) {
                best_active = Some(best_active.map_or(shard, |b| b.max(shard)));
            }
            if FILE_NAMES.iter().any(|name| sealed_file_path(base, shard, name).exists()) {
                best_sealed = Some(best_sealed.map_or(shard, |b| b.max(shard)));
            }
        }
    }
    (best_active, best_sealed)
}

/// The last block a sealed sub-shard covers.
const fn sealed_last_block(shard: ShardId) -> u64 {
    let (_, end) = crate::shard::get_shard_range(shard);
    end - 1
}

/// The recovery step: remove the last 2 lines from the active sub-shard's `b.jsonl`, then align
/// the other active files to the resulting `BX`. Returns `BX`.
///
/// Two boundaries are handled explicitly, because getting them wrong would wipe the DB:
/// - If the last batch sealed its sub-shard (ended exactly on a 10k boundary), there is no
///   `.jsonl`; `BX` is that sealed sub-shard's last block and nothing is removed.
/// - If removing 2 lines empties the active sub-shard, `BX` falls back to the previous sealed
///   sub-shard's last block (or 0 on a genuinely young chain), never below data the FS holds.
pub fn remove_last_lines(base: impl AsRef<Path>) -> FileTierResult<u64> {
    let base = base.as_ref();
    reconcile_seals(base)?;

    let (active_shard, sealed_shard) = scan_shards(base);
    let Some(shard) = active_shard else {
        // Everything is sealed; the FS is complete through the highest sealed sub-shard.
        return Ok(sealed_shard.map_or(0, sealed_last_block));
    };

    // Step 1: truncate the anchor, then re-read `BX`. Genesis (block 0) is the chain's
    // immutable anchor — it is never re-synced, so it must survive even when it is one of
    // the last two lines (a fresh chain that has only seen block 0, or 0..=1).
    let b = active_file_path(base, shard, "b");
    rm_last_lines_keeping_genesis(&b, 2)?;
    let bx = last_line_block(&b)?;
    let bx = if bx > 0 {
        bx
    } else {
        // The active sub-shard was emptied; fall back to the previous sealed sub-shard.
        sealed_shard.map_or(0, sealed_last_block)
    };

    // Step 2: align the other active files to `BX`.
    for name in FILE_NAMES {
        if name == "b" {
            continue;
        }
        rm_above_block(&active_file_path(base, shard, name), bx)?;
    }
    Ok(bx)
}

/// Reconciles a half-finished seal across all sub-shards (A3). Only two persistent states
/// exist per file — `{name}.jsonl` (source) and `{name}.gz` (sealed) — plus the transient
/// `{name}.gz.tmp`. Per file:
/// - `.jsonl` + `.gz`: the seal completed (crash after rename, before delete) → delete `.jsonl`;
/// - `.jsonl` + `.gz.tmp`: the compression was interrupted → delete `.gz.tmp` (the `.jsonl` is the
///   source);
/// - `.jsonl` only: the shard is active — nothing to do;
/// - `.gz` only: sealed — nothing to do;
/// - `.gz.tmp` only (no `.jsonl`, no `.gz`): corruption (the source was deleted before the sealed
///   file was in place) → escalate.
///
/// Idempotent. Called unconditionally at boot (so a clean shutdown that left a seal in flight
/// is resolved) and again from [`remove_last_lines`] on the crash path.
pub fn reconcile_seals(base: impl AsRef<Path>) -> FileTierResult<()> {
    let base = base.as_ref();
    let Ok(s_dirs) = std::fs::read_dir(base.join("blocks")) else { return Ok(()) };
    for s_entry in s_dirs.flatten() {
        let Some(s) = s_entry.file_name().to_str().and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let Ok(y_dirs) = std::fs::read_dir(s_entry.path()) else { continue };
        for y_entry in y_dirs.flatten() {
            let Some(y) = y_entry.file_name().to_str().and_then(|n| n.parse::<u64>().ok()) else {
                continue;
            };
            let shard = ShardId { s, y };
            for name in FILE_NAMES {
                let active = active_file_path(base, shard, name);
                let sealed = sealed_file_path(base, shard, name);
                let tmp = seal_tmp_path(base, shard, name);

                if tmp.exists() {
                    if active.exists() || sealed.exists() {
                        // Interrupted compression; the source (or the sealed file) is present.
                        smartnet_fs_util::remove_file_if_exists(&tmp)?;
                    } else {
                        // A lone partial gzip with no source and no sealed file: corruption.
                        return Err(FileTierError::RecordField(format!(
                            "lone seal temp file with no source or sealed file: {tmp:?}"
                        )));
                    }
                }
                if sealed.exists() && active.exists() {
                    // The seal completed; the `.gz` is authoritative.
                    smartnet_fs_util::remove_file_if_exists(&active)?;
                }
            }
        }
    }
    Ok(())
}

/// Drops up to `count` trailing lines from a plain JSONL file, but never the genesis line
/// (`"b":0`). Genesis is written once by `smartnet init` and is not reproduced by sync, so
/// crash recovery must keep it even when it is one of the lines that would otherwise be
/// dropped (a fresh chain holding only blocks 0..=1).
fn rm_last_lines_keeping_genesis(path: &Path, count: u32) -> FileTierResult<u64> {
    assert!(count >= 1, "count must be at least 1");
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(FsPathError::read(err, path).into()),
    };
    let mut lines = Vec::new();
    let mut start = 0usize;
    for line in data.split_inclusive(|&b| b == b'\n') {
        lines.push((start, start + line.len()));
        start += line.len();
    }
    let mut kept = data.len();
    let mut remaining = count as usize;
    for &(line_start, line_end) in lines.iter().rev() {
        if remaining == 0 {
            break;
        }
        // Genesis never moves, and it is the first line: once reached, nothing earlier exists.
        if line_block(&data[line_start..line_end]) == Some(0) {
            break;
        }
        kept = line_start;
        remaining -= 1;
    }
    truncate_file(path, &data, kept)?;
    Ok(kept as u64)
}

/// `rmLastLine`: removes the last `count` lines from a plain JSONL file and returns the new end
/// (bytes). A partial trailing line (a crash mid-append) counts as a line and is removed with
/// them. With fewer than `count` lines the file is removed outright.
pub fn rm_last_line(path: &Path, count: u32) -> FileTierResult<u64> {
    assert!(count >= 1, "count must be at least 1");
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(FsPathError::read(err, path).into()),
    };
    let newlines: Vec<usize> =
        data.iter().enumerate().filter_map(|(i, &b)| (b == b'\n').then_some(i)).collect();
    // A trailing fragment after the last `\n` is a partial line.
    let has_partial = data.last().is_some_and(|&b| b != b'\n');
    let total_lines = newlines.len() + usize::from(has_partial);
    let keep_lines = total_lines.saturating_sub(count as usize);
    let kept = if keep_lines == 0 { 0 } else { newlines[keep_lines - 1] + 1 };
    truncate_file(path, &data, kept)?;
    Ok(kept as u64)
}

/// `rmAboveBlock`: removes every line whose block number is `> block`, and returns the new end
/// (bytes). A partial trailing line is dropped by the same scan (it has no valid `b`).
pub fn rm_above_block(path: &Path, block: u64) -> FileTierResult<u64> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(FsPathError::read(err, path).into()),
    };
    let mut kept = data.len();
    let mut start = 0usize;
    for line in data.split_inclusive(|&b| b == b'\n') {
        let end = start + line.len();
        match line_block(line) {
            Some(b) if b <= block => {}
            // First line above the cursor (or unparseable/partial): cut from its start.
            _ => {
                kept = start;
                break;
            }
        }
        start = end;
    }
    truncate_file(path, &data, kept)?;
    Ok(kept as u64)
}

/// The block number of the last complete line in `path`, or 0 when the file is absent/empty.
fn last_line_block(path: &Path) -> FileTierResult<u64> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(FsPathError::read(err, path).into()),
    };
    Ok(data
        .split(|&b| b == b'\n').rfind(|line| !line.is_empty())
        .and_then(line_block)
        .unwrap_or(0))
}

/// Parses a line's `b` field, `None` when the line is empty, partial, or malformed.
fn line_block(line: &[u8]) -> Option<u64> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    if line.is_empty() {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(line)
        .ok()
        .and_then(|value| value.get("b").and_then(|b| b.as_u64()))
}

/// Cuts `path` to `kept` bytes: a no-op when nothing is dropped, removal when `kept == 0`.
/// Opens for write **without truncating**, so `set_len` keeps the first `kept` bytes.
fn truncate_file(path: &Path, data: &[u8], kept: usize) -> FileTierResult<()> {
    if kept >= data.len() {
        return Ok(());
    }
    if kept == 0 {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(FsPathError::remove_file(err, path).into()),
        }
    } else {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|file| file.set_len(kept as u64))
            .map_err(|err| FsPathError::write(err, path))?;
        Ok(())
    }
}

/// Makes the buffered appends durable: fsync every file under `blocks/` (plain `.jsonl` and
/// sealed `.gz`). Called by the clean-shutdown handler **before** it sets the clean flag, so
/// the flag's presence implies the payload is on the device — files *and* the directories that
/// name them (RECOVERY.md §3).
pub fn flush(base: impl AsRef<Path>) -> FileTierResult<()> {
    fsync_dir_recursive(&base.as_ref().join("blocks"))
}

/// Recursively fsyncs every regular file under `dir`, ignoring a missing directory.
fn fsync_dir_recursive(dir: &Path) -> FileTierResult<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(FsPathError::read_dir(err, dir).into()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_dir() {
            fsync_dir_recursive(&path)?;
        } else if file_type.is_file() {
            let file = std::fs::File::open(&path).map_err(|err| FsPathError::open(err, &path))?;
            file.sync_all().map_err(|err| FsPathError::fsync(err, &path))?;
        }
    }
    // The files are on the device, but the names pointing at them are not until *this*
    // directory is synced — children first, then the directory that holds them. Without it a
    // crash can drop a completed rename (`.gz.tmp` back, `.gz` gone) even though every byte
    // was written.
    smartnet_fs_util::fsync_dir(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::{write_active, write_sealed};

    fn line(b: u64) -> String {
        format!("{{\"b\":{b},\"h\":\"0x{b:04x}\"}}\n")
    }

    /// `remove_last_lines` drops the last 2 lines of `b.jsonl` and returns the new last block.
    #[test]
    fn removes_two_lines() {
        let dir = tempfile::tempdir().unwrap();
        let shard = crate::get_shard(1);
        let path = active_file_path(dir.path(), shard, "b");
        let data: String = (1..=5u64).map(line).collect();
        write_active(dir.path(), shard, "b", data.as_bytes());

        assert_eq!(remove_last_lines(dir.path()).unwrap(), 3);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            (1..=3u64).map(line).collect::<String>()
        );
    }

    /// A partial trailing line is removed with the last 2 lines (the partial counts as one),
    /// leaving a clean file.
    #[test]
    fn partial_tail_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let shard = crate::get_shard(1);
        let path = active_file_path(dir.path(), shard, "b");
        let mut data: String = (1..=4u64).map(line).collect();
        data.push_str("{\"b\":5,\"h\""); // crash mid-append
        write_active(dir.path(), shard, "b", data.as_bytes());

        // 4 complete lines + a partial = 5 lines; removing the last 2 keeps 3 (blocks 1..=3).
        assert_eq!(remove_last_lines(dir.path()).unwrap(), 3);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            (1..=3u64).map(line).collect::<String>()
        );
    }

    /// With fewer than 2 lines the file is emptied and `BX` is 0.
    #[test]
    fn fewer_than_two_lines_yields_zero() {
        let dir = tempfile::tempdir().unwrap();
        let shard = crate::get_shard(1);
        let path = active_file_path(dir.path(), shard, "b");
        write_active(dir.path(), shard, "b", line(1).as_bytes());

        assert_eq!(remove_last_lines(dir.path()).unwrap(), 0);
        assert!(!path.exists(), "emptied file is removed");
    }

    /// Genesis survives recovery even when it is one of the last two lines: a fresh chain
    /// holding only block 0 keeps it (`BX` stays 0).
    #[test]
    fn genesis_only_survives_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let shard = crate::get_shard(0);
        let path = active_file_path(dir.path(), shard, "b");
        write_active(dir.path(), shard, "b", line(0).as_bytes());

        assert_eq!(remove_last_lines(dir.path()).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), line(0));
    }

    /// A chain holding blocks 0..=1 drops block 1 (re-synced) but keeps genesis.
    #[test]
    fn recovery_keeps_genesis_when_it_falls_in_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let shard = crate::get_shard(0);
        let path = active_file_path(dir.path(), shard, "b");
        let data: String = [0u64, 1].iter().map(|&b| line(b)).collect();
        write_active(dir.path(), shard, "b", data.as_bytes());

        assert_eq!(remove_last_lines(dir.path()).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), line(0));
    }

    /// `rm_above_block` drops lines above `BX` (and a partial tail) in one pass.
    #[test]
    fn rm_above_block_truncates_by_block() {
        let dir = tempfile::tempdir().unwrap();
        let shard = crate::get_shard(1);
        let path = active_file_path(dir.path(), shard, "txs");
        let data: String = [1u64, 2, 2, 3, 4].iter().map(|&b| line(b)).collect();
        write_active(dir.path(), shard, "txs", format!("{data}{{\"b\":5").as_bytes());

        rm_above_block(&path, 2).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            [1u64, 2, 2].iter().map(|&b| line(b)).collect::<String>()
        );
    }

    /// Emptied active shard falls back to the previous sealed shard's last block, not 0.
    #[test]
    fn falls_back_to_previous_sealed_when_active_emptied() {
        let dir = tempfile::tempdir().unwrap();
        write_sealed(dir.path(), ShardId { s: 0, y: 0 }, "b", b"sealed");
        let shard = ShardId { s: 0, y: 1 };
        write_active(dir.path(), shard, "b", line(10_000).as_bytes());

        assert_eq!(remove_last_lines(dir.path()).unwrap(), 9_999);
        assert!(!active_file_path(dir.path(), shard, "b").exists());
    }

    /// No active shard at all: `BX` is the highest sealed shard's last block.
    #[test]
    fn sealed_only_yields_sealed_last_block() {
        let dir = tempfile::tempdir().unwrap();
        write_sealed(dir.path(), ShardId { s: 0, y: 0 }, "b", b"sealed");
        assert_eq!(remove_last_lines(dir.path()).unwrap(), 9_999);
    }

    /// A crash after the seal's rename but before the delete leaves both files; recovery keeps
    /// the `.gz` and removes the stale `.jsonl`.
    #[test]
    fn reconciles_stale_jsonl_when_gz_present() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        write_active(dir.path(), shard, "b", line(1).as_bytes());
        write_sealed(dir.path(), shard, "b", b"sealed");

        assert_eq!(remove_last_lines(dir.path()).unwrap(), 9_999);
        assert!(!active_file_path(dir.path(), shard, "b").exists());
        assert!(sealed_file_path(dir.path(), shard, "b").exists());
    }

    /// A leftover `.gz.tmp` (crash mid-compression) is removed; the `.jsonl` is then recovered.
    #[test]
    fn removes_partial_seal_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        write_active(dir.path(), shard, "b", (1..=3u64).map(line).collect::<String>().as_bytes());
        let tmp = seal_tmp_path(dir.path(), shard, "b");
        std::fs::write(&tmp, b"partial-gz").unwrap();

        // 3 lines: removing 2 leaves 1 → BX = 1.
        assert_eq!(remove_last_lines(dir.path()).unwrap(), 1);
        assert!(!tmp.exists(), "partial seal tmp is removed");
    }

    /// `reconcile_seals` deletes a `.gz.tmp` left by an interrupted compression (the `.jsonl`
    /// is the source), and drops a stale `.jsonl` once the `.gz` is in place.
    #[test]
    fn reconcile_resolves_interrupted_seal() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };

        // Interrupted compression: `.jsonl` + partial `.gz.tmp`.
        write_active(dir.path(), shard, "b", line(7).as_bytes());
        let tmp = seal_tmp_path(dir.path(), shard, "b");
        std::fs::write(&tmp, b"partial").unwrap();
        reconcile_seals(dir.path()).unwrap();
        assert!(!tmp.exists(), "partial gz.tmp removed");
        assert!(active_file_path(dir.path(), shard, "b").exists(), ".jsonl is the source");

        // Completed seal: `.gz` present + stale `.jsonl`.
        write_sealed(dir.path(), shard, "txs", b"sealed");
        write_active(dir.path(), shard, "txs", line(8).as_bytes());
        reconcile_seals(dir.path()).unwrap();
        assert!(!active_file_path(dir.path(), shard, "txs").exists());
        assert!(sealed_file_path(dir.path(), shard, "txs").exists());
    }

    /// `flush` walks every level of the blocks tree, directories included: the directory
    /// fsync is what makes a completed rename's *name* survive the crash this is called for.
    #[test]
    fn flush_syncs_a_nested_blocks_tree() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 3, y: 12 };
        write_active(dir.path(), shard, "b", line(1).as_bytes());
        write_sealed(dir.path(), shard, "b", b"sealed");
        write_active(dir.path(), shard, "txs", line(2).as_bytes());

        // Two levels of shard directory plus the `blocks` root — all of them get opened and
        // synced, which is the code path that used to be missing entirely.
        assert!(flush(dir.path()).is_ok(), "every directory in the tree must be syncable");
        assert!(sealed_file_path(dir.path(), shard, "b").exists());
    }

    /// No tree yet (nothing written) is not an error — the same tolerance `flush` had for a
    /// missing `blocks` directory.
    #[test]
    fn flush_tolerates_a_missing_blocks_tree() {
        let dir = tempfile::tempdir().unwrap();
        assert!(flush(dir.path()).is_ok());
    }

    /// A lone `.gz.tmp` (no source, no sealed file) is corruption and escalates.
    #[test]
    fn reconcile_escalates_lone_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        let tmp = seal_tmp_path(dir.path(), shard, "b");
        smartnet_fs_util::create_dir_all(tmp.parent().unwrap()).unwrap();
        std::fs::write(&tmp, b"partial").unwrap();
        assert!(reconcile_seals(dir.path()).is_err());
    }
}
