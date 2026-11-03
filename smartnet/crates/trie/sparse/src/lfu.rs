use core::{fmt, hash};

use alloc::vec::Vec;
use alloy_primitives::map::HashMap;

/// Maximum frequency value. Frequencies are capped here to bound bucket storage.
const LFU_MAX_FREQ: u16 = 255;

/// Per-entry metadata stored in the LFU lookup map.
#[derive(Debug, Clone, Copy)]
struct LfuEntryMeta {
    /// Current frequency (1..=`LFU_MAX_FREQ`).
    freq: u16,
    /// Index of this key within `buckets[freq]`.
    pos: usize,
}

/// Bucketed LFU cache with O(1) eviction and O(1) touch operations.
///
/// Generic over the key type `K`.
#[derive(Debug)]
pub(crate) struct BucketedLfu<K> {
    capacity: usize,
    /// Maps each key to its frequency and bucket position.
    entries: HashMap<K, LfuEntryMeta>,
    /// `buckets[f]` holds all keys currently at frequency `f`.
    /// Index 0 is unused; valid frequencies are 1..=`LFU_MAX_FREQ`.
    buckets: Vec<Vec<K>>,
    /// Smallest non-empty frequency bucket.
    min_freq: u16,
}

impl<K> Default for BucketedLfu<K> {
    fn default() -> Self {
        Self::new(0)
    }
}

impl<K> BucketedLfu<K> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::default(),
            buckets: (0..=LFU_MAX_FREQ).map(|_| Vec::new()).collect(),
            min_freq: 1,
        }
    }
}

impl<K: fmt::Debug + Copy + Eq + hash::Hash> BucketedLfu<K> {
    /// Updates the capacity and evicts the least-frequently-used entries that exceed it.
    ///
    /// Entries accumulate via [`Self::touch`] over time. This method trims the cache
    /// so that only the `capacity` hottest entries are retained.
    pub(crate) fn decay_and_evict(&mut self, capacity: usize) {
        self.capacity = capacity;

        if self.capacity == 0 {
            self.entries.clear();
            for b in &mut self.buckets {
                b.clear();
            }
            self.min_freq = 1;
            return;
        }

        while self.entries.len() > self.capacity {
            self.evict_one();
        }
    }

    /// Evicts a single entry from the lowest-frequency bucket.
    fn evict_one(&mut self) {
        if self.entries.is_empty() {
            self.min_freq = 1;
            return;
        }

        // Advance min_freq to the next non-empty bucket.
        while (self.min_freq as usize) < self.buckets.len() &&
            self.buckets[self.min_freq as usize].is_empty()
        {
            self.min_freq += 1;
        }

        if (self.min_freq as usize) >= self.buckets.len() {
            self.min_freq = 1;
            return;
        }

        if let Some(key) = self.buckets[self.min_freq as usize].pop() {
            self.entries.remove(&key);

            while (self.min_freq as usize) < self.buckets.len() &&
                self.buckets[self.min_freq as usize].is_empty()
            {
                self.min_freq += 1;
            }

            if (self.min_freq as usize) >= self.buckets.len() {
                self.min_freq = 1;
            }
        }
    }

    /// Records a key touch. O(1) amortized.
    pub(crate) fn touch(&mut self, key: K) {
        if self.capacity == 0 {
            return;
        }

        if let Some(meta) = self.entries.get(&key).copied() {
            debug_assert_eq!(self.buckets[meta.freq as usize][meta.pos], key);

            let old_freq = meta.freq as usize;
            let new_freq = meta.freq.saturating_add(1).min(LFU_MAX_FREQ);

            if new_freq as usize != old_freq {
                // Remove from old bucket via swap_remove and fix the swapped element's pos.
                self.buckets[old_freq].swap_remove(meta.pos);
                if let Some(&moved_key) = self.buckets[old_freq].get(meta.pos) {
                    self.entries.get_mut(&moved_key).expect("moved key must exist").pos = meta.pos;
                }

                // Insert into new bucket.
                let new_pos = self.buckets[new_freq as usize].len();
                self.buckets[new_freq as usize].push(key);

                let entry = self.entries.get_mut(&key).expect("key must exist");
                entry.freq = new_freq;
                entry.pos = new_pos;

                // Update min_freq if old bucket is now empty.
                if self.buckets[old_freq].is_empty() && old_freq == self.min_freq as usize {
                    self.min_freq = new_freq;
                }
            }
        } else {
            // Evict if at capacity before inserting.
            if self.entries.len() >= self.capacity {
                self.evict_one();
            }

            // New entry at frequency 1.
            let pos = self.buckets[1].len();
            self.buckets[1].push(key);
            self.entries.insert(key, LfuEntryMeta { freq: 1, pos });
            self.min_freq = 1;
        }
    }

    /// Returns an iterator over all retained keys.
    #[cfg(any(test, feature = "std"))]
    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.keys()
    }

    /// Returns the number of retained keys.
    #[cfg(any(test, feature = "std"))]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}
