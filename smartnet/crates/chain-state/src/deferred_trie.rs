use alloy_primitives::B256;
use parking_lot::Mutex;
use smartnet_trie::{
    updates::{TrieUpdates, TrieUpdatesSorted},
    HashedPostState, HashedPostStateSorted, TrieInputSorted,
};
use smartnet_metrics::{metrics::Counter, Metrics};
use std::{
    fmt,
    sync::{Arc, LazyLock},
};
use tracing::{debug_span, instrument};

/// Shared handle to asynchronously populated trie data.
///
/// Uses a try-lock + fallback computation approach for deadlock-free access.
/// If the deferred task hasn't completed, computes trie data synchronously
/// from stored unsorted inputs rather than blocking.
#[derive(Clone)]
pub struct DeferredTrieData {
    /// Shared deferred state holding either raw inputs (pending) or computed result (ready).
    state: Arc<Mutex<DeferredState>>,
}

/// Sorted trie data computed for an executed block.
/// These represent the complete set of sorted trie data required to persist
/// block state for, and generate proofs on top of, a block.
#[derive(Clone, Debug, Default)]
pub struct ComputedTrieData {
    /// Sorted hashed post-state produced by execution.
    pub hashed_state: Arc<HashedPostStateSorted>,
    /// Sorted trie updates produced by state root computation.
    pub trie_updates: Arc<TrieUpdatesSorted>,
    /// Trie input bundled with its anchor hash, if available.
    pub anchored_trie_input: Option<AnchoredTrieInput>,
}

/// Trie input bundled with its anchor hash.
///
/// The `trie_input` contains the **cumulative** overlay of all in-memory ancestor blocks,
/// not just this block's changes. Child blocks reuse the parent's overlay in O(1) by
/// cloning the Arc-wrapped data.
///
/// The `anchor_hash` is metadata indicating which persisted base state this overlay
/// sits on top of. It is CRITICAL for overlay reuse decisions: an overlay built on top
/// of Anchor A cannot be reused for a block anchored to Anchor B, as it would result
/// in an incorrect state.
#[derive(Clone, Debug)]
pub struct AnchoredTrieInput {
    /// The persisted ancestor hash this trie input is anchored to.
    pub anchor_hash: B256,
    /// Cumulative trie input overlay from all in-memory ancestors.
    pub trie_input: Arc<TrieInputSorted>,
}

/// Metrics for deferred trie computation.
#[derive(Metrics)]
#[metrics(scope = "sync.block_validation")]
struct DeferredTrieMetrics {
    /// Number of times deferred trie data was ready (async task completed first).
    deferred_trie_async_ready: Counter,
    /// Number of times deferred trie data required synchronous computation (fallback path).
    deferred_trie_sync_fallback: Counter,
}

static DEFERRED_TRIE_METRICS: LazyLock<DeferredTrieMetrics> =
    LazyLock::new(DeferredTrieMetrics::default);

/// Internal state for deferred trie data.
enum DeferredState {
    /// Data is not yet available; raw inputs stored for fallback computation.
    /// Wrapped in `Option` to allow taking ownership during computation.
    Pending(Option<PendingInputs>),
    /// Data has been computed and is ready.
    Ready(ComputedTrieData),
}

/// Inputs kept while a deferred trie computation is pending.
#[derive(Clone, Debug)]
struct PendingInputs {
    /// Unsorted hashed post-state from execution.
    hashed_state: Arc<HashedPostState>,
    /// Unsorted trie updates from state root computation.
    trie_updates: Arc<TrieUpdates>,
    /// The persisted ancestor hash this trie input is anchored to.
    anchor_hash: B256,
    /// Deferred trie data from ancestor blocks for merging.
    ancestors: Vec<DeferredTrieData>,
}

