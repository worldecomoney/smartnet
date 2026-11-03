//! Cross-block execution cache for payload processing.
//!
//! This crate provides the core caching infrastructure used during block execution:
//! - [`ExecutionCache`]: Fixed-size concurrent caches for accounts, storage, and bytecode
//! - [`SavedCache`]: An execution cache snapshot associated with a specific block hash
//! - [`PayloadExecutionCache`]: Thread-safe wrapper for sharing cached state across payload
//!   processing tasks

#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod cached_state;
pub use cached_state::*;

use alloy_primitives::B256;
use metrics::{Counter, Histogram};
use parking_lot::Mutex;
use smartnet_primitives_traits::FastInstant as Instant;
use smartnet_metrics::Metrics;
use std::{sync::Arc, time::Duration};
use tracing::{debug, instrument, warn};

/// A guarded, thread-safe cache of execution state that tracks the most recent block's caches.
///
/// This is the cross-block cache used to accelerate sequential payload processing.
/// When a new block arrives, its parent's cached state can be reused to avoid
/// redundant database lookups.
///
/// This process assumes that payloads are received sequentially.
///
/// ## Cache Safety
///
/// **CRITICAL**: Cache update operations require exclusive access. All concurrent cache users
/// (such as prewarming tasks) must be terminated before calling
/// [`PayloadExecutionCache::update_with_guard`], otherwise the cache may be corrupted or cleared.
#[derive(Clone, Debug, Default)]
pub struct PayloadExecutionCache {
    /// Guarded cloneable cache identified by a block hash.
    inner: Arc<Mutex<Option<SavedCache>>>,
    /// Fresh caches stockpiled at startup so the first block after boot does not pay the
    /// multi-hundred-millisecond zero-fill of [`ExecutionCache::new`] on its critical path.
    prebuilt: Arc<Mutex<Vec<ExecutionCache>>>,
    /// Metrics for cache operations.
    metrics: PayloadExecutionCacheMetrics,
}

impl PayloadExecutionCache {
    /// How many fresh execution caches the background pre-allocation holds at most.
    ///
    /// One covers the unavoidable first miss after boot; a later miss (e.g. a fork block)
    /// falls back to the synchronous path with a warning — same behaviour as before, just
    /// no longer on the first block of every process.
    const PREBUILT_CACHE_STOCK: usize = 1;

    /// Starts stockpiling fresh execution caches in a background thread.
    ///
    /// `FixedCache::new` zeroes its entire backing array, so allocating a multi-GB
    /// cross-block cache costs hundreds of milliseconds to seconds — work that must not
    /// happen inside block validation (`PayloadProcessor::cache_for` on a miss).
    pub fn prebuild_in_background(&self, size_bytes: usize) {
        let stock = self.prebuilt.clone();
        let spawned =
            std::thread::Builder::new().name("exec-cache-prealloc".to_string()).spawn(move || {
                for stock_index in 0..Self::PREBUILT_CACHE_STOCK {
                    let start = Instant::now();
                    let cache = ExecutionCache::new(size_bytes);
                    tracing::info!(
                        target: "engine::execution_cache",
                        stock_index,
                        size_bytes,
                        elapsed = ?start.elapsed(),
                        "pre-allocated cross-block execution cache"
                    );
                    stock.lock().push(cache);
                }
            });
        if let Err(err) = spawned {
            tracing::warn!(
                target: "engine::execution_cache",
                %err,
                "failed to spawn execution cache pre-allocation thread; the first block after boot will allocate synchronously"
            );
        }
    }

    /// Returns a fresh execution cache: pops a stockpiled one when available, otherwise
    /// allocates one synchronously (the slow path the background stock is meant to avoid).
    pub fn get_or_create(&self, size_bytes: usize) -> ExecutionCache {
        if let Some(cached) = self.prebuilt.lock().pop() {
            return cached;
        }
        let start = Instant::now();
        let cache = ExecutionCache::new(size_bytes);
        let elapsed = start.elapsed();
        if elapsed.as_millis() > 100 {
            warn!(
                blocked_for = ?elapsed,
                size_bytes,
                "execution cache allocated synchronously — prebuilt stock empty"
            );
        }
        cache
    }

