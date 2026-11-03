use smartnet_prune_types::{PruneInterruptReason, PruneProgress};
use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

/// Limits a pruner run by either the number of entries (rows in the database) that can be deleted
/// or the time it can run.
#[derive(Debug, Clone, Default)]
pub struct PruneLimiter {
    /// Maximum entries (rows in the database) to delete from the database per run.
    deleted_entries_limit: Option<PruneDeletedEntriesLimit>,
    /// Maximum duration of one prune run.
    time_limit: Option<PruneTimeLimit>,
}

#[derive(Debug, Clone)]
struct PruneDeletedEntriesLimit {
    /// Maximum entries (rows in the database) to delete from the database.
    limit: usize,
    /// Current number of entries (rows in the database) that have been deleted.
    deleted: usize,
}

impl PruneDeletedEntriesLimit {
    const fn new(limit: usize) -> Self {
        Self { limit, deleted: 0 }
    }

    const fn is_limit_reached(&self) -> bool {
        self.deleted >= self.limit
    }
}

#[derive(Debug, Clone)]
struct PruneTimeLimit {
    /// Maximum duration of one prune run.
    limit: Duration,
    /// Time when the prune run has started.
    start: Instant,
}

impl PruneTimeLimit {
    fn new(limit: Duration) -> Self {
        Self { limit, start: Instant::now() }
    }

    fn is_limit_reached(&self) -> bool {
        self.start.elapsed() > self.limit
    }
}

impl PruneLimiter {
    /// Sets the limit on the number of deleted entries (rows in the database).
    /// If the limit was already set, it will be overwritten but the deleted count preserved.
    pub const fn set_deleted_entries_limit(mut self, limit: usize) -> Self {
        if let Some(deleted_entries_limit) = self.deleted_entries_limit.as_mut() {
            deleted_entries_limit.limit = limit;
        } else {
            self.deleted_entries_limit = Some(PruneDeletedEntriesLimit::new(limit));
        }

        self
    }

    /// Sets the limit on the number of deleted entries (rows in the database) to a biggest
    /// multiple of the given denominator that is smaller than the existing limit.
    ///
    /// If the limit wasn't set, does nothing.
    pub fn floor_deleted_entries_limit_to_multiple_of(mut self, denominator: NonZeroUsize) -> Self {
        if let Some(deleted_entries_limit) = self.deleted_entries_limit.as_mut() {
            deleted_entries_limit.limit =
                (deleted_entries_limit.limit / denominator) * denominator.get();
        }

        self
    }

    /// Returns `true` if the limit on the number of deleted entries (rows in the database) is
    /// reached.
    pub fn is_deleted_entries_limit_reached(&self) -> bool {
        self.deleted_entries_limit.as_ref().is_some_and(|limit| limit.is_limit_reached())
    }

    /// Increments the number of deleted entries by the given number.
    pub const fn increment_deleted_entries_count_by(&mut self, entries: usize) {
        if let Some(limit) = self.deleted_entries_limit.as_mut() {
            limit.deleted += entries;
        }
    }

    /// Increments the number of deleted entries by one.
    pub const fn increment_deleted_entries_count(&mut self) {
        self.increment_deleted_entries_count_by(1)
    }

    /// Returns the number of deleted entries left before the limit is reached.
    pub fn deleted_entries_limit_left(&self) -> Option<usize> {
        self.deleted_entries_limit.as_ref().map(|limit| limit.limit.saturating_sub(limit.deleted))
    }

    /// Returns the limit on the number of deleted entries (rows in the database).
    pub fn deleted_entries_limit(&self) -> Option<usize> {
        self.deleted_entries_limit.as_ref().map(|limit| limit.limit)
    }

    /// Sets the time limit.
    pub fn set_time_limit(mut self, limit: Duration) -> Self {
        self.time_limit = Some(PruneTimeLimit::new(limit));

        self
    }

    /// Returns `true` if time limit is reached.
    pub fn is_time_limit_reached(&self) -> bool {
        self.time_limit.as_ref().is_some_and(|limit| limit.is_limit_reached())
    }

    /// Returns `true` if any limit is reached.
    pub fn is_limit_reached(&self) -> bool {
        self.is_deleted_entries_limit_reached() || self.is_time_limit_reached()
    }

    /// Creates new [`PruneInterruptReason`] based on the limiter's state.
    pub fn interrupt_reason(&self) -> PruneInterruptReason {
        if self.is_time_limit_reached() {
            PruneInterruptReason::Timeout
        } else if self.is_deleted_entries_limit_reached() {
            PruneInterruptReason::DeletedEntriesLimitReached
        } else {
            PruneInterruptReason::Unknown
        }
    }

    /// Creates new [`PruneProgress`].
    ///
    /// If `done == true`, returns [`PruneProgress::Finished`], otherwise
    /// [`PruneProgress::HasMoreData`] is returned with [`PruneInterruptReason`] according to the
    /// limiter's state.
    pub fn progress(&self, done: bool) -> PruneProgress {
        if done {
            PruneProgress::Finished
        } else {
            PruneProgress::HasMoreData(self.interrupt_reason())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prune_deleted_entries_limit_initial_state() {
        let limit_tracker = PruneDeletedEntriesLimit::new(10);
        // Limit should be set properly
        assert_eq!(limit_tracker.limit, 10);
        // No entries should be deleted
        assert_eq!(limit_tracker.deleted, 0);
        assert!(!limit_tracker.is_limit_reached());
    }

    #[test]
    fn test_prune_time_limit_initial_state() {
        let time_limit = PruneTimeLimit::new(Duration::from_secs(10));
        // The limit should be set correctly
        assert_eq!(time_limit.limit, Duration::from_secs(10));
        // The elapsed time should be very small right after creation
        assert!(time_limit.start.elapsed() < Duration::from_secs(1));
        // Limit should not be reached initially
        assert!(!time_limit.is_limit_reached());
    }

    #[test]
    fn test_set_deleted_entries_limit_initial_state() {
        let pruner = PruneLimiter::default().set_deleted_entries_limit(100);
        // The deleted_entries_limit should be set with the correct limit
        assert!(pruner.deleted_entries_limit.is_some());
        let deleted_entries_limit = pruner.deleted_entries_limit.unwrap();
        assert_eq!(deleted_entries_limit.limit, 100);
        // The deleted count should be initially zero
        assert_eq!(deleted_entries_limit.deleted, 0);
        // The limit should not be reached initially
        assert!(!deleted_entries_limit.is_limit_reached());
    }

    #[test]
    fn test_floor_deleted_entries_limit_to_multiple_of() {
        let limiter = PruneLimiter::default().set_deleted_entries_limit(15);
        let denominator = NonZeroUsize::new(4).unwrap();

        // Floor limit to the largest multiple of 4 less than or equal to 15 (that is 12)
        let updated_limiter = limiter.floor_deleted_entries_limit_to_multiple_of(denominator);
        assert_eq!(updated_limiter.deleted_entries_limit.unwrap().limit, 12);

        // Test when the limit is already a multiple of the denominator
        let limiter = PruneLimiter::default().set_deleted_entries_limit(16);
        let updated_limiter = limiter.floor_deleted_entries_limit_to_multiple_of(denominator);
        assert_eq!(updated_limiter.deleted_entries_limit.unwrap().limit, 16);

        // Test when there's no limit set (should not panic)
        let limiter = PruneLimiter::default();
        let updated_limiter = limiter.floor_deleted_entries_limit_to_multiple_of(denominator);
        assert!(updated_limiter.deleted_entries_limit.is_none());
    }
}
