use crate::{
    identifier::{SenderId, TransactionId},
    pool::size::SizeTracker,
    PoolTransaction, SubPoolLimit, ValidPoolTransaction, TXPOOL_SCAN_INLINE_CAP,
};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use std::{
    cmp::Ordering,
    collections::{hash_map::Entry, BTreeMap, BTreeSet},
    ops::{Bound::Unbounded, Deref},
    sync::Arc,
};

/// A pool of transactions that are currently parked and are waiting for external changes (e.g.
/// basefee, ancestor transactions, balance) that eventually move the transaction into the pending
/// pool.
///
/// Note: This type is generic over [`ParkedPool`] which enforces that the underlying transaction
/// type is [`ValidPoolTransaction`] wrapped in an [Arc].
#[derive(Debug, Clone)]
pub struct ParkedPool<T: ParkedOrd> {
    /// Keeps track of transactions inserted in the pool.
    ///
    /// This way we can determine when transactions were submitted to the pool.
    submission_id: u64,
    /// _All_ Transactions that are currently inside the pool grouped by their identifier.
    by_id: BTreeMap<TransactionId, ParkedPoolTransaction<T>>,
    /// Keeps track of last submission id for each sender.
    ///
    /// This are sorted in reverse order, so the last (highest) submission id is first, and the
    /// lowest (oldest) is the last.
    last_sender_submission: BTreeSet<SubmissionSenderId>,
    /// Keeps track of the number of transactions in the pool by the sender and the last submission
    /// id.
    sender_transaction_count: FxHashMap<SenderId, SenderTransactionCount>,
    /// Keeps track of the size of this pool.
    ///
    /// See also [`smartnet_primitives_traits::InMemorySize::size`].
    size_of: SizeTracker,
}

// === impl ParkedPool ===

impl<T: ParkedOrd> ParkedPool<T> {
    /// Adds a new transactions to the pending queue.
    pub fn add_transaction(&mut self, tx: Arc<ValidPoolTransaction<T::Transaction>>) {
        let id = *tx.id();
        debug_assert!(
            !self.contains(&id),
            "transaction already included {:?}",
            self.get(&id).unwrap().transaction.transaction
        );
        let submission_id = self.next_id();

        // keep track of size
        self.size_of += tx.size();

        // update or create sender entry
        self.add_sender_count(tx.sender_id(), submission_id);
        let transaction = ParkedPoolTransaction { submission_id, transaction: tx.into() };

        self.by_id.insert(id, transaction);
    }

    /// Increments the count of transactions for the given sender and updates the tracked submission
    /// id.
    fn add_sender_count(&mut self, sender: SenderId, submission_id: u64) {
        match self.sender_transaction_count.entry(sender) {
            Entry::Occupied(mut entry) => {
                let value = entry.get_mut();
                // remove the __currently__ tracked submission id
                self.last_sender_submission
                    .remove(&SubmissionSenderId::new(sender, value.last_submission_id));

                value.count += 1;
                value.last_submission_id = submission_id;
            }
            Entry::Vacant(entry) => {
                entry
                    .insert(SenderTransactionCount { count: 1, last_submission_id: submission_id });
            }
        }
        // insert a new entry
        self.last_sender_submission.insert(SubmissionSenderId::new(sender, submission_id));
    }

    /// Decrements the count of transactions for the given sender.
    ///
    /// If the count reaches zero, the sender is removed from the map.
    ///
    /// Note: this does not update the tracked submission id for the sender, because we're only
    /// interested in the __last__ submission id when truncating the pool.
    fn remove_sender_count(&mut self, sender_id: SenderId) {
        let removed_sender = match self.sender_transaction_count.entry(sender_id) {
            Entry::Occupied(mut entry) => {
                let value = entry.get_mut();
                value.count -= 1;
                if value.count == 0 {
                    entry.remove()
                } else {
                    return;
                }
            }
            Entry::Vacant(_) => {
                // This should never happen because the bisection between the two maps
                unreachable!("sender count not found {:?}", sender_id);
            }
        };

        // all transactions for this sender have been removed
        assert!(
            self.last_sender_submission
                .remove(&SubmissionSenderId::new(sender_id, removed_sender.last_submission_id)),
            "last sender transaction not found {sender_id:?}"
        );
    }

