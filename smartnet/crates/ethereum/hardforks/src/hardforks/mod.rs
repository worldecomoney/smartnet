mod dev;
pub use dev::DEV_HARDFORKS;

use crate::{ForkCondition, ForkFilter, ForkId, Hardfork, Head};
#[cfg(feature = "std")]
use rustc_hash::FxHashMap;

use alloc::{boxed::Box, vec::Vec};

/// Generic trait over a set of ordered hardforks
#[auto_impl::auto_impl(&, Arc)]
pub trait Hardforks: Clone {
    /// Retrieves [`ForkCondition`] from `fork`. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition;

    /// Get an iterator of all hardforks with their respective activation conditions.
    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)>;

    /// Convenience method to check if a fork is active at a given timestamp.
    fn is_fork_active_at_timestamp<H: Hardfork>(&self, fork: H, timestamp: u64) -> bool {
        self.fork(fork).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if a fork is active at a given block number.
    fn is_fork_active_at_block<H: Hardfork>(&self, fork: H, block_number: u64) -> bool {
        self.fork(fork).active_at_block(block_number)
    }

    /// Compute the [`ForkId`] for the given [`Head`] following eip-6122 spec
    fn fork_id(&self, head: &Head) -> ForkId;

    /// Returns the [`ForkId`] for the last fork.
    ///
    /// NOTE: This returns the latest implemented [`ForkId`]. In many cases this will be the future
    /// [`ForkId`] on given network.
    fn latest_fork_id(&self) -> ForkId;

    /// Creates a [`ForkFilter`] for the block described by [Head].
    fn fork_filter(&self, head: Head) -> ForkFilter;
}

/// Ordered list of a chain hardforks that implement [`Hardfork`].
#[derive(Default, Clone, PartialEq, Eq)]
pub struct ChainHardforks {
    forks: Vec<(Box<dyn Hardfork>, ForkCondition)>,
    #[cfg(feature = "std")]
    map: FxHashMap<&'static str, ForkCondition>,
    #[cfg(not(feature = "std"))]
    map: alloc::collections::BTreeMap<&'static str, ForkCondition>,
}

impl ChainHardforks {
    /// Creates a new [`ChainHardforks`] from a list which **must be ordered** by activation.
    ///
    /// Equivalent Ethereum hardforks **must be included** as well.
    pub fn new(forks: Vec<(Box<dyn Hardfork>, ForkCondition)>) -> Self {
        let map = forks.iter().map(|(fork, condition)| (fork.name(), *condition)).collect();

        Self { forks, map }
    }

    /// Total number of hardforks.
    pub fn len(&self) -> usize {
        self.forks.len()
    }

    /// Checks if the fork list is empty.
    pub fn is_empty(&self) -> bool {
        self.forks.is_empty()
    }

