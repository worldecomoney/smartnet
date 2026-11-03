use alloc::vec::Vec;
use core::cmp::Ordering;
use itertools::Itertools;

/// Merge sorted slices into a sorted `Vec`. First occurrence wins for duplicate keys.
///
/// Callers pass slices in priority order (index 0 = highest priority), so the first
/// slice's value for a key takes precedence over later slices.
pub(crate) fn kway_merge_sorted<'a, K, V>(
    slices: impl IntoIterator<Item = &'a [(K, V)]>,
) -> Vec<(K, V)>
where
    K: Ord + Clone + 'a,
    V: Clone + 'a,
{
    slices
        .into_iter()
        .filter(|s| !s.is_empty())
        .enumerate()
        // Merge by reference: (priority, &K, &V) - avoids cloning all elements upfront
        .map(|(i, s)| s.iter().map(move |(k, v)| (i, k, v)))
        .kmerge_by(|(i1, k1, _), (i2, k2, _)| (k1, i1) < (k2, i2))
        .dedup_by(|(_, k1, _), (_, k2, _)| *k1 == *k2)
        // Clone only surviving elements after dedup
        .map(|(_, k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Extend a sorted vector with another sorted vector using 2 pointer merge.
/// Values from `other` take precedence for duplicate keys.
pub(crate) fn extend_sorted_vec<K, V>(target: &mut Vec<(K, V)>, other: &[(K, V)])
where
    K: Clone + Ord,
    V: Clone,
{
    if other.is_empty() {
        return;
    }

    if target.is_empty() {
        target.extend_from_slice(other);
        return;
    }

    // Fast path: non-overlapping ranges - just append
    if target.last().map(|(k, _)| k) < other.first().map(|(k, _)| k) {
        target.extend_from_slice(other);
        return;
    }

    // Move ownership of target to avoid cloning owned elements
    let left = core::mem::take(target);
    let mut out = Vec::with_capacity(left.len() + other.len());

    let mut a = left.into_iter().peekable();
    let mut b = other.iter().peekable();

    while let (Some(aa), Some(bb)) = (a.peek(), b.peek()) {
        match aa.0.cmp(&bb.0) {
            Ordering::Less => {
                out.push(a.next().unwrap());
            }
            Ordering::Greater => {
                out.push(b.next().unwrap().clone());
            }
            Ordering::Equal => {
                // `other` takes precedence for duplicate keys - reuse key from `a`
                let (k, _) = a.next().unwrap();
                out.push((k, b.next().unwrap().1.clone()));
            }
        }
    }

    // Drain remaining: `a` moves, `b` clones
    out.extend(a);
    out.extend(b.cloned());

    *target = out;
}