    /// Returns the cache for `parent_hash` if it's available for use.
    ///
    /// A cache is considered available when:
    /// - It exists and matches the requested parent hash
    /// - No other tasks are currently using it (checked via Arc reference count)
    #[instrument(level = "debug", target = "engine::tree::payload_processor", skip(self))]
    pub fn get_cache_for(&self, parent_hash: B256) -> Option<SavedCache> {
        let start = Instant::now();
        let mut cache = self.inner.lock();

        let elapsed = start.elapsed();
        self.metrics.execution_cache_wait_duration.record(elapsed.as_secs_f64());
        if elapsed.as_millis() > 5 {
            warn!(blocked_for=?elapsed, "Blocked waiting for execution cache mutex");
        }

        if let Some(c) = cache.as_mut() {
            let cached_hash = c.executed_block_hash();
            // Check that the cache hash matches the parent hash of the current block. It won't
            // match in case it's a fork block.
            let hash_matches = cached_hash == parent_hash;
            // Check `is_available()` to ensure no other tasks (e.g., prewarming) currently hold
            // a reference to this cache. We can only reuse it when we have exclusive access.
            let available = c.is_available();
            let usage_count = c.usage_count();

            debug!(
                target: "engine::caching",
                %cached_hash,
                %parent_hash,
                hash_matches,
                available,
                usage_count,
                "Existing cache found"
            );

            if available {
                if !hash_matches {
                    // Fork block: clear and update the hash on the ORIGINAL before cloning.
                    // This prevents the canonical chain from matching on the stale hash
                    // and picking up polluted data if the fork block fails.
                    c.clear_with_hash(parent_hash);
                }
                return Some(c.clone());
            } else if hash_matches {
                self.metrics.execution_cache_in_use.increment(1);
            }
        } else {
            debug!(target: "engine::caching", %parent_hash, "No cache found");
        }

        None
    }

    /// Waits until the execution cache becomes available for use.
    ///
    /// This acquires a write lock to ensure exclusive access, then immediately releases it.
    /// This is useful for synchronization before starting payload processing.
    ///
    /// Returns the time spent waiting for the lock.
    pub fn wait_for_availability(&self) -> Duration {
        let start = Instant::now();
        // Acquire lock to wait for any current holders to finish
        let _guard = self.inner.lock();
        let elapsed = start.elapsed();
        if elapsed.as_millis() > 5 {
            debug!(
                target: "engine::tree::payload_processor",
                blocked_for=?elapsed,
                "Waited for execution cache to become available"
            );
        }
        elapsed
    }

    /// Updates the cache with a closure that has exclusive access to the guard.
    /// This ensures that all cache operations happen atomically.
    ///
    /// ## CRITICAL SAFETY REQUIREMENT
    ///
    /// **Before calling this method, you MUST ensure there are no other active cache users.**
    /// This includes:
    /// - No running prewarming task instances that could write to the cache
    /// - No concurrent transactions that might access the cached state
    /// - All prewarming operations must be completed or cancelled
    ///
    /// Violating this requirement can result in cache corruption, incorrect state data,
    /// and potential consensus failures.
    pub fn update_with_guard<F>(&self, update_fn: F)
    where
        F: FnOnce(&mut Option<SavedCache>),
    {
        let mut guard = self.inner.lock();
        update_fn(&mut guard);
    }
}

/// Metrics for [`PayloadExecutionCache`] operations.
#[derive(Metrics, Clone)]
#[metrics(scope = "consensus.engine.beacon")]
struct PayloadExecutionCacheMetrics {
    /// Counter for when the execution cache was unavailable because other threads
    /// (e.g., prewarming) are still using it.
    execution_cache_in_use: Counter,
    /// Time spent waiting for execution cache mutex to become available.
    execution_cache_wait_duration: Histogram,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_checkout_blocks_second() {
        let cache = PayloadExecutionCache::default();
        let hash = B256::from([1u8; 32]);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash, ExecutionCache::new(1_000)))
        });

        let first = cache.get_cache_for(hash);
        assert!(first.is_some());

        let second = cache.get_cache_for(hash);
        assert!(second.is_none());
    }

    #[test]
    fn checkout_available_after_drop() {
        let cache = PayloadExecutionCache::default();
        let hash = B256::from([2u8; 32]);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash, ExecutionCache::new(1_000)))
        });

        let checked_out = cache.get_cache_for(hash);
        assert!(checked_out.is_some());
        drop(checked_out);

        let second = cache.get_cache_for(hash);
        assert!(second.is_some());
    }

    #[test]
    fn hash_mismatch_clears_and_retags() {
        let cache = PayloadExecutionCache::default();
        let hash_a = B256::from([0xAA; 32]);
        let hash_b = B256::from([0xBB; 32]);

        cache.update_with_guard(|slot| {
            *slot = Some(SavedCache::new(hash_a, ExecutionCache::new(1_000)))
        });

        let checked_out = cache.get_cache_for(hash_b);
        assert!(checked_out.is_some());
        assert_eq!(checked_out.unwrap().executed_block_hash(), hash_b);
    }

    #[test]
    fn empty_cache_returns_none() {
        let cache = PayloadExecutionCache::default();
        assert!(cache.get_cache_for(B256::ZERO).is_none());
    }
}