impl fmt::Debug for DeferredTrieData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock();
        match &*state {
            DeferredState::Pending(_) => {
                f.debug_struct("DeferredTrieData").field("state", &"pending").finish()
            }
            DeferredState::Ready(_) => {
                f.debug_struct("DeferredTrieData").field("state", &"ready").finish()
            }
        }
    }
}

impl DeferredTrieData {
    /// Create a new pending handle with fallback inputs for synchronous computation.
    ///
    /// If the async task hasn't completed when `wait_cloned` is called, the trie data
    /// will be computed synchronously from these inputs. This eliminates deadlock risk.
    ///
    /// # Arguments
    /// * `hashed_state` - Unsorted hashed post-state from execution
    /// * `trie_updates` - Unsorted trie updates from state root computation
    /// * `anchor_hash` - The persisted ancestor hash this trie input is anchored to
    /// * `ancestors` - Deferred trie data from ancestor blocks for merging
    pub fn pending(
        hashed_state: Arc<HashedPostState>,
        trie_updates: Arc<TrieUpdates>,
        anchor_hash: B256,
        ancestors: Vec<Self>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(DeferredState::Pending(Some(PendingInputs {
                hashed_state,
                trie_updates,
                anchor_hash,
                ancestors,
            })))),
        }
    }

    /// Create a handle that is already populated with the given [`ComputedTrieData`].
    ///
    /// Useful when trie data is available immediately.
    /// [`Self::wait_cloned`] will return without any computation.
    pub fn ready(bundle: ComputedTrieData) -> Self {
        Self { state: Arc::new(Mutex::new(DeferredState::Ready(bundle))) }
    }

    /// Sort block execution outputs and build a [`TrieInputSorted`] overlay.
    ///
    /// The trie input overlay accumulates sorted hashed state (account/storage changes) and
    /// trie node updates from all in-memory ancestor blocks. This overlay is required for:
    /// - Computing state roots on top of in-memory blocks
    /// - Generating storage/account proofs for unpersisted state
    ///
    /// # Process
    /// 1. Sort the current block's hashed state and trie updates
    /// 2. Reuse parent's cached overlay if available (O(1) - the common case)
    /// 3. Otherwise, rebuild overlay from ancestors (rare fallback)
    /// 4. Extend the overlay with this block's sorted data
    ///
    /// Used by both the async background task and the synchronous fallback path.
    ///
    /// # Arguments
    /// * `hashed_state` - Unsorted hashed post-state (account/storage changes) from execution
    /// * `trie_updates` - Unsorted trie node updates from state root computation
    /// * `anchor_hash` - The persisted ancestor hash this trie input is anchored to
    /// * `ancestors` - Deferred trie data from ancestor blocks for merging (oldest -> newest)
    pub fn sort_and_build_trie_input(
        hashed_state: Arc<HashedPostState>,
        trie_updates: Arc<TrieUpdates>,
        anchor_hash: B256,
        ancestors: &[Self],
    ) -> ComputedTrieData {
        let _span = debug_span!(target: "engine::tree::deferred_trie", "sort_inputs").entered();

        #[cfg(feature = "rayon")]
        let (sorted_hashed_state, sorted_trie_updates) = rayon::join(
            || match Arc::try_unwrap(hashed_state) {
                Ok(state) => state.into_sorted(),
                Err(arc) => arc.clone_into_sorted(),
            },
            || match Arc::try_unwrap(trie_updates) {
                Ok(updates) => updates.into_sorted(),
                Err(arc) => arc.clone_into_sorted(),
            },
        );

        #[cfg(not(feature = "rayon"))]
        let (sorted_hashed_state, sorted_trie_updates) = (
            match Arc::try_unwrap(hashed_state) {
                Ok(state) => state.into_sorted(),
                Err(arc) => arc.clone_into_sorted(),
            },
            match Arc::try_unwrap(trie_updates) {
                Ok(updates) => updates.into_sorted(),
                Err(arc) => arc.clone_into_sorted(),
            },
        );

        drop(_span);

        let _span = debug_span!(target: "engine::tree::deferred_trie", "build_overlay").entered();

        // Reuse parent's overlay if available and anchors match.
        // We can only reuse the parent's overlay if it was built on top of the same
        // persisted anchor. If the anchor has changed (e.g., due to persistence),
        // the parent's overlay is relative to an old state and cannot be used.
        let overlay = if let Some(parent) = ancestors.last() {
            let parent_data = parent.wait_cloned();

            match &parent_data.anchored_trie_input {
                // Case 1: Parent has cached overlay AND anchors match.
                Some(AnchoredTrieInput { anchor_hash: parent_anchor, trie_input })
                    if *parent_anchor == anchor_hash =>
                {
                    // O(1): Reuse parent's overlay, extend with current block's data.
                    let mut overlay = TrieInputSorted::new(
                        Arc::clone(&trie_input.nodes),
                        Arc::clone(&trie_input.state),
                        Default::default(), // prefix_sets are per-block, not cumulative
                    );
                    let _span =
                        debug_span!(target: "engine::tree::deferred_trie", "extend_overlay")
                            .entered();
                    // Only trigger COW clone if there's actually data to add.
                    #[cfg(feature = "rayon")]
                    {
                        rayon::join(
                            || {
                                if !sorted_hashed_state.is_empty() {
                                    Arc::make_mut(&mut overlay.state)
                                        .extend_ref_and_sort(&sorted_hashed_state);
                                }
                            },
                            || {
                                if !sorted_trie_updates.is_empty() {
                                    Arc::make_mut(&mut overlay.nodes)
                                        .extend_ref_and_sort(&sorted_trie_updates);
                                }
                            },
                        );
                    }
                    #[cfg(not(feature = "rayon"))]
                    {
                        if !sorted_hashed_state.is_empty() {
                            Arc::make_mut(&mut overlay.state)
                                .extend_ref_and_sort(&sorted_hashed_state);
                        }
                        if !sorted_trie_updates.is_empty() {
                            Arc::make_mut(&mut overlay.nodes)
                                .extend_ref_and_sort(&sorted_trie_updates);
                        }
                    }
                    overlay
                }
                // Case 2: Parent exists but anchor mismatch or no cached overlay.
                // We must rebuild from the ancestors list (which only contains unpersisted blocks).
                _ => Self::merge_ancestors_into_overlay(
                    ancestors,
                    &sorted_hashed_state,
                    &sorted_trie_updates,
                ),
            }
        } else {
            // Case 3: No in-memory ancestors (first block after persisted anchor).
            // Build overlay with just this block's data.
            Self::merge_ancestors_into_overlay(&[], &sorted_hashed_state, &sorted_trie_updates)
        };

        ComputedTrieData::with_trie_input(
            Arc::new(sorted_hashed_state),
            Arc::new(sorted_trie_updates),
            anchor_hash,
            Arc::new(overlay),
        )
    }

    /// Merge all ancestors and current block's data into a single overlay.
    ///
    /// This is a rare fallback path, only used when no ancestor has a cached
    /// `anchored_trie_input` (e.g., blocks created via alternative constructors).
    /// In normal operation, the parent always has a cached overlay and this
    /// function is never called.
    ///
    /// When the `rayon` feature is enabled:
    /// 1. Collects ancestor data (states and updates)
    /// 2. Merges states and trie updates in parallel using k-way merge
    #[cfg(feature = "rayon")]
    fn merge_ancestors_into_overlay(
        ancestors: &[Self],
        sorted_hashed_state: &HashedPostStateSorted,
        sorted_trie_updates: &TrieUpdatesSorted,
    ) -> TrieInputSorted {
        // Early exit: no ancestors means just wrap current block's data
        if ancestors.is_empty() {
            return TrieInputSorted::new(
                Arc::new(sorted_trie_updates.clone()),
                Arc::new(sorted_hashed_state.clone()),
                Default::default(),
            );
        }

        // Collect ancestor data in reverse (newest to oldest) for merge_slice
        let (states, updates): (Vec<_>, Vec<_>) = ancestors
            .iter()
            .rev()
            .map(|a| {
                // Note: we can assume that this trie data has already been computed
                let data = a.wait_cloned();
                (data.hashed_state, data.trie_updates)
            })
            .unzip();

        // Merge state and nodes in parallel using k-way merge
        let (state, nodes) = rayon::join(
            || {
                let mut merged = HashedPostStateSorted::merge_slice(&states);
                merged.extend_ref_and_sort(sorted_hashed_state);
                merged
            },
            || {
                let mut merged = TrieUpdatesSorted::merge_slice(&updates);
                merged.extend_ref_and_sort(sorted_trie_updates);
                merged
            },
        );

        TrieInputSorted::new(Arc::new(nodes), Arc::new(state), Default::default())
    }

    /// Sequential fallback when rayon is not available.
    #[cfg(not(feature = "rayon"))]
    fn merge_ancestors_into_overlay(
        ancestors: &[Self],
        sorted_hashed_state: &HashedPostStateSorted,
        sorted_trie_updates: &TrieUpdatesSorted,
    ) -> TrieInputSorted {
        let _span = debug_span!(target: "engine::tree::deferred_trie", "merge_ancestors", num_ancestors = ancestors.len()).entered();
        let mut overlay = TrieInputSorted::default();

        let state_mut = Arc::make_mut(&mut overlay.state);
        let nodes_mut = Arc::make_mut(&mut overlay.nodes);

        for ancestor in ancestors {
            let ancestor_data = ancestor.wait_cloned();
            state_mut.extend_ref_and_sort(ancestor_data.hashed_state.as_ref());
            nodes_mut.extend_ref_and_sort(ancestor_data.trie_updates.as_ref());
        }

        state_mut.extend_ref_and_sort(sorted_hashed_state);
        nodes_mut.extend_ref_and_sort(sorted_trie_updates);

        overlay
    }

    /// Returns trie data, computing synchronously if the async task hasn't completed.
    ///
    /// - If the async task has completed (`Ready`), returns the cached result.
    /// - If pending, computes synchronously from stored inputs.
    ///
    /// Deadlock is avoided as long as the provided ancestors form a true ancestor chain (a DAG):
    /// - Each block only waits on its ancestors (blocks on the path to the persisted root)
    /// - Sibling blocks (forks) are never in each other's ancestor lists
    /// - A block never waits on its descendants
    ///
    /// Given that invariant, circular wait dependencies are impossible.
    #[instrument(level = "debug", target = "engine::tree::deferred_trie", skip_all)]
    pub fn wait_cloned(&self) -> ComputedTrieData {
        let mut state = self.state.lock();
        match &mut *state {
            // If the deferred trie data is ready, return the cached result.
            DeferredState::Ready(bundle) => {
                DEFERRED_TRIE_METRICS.deferred_trie_async_ready.increment(1);
                bundle.clone()
            }
            // If the deferred trie data is pending, compute the trie data synchronously and return
            // the result. This is the fallback path if the async task hasn't completed.
            DeferredState::Pending(maybe_inputs) => {
                DEFERRED_TRIE_METRICS.deferred_trie_sync_fallback.increment(1);

                let inputs = maybe_inputs.take().expect("inputs must be present in Pending state");

                let computed = Self::sort_and_build_trie_input(
                    inputs.hashed_state,
                    inputs.trie_updates,
                    inputs.anchor_hash,
                    &inputs.ancestors,
                );
                *state = DeferredState::Ready(computed.clone());

                // Release lock before inputs (and its ancestors) drop to avoid holding it
                // while their potential last Arc refs drop (which could trigger recursive locking)
                drop(state);

                computed
            }
        }
    }
}

