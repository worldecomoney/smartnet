use crate::{
    error::{Eip4844PoolTransactionError, InvalidPoolTransactionError},
    identifier::{SenderId, TransactionId},
    pool::pending::PendingTransaction,
    PoolTransaction, Priority, TransactionOrdering, ValidPoolTransaction,
};
use alloy_consensus::Transaction;
use alloy_primitives::map::AddressSet;
use core::fmt;
use imbl::OrdMap;
use smartnet_primitives_traits::transaction::error::InvalidTransactionError;
use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    sync::Arc,
};
use tokio::sync::broadcast::{error::TryRecvError, Receiver};
use tracing::debug;

const MAX_NEW_TRANSACTIONS_PER_BATCH: usize = 16;

/// An iterator that returns transactions that can be executed on the current state (*best*
/// transactions).
///
/// This is a wrapper around [`BestTransactions`] that also enforces a specific basefee.
///
/// This iterator guarantees that all transactions it returns satisfy both the base fee and blob
/// fee!
pub(crate) struct BestTransactionsWithFees<T: TransactionOrdering> {
    pub(crate) best: BestTransactions<T>,
    pub(crate) base_fee: u64,
    pub(crate) base_fee_per_blob_gas: u64,
}

impl<T: TransactionOrdering> crate::traits::BestTransactions for BestTransactionsWithFees<T> {
    fn mark_invalid(&mut self, tx: &Self::Item, kind: &InvalidPoolTransactionError) {
        BestTransactions::mark_invalid(&mut self.best, tx, kind)
    }

    fn no_updates(&mut self) {
        self.best.no_updates()
    }

    fn skip_blobs(&mut self) {
        self.set_skip_blobs(true)
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.best.set_skip_blobs(skip_blobs)
    }
}

impl<T: TransactionOrdering> Iterator for BestTransactionsWithFees<T> {
    type Item = Arc<ValidPoolTransaction<T::Transaction>>;

    fn next(&mut self) -> Option<Self::Item> {
        // find the next transaction that satisfies the base fee
        loop {
            let best = Iterator::next(&mut self.best)?;
            // If both the base fee and blob fee (if applicable for EIP-4844) are satisfied, return
            // the transaction
            if best.transaction.max_fee_per_gas() >= self.base_fee as u128 &&
                best.transaction
                    .max_fee_per_blob_gas()
                    .is_none_or(|fee| fee >= self.base_fee_per_blob_gas as u128)
            {
                return Some(best);
            }
            crate::traits::BestTransactions::mark_invalid(
                self,
                &best,
                &InvalidPoolTransactionError::Underpriced,
            );
        }
    }
}

/// An iterator that returns transactions that can be executed on the current state (*best*
/// transactions).
///
/// The [`PendingPool`](crate::pool::pending::PendingPool) contains transactions that *could* all
/// be executed on the current state, but only yields transactions that are ready to be executed
/// now. While it contains all gapless transactions of a sender, it _always_ only returns the
/// transaction with the current on chain nonce.
#[derive(Debug)]
pub struct BestTransactions<T: TransactionOrdering> {
    /// Contains a copy of _all_ transactions of the pending pool at the point in time this
    /// iterator was created.
    pub(crate) all: OrdMap<TransactionId, PendingTransaction<T>>,
    /// Transactions that can be executed right away: these have the expected nonce.
    ///
    /// Once an `independent` transaction with the nonce `N` is returned, it unlocks `N+1`, which
    /// then can be moved from the `all` set to the `independent` set.
    pub(crate) independent: BTreeSet<PendingTransaction<T>>,
    /// There might be the case where a yielded transactions is invalid, this will track it.
    pub(crate) invalid: HashSet<SenderId>,
    /// Used to receive any new pending transactions that have been added to the pool after this
    /// iterator was static filtered
    ///
    /// These new pending transactions are inserted into this iterator's pool before yielding the
    /// next value
    pub(crate) new_transaction_receiver: Option<Receiver<PendingTransaction<T>>>,
    /// The priority value of most recently yielded transaction.
    ///
    /// This is required if new pending transactions are fed in while it yields new values.
    pub(crate) last_priority: Option<Priority<T::PriorityValue>>,
    /// Flag to control whether to skip blob transactions (EIP4844).
    pub(crate) skip_blobs: bool,
}

