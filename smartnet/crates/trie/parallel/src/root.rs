#[cfg(feature = "metrics")]
use crate::metrics::ParallelStateRootMetrics;
use crate::{stats::ParallelTrieTracker, storage_root_targets::StorageRootTargets};
use alloy_primitives::B256;
use alloy_rlp::{BufMut, Encodable};
use itertools::Itertools;
use smartnet_trie::{SparseTrieError, StateProofError, StorageRootError};
use smartnet_trie::{
    hashed_cursor::HashedCursorFactory,
    node_iter::{TrieElement, TrieNodeIter},
    prefix_set::TriePrefixSets,
    trie_cursor::TrieCursorFactory,
    updates::TrieUpdates,
    walker::TrieWalker,
    HashBuilder, Nibbles, StorageRoot, TRIE_ACCOUNT_RLP_MAX_SIZE,
};
use smartnet_storage_api::DatabaseProviderROFactory;
use smartnet_storage_errors::ProviderError;
use smartnet_storage_errors::db::DatabaseError;
use smartnet_tasks::Runtime;
use std::{collections::HashMap, sync::mpsc};
use thiserror::Error;
use tracing::*;

/// Parallel incremental state root calculator.
///
/// The calculator starts off by launching tasks to compute storage roots.
/// Then, it immediately starts walking the state trie updating the necessary trie
/// nodes in the process. Upon encountering a leaf node, it will poll the storage root
/// task for the corresponding hashed address.
///
/// Note: This implementation only serves as a fallback for the sparse trie-based
/// state root calculation. The sparse trie approach is more efficient as it avoids traversing
/// the entire trie, only operating on the modified parts.
#[derive(Debug)]
pub struct ParallelStateRoot<Factory> {
    /// Factory for creating state providers.
    factory: Factory,
    // Prefix sets indicating which portions of the trie need to be recomputed.
    prefix_sets: TriePrefixSets,
    /// The runtime handle for spawning blocking tasks.
    runtime: Runtime,
    /// Parallel state root metrics.
    #[cfg(feature = "metrics")]
    metrics: ParallelStateRootMetrics,
}

impl<Factory> ParallelStateRoot<Factory> {
    /// Create new parallel state root calculator.
    pub fn new(factory: Factory, prefix_sets: TriePrefixSets, runtime: Runtime) -> Self {
        Self {
            factory,
            prefix_sets,
            runtime,
            #[cfg(feature = "metrics")]
            metrics: ParallelStateRootMetrics::default(),
        }
    }
}

