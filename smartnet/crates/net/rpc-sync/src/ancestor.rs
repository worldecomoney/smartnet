//! Common-ancestor search between the local chain and a remote peer.
//!
//! Assumption (holds for `SmartNet`: single canonical producer, no reorgs across the fork
//! height): the local chain is a *prefix* of the remote chain, so the common ancestor is the
//! highest block number whose hash matches on both sides. Binary search costs O(log n) round
//! trips — a joining node anchors at genesis, an already-synced node never re-walks its
//! trusted prefix.

use alloy_primitives::B256;

/// Outcome of the ancestor search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonAncestor {
    /// Highest block number where local and remote hashes match.
    pub number: u64,
    /// Hash at that number.
    pub hash: B256,
}

/// Find the common ancestor by binary search.
///
/// `local_height`/`local_hash` describe the trusted local tip; `remote_hash` returns the
/// peer's hash at a number (`None` = peer lacks the block). Both hashes must match at the
/// tip for [`CommonAncestor`] at full height; otherwise the search descends.
///
/// Returns `None` if even the anchor (block 0 / genesis) disagrees — the peer is on a
/// different chain and must not be synced from.
pub async fn find_common_ancestor<LFut, RFut>(
    local_height: u64,
    local_hash: impl Fn(u64) -> LFut + Send,
    remote_hash: impl Fn(u64) -> RFut + Send,
) -> Result<Option<CommonAncestor>, String>
where
    LFut: std::future::Future<Output = Option<B256>> + Send,
    RFut: std::future::Future<Output = Result<Option<B256>, String>> + Send,
{
    // Fast path: tips already agree.
    if let (Some(l), Ok(Some(r))) =
        (local_hash(local_height).await, remote_hash(local_height).await) &&
        l == r
    {
        return Ok(Some(CommonAncestor { number: local_height, hash: l }));
    }

    // Invariant: hash(lo) matches on both sides (starts at genesis), hash(hi) does not.
    let anchor_hash = match (local_hash(0).await, remote_hash(0).await) {
        (Some(l), Ok(Some(r))) if l == r => l,
        _ => return Ok(None),
    };
    let mut lo = 0u64;
    let mut hi = local_height + 1; // exclusive upper bound: known-mismatch
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        let matched = match (local_hash(mid).await, remote_hash(mid).await) {
            (Some(l), Ok(Some(r))) => l == r,
            // Peer lacking a header mid-range: treat as mismatch and descend.
            _ => false,
        };
        if matched {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(Some(CommonAncestor { number: lo, hash: anchor_hash }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Synthetic 100-block chain; local knows blocks 0..=40 (prefix). First byte is
    /// `n + 1` so no hash collides with `B256::ZERO` (all-zeros sentinel in tests).
    fn chain() -> HashMap<u64, B256> {
        (0..100).map(|n| (n, B256::from(sha_distinct(n)))).collect()
    }

    fn sha_distinct(n: u64) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = (n + 1) as u8;
        bytes
    }

    #[tokio::test]
    async fn finds_midchain_ancestor_when_local_diverged() {
        // Local tip 40 points at a different chain; common ancestor is 25.
        let remote = std::sync::Arc::new(chain());
        let mut local = (*remote).clone();
        for n in 26..=40 {
            let mut bytes = [0u8; 32];
            bytes[0] = 0xFF;
            bytes[1] = (n + 1) as u8;
            local.insert(n, B256::from(bytes));
        }
        let local = std::sync::Arc::new(local);
        let got = find_common_ancestor(
            40,
            |n| {
                let local = local.clone();
                async move { local.get(&n).copied() }
            },
            |n| {
                let remote = remote.clone();
                async move { Ok(remote.get(&n).copied()) }
            },
        )
        .await
        .unwrap()
        .expect("same genesis");
        assert_eq!(got.number, 25);
    }

    #[tokio::test]
    async fn none_when_genesis_differs() {
        // A different chain entirely: every local hash (incl. genesis) differs.
        fn other_chain() -> HashMap<u64, B256> {
            (0..100)
                .map(|n| {
                    let mut bytes = [0xEEu8; 32];
                    bytes[1] = (n + 1) as u8;
                    (n, B256::from(bytes))
                })
                .collect()
        }
        let remote = std::sync::Arc::new(chain());
        let local = std::sync::Arc::new(other_chain());
        let got = find_common_ancestor(
            40,
            |n| {
                let local = local.clone();
                async move { local.get(&n).copied() }
            },
            |n| {
                let remote = remote.clone();
                async move { Ok(remote.get(&n).copied()) }
            },
        )
        .await
        .unwrap();
        assert!(got.is_none());
    }
}