impl<T: TransactionOrdering> BestTransactions<T> {
    /// Mark the transaction and its descendants as invalid.
    pub(crate) fn mark_invalid(
        &mut self,
        tx: &Arc<ValidPoolTransaction<T::Transaction>>,
        _kind: &InvalidPoolTransactionError,
    ) {
        self.invalid.insert(tx.sender_id());
    }

    /// Returns the ancestor the given transaction, the transaction with `nonce - 1`.
    ///
    /// Note: for a transaction with nonce higher than the current on chain nonce this will always
    /// return an ancestor since all transactions in this pool are gapless.
    pub(crate) fn ancestor(&self, id: &TransactionId) -> Option<&PendingTransaction<T>> {
        self.all.get(&id.unchecked_ancestor()?)
    }

    /// Non-blocking read on the new pending transactions subscription channel
    fn try_recv(&mut self) -> Option<IncomingTransaction<T>> {
        loop {
            match self.new_transaction_receiver.as_mut()?.try_recv() {
                Ok(tx) => {
                    if let Some(last_priority) = &self.last_priority &&
                        &tx.priority > last_priority
                    {
                        // we skip transactions if we already yielded a transaction with lower
                        // priority
                        return Some(IncomingTransaction::Stash(tx));
                    }
                    return Some(IncomingTransaction::Process(tx));
                }
                // note TryRecvError::Lagged can be returned here, which is an error that attempts
                // to correct itself on consecutive try_recv() attempts

                // the cost of ignoring this error is allowing old transactions to get
                // overwritten after the chan buffer size is met
                Err(TryRecvError::Lagged(_)) => {
                    // Handle the case where the receiver lagged too far behind.
                    // `num_skipped` indicates the number of messages that were skipped.
                }

                // this case is still better than the existing iterator behavior where no new
                // pending txs are surfaced to consumers
                Err(_) => return None,
            }
        }
    }

    /// Removes the currently best independent transaction from the independent set and the total
    /// set.
    fn pop_best(&mut self) -> Option<PendingTransaction<T>> {
        self.independent.pop_last().inspect(|best| {
            self.all.remove(best.transaction.id());
        })
    }

    /// Checks for new transactions that have come into the `PendingPool` after this iterator was
    /// created and inserts them
    fn add_new_transactions(&mut self) {
        for _ in 0..MAX_NEW_TRANSACTIONS_PER_BATCH {
            if let Some(pending_tx) = self.try_recv() {
                //  same logic as PendingPool::add_transaction/PendingPool::best_with_unlocked

                match pending_tx {
                    IncomingTransaction::Process(tx) => {
                        let tx_id = *tx.transaction.id();
                        if self.ancestor(&tx_id).is_none() {
                            self.independent.insert(tx.clone());
                        }
                        self.all.insert(tx_id, tx);
                    }
                    IncomingTransaction::Stash(tx) => {
                        let tx_id = *tx.transaction.id();
                        self.all.insert(tx_id, tx);
                    }
                }
            } else {
                break;
            }
        }
    }

    /// Returns the next best transaction and its priority value.
    #[expect(clippy::type_complexity)]
    pub fn next_tx_and_priority(
        &mut self,
    ) -> Option<(Arc<ValidPoolTransaction<T::Transaction>>, Priority<T::PriorityValue>)> {
        loop {
            self.add_new_transactions();
            // Remove the next independent tx with the highest priority
            let best = self.pop_best()?;
            let sender_id = best.transaction.sender_id();

            // skip transactions for which sender was marked as invalid
            if self.invalid.contains(&sender_id) {
                debug!(
                    target: "txpool",
                    "[{:?}] skipping invalid transaction",
                    best.transaction.hash()
                );
                continue;
            }

            // Insert transactions that just got unlocked.
            if let Some(unlocked) = self.all.get(&best.unlocks()) {
                self.independent.insert(unlocked.clone());
            }

            if self.skip_blobs && best.transaction.is_eip4844() {
                // blobs should be skipped, marking them as invalid will ensure that no dependent
                // transactions are returned
                self.mark_invalid(
                    &best.transaction,
                    &InvalidPoolTransactionError::Eip4844(
                        Eip4844PoolTransactionError::NoEip4844Blobs,
                    ),
                )
            } else {
                if self.new_transaction_receiver.is_some() {
                    self.last_priority = Some(best.priority.clone())
                }
                return Some((best.transaction, best.priority));
            }
        }
    }
}