    /// Returns an iterator over all transactions in the pool
    pub(crate) fn all(
        &self,
    ) -> impl ExactSizeIterator<Item = Arc<ValidPoolTransaction<T::Transaction>>> + '_ {
        self.by_id.values().map(|tx| tx.transaction.clone().into())
    }

    /// Removes the transaction from the pool
    pub(crate) fn remove_transaction(
        &mut self,
        id: &TransactionId,
    ) -> Option<Arc<ValidPoolTransaction<T::Transaction>>> {
        // remove from queues
        let tx = self.by_id.remove(id)?;
        self.remove_sender_count(tx.transaction.sender_id());

        // keep track of size
        self.size_of -= tx.transaction.size();

        Some(tx.transaction.into())
    }

    /// Retrieves transactions by sender, using `SmallVec` (stack batch of
    /// `TXPOOL_SCAN_INLINE_CAP` ids; spill to heap beyond that).
    pub(crate) fn get_txs_by_sender(
        &self,
        sender: SenderId,
    ) -> SmallVec<[TransactionId; TXPOOL_SCAN_INLINE_CAP]> {
        self.by_id
            .range((sender.start_bound(), Unbounded))
            .take_while(move |(other, _)| sender == other.sender)
            .map(|(tx_id, _)| *tx_id)
            .collect()
    }

    /// Returns all transactions for the given sender, using a `BTree` range query.
    pub(crate) fn txs_by_sender(
        &self,
        sender: SenderId,
    ) -> Vec<Arc<ValidPoolTransaction<T::Transaction>>> {
        self.by_id
            .range((sender.start_bound(), Unbounded))
            .take_while(move |(other, _)| sender == other.sender)
            .map(|(_, tx)| Arc::clone(&tx.transaction))
            .collect()
    }

    /// Truncates the pool by removing transactions, until the given [`SubPoolLimit`] has been met.
    ///
    /// This is done by first ordering senders by the last time they have submitted a transaction
    ///
    /// Uses sender ids sorted by each sender's last submission id. Senders with older last
    /// submission ids are first. Note that _last_ submission ids are the newest submission id for
    /// that sender, so this sorts senders by the last time they submitted a transaction in
    /// descending order. Senders that have least recently submitted a transaction are first.
    ///
    /// Then, for each sender, all transactions for that sender are removed, until the pool limits
    /// have been met.
    ///
    /// Any removed transactions are returned.
    pub fn truncate_pool(
        &mut self,
        limit: SubPoolLimit,
    ) -> Vec<Arc<ValidPoolTransaction<T::Transaction>>> {
        if !self.exceeds(&limit) {
            // if we are below the limits, we don't need to drop anything
            return Vec::new();
        }

        let mut removed = Vec::with_capacity(limit.tx_excess(self.len()).unwrap_or(1));

        while !self.last_sender_submission.is_empty() && limit.is_exceeded(self.len(), self.size())
        {
            // NOTE: This will not panic due to `!last_sender_transaction.is_empty()`
            let sender_id = self.last_sender_submission.last().unwrap().sender_id;

            // Drop transactions from this sender until the pool is under limits
            while let Some((tx_id, _)) = self.by_id.range(sender_id.range()).next_back() {
                let tx_id = *tx_id;
                if let Some(tx) = self.remove_transaction(&tx_id) {
                    removed.push(tx);
                }

                if !self.exceeds(&limit) {
                    break;
                }
            }
        }

        removed
    }

    const fn next_id(&mut self) -> u64 {
        let id = self.submission_id;
        self.submission_id = self.submission_id.wrapping_add(1);
        id
    }

    /// The reported size of all transactions in this pool.
    pub(crate) fn size(&self) -> usize {
        self.size_of.into()
    }

    /// Number of transactions in the entire pool
    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Returns true if the pool exceeds the given limit
    #[inline]
    pub(crate) fn exceeds(&self, limit: &SubPoolLimit) -> bool {
        limit.is_exceeded(self.len(), self.size())
    }

    /// Returns whether the pool is empty
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Returns `true` if the transaction with the given id is already included in this pool.
    pub(crate) fn contains(&self, id: &TransactionId) -> bool {
        self.by_id.contains_key(id)
    }

    /// Retrieves a transaction with the given ID from the pool, if it exists.
    fn get(&self, id: &TransactionId) -> Option<&ParkedPoolTransaction<T>> {
        self.by_id.get(id)
    }

    /// Asserts that all subpool invariants
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn assert_invariants(&self) {
        assert_eq!(
            self.last_sender_submission.len(),
            self.sender_transaction_count.len(),
            "last_sender_submission.len() != sender_transaction_count.len()"
        );
    }
}

