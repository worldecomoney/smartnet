//! Seal-time helpers. The seal trigger itself is pure shard math (`is_last_block_of_shard` =
//! `stageShardY == 1`), never a record count — a sub-shard is complete by construction once its
//! last block is appended. [`verify_bt_records`] remains only as a test utility that counts a
//! file's records to assert on it.

use crate::{
    error::FileTierResult,
    reader::is_gzip,
    seal::{active_file_path, sealed_file_path},
    shard::ShardId,
};
use flate2::read::MultiGzDecoder;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

/// Counts newline-terminated records in a JSONL file, decompressing it when it is gzip (magic
/// sniff — the sealed shards are gzip, the active/current one is plain).
fn count_lines(path: &Path) -> FileTierResult<u64> {
    let file =
        File::open(path).map_err(|source| smartnet_fs_util::FsPathError::read(source, path))?;
    let reader: Box<dyn BufRead> = if is_gzip(path)? {
        Box::new(BufReader::new(MultiGzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut count = 0u64;
    for line in reader.lines() {
        line.map_err(|source| smartnet_fs_util::FsPathError::read(source, path))?;
        count += 1;
    }
    Ok(count)
}

/// Whether the `b` file (`.jsonl` active/mid-seal, or `.gz` sealed) holds exactly `expected`
/// records.
///
/// Test utility only: production sealing never counts records (shards are pure math).
pub fn verify_bt_records(
    base: impl AsRef<Path>,
    shard: ShardId,
    expected: u64,
) -> FileTierResult<bool> {
    let base = base.as_ref();
    let path = [active_file_path(base, shard, "b"), sealed_file_path(base, shard, "b")]
        .into_iter()
        .find(|path| path.exists());
    let Some(path) = path else { return Ok(false) };
    Ok(count_lines(&path)? == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::{write_active, write_sealed};

    fn write_gz_lines(path: &std::path::Path, n: usize) {
        let file = std::fs::File::create(path).unwrap();
        let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        for i in 0..n {
            std::io::Write::write_all(&mut enc, format!("{{\"b\":{i}}}\n").as_bytes()).unwrap();
        }
        enc.finish().unwrap();
    }

    /// A plain active `b.jsonl` with the expected number of records verifies.
    #[test]
    fn exact_count_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        let data: String = (0..5).map(|i| format!("{{\"b\":{i}}}\n")).collect();
        write_active(dir.path(), shard, "b", data.as_bytes());
        assert!(verify_bt_records(dir.path(), shard, 5).unwrap());
    }

    /// A short active `b.jsonl` (crash before the last blocks) does not match a larger count.
    #[test]
    fn short_bt_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        let data: String = (0..4).map(|i| format!("{{\"b\":{i}}}\n")).collect();
        write_active(dir.path(), shard, "b", data.as_bytes());
        assert!(!verify_bt_records(dir.path(), shard, 5).unwrap());
    }

    /// A missing `b` file reports no records (never observed a block).
    #[test]
    fn missing_bt_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        assert!(!verify_bt_records(dir.path(), shard, 5).unwrap());
    }

    /// After a seal the verification reads the sealed `.gz` name.
    #[test]
    fn verification_reads_sealed_name_after_seal() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        smartnet_fs_util::create_dir_all(crate::shard::shard_dir(dir.path(), shard)).unwrap();
        write_gz_lines(&sealed_file_path(dir.path(), shard, "b"), 5);
        assert!(verify_bt_records(dir.path(), shard, 5).unwrap());
    }

    /// `write_active`/`write_sealed` helpers round-trip raw bytes (smoke).
    #[test]
    fn helpers_still_write_raw() {
        let dir = tempfile::tempdir().unwrap();
        let shard = ShardId { s: 0, y: 0 };
        write_active(dir.path(), shard, "txs", b"x");
        write_sealed(dir.path(), shard, "txs", b"y");
        assert!(active_file_path(dir.path(), shard, "txs").exists());
        assert!(sealed_file_path(dir.path(), shard, "txs").exists());
    }
}