/// Result of attempting to receive a new transaction from the channel during iteration.
///
/// This enum determines how a newly received transaction should be handled based on its priority
/// relative to transactions already yielded by the iterator.
enum IncomingTransaction<T: TransactionOrdering> {
    /// Process the transaction normally: add to both `all` map and potentially to `independent`
    /// set (if it has no ancestor).
    ///
    /// This variant is used when the transaction's priority is lower than or equal to the last
    /// yielded transaction, meaning it can be safely processed without breaking the descending
    /// priority order.
    Process(PendingTransaction<T>),

    /// Stash the transaction: add only to the `all` map, but NOT to the `independent` set.
    ///
    /// This variant is used when the transaction has a higher priority than the last yielded
    /// transaction. We cannot yield it immediately (to maintain strict priority ordering), but we
    /// must still track it so that:
    /// - Its descendants can find it via `ancestor()` lookups
    /// - We prevent those descendants from being incorrectly promoted to `independent`
    ///
    /// Without stashing, if a child of this transaction arrives later, it would fail to find its
    /// parent in `all`, be marked as `independent`, and be yielded out of order (before its
    /// parent), causing nonce gaps.
    Stash(PendingTransaction<T>),
}

impl<T: TransactionOrdering> crate::traits::BestTransactions for BestTransactions<T> {
    fn mark_invalid(&mut self, tx: &Self::Item, kind: &InvalidPoolTransactionError) {
        Self::mark_invalid(self, tx, kind)
    }

    fn no_updates(&mut self) {
        self.new_transaction_receiver.take();
        self.last_priority.take();
    }

    fn skip_blobs(&mut self) {
        self.set_skip_blobs(true);
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.skip_blobs = skip_blobs;
    }
}

impl<T: TransactionOrdering> Iterator for BestTransactions<T> {
    type Item = Arc<ValidPoolTransaction<T::Transaction>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_tx_and_priority().map(|(tx, _)| tx)
    }
}

/// A [`BestTransactions`](crate::traits::BestTransactions) implementation that filters the
/// transactions of iter with predicate.
///
/// Filter out transactions are marked as invalid:
/// [`BestTransactions::mark_invalid`](crate::traits::BestTransactions::mark_invalid).
pub struct BestTransactionFilter<I, P> {
    pub(crate) best: I,
    pub(crate) predicate: P,
}

impl<I, P> BestTransactionFilter<I, P> {
    /// Create a new [`BestTransactionFilter`] with the given predicate.
    pub const fn new(best: I, predicate: P) -> Self {
        Self { best, predicate }
    }
}

impl<I, P> Iterator for BestTransactionFilter<I, P>
where
    I: crate::traits::BestTransactions,
    P: FnMut(&<I as Iterator>::Item) -> bool,
{
    type Item = <I as Iterator>::Item;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let best = self.best.next()?;
            if (self.predicate)(&best) {
                return Some(best);
            }
            self.best.mark_invalid(
                &best,
                &InvalidPoolTransactionError::Consensus(
                    InvalidTransactionError::TxTypeNotSupported,
                ),
            );
        }
    }
}

impl<I, P> crate::traits::BestTransactions for BestTransactionFilter<I, P>
where
    I: crate::traits::BestTransactions,
    P: FnMut(&<I as Iterator>::Item) -> bool + Send,
{
    fn mark_invalid(&mut self, tx: &Self::Item, kind: &InvalidPoolTransactionError) {
        crate::traits::BestTransactions::mark_invalid(&mut self.best, tx, kind)
    }

    fn no_updates(&mut self) {
        self.best.no_updates()
    }

    fn skip_blobs(&mut self) {
        self.set_skip_blobs(true)
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.best.set_skip_blobs(skip_blobs)
    }
}