impl<T: PoolTransaction> ParkedPool<BasefeeOrd<T>> {
    /// Returns all transactions that satisfy the given basefee.
    ///
    /// Note: this does _not_ remove the transactions
    pub(crate) fn satisfy_base_fee_transactions(
        &self,
        basefee: u64,
    ) -> Vec<Arc<ValidPoolTransaction<T>>> {
        let mut txs = Vec::new();
        self.satisfy_base_fee_ids(basefee as u128, |tx| {
            txs.push(tx.clone());
        });
        txs
    }

    /// Returns all transactions that satisfy the given basefee.
    fn satisfy_base_fee_ids<F>(&self, basefee: u128, mut tx_handler: F)
    where
        F: FnMut(&Arc<ValidPoolTransaction<T>>),
    {
        let mut iter = self.by_id.iter().peekable();

        while let Some((id, tx)) = iter.next() {
            if tx.transaction.max_fee_per_gas() < basefee {
                // still parked -> skip descendant transactions
                'this: while let Some((peek, _)) = iter.peek() {
                    if peek.sender != id.sender {
                        break 'this;
                    }
                    iter.next();
                }
            } else {
                tx_handler(&tx.transaction);
            }
        }
    }

    /// Removes all transactions from this subpool that can afford the given basefee,
    /// invoking the provided handler for each transaction as it is removed.
    ///
    /// This method enforces the basefee constraint by identifying transactions that now
    /// satisfy the basefee requirement (typically after a basefee decrease) and processing
    /// them via the provided transaction handler closure.
    ///
    /// Respects per-sender nonce ordering: if the lowest-nonce transaction for a sender
    /// still cannot afford the basefee, higher-nonce transactions from that sender are skipped.
    ///
    /// Note: the transactions are not returned in a particular order.
    pub(crate) fn enforce_basefee_with<F>(&mut self, basefee: u64, mut tx_handler: F)
    where
        F: FnMut(Arc<ValidPoolTransaction<T>>),
    {
        let mut to_remove = Vec::new();
        self.satisfy_base_fee_ids(basefee as u128, |tx| {
            to_remove.push(*tx.id());
        });

        for id in to_remove {
            if let Some(tx) = self.remove_transaction(&id) {
                tx_handler(tx);
            }
        }
    }
}

impl<T: ParkedOrd> Default for ParkedPool<T> {
    fn default() -> Self {
        Self {
            submission_id: 0,
            by_id: Default::default(),
            last_sender_submission: Default::default(),
            sender_transaction_count: Default::default(),
            size_of: Default::default(),
        }
    }
}

/// Keeps track of the number of transactions and the latest submission id for each sender.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SenderTransactionCount {
    count: u64,
    last_submission_id: u64,
}

/// Represents a transaction in this pool.
#[derive(Debug)]
struct ParkedPoolTransaction<T: ParkedOrd> {
    /// Identifier that tags when transaction was submitted in the pool.
    submission_id: u64,
    /// Actual transaction.
    transaction: T,
}

impl<T: ParkedOrd> Clone for ParkedPoolTransaction<T> {
    fn clone(&self) -> Self {
        Self { submission_id: self.submission_id, transaction: self.transaction.clone() }
    }
}

impl<T: ParkedOrd> Eq for ParkedPoolTransaction<T> {}

impl<T: ParkedOrd> PartialEq<Self> for ParkedPoolTransaction<T> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl<T: ParkedOrd> PartialOrd<Self> for ParkedPoolTransaction<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: ParkedOrd> Ord for ParkedPoolTransaction<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        // This compares by the transactions first, and only if two tx are equal this compares
        // the unique `submission_id`.
        // "better" transactions are Greater
        self.transaction
            .cmp(&other.transaction)
            .then_with(|| other.submission_id.cmp(&self.submission_id))
    }
}