impl ComputedTrieData {
    /// Construct a bundle that includes trie input anchored to a persisted ancestor.
    pub const fn with_trie_input(
        hashed_state: Arc<HashedPostStateSorted>,
        trie_updates: Arc<TrieUpdatesSorted>,
        anchor_hash: B256,
        trie_input: Arc<TrieInputSorted>,
    ) -> Self {
        Self {
            hashed_state,
            trie_updates,
            anchored_trie_input: Some(AnchoredTrieInput { anchor_hash, trie_input }),
        }
    }

    /// Construct a bundle without trie input or anchor information.
    ///
    /// Unlike [`Self::with_trie_input`], this constructor omits the accumulated trie input overlay
    /// and its anchor hash. Use this when the trie input is not needed, such as in block builders
    /// or sequencers that don't require proof generation on top of in-memory state.
    ///
    /// The trie input anchor identifies the persisted block hash from which the in-memory overlay
    /// was built. Without it, consumers cannot determine which on-disk state to combine with.
    pub const fn without_trie_input(
        hashed_state: Arc<HashedPostStateSorted>,
        trie_updates: Arc<TrieUpdatesSorted>,
    ) -> Self {
        Self { hashed_state, trie_updates, anchored_trie_input: None }
    }

    /// Returns the anchor hash, if present.
    pub fn anchor_hash(&self) -> Option<B256> {
        self.anchored_trie_input.as_ref().map(|anchored| anchored.anchor_hash)
    }

