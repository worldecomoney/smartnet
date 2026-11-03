//! Short-lived object cache: retains full rumor bytes so a node can answer `RUMOR_GET`.
//!
//! The duplicate-suppression TTL is owned by [`crate::seen::SeenCache`]; this cache only
//! keeps payload bytes for a short window after announce so peers that answered a request
//! can be served. It is bounded by entry count and total bytes, evicting oldest first.

use crate::rumor::Rumor;
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Default time an object stays available for `RUMOR_GET`.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);
/// Default entry cap.
pub const DEFAULT_MAX_ENTRIES: usize = 4096;
/// Default total payload byte cap (64 MiB).
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Bounded map of object hash to full rumor bytes.
#[derive(Debug)]
pub struct ObjectCache {
    state: Mutex<State>,
    ttl: Duration,
    max_entries: usize,
    max_bytes: usize,
}

#[derive(Debug, Default)]
struct State {
    entries: HashMap<[u8; 32], Entry>,
    /// Insertion order — entries are first-write-wins and only leave through TTL or
    /// the caps (both oldest-first), so FIFO is exactly the eviction order. This keeps
    /// prune/enforce amortized O(1); a full-map scan per gossip op is not.
    order: VecDeque<([u8; 32], Instant)>,
    bytes: usize,
}

#[derive(Debug)]
struct Entry {
    rumor: Rumor,
}

impl Default for ObjectCache {
    fn default() -> Self {
        Self::new(DEFAULT_TTL, DEFAULT_MAX_ENTRIES, DEFAULT_MAX_BYTES)
    }
}

impl ObjectCache {
    /// Creates a cache with explicit bounds.
    pub fn new(ttl: Duration, max_entries: usize, max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            ttl,
            max_entries,
            max_bytes,
        }
    }

    /// Stores `rumor` if its hash is new; returns whether it was inserted.
    pub fn insert(&self, rumor: Rumor) -> bool {
        self.insert_at(rumor, Instant::now())
    }

    /// [`Self::insert`] with an explicit clock (tests / deterministic expiry).
    pub fn insert_at(&self, rumor: Rumor, now: Instant) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        self.prune(&mut state, now);

        if state.entries.contains_key(&rumor.hash) {
            return false;
        }
        let size = rumor.data.len();
        if size > self.max_bytes {
            return false;
        }
        state.bytes += size;
        state.order.push_back((rumor.hash, now));
        state.entries.insert(rumor.hash, Entry { rumor });
        self.enforce_limits(&mut state);
        true
    }

    /// Returns a stored rumor by hash, if still fresh.
    pub fn get(&self, hash: &[u8; 32]) -> Option<Rumor> {
        self.get_at(hash, Instant::now())
    }

    /// [`Self::get`] with an explicit clock.
    pub fn get_at(&self, hash: &[u8; 32], now: Instant) -> Option<Rumor> {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        self.prune(&mut state, now);
        state.entries.get(hash).map(|entry| entry.rumor.clone())
    }

    /// Whether a fresh entry exists for `hash`.
    pub fn contains(&self, hash: &[u8; 32]) -> bool {
        self.get(hash).is_some()
    }

    /// Number of retained entries (test/diagnostic helper).
    pub fn len(&self) -> usize {
        self.state.lock().unwrap_or_else(|err| err.into_inner()).entries.len()
    }

    /// Whether no entries are retained.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops expired entries from the front of the insertion order.
    fn prune(&self, state: &mut State, now: Instant) {
        let ttl = self.ttl;
        while let Some((hash, stored_at)) = state.order.front().copied() {
            if now.saturating_duration_since(stored_at) < ttl {
                break;
            }
            state.order.pop_front();
            if let Some(entry) = state.entries.remove(&hash) {
                state.bytes = state.bytes.saturating_sub(entry.rumor.data.len());
            }
        }
    }

    /// Evicts oldest entries until the cache fits within the configured caps.
    fn enforce_limits(&self, state: &mut State) {
        while state.entries.len() > self.max_entries || state.bytes > self.max_bytes {
            let Some((hash, _)) = state.order.pop_front() else { break };
            if let Some(entry) = state.entries.remove(&hash) {
                state.bytes = state.bytes.saturating_sub(entry.rumor.data.len());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;

    #[test]
    fn insert_is_first_write_wins_and_get_returns_bytes() {
        let cache = ObjectCache::default();
        let rumor = Rumor::tx(vec![0x01, 0x02]);
        assert!(cache.insert(rumor.clone()));
        assert!(!cache.insert(rumor.clone()));
        assert_eq!(cache.get(&rumor.hash), Some(rumor));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn entries_expire_after_ttl() {
        let cache = ObjectCache::new(Duration::from_secs(10), 10, 1024);
        let t0 = Instant::now();
        let rumor = Rumor::tx(vec![0x09]);
        assert!(cache.insert_at(rumor.clone(), t0));
        assert!(cache.get_at(&rumor.hash, t0 + Duration::from_secs(9)).is_some());
        assert!(cache.get_at(&rumor.hash, t0 + Duration::from_secs(11)).is_none());
    }

    #[test]
    fn byte_cap_evicts_oldest() {
        let cache = ObjectCache::new(Duration::from_secs(60), 10, 4);
        let first = Rumor::tx(vec![1, 2, 3, 4]);
        let second = Rumor::tx(vec![5, 6, 7, 8]);
        assert!(cache.insert(first.clone()));
        assert!(cache.insert(second.clone()));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&first.hash).is_none());
        assert!(cache.get(&second.hash).is_some());
    }

    #[test]
    fn oversized_object_is_not_stored() {
        let cache = ObjectCache::new(Duration::from_secs(60), 10, 2);
        assert!(!cache.insert(Rumor::block(1, Bytes::from(vec![0, 0, 0]))));
        assert!(cache.is_empty());
    }
}
