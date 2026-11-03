//! Shard math and path derivation.
//!
//! Pure functions transcribed from `fjall4a.md`. A block's height maps to exactly one
//! sub-shard directory `blocks/{S}/{Y:02}/` by integer division; no index or count is
//! consulted. The sub-shard index `Y` is zero-padded to two digits (`00`..`99`).

use std::path::{Path, PathBuf};

/// Blocks per sub-shard.
pub const SHARD_Z_SIZE: u64 = 10_000;
/// Sub-shards per main shard.
pub const SHARD_Y_SIZE: u64 = 100;
/// Blocks per main shard (computed).
pub const SHARD_S_SIZE: u64 = SHARD_Z_SIZE * SHARD_Y_SIZE;

/// The `(S, Y)` coordinates of a block's sub-shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardId {
    /// Main shard index (`S`), one per `SHARD_S_SIZE` blocks.
    pub s: u64,
    /// Sub-shard index (`Y`) within the main shard, one per `SHARD_Z_SIZE` blocks.
    pub y: u64,
}

/// Maps a block height to its sub-shard `(S, Y)`.
///
/// `S = N / SHARD_S_SIZE`, `Y = (N % SHARD_S_SIZE) / SHARD_Z_SIZE`.
pub const fn get_shard(block_number: u64) -> ShardId {
    let s = block_number / SHARD_S_SIZE;
    let y = (block_number % SHARD_S_SIZE) / SHARD_Z_SIZE;
    ShardId { s, y }
}

/// Inverse of [`get_shard`]: the half-open block range `[start, end)` a sub-shard covers.
pub const fn get_shard_range(shard: ShardId) -> (u64, u64) {
    let start = shard.s * SHARD_S_SIZE + shard.y * SHARD_Z_SIZE;
    (start, start + SHARD_Z_SIZE)
}

/// Whether `block_number` is the last block of its sub-shard (the seal trigger).
pub const fn is_last_block_of_shard(block_number: u64) -> bool {
    block_number % SHARD_Z_SIZE == SHARD_Z_SIZE - 1
}

/// Whether `block_number` is in the same sub-shard as `head` (the current shard).
pub const fn is_in_current_shard(block_number: u64, head: u64) -> bool {
    let block = get_shard(block_number);
    let current = get_shard(head);
    block.s == current.s && block.y == current.y
}

/// Whether `block_number` is within the last `shards` sub-shards ending at `head` — the current
/// shard plus the previous `shards - 1`.
///
/// Pure math: the global sub-shard index is `block / SHARD_Z_SIZE`, so `shards = 1` is the
/// current shard and `shards = 2` is the current plus the previous (the "last 2 shards" window
/// the seal and reader care about). `block_number` above `head` is `false`.
pub const fn is_in_recent_shards(block_number: u64, head: u64, shards: u64) -> bool {
    let current = head / SHARD_Z_SIZE;
    let block = block_number / SHARD_Z_SIZE;
    current >= block && current - block < shards
}

/// Convenience for [`is_in_recent_shards`] with `shards = 2` (current + previous).
pub const fn is_in_last_two_shards(block_number: u64, head: u64) -> bool {
    is_in_recent_shards(block_number, head, 2)
}

/// The directory holding a sub-shard's files: `blocks/{S}/{Y:02}` relative to `base`.
pub fn shard_dir(base: impl AsRef<Path>, shard: ShardId) -> PathBuf {
    base.as_ref().join("blocks").join(shard.s.to_string()).join(format!("{:02}", shard.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shard derivation matches the spec's worked examples and boundaries.
    #[test]
    fn shard_derivation_matches_spec() {
        assert_eq!(get_shard(0), ShardId { s: 0, y: 0 });
        assert_eq!(get_shard(9_999), ShardId { s: 0, y: 0 });
        assert_eq!(get_shard(10_000), ShardId { s: 0, y: 1 });
        assert_eq!(get_shard(999_999), ShardId { s: 0, y: 99 });
        assert_eq!(get_shard(1_000_000), ShardId { s: 1, y: 0 });
    }

    /// The inverse maps a shard back to its block range, consistent with `get_shard`.
    #[test]
    fn shard_range_is_inverse_of_get_shard() {
        for n in [0u64, 9_999, 10_000, 999_999, 1_000_000, 12_345_678] {
            let shard = get_shard(n);
            let (start, end) = get_shard_range(shard);
            assert!((start..end).contains(&n), "block {n} outside range {start}..{end}");
            assert_eq!(end - start, SHARD_Z_SIZE);
        }
    }

    /// The seal trigger fires only on the last block of a sub-shard.
    #[test]
    fn last_block_of_shard_boundary() {
        assert!(!is_last_block_of_shard(9_998));
        assert!(is_last_block_of_shard(9_999));
        assert!(!is_last_block_of_shard(10_000));
        assert!(is_last_block_of_shard(19_999));
    }

    /// The recent-shard window is pure math: current, current+previous, and older.
    #[test]
    fn recent_shard_window() {
        // Head in sub-shard 2 (blocks 20_000..=29_999).
        let head = 25_000;
        assert!(is_in_current_shard(22_000, head));
        assert!(!is_in_current_shard(19_999, head));
        // Last 2 shards: current (2) and previous (1).
        assert!(is_in_last_two_shards(25_000, head));
        assert!(is_in_last_two_shards(15_000, head)); // sub-shard 1
        assert!(!is_in_last_two_shards(5_000, head)); // sub-shard 0
        assert!(is_in_recent_shards(5_000, head, 3));
        assert!(!is_in_recent_shards(35_000, head, 2)); // above the head (next shard)
    }

    /// Sub-shard directories are zero-padded to two digits: `blocks/0/00` … `blocks/0/99`.
    #[test]
    fn shard_dir_zero_pads_sub_shard() {
        let base = Path::new("/data");
        assert_eq!(shard_dir(base, get_shard(0)), PathBuf::from("/data/blocks/0/00"));
        assert_eq!(shard_dir(base, get_shard(9_999)), PathBuf::from("/data/blocks/0/00"));
        assert_eq!(shard_dir(base, get_shard(10_000)), PathBuf::from("/data/blocks/0/01"));
        assert_eq!(shard_dir(base, get_shard(999_999)), PathBuf::from("/data/blocks/0/99"));
        assert_eq!(shard_dir(base, get_shard(1_000_000)), PathBuf::from("/data/blocks/1/00"));
    }
}
