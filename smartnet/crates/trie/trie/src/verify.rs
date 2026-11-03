use crate::{
    hashed_cursor::{HashedCursor, HashedCursorFactory},
    progress::{IntermediateStateRootState, StateRootProgress},
    trie::StateRoot,
    trie_cursor::{
        depth_first::{self, DepthFirstTrieIterator},
        noop::NoopTrieCursorFactory,
        TrieCursor, TrieCursorFactory,
    },
    Nibbles,
};
use alloy_primitives::B256;
use alloy_trie::BranchNodeCompact;
use smartnet_trie_common::StateRootError;
use smartnet_storage_errors::db::DatabaseError;
use std::cmp::{Ordering, Reverse};
use tracing::trace;

/// Used by [`StateRootBranchNodesIter`] to iterate over branch nodes in a state root.
#[derive(Debug)]
enum BranchNode {
    Account(Nibbles, BranchNodeCompact),
    Storage(B256, Nibbles, BranchNodeCompact),
}

/// Iterates over branch nodes produced by a [`StateRoot`]. The `StateRoot` will only used the
/// hashed accounts/storages tables, meaning it is recomputing the trie from scratch without the use
/// of the trie tables.
///
/// [`BranchNode`]s are iterated over such that:
/// * Account nodes and storage nodes may be interleaved.
/// * Storage nodes for the same account will be ordered by ascending path relative to each other.
/// * Account nodes will be ordered by ascending path relative to each other.
/// * All storage nodes for one account will finish before storage nodes for another account are
///   started. In other words, if the current storage account is not equal to the previous, the
///   previous has no more nodes.
#[derive(Debug)]
struct StateRootBranchNodesIter<H> {
    hashed_cursor_factory: H,
    account_nodes: Vec<(Nibbles, BranchNodeCompact)>,
    storage_tries: Vec<(B256, Vec<(Nibbles, BranchNodeCompact)>)>,
    curr_storage: Option<(B256, Vec<(Nibbles, BranchNodeCompact)>)>,
    intermediate_state: Option<Box<IntermediateStateRootState>>,
    complete: bool,
}

impl<H> StateRootBranchNodesIter<H> {
    fn new(hashed_cursor_factory: H) -> Self {
        Self {
            hashed_cursor_factory,
            account_nodes: Default::default(),
            storage_tries: Default::default(),
            curr_storage: None,
            intermediate_state: None,
            complete: false,
        }
    }

    /// Sorts a Vec of updates such that it is ready to be yielded from the `next` method. We yield
    /// by popping off of the account/storage vecs, so we sort them in reverse order.
    ///
    /// Depth-first sorting is used because this is the order that the `HashBuilder` computes
    /// branch nodes internally, even if it produces them as `B256Map`s.
    fn sort_updates(updates: &mut [(Nibbles, BranchNodeCompact)]) {
        updates.sort_unstable_by(|a, b| depth_first::cmp(&b.0, &a.0));
    }
}

impl<H: HashedCursorFactory + Clone> Iterator for StateRootBranchNodesIter<H> {
    type Item = Result<BranchNode, StateRootError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // If we already started iterating through a storage trie's updates, continue doing
            // so.
            if let Some((account, storage_updates)) = self.curr_storage.as_mut() &&
                let Some((path, node)) = storage_updates.pop()
            {
                let node = BranchNode::Storage(*account, path, node);
                return Some(Ok(node));
            }

            // If there's not a storage trie already being iterated over then check if there's a
            // storage trie we could start iterating over.
            if let Some((account, storage_updates)) = self.storage_tries.pop() {
                debug_assert!(!storage_updates.is_empty());

                self.curr_storage = Some((account, storage_updates));
                continue;
            }

            // `storage_updates` is empty, check if there are account updates.
            if let Some((path, node)) = self.account_nodes.pop() {
                return Some(Ok(BranchNode::Account(path, node)));
            }

            // All data from any previous runs of the `StateRoot` has been produced, run the next
            // partial computation, unless `StateRootProgress::Complete` has been returned in which
            // case iteration is over.
            if self.complete {
                return None;
            }

            let state_root =
                StateRoot::new(NoopTrieCursorFactory, self.hashed_cursor_factory.clone())
                    .with_intermediate_state(self.intermediate_state.take().map(|s| *s));