/// Includes a [`SenderId`] and `submission_id`. This is used to sort senders by their last
/// submission id.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub(crate) struct SubmissionSenderId {
    /// The sender id
    pub(crate) sender_id: SenderId,
    /// The submission id
    pub(crate) submission_id: u64,
}

impl SubmissionSenderId {
    /// Creates a new [`SubmissionSenderId`] based on the [`SenderId`] and `submission_id`.
    const fn new(sender_id: SenderId, submission_id: u64) -> Self {
        Self { sender_id, submission_id }
    }
}

impl Ord for SubmissionSenderId {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering for `submission_id`
        other.submission_id.cmp(&self.submission_id)
    }
}

impl PartialOrd for SubmissionSenderId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Helper trait used for custom `Ord` wrappers around a transaction.
///
/// This is effectively a wrapper for `Arc<ValidPoolTransaction>` with custom `Ord` implementation.
pub trait ParkedOrd:
    Ord
    + Clone
    + From<Arc<ValidPoolTransaction<Self::Transaction>>>
    + Into<Arc<ValidPoolTransaction<Self::Transaction>>>
    + Deref<Target = Arc<ValidPoolTransaction<Self::Transaction>>>
{
    /// The wrapper transaction type.
    type Transaction: PoolTransaction;
}

/// Helper macro to implement necessary conversions for `ParkedOrd` trait
macro_rules! impl_ord_wrapper {
    ($name:ident) => {
        impl<T: PoolTransaction> Clone for $name<T> {
            fn clone(&self) -> Self {
                Self(self.0.clone())
            }
        }

        impl<T: PoolTransaction> Eq for $name<T> {}

        impl<T: PoolTransaction> PartialEq<Self> for $name<T> {
            fn eq(&self, other: &Self) -> bool {
                self.cmp(other) == Ordering::Equal
            }
        }

        impl<T: PoolTransaction> PartialOrd<Self> for $name<T> {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }
        impl<T: PoolTransaction> Deref for $name<T> {
            type Target = Arc<ValidPoolTransaction<T>>;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl<T: PoolTransaction> ParkedOrd for $name<T> {
            type Transaction = T;
        }

        impl<T: PoolTransaction> From<Arc<ValidPoolTransaction<T>>> for $name<T> {
            fn from(value: Arc<ValidPoolTransaction<T>>) -> Self {
                Self(value)
            }
        }

        impl<T: PoolTransaction> From<$name<T>> for Arc<ValidPoolTransaction<T>> {
            fn from(value: $name<T>) -> Arc<ValidPoolTransaction<T>> {
                value.0
            }
        }
    };
}

/// A new type wrapper for [`ValidPoolTransaction`]
///
/// This sorts transactions by their base fee.
///
/// Caution: This assumes all transaction in the `BaseFee` sub-pool have a fee value.
#[derive(Debug)]
pub struct BasefeeOrd<T: PoolTransaction>(Arc<ValidPoolTransaction<T>>);

impl_ord_wrapper!(BasefeeOrd);

impl<T: PoolTransaction> Ord for BasefeeOrd<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.transaction.max_fee_per_gas().cmp(&other.0.transaction.max_fee_per_gas())
    }
}

/// A new type wrapper for [`ValidPoolTransaction`]
///
/// This sorts transactions by their distance.
///
/// `Queued` transactions are transactions that are currently blocked by other parked (basefee,
/// queued) or missing transactions.
///
/// The primary order function always compares the transaction costs first. In case these
/// are equal, it compares the timestamps when the transactions were created.
#[derive(Debug)]
pub struct QueuedOrd<T: PoolTransaction>(Arc<ValidPoolTransaction<T>>);

impl_ord_wrapper!(QueuedOrd);

impl<T: PoolTransaction> Ord for QueuedOrd<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher fee is better
        self.max_fee_per_gas().cmp(&other.max_fee_per_gas()).then_with(||
            // Lower timestamp is better
            other.timestamp.cmp(&self.timestamp))
    }
}