impl<I: fmt::Debug, P> fmt::Debug for BestTransactionFilter<I, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BestTransactionFilter").field("best", &self.best).finish()
    }
}

/// Wrapper over [`crate::traits::BestTransactions`] that prioritizes transactions of certain
/// senders capping total gas used by such transactions.
#[derive(Debug)]
pub struct BestTransactionsWithPrioritizedSenders<I: Iterator> {
    /// Inner iterator
    inner: I,
    /// A set of senders which transactions should be prioritized
    prioritized_senders: AddressSet,
    /// Maximum total gas limit of prioritized transactions
    max_prioritized_gas: u64,
    /// Buffer with transactions that are not being prioritized. Those will be the first to be
    /// included after the prioritized transactions
    buffer: VecDeque<I::Item>,
    /// Tracker of total gas limit of prioritized transactions. Once it reaches
    /// `max_prioritized_gas` no more transactions will be prioritized
    prioritized_gas: u64,
}

impl<I: Iterator> BestTransactionsWithPrioritizedSenders<I> {
    /// Constructs a new [`BestTransactionsWithPrioritizedSenders`].
    pub fn new(prioritized_senders: AddressSet, max_prioritized_gas: u64, inner: I) -> Self {
        Self {
            inner,
            prioritized_senders,
            max_prioritized_gas,
            buffer: Default::default(),
            prioritized_gas: Default::default(),
        }
    }
}

impl<I, T> Iterator for BestTransactionsWithPrioritizedSenders<I>
where
    I: crate::traits::BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    T: PoolTransaction,
{
    type Item = <I as Iterator>::Item;

    fn next(&mut self) -> Option<Self::Item> {
        // If we have space, try prioritizing transactions
        if self.prioritized_gas < self.max_prioritized_gas {
            for item in &mut self.inner {
                if self.prioritized_senders.contains(&item.transaction.sender()) &&
                    self.prioritized_gas + item.transaction.gas_limit() <=
                        self.max_prioritized_gas
                {
                    self.prioritized_gas += item.transaction.gas_limit();
                    return Some(item);
                }
                self.buffer.push_back(item);
            }
        }

        if let Some(item) = self.buffer.pop_front() {
            Some(item)
        } else {
            self.inner.next()
        }
    }
}

impl<I, T> crate::traits::BestTransactions for BestTransactionsWithPrioritizedSenders<I>
where
    I: crate::traits::BestTransactions<Item = Arc<ValidPoolTransaction<T>>>,
    T: PoolTransaction,
{
    fn mark_invalid(&mut self, tx: &Self::Item, kind: &InvalidPoolTransactionError) {
        self.inner.mark_invalid(tx, kind)
    }

    fn no_updates(&mut self) {
        self.inner.no_updates()
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        if skip_blobs {
            self.buffer.retain(|tx| !tx.transaction.is_eip4844())
        }
        self.inner.set_skip_blobs(skip_blobs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pool::pending::PendingPool,
        test_utils::{MockOrdering, MockTransaction, MockTransactionFactory},
        BestTransactions,
    };

    #[test]
    fn test_best_transactions_with_skipping_blobs() {
        // Tests the skip_blobs functionality to ensure blob transactions are skipped.
        let mut pool = PendingPool::new(MockOrdering::default());
        let mut f = MockTransactionFactory::default();

        // Add a blob transaction
        let tx_blob = MockTransaction::eip4844().rng_hash().with_nonce(0).with_blob_fee(100);
        let valid_blob_tx = f.validated(tx_blob);
        pool.add_transaction(Arc::new(valid_blob_tx), 0);

        // Add a non-blob transaction
        let tx_non_blob = MockTransaction::eip1559().rng_hash().with_nonce(1).with_max_fee(200);
        let valid_non_blob_tx = f.validated(tx_non_blob.clone());
        pool.add_transaction(Arc::new(valid_non_blob_tx), 0);

        let mut best = pool.best();
        best.skip_blobs();

        // Only the non-blob transaction should be returned
        let tx = best.next().expect("Transaction should be returned");
        assert_eq!(tx.transaction, tx_non_blob);

        // Ensure no more transactions are left
        assert!(best.next().is_none());
    }
}