            let updates = match state_root.root_with_progress() {
                Err(err) => return Some(Err(err)),
                Ok(StateRootProgress::Complete(_, _, updates)) => {
                    self.complete = true;
                    updates
                }
                Ok(StateRootProgress::Progress(intermediate_state, _, updates)) => {
                    self.intermediate_state = Some(intermediate_state);
                    updates
                }
            };

            // collect account updates and sort them in descending order, so that when we pop them
            // off the Vec they are popped in ascending order.
            self.account_nodes.extend(updates.account_nodes);
            Self::sort_updates(&mut self.account_nodes);

            self.storage_tries = updates
                .storage_tries
                .into_iter()
                .filter_map(|(account, t)| {
                    (!t.storage_nodes.is_empty()).then(|| {
                        let mut storage_nodes = t.storage_nodes.into_iter().collect::<Vec<_>>();
                        Self::sort_updates(&mut storage_nodes);
                        (account, storage_nodes)
                    })
                })
                .collect::<Vec<_>>();

            // `root_with_progress` will output storage updates ordered by their account hash. If
            // `root_with_progress` only returns a partial result then it will pick up where
            // it left off in the storage trie on the next run.
            //
            // By sorting by the account we ensure that we continue with the partially processed
            // trie (the last of the previous run) first. We sort in reverse order because we pop
            // off of this Vec.
            self.storage_tries.sort_unstable_by_key(|a| Reverse(a.0));

            // loop back to the top.
        }
    }
}

/// Output describes an inconsistency found when comparing the hashed state tables
/// ([`HashedCursorFactory`]) with that of the trie tables ([`TrieCursorFactory`]). The hashed
/// tables are considered the source of truth; outputs are on the part of the trie tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// An extra account node was found.
    AccountExtra(Nibbles, BranchNodeCompact),
    /// An extra storage node was found.
    StorageExtra(B256, Nibbles, BranchNodeCompact),
    /// An account node had the wrong value.
    AccountWrong {
        /// Path of the node
        path: Nibbles,
        /// The node's expected value.
        expected: BranchNodeCompact,
        /// The node's found value.
        found: BranchNodeCompact,
    },
    /// A storage node had the wrong value.
    StorageWrong {
        /// The account the storage trie belongs to.
        account: B256,
        /// Path of the node
        path: Nibbles,
        /// The node's expected value.
        expected: BranchNodeCompact,
        /// The node's found value.
        found: BranchNodeCompact,
    },
    /// An account node was missing.
    AccountMissing(Nibbles, BranchNodeCompact),
    /// A storage node was missing.
    StorageMissing(B256, Nibbles, BranchNodeCompact),
    /// Progress indicator with the last seen account path.
    Progress(Nibbles),
}

/// Verifies the contents of a trie table against some other data source which is able to produce
/// stored trie nodes.
#[derive(Debug)]
struct SingleVerifier<I> {
    account: Option<B256>, // None for accounts trie
    trie_iter: I,
    curr: Option<(Nibbles, BranchNodeCompact)>,
}

impl<C: TrieCursor> SingleVerifier<DepthFirstTrieIterator<C>> {
    fn new(account: Option<B256>, trie_cursor: C) -> Result<Self, DatabaseError> {
        let mut trie_iter = DepthFirstTrieIterator::new(trie_cursor);
        let curr = trie_iter.next().transpose()?;
        Ok(Self { account, trie_iter, curr })
    }

    const fn output_extra(&self, path: Nibbles, node: BranchNodeCompact) -> Output {
        if let Some(account) = self.account {
            Output::StorageExtra(account, path, node)
        } else {
            Output::AccountExtra(path, node)
        }
    }

    const fn output_wrong(
        &self,
        path: Nibbles,
        expected: BranchNodeCompact,
        found: BranchNodeCompact,
    ) -> Output {
        if let Some(account) = self.account {
            Output::StorageWrong { account, path, expected, found }
        } else {
            Output::AccountWrong { path, expected, found }
        }
    }

    const fn output_missing(&self, path: Nibbles, node: BranchNodeCompact) -> Output {
        if let Some(account) = self.account {
            Output::StorageMissing(account, path, node)
        } else {
            Output::AccountMissing(path, node)
        }
    }