    /// Returns the trie input, if present.
    pub fn trie_input(&self) -> Option<&Arc<TrieInputSorted>> {
        self.anchored_trie_input.as_ref().map(|anchored| &anchored.trie_input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{map::B256Map, U256};
    use smartnet_primitives_traits::Account;
    use smartnet_trie::updates::TrieUpdates;
    use std::sync::Arc;

    /// Helper to create a ready block with anchored trie input containing specific state.
    fn ready_block_with_state(
        anchor_hash: B256,
        accounts: Vec<(B256, Option<Account>)>,
    ) -> DeferredTrieData {
        let hashed_state = Arc::new(HashedPostStateSorted::new(accounts, B256Map::default()));
        let trie_updates = Arc::default();
        let mut overlay = TrieInputSorted::default();
        Arc::make_mut(&mut overlay.state).extend_ref_and_sort(hashed_state.as_ref());

        DeferredTrieData::ready(ComputedTrieData {
            hashed_state,
            trie_updates,
            anchored_trie_input: Some(AnchoredTrieInput {
                anchor_hash,
                trie_input: Arc::new(overlay),
            }),
        })
    }

    /// Verifies that parent's overlay is reused regardless of anchor.
    #[test]
    fn reuses_parent_overlay() {
        let anchor = B256::with_last_byte(1);
        let key = B256::with_last_byte(42);
        let account = Account { nonce: 100, balance: U256::ZERO, bytecode_hash: None };

        // Create parent with anchored trie input
        let parent = ready_block_with_state(anchor, vec![(key, Some(account))]);

        // Create child - should reuse parent's overlay
        let child = DeferredTrieData::pending(
            Arc::new(HashedPostState::default()),
            Arc::new(TrieUpdates::default()),
            anchor,
            vec![parent],
        );

        let result = child.wait_cloned();

        // Verify parent's account is in the overlay
        let overlay = result.anchored_trie_input.as_ref().unwrap();
        assert_eq!(overlay.anchor_hash, anchor);
        assert_eq!(overlay.trie_input.state.accounts.len(), 1);
        let (found_key, found_account) = &overlay.trie_input.state.accounts[0];
        assert_eq!(*found_key, key);
        assert_eq!(found_account.unwrap().nonce, 100);
    }

    /// Verifies that parent's overlay is NOT reused when anchor changes (after persist).
    /// The overlay data is dependent on the anchor, so it must be rebuilt from the
    /// remaining ancestors.
    #[test]
    fn rebuilds_overlay_when_anchor_changes() {
        let old_anchor = B256::with_last_byte(1);
        let new_anchor = B256::with_last_byte(2);
        let key = B256::with_last_byte(42);
        let account = Account { nonce: 50, balance: U256::ZERO, bytecode_hash: None };

        // Create parent with OLD anchor
        let parent = ready_block_with_state(old_anchor, vec![(key, Some(account))]);

        // Create child with NEW anchor (simulates after persist)
        // Should NOT reuse parent's overlay because anchor changed
        let child = DeferredTrieData::pending(
            Arc::new(HashedPostState::default()),
            Arc::new(TrieUpdates::default()),
            new_anchor,
            vec![parent],
        );

        let result = child.wait_cloned();

        // Verify result uses new anchor
        let overlay = result.anchored_trie_input.as_ref().unwrap();
        assert_eq!(overlay.anchor_hash, new_anchor);

        // Crucially, since we provided `parent` in ancestors but it has a different anchor,
        // the code falls back to `merge_ancestors_into_overlay`.
        // `merge_ancestors_into_overlay` reads `parent.hashed_state` (which has the account).
        // So the account IS present, but it was obtained via REBUILD, not REUSE.
        // We can check `DEFERRED_TRIE_METRICS` if we want to be sure, but functionally:
        assert_eq!(overlay.trie_input.state.accounts.len(), 1);
        let (found_key, found_account) = &overlay.trie_input.state.accounts[0];
        assert_eq!(*found_key, key);
        assert_eq!(found_account.unwrap().nonce, 50);
    }

    /// Verifies that a chain of blocks with matching anchors builds correct cumulative overlay.
    #[test]
    fn chain_of_blocks_builds_cumulative_overlay() {
        let anchor = B256::with_last_byte(1);
        let key1 = B256::with_last_byte(1);
        let key2 = B256::with_last_byte(2);
        let key3 = B256::with_last_byte(3);

        // Block 1: sets account at key1
        let block1 = ready_block_with_state(
            anchor,
            vec![(key1, Some(Account { nonce: 1, balance: U256::ZERO, bytecode_hash: None }))],
        );

        // Block 2: adds account at key2, ancestor is block1
        let block2_hashed = HashedPostState::default().with_accounts([(
            key2,
            Some(Account { nonce: 2, balance: U256::ZERO, bytecode_hash: None }),
        )]);
        let block2 = DeferredTrieData::pending(
            Arc::new(block2_hashed),
            Arc::new(TrieUpdates::default()),
            anchor,
            vec![block1.clone()],
        );
        // Compute block2's trie data
        let block2_computed = block2.wait_cloned();
        let block2_ready = DeferredTrieData::ready(block2_computed);

        // Block 3: adds account at key3, ancestor is block2 (which includes block1)
        let block3_hashed = HashedPostState::default().with_accounts([(
            key3,
            Some(Account { nonce: 3, balance: U256::ZERO, bytecode_hash: None }),
        )]);
        let block3 = DeferredTrieData::pending(
            Arc::new(block3_hashed),
            Arc::new(TrieUpdates::default()),
            anchor,
            vec![block1, block2_ready],
        );

        let result = block3.wait_cloned();

        // Verify all three accounts are in the cumulative overlay
        let overlay = result.anchored_trie_input.as_ref().unwrap();
        assert_eq!(overlay.trie_input.state.accounts.len(), 3);

        // Accounts should be sorted by key (B256 ordering)
        let accounts = &overlay.trie_input.state.accounts;
        assert!(accounts.iter().any(|(k, a)| *k == key1 && a.unwrap().nonce == 1));
        assert!(accounts.iter().any(|(k, a)| *k == key2 && a.unwrap().nonce == 2));
        assert!(accounts.iter().any(|(k, a)| *k == key3 && a.unwrap().nonce == 3));
    }

    /// Verifies that a multi-ancestor overlay is rebuilt when anchor changes.
    /// This simulates the "persist prefix then keep building" scenario where:
    /// 1. A chain of blocks is built with anchor A
    /// 2. Some blocks are persisted, changing anchor to B
    /// 3. New blocks must rebuild the overlay from the remaining ancestors
    #[test]
    fn multi_ancestor_overlay_rebuilt_after_anchor_change() {
        let old_anchor = B256::with_last_byte(1);
        let new_anchor = B256::with_last_byte(2);
        let key1 = B256::with_last_byte(1);
        let key2 = B256::with_last_byte(2);
        let key3 = B256::with_last_byte(3);
        let key4 = B256::with_last_byte(4);

        // Build a chain of 3 blocks with old_anchor
        let block1 = ready_block_with_state(
            old_anchor,
            vec![(key1, Some(Account { nonce: 1, balance: U256::ZERO, bytecode_hash: None }))],
        );

        let block2_hashed = HashedPostState::default().with_accounts([(
            key2,
            Some(Account { nonce: 2, balance: U256::ZERO, bytecode_hash: None }),
        )]);
        let block2 = DeferredTrieData::pending(
            Arc::new(block2_hashed),
            Arc::new(TrieUpdates::default()),
            old_anchor,
            vec![block1.clone()],
        );
        let block2_ready = DeferredTrieData::ready(block2.wait_cloned());

        let block3_hashed = HashedPostState::default().with_accounts([(
            key3,
            Some(Account { nonce: 3, balance: U256::ZERO, bytecode_hash: None }),
        )]);
        let block3 = DeferredTrieData::pending(
            Arc::new(block3_hashed),
            Arc::new(TrieUpdates::default()),
            old_anchor,
            vec![block1.clone(), block2_ready.clone()],
        );
        let block3_ready = DeferredTrieData::ready(block3.wait_cloned());

        // Verify block3's overlay has all 3 accounts with old_anchor
        let block3_overlay = block3_ready.wait_cloned().anchored_trie_input.unwrap();
        assert_eq!(block3_overlay.anchor_hash, old_anchor);
        assert_eq!(block3_overlay.trie_input.state.accounts.len(), 3);

        // Now simulate persist: create block4 with NEW anchor but same ancestors.
        // To verify correct rebuilding, we must provide ALL unpersisted ancestors.
        // If we only provided block3, the rebuild would only see block3's state.
        // We pass block1, block2, block3 to simulate that they are all still in memory
        // but the anchor check forces a rebuild (e.g. artificial anchor change).
        let block4_hashed = HashedPostState::default().with_accounts([(
            key4,
            Some(Account { nonce: 4, balance: U256::ZERO, bytecode_hash: None }),
        )]);
        let block4 = DeferredTrieData::pending(
            Arc::new(block4_hashed),
            Arc::new(TrieUpdates::default()),
            new_anchor, // Different anchor - simulates post-persist
            vec![block1, block2_ready, block3_ready],
        );

        let result = block4.wait_cloned();

        // Verify:
        // 1. New anchor is used in result
        assert_eq!(result.anchor_hash(), Some(new_anchor));

        // 2. All 4 accounts are in the overlay (rebuilt from ancestors + extended)
        let overlay = result.anchored_trie_input.as_ref().unwrap();
        assert_eq!(overlay.trie_input.state.accounts.len(), 4);

        // 3. All accounts have correct values
        let accounts = &overlay.trie_input.state.accounts;
        assert!(accounts.iter().any(|(k, a)| *k == key1 && a.unwrap().nonce == 1));
        assert!(accounts.iter().any(|(k, a)| *k == key2 && a.unwrap().nonce == 2));
        assert!(accounts.iter().any(|(k, a)| *k == key3 && a.unwrap().nonce == 3));
        assert!(accounts.iter().any(|(k, a)| *k == key4 && a.unwrap().nonce == 4));
    }
}
