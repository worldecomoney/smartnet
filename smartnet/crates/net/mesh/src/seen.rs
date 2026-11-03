//! TTL-bounded cache of object hashes we have already originated, injected, or forwarded.
//!
//! The cache is the mesh's only loop guard besides the "receivers never re-spread" rule:
//!
//! - **Origin**: marks an object hash *before* spreading, so the node's own pool/canon callbacks
//!   (or a round-tripped echo) see it as already seen and stay silent.
//! - **Receiver**: check-and-marks *before* local inject, so the pool/canon event caused by that
//!   very inject is already marked and never re-spread.
//!
//! Entries expire after [`SeenCache::ttl`] and the map is capped at [`SeenCache::max`]
//! entries (oldest evicted) so a long-running node cannot grow it without bound.

use std::{
    collections::{hash_map::Entry, HashMap},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Default time-to-live of a seen-object entry.
pub const DEFAULT_TTL: Duration = Duration::from_secs(3_600);
/// Default maximum number of retained entries.
pub const DEFAULT_MAX: usize = 100_000;
/// How often (as a fraction of the TTL) expired entries are swept.
const PRUNE_EVERY: Duration = Duration::from_secs(900);

/// "Have we seen this object?" cache, keyed by the stable object hash in
/// [`Rumor`](crate::rumor::Rumor).
#[derive(Debug)]
pub struct SeenCache {
    state: Mutex<SeenState>,
    ttl: Duration,
    max: usize,
}

#[derive(Debug, Default)]
struct SeenState {
    map: HashMap<[u8; 32], Instant>,
    last_prune: Option<Instant>,
}

impl Default for SeenCache {
    fn default() -> Self {
        Self::new(DEFAULT_TTL, DEFAULT_MAX)
    }
}

impl SeenCache {
    /// Creates a cache with the given TTL and entry cap.
    pub fn new(ttl: Duration, max: usize) -> Self {
        Self { state: Mutex::new(SeenState::default()), ttl, max }
    }

    /// Records `id` if it is new.
    ///
    /// Returns `true` when the id was **not** present (caller should process the rumor:
    /// inject locally and, if we are the origin, spread it), `false` when it was already
    /// known (caller must drop the rumor).
    pub fn check_and_mark(&self, id: [u8; 32]) -> bool {
        self.check_and_mark_at(id, Instant::now())
    }

    /// [`Self::check_and_mark`] with an explicit clock (tests / deterministic expiry).
    pub fn check_and_mark_at(&self, id: [u8; 32], now: Instant) -> bool {
        // Poison-safe: a panicked holder must not wedge gossip for the node's lifetime.
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());

        if state.last_prune.is_none_or(|at| now.saturating_duration_since(at) >= PRUNE_EVERY) {
            let ttl = self.ttl;
            state.map.retain(|_, at| now.saturating_duration_since(*at) < ttl);
            state.last_prune = Some(now);
        }

        let is_fresh = |at: Instant| now.saturating_duration_since(at) < self.ttl;
        let is_new = match state.map.entry(id) {
            // Already seen and within the TTL → caller must drop the rumor.
            Entry::Occupied(slot) if is_fresh(*slot.get()) => false,
            // Seen but expired (or brand new) → process again, stamped fresh.
            Entry::Occupied(mut slot) => {
                slot.insert(now);
                true
            }
            Entry::Vacant(slot) => {
                slot.insert(now);
                true
            }
        };
        if is_new {
            self.enforce_cap(&mut state.map, now);
        }
        is_new
    }

    /// Whether `id` is currently fresh in the cache.
    pub fn contains(&self, id: &[u8; 32]) -> bool {
        let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.map.get(id).is_some_and(|at| at.elapsed() < self.ttl)
    }

    /// Number of retained entries (test/diagnostic helper).
    pub fn len(&self) -> usize {
        self.state.lock().unwrap_or_else(|err| err.into_inner()).map.len()
    }

    /// Whether no entries are retained.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Evicts oldest entries until the map fits within the configured cap.
    fn enforce_cap(&self, map: &mut HashMap<[u8; 32], Instant>, now: Instant) {
        if map.len() <= self.max {
            return;
        }
        let ttl = self.ttl;
        map.retain(|_, at| now.saturating_duration_since(*at) < ttl);
        while map.len() > self.max {
            let Some(oldest) = map.iter().min_by_key(|(_, at)| *at).map(|(id, _)| *id) else {
                break;
            };
            map.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_mark_is_new_second_is_not() {
        let cache = SeenCache::default();
        let id = [7u8; 32];
        assert!(cache.check_and_mark(id));
        assert!(!cache.check_and_mark(id));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn distinct_ids_are_independent() {
        let cache = SeenCache::default();
        assert!(cache.check_and_mark([1u8; 32]));
        assert!(cache.check_and_mark([2u8; 32]));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn entries_expire_after_ttl() {
        let cache = SeenCache::new(Duration::from_secs(10), 100);
        let t0 = Instant::now();
        assert!(cache.check_and_mark_at([9u8; 32], t0));
        // still fresh right before the TTL
        assert!(!cache.check_and_mark_at([9u8; 32], t0 + Duration::from_secs(9)));
        // expired → seen again as new
        assert!(cache.check_and_mark_at([9u8; 32], t0 + Duration::from_secs(11)));
    }
}