    /// Called with the next path and node in the canonical sequence of stored trie nodes. Will
    /// append to the given `outputs` Vec if walking the trie cursor produces data
    /// inconsistent with that given.
    ///
    /// `next` must be called with paths in depth-first order.
    fn next(
        &mut self,
        outputs: &mut Vec<Output>,
        path: Nibbles,
        node: BranchNodeCompact,
    ) -> Result<(), DatabaseError> {
        loop {
            // `curr` is None only if the end of the iterator has been reached. Any further nodes
            // found must be considered missing.
            if self.curr.is_none() {
                outputs.push(self.output_missing(path, node));
                return Ok(());
            }

            let (curr_path, curr_node) = self.curr.as_ref().expect("not None");
            trace!(target: "trie::verify", account=?self.account, ?curr_path, ?path, "Current cursor node");

            // Use depth-first ordering for comparison
            match depth_first::cmp(&path, curr_path) {
                Ordering::Less => {
                    // If the given path comes before the cursor's current path in depth-first
                    // order, then the given path was not produced by the cursor.
                    outputs.push(self.output_missing(path, node));
                    return Ok(());
                }
                Ordering::Equal => {
                    // If the current path matches the given one (happy path) but the nodes
                    // aren't equal then we produce a wrong node. Either way we want to move the
                    // iterator forward.
                    if *curr_node != node {
                        outputs.push(self.output_wrong(path, node, curr_node.clone()))
                    }
                    self.curr = self.trie_iter.next().transpose()?;
                    return Ok(());
                }
                Ordering::Greater => {
                    // If the given path comes after the current path in depth-first order,
                    // it means the cursor's path was not found by the caller (otherwise it would
                    // have hit the equal case) and so is extraneous.
                    outputs.push(self.output_extra(*curr_path, curr_node.clone()));
                    self.curr = self.trie_iter.next().transpose()?;
                    // back to the top of the loop to check the latest `self.curr` value against the
                    // given path/node.
                }
            }
        }
    }

    /// Must be called once there are no more calls to `next` to made. All further nodes produced
    /// by the iterator will be considered extraneous.
    fn finalize(&mut self, outputs: &mut Vec<Output>) -> Result<(), DatabaseError> {
        loop {
            if let Some((curr_path, curr_node)) = self.curr.take() {
                outputs.push(self.output_extra(curr_path, curr_node));
                self.curr = self.trie_iter.next().transpose()?;
            } else {
                return Ok(());
            }
        }
    }
}

/// Checks that data stored in the trie database is consistent, using hashed accounts/storages
/// database tables as the source of truth. This will iteratively recompute the entire trie based
/// on the hashed state, and produce any discovered [`Output`]s via the `next` method.
#[derive(Debug)]
pub struct Verifier<'a, T: TrieCursorFactory, H> {
    trie_cursor_factory: &'a T,
    hashed_cursor_factory: H,
    branch_node_iter: StateRootBranchNodesIter<H>,
    outputs: Vec<Output>,
    account: SingleVerifier<DepthFirstTrieIterator<T::AccountTrieCursor<'a>>>,
    storage: Option<(B256, SingleVerifier<DepthFirstTrieIterator<T::StorageTrieCursor<'a>>>)>,
    complete: bool,
}

impl<'a, T: TrieCursorFactory, H: HashedCursorFactory + Clone> Verifier<'a, T, H> {
    /// Creates a new verifier instance.
    pub fn new(
        trie_cursor_factory: &'a T,
        hashed_cursor_factory: H,
    ) -> Result<Self, DatabaseError> {
        Ok(Self {
            trie_cursor_factory,
            hashed_cursor_factory: hashed_cursor_factory.clone(),
            branch_node_iter: StateRootBranchNodesIter::new(hashed_cursor_factory),
            outputs: Default::default(),
            account: SingleVerifier::new(None, trie_cursor_factory.account_trie_cursor()?)?,
            storage: None,
            complete: false,
        })
    }
}