impl<Factory> ParallelStateRoot<Factory>
where
    Factory: DatabaseProviderROFactory<Provider: TrieCursorFactory + HashedCursorFactory>
        + Clone
        + Send
        + 'static,
{
    /// Calculate incremental state root in parallel.
    pub fn incremental_root(self) -> Result<B256, ParallelStateRootError> {
        self.calculate(false).map(|(root, _)| root)
    }

    /// Calculate incremental state root with updates in parallel.
    pub fn incremental_root_with_updates(
        self,
    ) -> Result<(B256, TrieUpdates), ParallelStateRootError> {
        self.calculate(true)
    }

    /// Computes the state root by calculating storage roots in parallel for modified accounts,
    /// then walking the state trie to build the final state root hash.
    fn calculate(
        self,
        retain_updates: bool,
    ) -> Result<(B256, TrieUpdates), ParallelStateRootError> {
        let mut tracker = ParallelTrieTracker::default();
        let storage_root_targets = StorageRootTargets::new(
            self.prefix_sets
                .account_prefix_set
                .iter()
                .map(|nibbles| B256::from_slice(&nibbles.pack())),
            self.prefix_sets.storage_prefix_sets,
        );

        // Pre-calculate storage roots in parallel for accounts which were changed.
        tracker.set_precomputed_storage_roots(storage_root_targets.len() as u64);
        debug!(target: "trie::parallel_state_root", len = storage_root_targets.len(), "pre-calculating storage roots");
        let mut storage_roots = HashMap::with_capacity(storage_root_targets.len());

        let handle = self.runtime.handle().clone();

        for (hashed_address, prefix_set) in
            storage_root_targets.into_iter().sorted_unstable_by_key(|(address, _)| *address)
        {
            let factory = self.factory.clone();
            #[cfg(feature = "metrics")]
            let metrics = self.metrics.storage_trie.clone();

            let (tx, rx) = mpsc::sync_channel(1);

            // Spawn a blocking task to calculate account's storage root from database I/O
            drop(handle.spawn_blocking(move || {
                let result = (|| -> Result<_, ParallelStateRootError> {
                    let provider = factory.database_provider_ro()?;
                    Ok(StorageRoot::new_hashed(
                        &provider,
                        &provider,
                        hashed_address,
                        prefix_set,
                        #[cfg(feature = "metrics")]
                        metrics,
                    )
                    .calculate(retain_updates)?)
                })();
                let _ = tx.send(result);
            }));
            storage_roots.insert(hashed_address, rx);
        }

        trace!(target: "trie::parallel_state_root", "calculating state root");
        let mut trie_updates = TrieUpdates::default();

        let provider = self.factory.database_provider_ro()?;

        let walker = TrieWalker::<_>::state_trie(
            provider.account_trie_cursor().map_err(ProviderError::Database)?,
            self.prefix_sets.account_prefix_set,
        )
        .with_deletions_retained(retain_updates);
        let mut account_node_iter = TrieNodeIter::state_trie(
            walker,
            provider.hashed_account_cursor().map_err(ProviderError::Database)?,
        );

        let mut hash_builder = HashBuilder::default().with_updates(retain_updates);
        let mut account_rlp = Vec::with_capacity(TRIE_ACCOUNT_RLP_MAX_SIZE);
        while let Some(node) = account_node_iter.try_next().map_err(ProviderError::Database)? {
            match node {
                TrieElement::Branch(node) => {
                    hash_builder.add_branch(node.key, node.value, node.children_are_in_trie);
                }
                TrieElement::Leaf(hashed_address, account) => {
                    let storage_root_result = match storage_roots.remove(&hashed_address) {
                        Some(rx) => rx.recv().map_err(|_| {
                            ParallelStateRootError::StorageRoot(StorageRootError::Database(
                                DatabaseError::Other(format!(
                                    "channel closed for {hashed_address}"
                                )),
                            ))
                        })??,
                        // Since we do not store all intermediate nodes in the database, there might
                        // be a possibility of re-adding a non-modified leaf to the hash builder.
                        None => {
                            tracker.inc_missed_leaves();
                            StorageRoot::new_hashed(
                                &provider,
                                &provider,
                                hashed_address,
                                Default::default(),
                                #[cfg(feature = "metrics")]
                                self.metrics.storage_trie.clone(),
                            )
                            .calculate(retain_updates)?
                        }
                    };

                    let (storage_root, _, updates) = match storage_root_result {
                        smartnet_trie::StorageRootProgress::Complete(root, _, updates) => (root, (), updates),
                        smartnet_trie::StorageRootProgress::Progress(..) => {
                            return Err(ParallelStateRootError::StorageRoot(
                                StorageRootError::Database(DatabaseError::Other(
                                    "StorageRoot returned Progress variant in parallel trie calculation".to_string()
                                ))
                            ))
                        }
                    };

                    if retain_updates {
                        trie_updates.insert_storage_updates(hashed_address, updates);
                    }

                    account_rlp.clear();
                    let account = account.into_trie_account(storage_root);
                    account.encode(&mut account_rlp as &mut dyn BufMut);
                    hash_builder.add_leaf(Nibbles::unpack(hashed_address), &account_rlp);
                }
            }
        }

        let root = hash_builder.root();

        let removed_keys = account_node_iter.walker.take_removed_keys();
        trie_updates.finalize(hash_builder, removed_keys, self.prefix_sets.destroyed_accounts);

        let stats = tracker.finish();

        #[cfg(feature = "metrics")]
        self.metrics.record_state_trie(stats);

        trace!(
            target: "trie::parallel_state_root",
            %root,
            duration = ?stats.duration(),
            branches_added = stats.branches_added(),
            leaves_added = stats.leaves_added(),
            missed_leaves = stats.missed_leaves(),
            precomputed_storage_roots = stats.precomputed_storage_roots(),
            "Calculated state root"
        );

        Ok((root, trie_updates))
    }
}

/// Error during parallel state root calculation.
#[derive(Error, Debug)]
pub enum ParallelStateRootError {
    /// Error while calculating storage root.
    #[error(transparent)]
    StorageRoot(#[from] StorageRootError),
    /// Provider error.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Sparse trie error.
    #[error(transparent)]
    SparseTrie(#[from] SparseTrieError),
    /// Other unspecified error.
    #[error("{_0}")]
    Other(String),
}

impl From<ParallelStateRootError> for ProviderError {
    fn from(error: ParallelStateRootError) -> Self {
        match error {
            ParallelStateRootError::Provider(error) => error,
            ParallelStateRootError::StorageRoot(StorageRootError::Database(error)) => {
                Self::Database(error)
            }
            ParallelStateRootError::SparseTrie(error) => Self::other(error),
            ParallelStateRootError::Other(other) => Self::Database(DatabaseError::Other(other)),
        }
    }
}

impl From<alloy_rlp::Error> for ParallelStateRootError {
    fn from(error: alloy_rlp::Error) -> Self {
        Self::Provider(ProviderError::Rlp(error))
    }
}

impl From<StateProofError> for ParallelStateRootError {
    fn from(error: StateProofError) -> Self {
        match error {
            StateProofError::Database(err) => Self::Provider(ProviderError::Database(err)),
            StateProofError::Rlp(err) => Self::Provider(ProviderError::Rlp(err)),
            StateProofError::TrieInconsistency(msg) => {
                Self::Provider(ProviderError::TrieWitnessError(msg))
            }
        }
    }
}