    /// Retrieves [`ForkCondition`] from `fork`. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    pub fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.get(fork).unwrap_or_default()
    }

    /// Retrieves [`ForkCondition`] from `fork` if it exists, otherwise `None`.
    pub fn get<H: Hardfork>(&self, fork: H) -> Option<ForkCondition> {
        self.map.get(fork.name()).copied()
    }

    /// Retrieves the fork block number or timestamp from `fork` if it exists, otherwise `None`.
    pub fn fork_block<H: Hardfork>(&self, fork: H) -> Option<u64> {
        match self.fork(fork) {
            ForkCondition::Block(block) => Some(block),
            ForkCondition::TTD { fork_block, .. } => fork_block,
            ForkCondition::Timestamp(ts) => Some(ts),
            ForkCondition::Never => None,
        }
    }

    /// Get an iterator of all hardforks with their respective activation conditions.
    pub fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.forks.iter().map(|(f, b)| (&**f, *b))
    }

    /// Get last hardfork from the list.
    pub fn last(&self) -> Option<(Box<dyn Hardfork>, ForkCondition)> {
        self.forks.last().map(|(f, b)| (f.clone(), *b))
    }

    /// Convenience method to check if a fork is active at a given timestamp.
    pub fn is_fork_active_at_timestamp<H: Hardfork>(&self, fork: H, timestamp: u64) -> bool {
        self.fork(fork).active_at_timestamp(timestamp)
    }

    /// Convenience method to check if a fork is active at a given block number.
    pub fn is_fork_active_at_block<H: Hardfork>(&self, fork: H, block_number: u64) -> bool {
        self.fork(fork).active_at_block(block_number)
    }

    /// Inserts a fork with the given [`ForkCondition`], maintaining forks in ascending order
    /// based on the `Ord` implementation of [`ForkCondition`].
    ///
    /// If the fork already exists (regardless of its current condition type), it will be removed
    /// and re-inserted at the appropriate position based on the new condition.
    ///
    /// # Ordering Behavior
    ///
    /// Forks are ordered according to [`ForkCondition`]'s `Ord` implementation:
    /// - [`ForkCondition::Never`] comes first
    /// - [`ForkCondition::Block`] ordered by block number
    /// - [`ForkCondition::Timestamp`] ordered by timestamp value
    /// - [`ForkCondition::TTD`] ordered by total difficulty
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut forks = ChainHardforks::default();
    /// forks.insert(Fork::Frontier, ForkCondition::Block(0));
    /// forks.insert(Fork::Homestead, ForkCondition::Block(1_150_000));
    /// forks.insert(Fork::Cancun, ForkCondition::Timestamp(1710338135));
    ///
    /// // Forks are ordered: Frontier (Block 0), Homestead (Block 1150000), Cancun (Timestamp)
    /// ```
    pub fn insert<H: Hardfork>(&mut self, fork: H, condition: ForkCondition) {
        // Remove existing fork if it exists
        self.remove(&fork);

        // Find the correct position based on ForkCondition's Ord implementation
        let pos = self
            .forks
            .iter()
            .position(|(_, existing_condition)| *existing_condition > condition)
            .unwrap_or(self.forks.len());

        self.map.insert(fork.name(), condition);
        self.forks.insert(pos, (Box::new(fork), condition));
    }

    /// Extends the list with multiple forks, updating existing entries with new
    /// [`ForkCondition`]s if they already exist.
    ///
    /// Each fork is inserted using [`Self::insert`], maintaining proper ordering based on
    /// [`ForkCondition`]'s `Ord` implementation.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut forks = ChainHardforks::default();
    /// forks.extend([
    ///     (Fork::Homestead, ForkCondition::Block(1_150_000)),
    ///     (Fork::Frontier, ForkCondition::Block(0)),
    ///     (Fork::Cancun, ForkCondition::Timestamp(1710338135)),
    /// ]);
    ///
    /// // Forks will be automatically ordered: Frontier, Homestead, Cancun
    /// ```
    pub fn extend<H: Hardfork + Clone>(
        &mut self,
        forks: impl IntoIterator<Item = (H, ForkCondition)>,
    ) {
        for (fork, condition) in forks {
            self.insert(fork, condition);
        }
    }

    /// Removes `fork` from list.
    pub fn remove<H: Hardfork>(&mut self, fork: &H) {
        self.forks.retain(|(inner_fork, _)| inner_fork.name() != fork.name());
        self.map.remove(fork.name());
    }
}

impl core::fmt::Debug for ChainHardforks {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ChainHardforks")
            .field("0", &self.forks_iter().map(|(hf, cond)| (hf.name(), cond)).collect::<Vec<_>>())
            .finish()
    }
}

impl<T: Hardfork, const N: usize> From<[(T, ForkCondition); N]> for ChainHardforks {
    fn from(list: [(T, ForkCondition); N]) -> Self {
        Self::new(
            list.into_iter()
                .map(|(fork, cond)| (Box::new(fork) as Box<dyn Hardfork>, cond))
                .collect(),
        )
    }
}