impl<'a, T: TrieCursorFactory, H: HashedCursorFactory + Clone> Verifier<'a, T, H> {
    fn new_storage(
        &mut self,
        account: B256,
        path: Nibbles,
        node: BranchNodeCompact,
    ) -> Result<(), DatabaseError> {
        let trie_cursor = self.trie_cursor_factory.storage_trie_cursor(account)?;
        let mut storage = SingleVerifier::new(Some(account), trie_cursor)?;
        storage.next(&mut self.outputs, path, node)?;
        self.storage = Some((account, storage));
        Ok(())
    }

    /// This method is called using the account hashes at the boundary of [`BranchNode::Storage`]
    /// sequences, ie once the [`StateRootBranchNodesIter`] has begun yielding storage nodes for a
    /// different account than it was yielding previously. All accounts between the two should have
    /// empty storages.
    fn verify_empty_storages(
        &mut self,
        last_account: B256,
        next_account: B256,
        start_inclusive: bool,
        end_inclusive: bool,
    ) -> Result<(), DatabaseError> {
        let mut account_cursor = self.hashed_cursor_factory.hashed_account_cursor()?;
        let mut account_seeked = false;

        if !start_inclusive {
            account_seeked = true;
            account_cursor.seek(last_account)?;
        }

        loop {
            let Some((curr_account, _)) = (if account_seeked {
                account_cursor.next()?
            } else {
                account_seeked = true;
                account_cursor.seek(last_account)?
            }) else {
                return Ok(());
            };

            if curr_account < next_account || (end_inclusive && curr_account == next_account) {
                trace!(target: "trie::verify", account = ?curr_account, "Verifying account has empty storage");

                let mut storage_cursor =
                    self.trie_cursor_factory.storage_trie_cursor(curr_account)?;
                let mut seeked = false;
                while let Some((path, node)) = if seeked {
                    storage_cursor.next()?
                } else {
                    seeked = true;
                    storage_cursor.seek(Nibbles::new())?
                } {
                    self.outputs.push(Output::StorageExtra(curr_account, path, node));
                }
            } else {
                return Ok(());
            }
        }
    }

    fn try_next(&mut self) -> Result<(), StateRootError> {
        match self.branch_node_iter.next().transpose()? {
            None => {
                self.account.finalize(&mut self.outputs)?;
                if let Some((prev_account, storage)) = self.storage.as_mut() {
                    storage.finalize(&mut self.outputs)?;

                    // If there was a previous storage account, and it is the final one, then we
                    // need to validate that all accounts coming after it have empty storages.
                    let prev_account = *prev_account;

                    // Calculate the max possible account address (all bits set).
                    let max_account = B256::from([0xFFu8; 32]);

                    self.verify_empty_storages(prev_account, max_account, false, true)?;
                }
                self.complete = true;
            }
            Some(BranchNode::Account(path, node)) => {
                trace!(target: "trie::verify", ?path, "Account node from state root");
                self.account.next(&mut self.outputs, path, node)?;
                // Push progress indicator
                if !path.is_empty() {
                    self.outputs.push(Output::Progress(path));
                }
            }
            Some(BranchNode::Storage(account, path, node)) => {
                trace!(target: "trie::verify", ?account, ?path, "Storage node from state root");
                match self.storage.as_mut() {
                    None => {
                        // First storage account - check for any empty storages before it
                        self.verify_empty_storages(B256::ZERO, account, true, false)?;
                        self.new_storage(account, path, node)?;
                    }
                    Some((prev_account, storage)) if *prev_account == account => {
                        storage.next(&mut self.outputs, path, node)?;
                    }
                    Some((prev_account, storage)) => {
                        storage.finalize(&mut self.outputs)?;
                        // Clear any storage entries between the previous account and the new one
                        let prev_account = *prev_account;
                        self.verify_empty_storages(prev_account, account, false, false)?;
                        self.new_storage(account, path, node)?;
                    }
                }
            }
        }

        // If any outputs were appended we want to reverse them, so they are popped off
        // in the same order they were appended.
        self.outputs.reverse();
        Ok(())
    }
}

impl<'a, T: TrieCursorFactory, H: HashedCursorFactory + Clone> Iterator for Verifier<'a, T, H> {
    type Item = Result<Output, StateRootError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(output) = self.outputs.pop() {
                return Some(Ok(output));
            }

            if self.complete {
                return None;
            }

            if let Err(err) = self.try_next() {
                return Some(Err(err));
            }
        }
    }
}

