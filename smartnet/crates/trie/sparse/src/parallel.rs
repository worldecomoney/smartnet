#[cfg(feature = "trie-debug")]
use crate::debug_recorder::{LeafUpdateRecord, ProofTrieNodeRecord, RecordedOp, TrieDebugRecorder};
use crate::{
    lower::LowerSparseSubtrie, provider::TrieNodeProvider, LeafLookup, LeafLookupError,
    RlpNodeStackItem, SparseNode, SparseNodeState, SparseNodeType, SparseTrie, SparseTrieUpdates,
};
use alloc::{borrow::Cow, boxed::Box, vec, vec::Vec};
use alloy_primitives::{
    map::{Entry, HashMap, HashSet},
    B256, U256,
};
use alloy_rlp::Decodable;
use alloy_trie::{BranchNodeCompact, TrieMask, EMPTY_ROOT_HASH};
use core::cmp::{Ord, Ordering, PartialOrd};
use smartnet_trie_common::{SparseTrieError, SparseTrieErrorKind, SparseTrieResult};
#[cfg(feature = "metrics")]
use smartnet_primitives_traits::FastInstant as Instant;
use smartnet_trie_common::{
    prefix_set::{PrefixSet, PrefixSetMut},
    BranchNodeMasks, BranchNodeMasksMap, BranchNodeRef, ExtensionNodeRef, LeafNodeRef, Nibbles,
    ProofTrieNodeV2, RlpNode, TrieNodeV2,
};
use smallvec::SmallVec;
use tracing::{instrument, trace};

/// The maximum length of a path, in nibbles, which belongs to the upper subtrie of a
/// [`ParallelSparseTrie`]. All longer paths belong to a lower subtrie.
pub const UPPER_TRIE_MAX_DEPTH: usize = 2;

/// Number of lower subtries which are managed by the [`ParallelSparseTrie`].
pub const NUM_LOWER_SUBTRIES: usize = 16usize.pow(UPPER_TRIE_MAX_DEPTH as u32);

/// Configuration for controlling when parallelism is enabled in [`ParallelSparseTrie`] operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParallelismThresholds {
    /// Minimum number of nodes to reveal before parallel processing is enabled.
    /// When `reveal_nodes` has fewer nodes than this threshold, they will be processed serially.
    pub min_revealed_nodes: usize,
    /// Minimum number of changed keys (prefix set length) before parallel processing is enabled
    /// for hash updates. When updating subtrie hashes with fewer changed keys than this threshold,
    /// the updates will be processed serially.
    pub min_updated_nodes: usize,
}

/// A revealed sparse trie with subtries that can be updated in parallel.
///
/// ## Structure
///
/// The trie is divided into two tiers for efficient parallel processing:
/// - **Upper subtrie**: Contains nodes with paths shorter than [`UPPER_TRIE_MAX_DEPTH`]
/// - **Lower subtries**: An array of [`NUM_LOWER_SUBTRIES`] subtries, each handling nodes with
///   paths of at least [`UPPER_TRIE_MAX_DEPTH`] nibbles
///
/// Node placement is determined by path depth:
/// - Paths with < [`UPPER_TRIE_MAX_DEPTH`] nibbles go to the upper subtrie
/// - Paths with >= [`UPPER_TRIE_MAX_DEPTH`] nibbles go to lower subtries, indexed by their first
///   [`UPPER_TRIE_MAX_DEPTH`] nibbles.
///
/// Each lower subtrie tracks its root via the `path` field, which represents the shortest path
/// in that subtrie. This path will have at least [`UPPER_TRIE_MAX_DEPTH`] nibbles, but may be
/// longer when an extension node in the upper trie "reaches into" the lower subtrie. For example,
/// if the upper trie has an extension from `0x1` to `0x12345`, then the lower subtrie for prefix
/// `0x12` will have its root at path `0x12345` rather than at `0x12`.
///
/// ## Node Revealing
///
/// The trie uses lazy loading to efficiently handle large state tries. Nodes can be:
/// - **Blind nodes**: Stored as hashes on [`SparseNode::Branch::blinded_hashes`]
/// - **Revealed nodes**: Fully loaded nodes (Branch, Extension, Leaf) with complete structure
///
/// Note: An empty trie contains an `EmptyRoot` node at the root path, rather than no nodes at all.
/// A trie with no nodes is blinded, its root may be `EmptyRoot` or some other node type.
///
/// Revealing is generally done using pre-loaded node data provided to via `reveal_nodes`. In
/// certain cases, such as edge-cases when updating/removing leaves, nodes are revealed on-demand.
///
/// ## Leaf Operations
///
/// **Update**: When updating a leaf, the new value is stored in the appropriate subtrie's values
/// map. If the leaf is new, the trie structure is updated by walking to the leaf from the root,
/// creating necessary intermediate branch nodes.
///
/// **Removal**: Leaf removal may require parent node modifications. The algorithm walks up the
/// trie, removing nodes that become empty and converting single-child branches to extensions.
///
/// During leaf operations the overall structure of the trie may change, causing nodes to be moved
/// from the upper to lower trie or vice-versa.
///
/// The `prefix_set` is modified during both leaf updates and removals to track changed leaf paths.
///
/// ## Root Hash Calculation
///
/// Root hash computation follows a bottom-up approach:
/// 1. Update hashes for all modified lower subtries (can be done in parallel)
/// 2. Update hashes for the upper subtrie (which may reference lower subtrie hashes)
/// 3. Calculate the final root hash from the upper subtrie's root node
///
/// The `prefix_set` tracks which paths have been modified, enabling incremental updates instead of
/// recalculating the entire trie.
///
/// ## Invariants
///
/// - Each leaf entry in the `subtries` and `upper_trie` collection must have a corresponding entry
///   in `values` collection. If the root node is a leaf, it must also have an entry in `values`.
/// - All keys in `values` collection are full leaf paths.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParallelSparseTrie {
    /// This contains the trie nodes for the upper part of the trie.
    upper_subtrie: Box<SparseSubtrie>,
    /// An array containing the subtries at the second level of the trie.
    lower_subtries: Box<[LowerSparseSubtrie; NUM_LOWER_SUBTRIES]>,
    /// Set of prefixes (key paths) that have been marked as updated.
    /// This is used to track which parts of the trie need to be recalculated.
    prefix_set: PrefixSetMut,
    /// Optional tracking of trie updates for later use.
    updates: Option<SparseTrieUpdates>,
    /// Branch node masks containing `tree_mask` and `hash_mask` for each path.
    /// - `tree_mask`: When a bit is set, the corresponding child subtree is stored in the
    ///   database.
    /// - `hash_mask`: When a bit is set, the corresponding child is stored as a hash in the
    ///   database.
    branch_node_masks: BranchNodeMasksMap,
    /// Reusable buffer pool used for collecting [`SparseTrieUpdatesAction`]s during hash
    /// computations.
    update_actions_buffers: Vec<Vec<SparseTrieUpdatesAction>>,
    /// Thresholds controlling when parallelism is enabled for different operations.
    parallelism_thresholds: ParallelismThresholds,
    /// Metrics for the parallel sparse trie.
    #[cfg(feature = "metrics")]
    metrics: crate::metrics::ParallelSparseTrieMetrics,
    /// Debug recorder for tracking mutating operations.
    #[cfg(feature = "trie-debug")]
    debug_recorder: TrieDebugRecorder,
}

impl Default for ParallelSparseTrie {
    fn default() -> Self {
        Self {
            upper_subtrie: Box::new(SparseSubtrie {
                nodes: HashMap::from_iter([(Nibbles::default(), SparseNode::Empty)]),
                ..Default::default()
            }),
            lower_subtries: Box::new(
                [const { LowerSparseSubtrie::Blind(None) }; NUM_LOWER_SUBTRIES],
            ),
            prefix_set: PrefixSetMut::default(),
            updates: None,
            branch_node_masks: BranchNodeMasksMap::default(),
            update_actions_buffers: Vec::default(),
            parallelism_thresholds: Default::default(),
            #[cfg(feature = "metrics")]
            metrics: Default::default(),
            #[cfg(feature = "trie-debug")]
            debug_recorder: Default::default(),
        }
    }
}

impl SparseTrie for ParallelSparseTrie {
    fn set_root(
        &mut self,
        root: TrieNodeV2,
        masks: Option<BranchNodeMasks>,
        retain_updates: bool,
    ) -> SparseTrieResult<()> {
        #[cfg(feature = "trie-debug")]
        self.debug_recorder.record(RecordedOp::SetRoot {
            node: ProofTrieNodeRecord::from_proof_trie_node_v2(&ProofTrieNodeV2 {
                path: Nibbles::default(),
                node: root.clone(),
                masks,
            }),
        });

        // A fresh/cleared `ParallelSparseTrie` has a `SparseNode::Empty` at its root in the upper
        // subtrie. Delete that so we can reveal the new root node.
        let path = Nibbles::default();
        let _removed_root = self.upper_subtrie.nodes.remove(&path).expect("root node should exist");
        debug_assert_eq!(_removed_root, SparseNode::Empty);

        self.set_updates(retain_updates);

        if let Some(masks) = masks {
            let branch_path = if let TrieNodeV2::Branch(branch) = &root {
                branch.key
            } else {
                Nibbles::default()
            };

            self.branch_node_masks.insert(branch_path, masks);
        }

        self.reveal_upper_node(Nibbles::default(), &root, masks)
    }

    fn set_updates(&mut self, retain_updates: bool) {
        self.updates = retain_updates.then(Default::default);
    }

    fn reveal_nodes(&mut self, nodes: &mut [ProofTrieNodeV2]) -> SparseTrieResult<()> {
        if nodes.is_empty() {
            return Ok(());
        }

        #[cfg(feature = "trie-debug")]
        self.debug_recorder.record(RecordedOp::RevealNodes {
            nodes: nodes.iter().map(ProofTrieNodeRecord::from_proof_trie_node_v2).collect(),
        });

        // Sort nodes first by their subtrie, and secondarily by their path. This allows for
        // grouping nodes by their subtrie using `chunk_by`.
        nodes.sort_unstable_by(
            |ProofTrieNodeV2 { path: path_a, .. }, ProofTrieNodeV2 { path: path_b, .. }| {
                let subtrie_type_a = SparseSubtrieType::from_path(path_a);
                let subtrie_type_b = SparseSubtrieType::from_path(path_b);
                subtrie_type_a.cmp(&subtrie_type_b).then_with(|| path_a.cmp(path_b))
            },
        );

        // Update the top-level branch node masks. This is simple and can't be done in parallel.
        self.branch_node_masks.reserve(nodes.len());
        for ProofTrieNodeV2 { path, masks, node } in nodes.iter() {
            if let Some(branch_masks) = masks {
                // Use proper path for branch nodes by combining path and extension key.
                let path = if let TrieNodeV2::Branch(branch) = node &&
                    !branch.key.is_empty()
                {
                    let mut path = *path;
                    path.extend(&branch.key);
                    path
                } else {
                    *path
                };
                self.branch_node_masks.insert(path, *branch_masks);
            }
        }

        // Due to the sorting all upper subtrie nodes will be at the front of the slice. We split
        // them off from the rest to be handled specially by
        // `ParallelSparseTrie::reveal_upper_node`.
        let num_upper_nodes = nodes
            .iter()
            .position(|n| !SparseSubtrieType::path_len_is_upper(n.path.len()))
            .unwrap_or(nodes.len());
        let (upper_nodes, lower_nodes) = nodes.split_at(num_upper_nodes);

        // Reserve the capacity of the upper subtrie's `nodes` HashMap before iterating, so we don't
        // end up making many small capacity changes as we loop.
        self.upper_subtrie.nodes.reserve(upper_nodes.len());
        for node in upper_nodes {
            self.reveal_upper_node(node.path, &node.node, node.masks)?;
        }

        let reachable_subtries = self.reachable_subtries();

        // Best-effort for boundary nodes: if the parent upper node exists as a branch and the
        // boundary child is still blinded, unset that blinded bit and carry the hash into
        // `reveal_node`. If the parent path is absent/non-branch (for example upper extension
        // crossing the boundary), skip without failing.
        let hashes_from_upper = nodes
            .iter()
            .filter_map(|node| {
                if node.path.len() != UPPER_TRIE_MAX_DEPTH ||
                    !reachable_subtries.get(path_subtrie_index_unchecked(&node.path))
                {
                    return None;
                }

                let parent_path = node.path.slice(0..UPPER_TRIE_MAX_DEPTH - 1);
                let Some(SparseNode::Branch { blinded_mask, blinded_hashes, .. }) =
                    self.upper_subtrie.nodes.get_mut(&parent_path)
                else {
                    return None;
                };

                let nibble = node.path.last().unwrap();
                blinded_mask.is_bit_set(nibble).then(|| {
                    blinded_mask.unset_bit(nibble);
                    (node.path, blinded_hashes[nibble as usize])
                })
            })
            .collect::<HashMap<_, _>>();

        if !self.is_reveal_parallelism_enabled(lower_nodes.len()) {
            for node in lower_nodes {
                let idx = path_subtrie_index_unchecked(&node.path);
                if !reachable_subtries.get(idx) {
                    trace!(
                        target: "trie::parallel_sparse",
                        reveal_path = ?node.path,
                        "Node's lower subtrie is not reachable, skipping",
                    );
                    continue;
                }
                // For boundary leaves, check reachability from upper subtrie's parent branch
                if node.path.len() == UPPER_TRIE_MAX_DEPTH &&
                    !Self::is_boundary_leaf_reachable(
                        &self.upper_subtrie.nodes,
                        &node.path,
                        &node.node,
                    )
                {
                    trace!(
                        target: "trie::parallel_sparse",
                        path = ?node.path,
                        "Boundary leaf not reachable from upper subtrie, skipping",
                    );
                    continue;
                }
                self.lower_subtries[idx].reveal(&node.path);
                self.lower_subtries[idx].as_revealed_mut().expect("just revealed").reveal_node(
                    node.path,
                    &node.node,
                    node.masks,
                    hashes_from_upper.get(&node.path).copied(),
                )?;
            }
            return Ok(());
        }

        #[cfg(not(feature = "std"))]
        unreachable!("nostd is checked by is_reveal_parallelism_enabled");

        #[cfg(feature = "std")]
        // Reveal lower subtrie nodes in parallel
        {
            use rayon::iter::{IntoParallelIterator, ParallelIterator};
            use tracing::Span;

            // Capture the current span so it can be propagated to rayon worker threads
            let parent_span = Span::current();

            // Capture reference to upper subtrie nodes for boundary leaf reachability checks
            let upper_nodes = &self.upper_subtrie.nodes;

            // Group the nodes by lower subtrie.
            let results = lower_nodes
                .chunk_by(|node_a, node_b| {
                    SparseSubtrieType::from_path(&node_a.path) ==
                        SparseSubtrieType::from_path(&node_b.path)
                })
                // Filter out chunks for unreachable subtries.
                .filter_map(|nodes| {
                    let mut nodes = nodes
                        .iter()
                        .filter(|node| {
                            // For boundary leaves, check reachability from upper subtrie's parent
                            // branch.
                            if node.path.len() == UPPER_TRIE_MAX_DEPTH &&
                                !Self::is_boundary_leaf_reachable(
                                    upper_nodes,
                                    &node.path,
                                    &node.node,
                                )
                            {
                                trace!(
                                    target: "trie::parallel_sparse",
                                    path = ?node.path,
                                    "Boundary leaf not reachable from upper subtrie, skipping",
                                );
                                false
                            } else {
                                true
                            }
                        })
                        .peekable();

                    let node = nodes.peek()?;
                    let idx =
                        SparseSubtrieType::from_path(&node.path).lower_index().unwrap_or_else(
                            || panic!("upper subtrie node {node:?} found amongst lower nodes"),
                        );

                    if !reachable_subtries.get(idx) {
                        trace!(
                            target: "trie::parallel_sparse",
                            nodes = ?nodes,
                            "Lower subtrie is not reachable, skipping reveal",
                        );
                        return None;
                    }

                    // due to the nodes being sorted secondarily on their path, and chunk_by keeping
                    // the first element of each group, the `path` here will necessarily be the
                    // shortest path being revealed for each subtrie. Therefore we can reveal the
                    // subtrie itself using this path and retain correct behavior.
                    self.lower_subtries[idx].reveal(&node.path);
                    Some((
                        idx,
                        self.lower_subtries[idx].take_revealed().expect("just revealed"),
                        nodes,
                    ))
                })
                .collect::<Vec<_>>()
                .into_par_iter()
                .map(|(subtrie_idx, mut subtrie, nodes)| {
                    // Enter the parent span to propagate context (e.g., hashed_address for storage
                    // tries) to the worker thread
                    let _guard = parent_span.enter();

                    // reserve space in the HashMap ahead of time; doing it on a node-by-node basis
                    // can cause multiple re-allocations as the hashmap grows.
                    subtrie.nodes.reserve(nodes.size_hint().1.unwrap_or(0));

                    for node in nodes {
                        // Reveal each node in the subtrie, returning early on any errors
                        let res = subtrie.reveal_node(
                            node.path,
                            &node.node,
                            node.masks,
                            hashes_from_upper.get(&node.path).copied(),
                        );
                        if res.is_err() {
                            return (subtrie_idx, subtrie, res.map(|_| ()));
                        }
                    }
                    (subtrie_idx, subtrie, Ok(()))
                })
                .collect::<Vec<_>>();

            // Put subtries back which were processed in the rayon pool, collecting the last
            // seen error in the process and returning that.
            let mut any_err = Ok(());
            for (subtrie_idx, subtrie, res) in results {
                self.lower_subtries[subtrie_idx] = LowerSparseSubtrie::Revealed(subtrie);
                if res.is_err() {
                    any_err = res;
                }
            }

            any_err
        }
    }

    fn update_leaf<P: TrieNodeProvider>(
        &mut self,
        full_path: Nibbles,
        value: Vec<u8>,
        _provider: P,
    ) -> SparseTrieResult<()> {
        debug_assert_eq!(
            full_path.len(),
            B256::len_bytes() * 2,
            "update_leaf full_path must be 64 nibbles (32 bytes), got {} nibbles",
            full_path.len()
        );

        trace!(
            target: "trie::parallel_sparse",
            ?full_path,
            value_len = value.len(),
            "Updating leaf",
        );

        // Check if the value already exists - if so, just update it (no structural changes needed)
        if self.upper_subtrie.inner.values.contains_key(&full_path) {
            self.prefix_set.insert(full_path);
            self.upper_subtrie.inner.values.insert(full_path, value);
            return Ok(());
        }
        // Also check lower subtries for existing value
        if let Some(subtrie) = self.lower_subtrie_for_path(&full_path) &&
            subtrie.inner.values.contains_key(&full_path)
        {
            self.prefix_set.insert(full_path);
            self.lower_subtrie_for_path_mut(&full_path)
                .expect("subtrie exists")
                .inner
                .values
                .insert(full_path, value);
            return Ok(());
        }

        // Insert value into upper subtrie temporarily. We'll move it to the correct subtrie
        // during traversal, or clean it up if we error.
        self.upper_subtrie.inner.values.insert(full_path, value.clone());

        // Start at the root, traversing until we find either the node to update or a subtrie to
        // update.
        //
        // We first traverse the upper subtrie for two levels, and moving any created nodes to a
        // lower subtrie if necessary.
        //
        // We use `next` to keep track of the next node that we need to traverse to, and
        // `new_nodes` to keep track of any nodes that were created during the traversal.
        let mut new_nodes = Vec::new();
        let mut next = Some(Nibbles::default());

        // Traverse the upper subtrie to find the node to update or the subtrie to update.
        //
        // We stop when the next node to traverse would be in a lower subtrie, or if there are no
        // more nodes to traverse.
        while let Some(current) =
            next.as_mut().filter(|next| SparseSubtrieType::path_len_is_upper(next.len()))
        {
            // Traverse the next node, keeping track of any changed nodes and the next step in the
            // trie. If traversal fails, clean up the value we inserted and propagate the error.
            let step_result = self.upper_subtrie.update_next_node(current, &full_path);

            if step_result.is_err() {
                self.upper_subtrie.inner.values.remove(&full_path);
                return step_result.map(|_| ());
            }

            match step_result? {
                LeafUpdateStep::Continue => {}
                LeafUpdateStep::Complete { inserted_nodes } => {
                    new_nodes.extend(inserted_nodes);
                    next = None;
                }
                LeafUpdateStep::NodeNotFound => {
                    next = None;
                }
            }
        }

        // Move nodes from upper subtrie to lower subtries
        for node_path in &new_nodes {
            // Skip nodes that belong in the upper subtrie
            if SparseSubtrieType::path_len_is_upper(node_path.len()) {
                continue;
            }

            let node =
                self.upper_subtrie.nodes.remove(node_path).expect("node belongs to upper subtrie");

            // If it's a leaf node, extract its value before getting mutable reference to subtrie.
            let leaf_value = if let SparseNode::Leaf { key, .. } = &node {
                let mut leaf_full_path = *node_path;
                leaf_full_path.extend(key);
                Some((
                    leaf_full_path,
                    self.upper_subtrie
                        .inner
                        .values
                        .remove(&leaf_full_path)
                        .expect("leaf nodes have associated values entries"),
                ))
            } else {
                None
            };

            // Get or create the subtrie with the exact node path (not truncated to 2 nibbles).
            let subtrie = self.subtrie_for_path_mut(node_path);

            // Insert the leaf value if we have one
            if let Some((leaf_full_path, value)) = leaf_value {
                subtrie.inner.values.insert(leaf_full_path, value);
            }

            // Insert the node into the lower subtrie
            subtrie.nodes.insert(*node_path, node);
        }

        // If we reached the max depth of the upper trie, we may have had more nodes to insert.
        if let Some(next_path) = next.filter(|n| !SparseSubtrieType::path_len_is_upper(n.len())) {
            // The value was inserted into the upper subtrie's `values` at the top of this method.
            // At this point we know the value is not in the upper subtrie, and the call to
            // `update_leaf` below will insert it into the lower subtrie. So remove it from the
            // upper subtrie.
            self.upper_subtrie.inner.values.remove(&full_path);

            // Use subtrie_for_path to ensure the subtrie has the correct path.
            //
            // The next_path here represents where we need to continue traversal, which may
            // be longer than 2 nibbles if we're following an extension node.
            let subtrie = self.subtrie_for_path_mut(&next_path);

            // Create an empty root at the subtrie path if the subtrie is empty
            if subtrie.nodes.is_empty() {
                subtrie.nodes.insert(subtrie.path, SparseNode::Empty);
            }

            // If we didn't update the target leaf, we need to call update_leaf on the subtrie
            // to ensure that the leaf is updated correctly.
            if let Err(e) = subtrie.update_leaf(full_path, value) {
                // Clean up: remove the value from lower subtrie if it was inserted
                if let Some(lower) = self.lower_subtrie_for_path_mut(&full_path) {
                    lower.inner.values.remove(&full_path);
                }
                return Err(e);
            }
        }

        // Insert into prefix_set only after all operations succeed
        self.prefix_set.insert(full_path);

        Ok(())
    }

    fn remove_leaf<P: TrieNodeProvider>(
        &mut self,
        full_path: &Nibbles,
        _provider: P,
    ) -> SparseTrieResult<()> {
        debug_assert_eq!(
            full_path.len(),
            B256::len_bytes() * 2,
            "remove_leaf full_path must be 64 nibbles (32 bytes), got {} nibbles",
            full_path.len()
        );

        trace!(
            target: "trie::parallel_sparse",
            ?full_path,
            "Removing leaf",
        );

        // When removing a leaf node it's possibly necessary to modify its parent node, and possibly
        // the parent's parent node. It is not ever necessary to descend further than that; once an
        // extension node is hit it must terminate in a branch or the root, which won't need further
        // updates. So the situation with maximum updates is:
        //
        // - Leaf
        // - Branch with 2 children, one being this leaf
        // - Extension
        //
        // ...which will result in just a leaf or extension, depending on what the branch's other
        // child is.
        //
        // Therefore, first traverse the trie in order to find the leaf node and at most its parent
        // and grandparent.

        let leaf_path;
        let leaf_subtrie_type;

        let mut branch_parent_path: Option<Nibbles> = None;
        let mut branch_parent_node: Option<SparseNode> = None;

        let mut ext_grandparent_path: Option<Nibbles> = None;
        let mut ext_grandparent_node: Option<SparseNode> = None;

        let mut curr_path = Nibbles::new(); // start traversal from root
        let mut curr_subtrie_type = SparseSubtrieType::Upper;

        // List of node paths which need to be marked dirty
        let mut paths_to_mark_dirty = Vec::new();

        loop {
            let curr_subtrie = match curr_subtrie_type {
                SparseSubtrieType::Upper => &mut self.upper_subtrie,
                SparseSubtrieType::Lower(idx) => {
                    self.lower_subtries[idx].as_revealed_mut().expect("lower subtrie is revealed")
                }
            };
            let curr_node = curr_subtrie.nodes.get_mut(&curr_path).unwrap();

            match Self::find_next_to_leaf(&curr_path, curr_node, full_path) {
                FindNextToLeafOutcome::NotFound => return Ok(()), // leaf isn't in the trie
                FindNextToLeafOutcome::BlindedNode(path) => {
                    return Err(SparseTrieErrorKind::BlindedNode(path).into())
                }
                FindNextToLeafOutcome::Found => {
                    // this node is the target leaf
                    leaf_path = curr_path;
                    leaf_subtrie_type = curr_subtrie_type;
                    break;
                }
                FindNextToLeafOutcome::ContinueFrom(next_path) => {
                    // Any branches/extensions along the path to the leaf will have their `hash`
                    // field unset, as it will no longer be valid once the leaf is removed.
                    match curr_node {
                        SparseNode::Branch { .. } => {
                            paths_to_mark_dirty
                                .push((SparseSubtrieType::from_path(&curr_path), curr_path));

                            // If there is already an extension leading into a branch, then that
                            // extension is no longer relevant.
                            match (&branch_parent_path, &ext_grandparent_path) {
                                (Some(branch), Some(ext)) if branch.len() > ext.len() => {
                                    ext_grandparent_path = None;
                                    ext_grandparent_node = None;
                                }
                                _ => (),
                            };
                            branch_parent_path = Some(curr_path);
                            branch_parent_node = Some(curr_node.clone());
                        }
                        SparseNode::Extension { .. } => {
                            paths_to_mark_dirty
                                .push((SparseSubtrieType::from_path(&curr_path), curr_path));

                            // We can assume a new branch node will be found after the extension, so
                            // there's no need to modify branch_parent_path/node even if it's
                            // already set.
                            ext_grandparent_path = Some(curr_path);
                            ext_grandparent_node = Some(curr_node.clone());
                        }
                        SparseNode::Empty | SparseNode::Leaf { .. } => {
                            unreachable!(
                                "find_next_to_leaf only continues to a branch or extension"
                            )
                        }
                    }

                    curr_path = next_path;

                    // Update subtrie type if we're crossing into the lower trie.
                    let next_subtrie_type = SparseSubtrieType::from_path(&curr_path);
                    if matches!(curr_subtrie_type, SparseSubtrieType::Upper) &&
                        matches!(next_subtrie_type, SparseSubtrieType::Lower(_))
                    {
                        curr_subtrie_type = next_subtrie_type;
                    }
                }
            };
        }

        // Before mutating, check if branch collapse would require revealing a blinded node.
        // This ensures remove_leaf is atomic: if it errors, the trie is unchanged.
        if let (Some(branch_path), Some(SparseNode::Branch { state_mask, blinded_mask, .. })) =
            (&branch_parent_path, &branch_parent_node)
        {
            let mut check_mask = *state_mask;
            let child_nibble = leaf_path.get_unchecked(branch_path.len());
            check_mask.unset_bit(child_nibble);

            if check_mask.count_bits() == 1 {
                let remaining_nibble =
                    check_mask.first_set_bit_index().expect("state mask is not empty");

                if blinded_mask.is_bit_set(remaining_nibble) {
                    let mut path = *branch_path;
                    path.push_unchecked(remaining_nibble);
                    return Err(SparseTrieErrorKind::BlindedNode(path).into());
                }
            }
        }

        // We've traversed to the leaf and collected its ancestors as necessary. Remove the leaf
        // from its SparseSubtrie and reset the hashes of the nodes along the path.
        self.prefix_set.insert(*full_path);
        let leaf_subtrie = match leaf_subtrie_type {
            SparseSubtrieType::Upper => &mut self.upper_subtrie,
            SparseSubtrieType::Lower(idx) => {
                self.lower_subtries[idx].as_revealed_mut().expect("lower subtrie is revealed")
            }
        };
        leaf_subtrie.inner.values.remove(full_path);
        for (subtrie_type, path) in paths_to_mark_dirty {
            let node = match subtrie_type {
                SparseSubtrieType::Upper => self.upper_subtrie.nodes.get_mut(&path),
                SparseSubtrieType::Lower(idx) => self.lower_subtries[idx]
                    .as_revealed_mut()
                    .expect("lower subtrie is revealed")
                    .nodes
                    .get_mut(&path),
            }
            .expect("node exists");

            match node {
                SparseNode::Extension { state, .. } | SparseNode::Branch { state, .. } => {
                    *state = SparseNodeState::Dirty
                }
                SparseNode::Empty | SparseNode::Leaf { .. } => {
                    unreachable!(
                        "only branch and extension nodes can be marked dirty when removing a leaf"
                    )
                }
            }
        }
        self.remove_node(&leaf_path);

        // If the leaf was at the root replace its node with the empty value. We can stop execution
        // here, all remaining logic is related to the ancestors of the leaf.
        if leaf_path.is_empty() {
            self.upper_subtrie.nodes.insert(leaf_path, SparseNode::Empty);
            return Ok(());
        }

        // If there is a parent branch node (very likely, unless the leaf is at the root) execute
        // any required changes for that node, relative to the removed leaf.
        if let (
            Some(branch_path),
            &Some(SparseNode::Branch { mut state_mask, blinded_mask, ref blinded_hashes, .. }),
        ) = (&branch_parent_path, &branch_parent_node)
        {
            let child_nibble = leaf_path.get_unchecked(branch_path.len());
            state_mask.unset_bit(child_nibble);

            let new_branch_node = if state_mask.count_bits() == 1 {
                // If only one child is left set in the branch node, we need to collapse it. Get
                // full path of the only child node left.
                let remaining_child_nibble =
                    state_mask.first_set_bit_index().expect("state mask is not empty");
                let mut remaining_child_path = *branch_path;
                remaining_child_path.push_unchecked(remaining_child_nibble);

                trace!(
                    target: "trie::parallel_sparse",
                    ?leaf_path,
                    ?branch_path,
                    ?remaining_child_path,
                    "Branch node has only one child",
                );

                // If the remaining child node is not yet revealed then we have to reveal it here,
                // otherwise it's not possible to know how to collapse the branch.
                if blinded_mask.is_bit_set(remaining_child_nibble) {
                    return Err(SparseTrieErrorKind::BlindedNode(remaining_child_path).into());
                }

                let remaining_child_node = self
                    .subtrie_for_path_mut(&remaining_child_path)
                    .nodes
                    .get(&remaining_child_path)
                    .unwrap();

                let (new_branch_node, remove_child) = Self::branch_changes_on_leaf_removal(
                    branch_path,
                    &remaining_child_path,
                    remaining_child_node,
                );

                if remove_child {
                    self.move_value_on_leaf_removal(
                        branch_path,
                        &new_branch_node,
                        &remaining_child_path,
                    );
                    self.remove_node(&remaining_child_path);
                }

                if let Some(updates) = self.updates.as_mut() {
                    updates.updated_nodes.remove(branch_path);
                    updates.removed_nodes.insert(*branch_path);
                }

                new_branch_node
            } else {
                // If more than one child is left set in the branch, we just re-insert it with the
                // updated state_mask.
                SparseNode::Branch {
                    state_mask,
                    blinded_mask,
                    blinded_hashes: blinded_hashes.clone(),
                    state: SparseNodeState::Dirty,
                }
            };

            let branch_subtrie = self.subtrie_for_path_mut(branch_path);
            branch_subtrie.nodes.insert(*branch_path, new_branch_node.clone());
            branch_parent_node = Some(new_branch_node);
        };

        // If there is a grandparent extension node then there will necessarily be a parent branch
        // node. Execute any required changes for the extension node, relative to the (possibly now
        // replaced with a leaf or extension) branch node.
        if let (Some(ext_path), Some(SparseNode::Extension { key: shortkey, .. })) =
            (ext_grandparent_path, &ext_grandparent_node)
        {
            let ext_subtrie = self.subtrie_for_path_mut(&ext_path);
            let branch_path = branch_parent_path.as_ref().unwrap();

            if let Some(new_ext_node) = Self::extension_changes_on_leaf_removal(
                &ext_path,
                shortkey,
                branch_path,
                branch_parent_node.as_ref().unwrap(),
            ) {
                ext_subtrie.nodes.insert(ext_path, new_ext_node.clone());
                self.move_value_on_leaf_removal(&ext_path, &new_ext_node, branch_path);
                self.remove_node(branch_path);
            }
        }

        Ok(())
    }

    #[instrument(level = "trace", target = "trie::sparse::parallel", skip(self))]
    fn root(&mut self) -> B256 {
        trace!(target: "trie::parallel_sparse", "Calculating trie root hash");

        #[cfg(feature = "trie-debug")]
        self.debug_recorder.record(RecordedOp::Root);

        if self.prefix_set.is_empty() &&
            let Some(rlp_node) = self
                .upper_subtrie
                .nodes
                .get(&Nibbles::default())
                .and_then(|node| node.cached_rlp_node())
        {
            return rlp_node
                .as_hash()
                .expect("RLP-encoding of the root node cannot be less than 32 bytes");
        }

        // Update all lower subtrie hashes
        self.update_subtrie_hashes();

        // Update hashes for the upper subtrie using our specialized function
        // that can access both upper and lower subtrie nodes
        let mut prefix_set = core::mem::take(&mut self.prefix_set).freeze();
        let root_rlp = self.update_upper_subtrie_hashes(&mut prefix_set);

        // Return the root hash
        root_rlp.as_hash().unwrap_or(EMPTY_ROOT_HASH)
    }

    fn is_root_cached(&self) -> bool {
        self.prefix_set.is_empty() &&
            self.upper_subtrie
                .nodes
                .get(&Nibbles::default())
                .is_some_and(|node| node.cached_rlp_node().is_some())
    }

    #[instrument(level = "trace", target = "trie::sparse::parallel", skip(self))]
    fn update_subtrie_hashes(&mut self) {
        trace!(target: "trie::parallel_sparse", "Updating subtrie hashes");

        #[cfg(feature = "trie-debug")]
        self.debug_recorder.record(RecordedOp::UpdateSubtrieHashes);

        // Take changed subtries according to the prefix set
        let mut prefix_set = core::mem::take(&mut self.prefix_set).freeze();
        let num_changed_keys = prefix_set.len();
        let (mut changed_subtries, unchanged_prefix_set) =
            self.take_changed_lower_subtries(&mut prefix_set);

        // update metrics
        #[cfg(feature = "metrics")]
        self.metrics.subtries_updated.record(changed_subtries.len() as f64);

        // Update the prefix set with the keys that didn't have matching subtries
        self.prefix_set = unchanged_prefix_set;

        // Update subtrie hashes serially parallelism is not enabled
        if !self.is_update_parallelism_enabled(num_changed_keys) {
            for changed_subtrie in &mut changed_subtries {
                changed_subtrie.subtrie.update_hashes(
                    &mut changed_subtrie.prefix_set,
                    &mut changed_subtrie.update_actions_buf,
                    &self.branch_node_masks,
                );
            }

            self.insert_changed_subtries(changed_subtries);
            return;
        }

        #[cfg(not(feature = "std"))]
        unreachable!("nostd is checked by is_update_parallelism_enabled");

        #[cfg(feature = "std")]
        // Update subtrie hashes in parallel
        {
            use rayon::prelude::*;

            changed_subtries.par_iter_mut().for_each(|changed_subtrie| {
                #[cfg(feature = "metrics")]
                let start = Instant::now();
                changed_subtrie.subtrie.update_hashes(
                    &mut changed_subtrie.prefix_set,
                    &mut changed_subtrie.update_actions_buf,
                    &self.branch_node_masks,
                );
                #[cfg(feature = "metrics")]
                self.metrics.subtrie_hash_update_latency.record(start.elapsed());
            });

            self.insert_changed_subtries(changed_subtries);
        }
    }

    fn get_leaf_value(&self, full_path: &Nibbles) -> Option<&Vec<u8>> {
        // `subtrie_for_path` is intended for a node path, but here we are using a full key path. So
        // we need to check if the subtrie that the key might belong to has any nodes; if not then
        // the key's portion of the trie doesn't have enough depth to reach into the subtrie, and
        // the key will be in the upper subtrie
        if let Some(subtrie) = self.subtrie_for_path(full_path) &&
            !subtrie.is_empty()
        {
            return subtrie.inner.values.get(full_path);
        }

        self.upper_subtrie.inner.values.get(full_path)
    }

    fn updates_ref(&self) -> Cow<'_, SparseTrieUpdates> {
        self.updates.as_ref().map_or(Cow::Owned(SparseTrieUpdates::default()), Cow::Borrowed)
    }

    fn take_updates(&mut self) -> SparseTrieUpdates {
        match self.updates.take() {
            Some(updates) => {
                // NOTE: we need to preserve Some case
                self.updates = Some(SparseTrieUpdates::with_capacity(
                    updates.updated_nodes.len(),
                    updates.removed_nodes.len(),
                ));
                updates
            }
            None => SparseTrieUpdates::default(),
        }
    }

    fn wipe(&mut self) {
        self.upper_subtrie.wipe();
        for trie in &mut *self.lower_subtries {
            trie.wipe();
        }
        self.prefix_set = PrefixSetMut::all();
        self.updates = self.updates.is_some().then(SparseTrieUpdates::wiped);
    }

    fn clear(&mut self) {
        self.upper_subtrie.clear();
        self.upper_subtrie.nodes.insert(Nibbles::default(), SparseNode::Empty);
        for subtrie in &mut *self.lower_subtries {
            subtrie.clear();
        }
        self.prefix_set.clear();
        self.updates = None;
        self.branch_node_masks.clear();
        #[cfg(feature = "trie-debug")]
        self.debug_recorder.reset();
        // `update_actions_buffers` doesn't need to be cleared; we want to reuse the Vecs it has
        // buffered, and all of those are already inherently cleared when they get used.
    }

    fn find_leaf(
        &self,
        full_path: &Nibbles,
        expected_value: Option<&Vec<u8>>,
    ) -> Result<LeafLookup, LeafLookupError> {
        // Inclusion proof
        //
        // First, do a quick check if the value exists in either the upper or lower subtrie's values
        // map. We assume that if there exists a leaf node, then its value will be in the `values`
        // map.
        if let Some(actual_value) = core::iter::once(self.upper_subtrie.as_ref())
            .chain(self.lower_subtrie_for_path(full_path))
            .filter_map(|subtrie| subtrie.inner.values.get(full_path))
            .next()
        {
            // We found the leaf, check if the value matches (if expected value was provided)
            return expected_value
                .is_none_or(|v| v == actual_value)
                .then_some(LeafLookup::Exists)
                .ok_or_else(|| LeafLookupError::ValueMismatch {
                    path: *full_path,
                    expected: expected_value.cloned(),
                    actual: actual_value.clone(),
                });
        }

        // If the value does not exist in the `values` map, then this means that the leaf either:
        // - Does not exist in the trie
        // - Is missing from the witness
        // We traverse the trie to find the location where this leaf would have been, showing
        // that it is not in the trie. Or we find a blinded node, showing that the witness is
        // not complete.
        let mut curr_path = Nibbles::new(); // start traversal from root
        let mut curr_subtrie = self.upper_subtrie.as_ref();
        let mut curr_subtrie_is_upper = true;

        loop {
            match curr_subtrie.nodes.get(&curr_path).unwrap() {
                SparseNode::Empty => return Ok(LeafLookup::NonExistent),
                SparseNode::Leaf { key, .. } => {
                    let mut found_full_path = curr_path;
                    found_full_path.extend(key);
                    assert!(&found_full_path != full_path, "target leaf {full_path:?} found, even though value wasn't in values hashmap");
                    return Ok(LeafLookup::NonExistent);
                }
                SparseNode::Extension { key, .. } => {
                    if full_path.len() == curr_path.len() {
                        return Ok(LeafLookup::NonExistent);
                    }
                    curr_path.extend(key);
                    if !full_path.starts_with(&curr_path) {
                        return Ok(LeafLookup::NonExistent);
                    }
                }
                SparseNode::Branch { state_mask, blinded_mask, blinded_hashes, .. } => {
                    if full_path.len() == curr_path.len() {
                        return Ok(LeafLookup::NonExistent);
                    }
                    let nibble = full_path.get_unchecked(curr_path.len());
                    if !state_mask.is_bit_set(nibble) {
                        return Ok(LeafLookup::NonExistent);
                    }
                    curr_path.push_unchecked(nibble);
                    if blinded_mask.is_bit_set(nibble) {
                        return Err(LeafLookupError::BlindedNode {
                            path: curr_path,
                            hash: blinded_hashes[nibble as usize],
                        });
                    }
                }
            }

            // If we were previously looking at the upper trie, and the new path is in the
            // lower trie, we need to pull out a ref to the lower trie.
            if curr_subtrie_is_upper &&
                let Some(lower_subtrie) = self.lower_subtrie_for_path(&curr_path)
            {
                curr_subtrie = lower_subtrie;
                curr_subtrie_is_upper = false;
            }
        }
    }

    fn shrink_nodes_to(&mut self, size: usize) {
        // Distribute the capacity across upper and lower subtries
        //
        // Always include upper subtrie, plus any lower subtries
        let total_subtries = 1 + NUM_LOWER_SUBTRIES;
        let size_per_subtrie = size / total_subtries;

        // Shrink the upper subtrie
        self.upper_subtrie.shrink_nodes_to(size_per_subtrie);

        // Shrink lower subtries (works for both revealed and blind with allocation)
        for subtrie in &mut *self.lower_subtries {
            subtrie.shrink_nodes_to(size_per_subtrie);
        }

        // shrink masks map
        self.branch_node_masks.shrink_to(size);
    }

    fn shrink_values_to(&mut self, size: usize) {
        // Distribute the capacity across upper and lower subtries
        //
        // Always include upper subtrie, plus any lower subtries
        let total_subtries = 1 + NUM_LOWER_SUBTRIES;
        let size_per_subtrie = size / total_subtries;

        // Shrink the upper subtrie
        self.upper_subtrie.shrink_values_to(size_per_subtrie);

        // Shrink lower subtries (works for both revealed and blind with allocation)
        for subtrie in &mut *self.lower_subtries {
            subtrie.shrink_values_to(size_per_subtrie);
        }
    }

    /// O(1) size hint based on total node count (including hash stubs).
    fn size_hint(&self) -> usize {
        let upper_count = self.upper_subtrie.nodes.len();
        let lower_count: usize = self
            .lower_subtries
            .iter()
            .filter_map(|s| s.as_revealed_ref())
            .map(|s| s.nodes.len())
            .sum();
        upper_count + lower_count
    }

    fn memory_size(&self) -> usize {
        self.memory_size()
    }

    fn prune(&mut self, retained_leaves: &[Nibbles]) -> usize {
        #[cfg(feature = "trie-debug")]
        self.debug_recorder.reset();

        let mut retained_leaves = retained_leaves.to_vec();
        retained_leaves.sort_unstable();

        let mut effective_pruned_roots = Vec::<Nibbles>::new();
        let mut stack: SmallVec<[Nibbles; 32]> = SmallVec::new();
        stack.push(Nibbles::default());

        while let Some(path) = stack.pop() {
            let Some(node) =
                self.subtrie_for_path(&path).and_then(|subtrie| subtrie.nodes.get(&path).cloned())
            else {
                continue;
            };

            match node {
                SparseNode::Empty | SparseNode::Leaf { .. } => {}
                SparseNode::Extension { key, state, .. } => {
                    let mut child = path;
                    child.extend(&key);

                    if has_retained_descendant(&retained_leaves, &child) {
                        stack.push(child);
                        continue;
                    }

                    // Root extension has no parent branch edge to blind; keep it as-is.
                    if path.is_empty() {
                        continue;
                    }

                    let Some(hash) = state.cached_hash() else { continue };
                    self.subtrie_for_path_mut_untracked(&path)
                        .expect("node subtrie exists")
                        .nodes
                        .remove(&path);

                    let parent_path = path.slice(0..path.len() - 1);
                    // Parent can live in a different subtrie when `path` is the root of a lower
                    // subtrie, so resolve it by `parent_path` rather than reusing `path`'s subtrie.
                    let SparseNode::Branch { blinded_mask, blinded_hashes, .. } = self
                        .subtrie_for_path_mut_untracked(&parent_path)
                        .expect("parent subtrie exists")
                        .nodes
                        .get_mut(&parent_path)
                        .expect("expected parent branch node")
                    else {
                        panic!("expected branch node at path {parent_path:?}");
                    };

                    let nibble = path.last().unwrap();
                    blinded_mask.set_bit(nibble);
                    blinded_hashes[nibble as usize] = hash;
                    effective_pruned_roots.push(path);
                }
                SparseNode::Branch { state_mask, blinded_mask, blinded_hashes, .. } => {
                    let mut blinded_mask = blinded_mask;
                    let mut blinded_hashes = blinded_hashes;
                    for nibble in state_mask.iter() {
                        if blinded_mask.is_bit_set(nibble) {
                            continue;
                        }

                        let mut child = path;
                        child.push_unchecked(nibble);
                        if has_retained_descendant(&retained_leaves, &child) {
                            stack.push(child);
                            continue;
                        }

                        let Entry::Occupied(entry) =
                            self.subtrie_for_path_mut_untracked(&child).unwrap().nodes.entry(child)
                        else {
                            panic!("expected node at path {child:?}");
                        };

                        let Some(hash) = entry.get().cached_hash() else {
                            continue;
                        };
                        entry.remove();
                        blinded_mask.set_bit(nibble);
                        blinded_hashes[nibble as usize] = hash;
                        effective_pruned_roots.push(child);
                    }

                    let SparseNode::Branch {
                        blinded_mask: old_blinded_mask,
                        blinded_hashes: old_blinded_hashes,
                        ..
                    } = self
                        .subtrie_for_path_mut_untracked(&path)
                        .unwrap()
                        .nodes
                        .get_mut(&path)
                        .unwrap()
                    else {
                        unreachable!("expected branch node at path {path:?}");
                    };
                    *old_blinded_mask = blinded_mask;
                    *old_blinded_hashes = blinded_hashes;
                }
            }
        }

        Self::finalize_pruned_roots(self, effective_pruned_roots)
    }

    fn update_leaves(
        &mut self,
        updates: &mut alloy_primitives::map::B256Map<crate::LeafUpdate>,
        mut proof_required_fn: impl FnMut(B256, u8),
    ) -> SparseTrieResult<()> {
        use crate::{provider::NoRevealProvider, LeafUpdate};

        #[cfg(feature = "trie-debug")]
        let recorded_updates: Vec<_> =
            updates.iter().map(|(k, v)| (*k, LeafUpdateRecord::from(v))).collect();
        #[cfg(feature = "trie-debug")]
        let mut recorded_proof_targets: Vec<(B256, u8)> = Vec::new();

        // Drain updates to avoid cloning keys while preserving the map's allocation.
        // On success, entries remain removed; on blinded node failure, they're re-inserted.
        let drained: Vec<_> = updates.drain().collect();

        for (key, update) in drained {
            let full_path = Nibbles::unpack(key);

            match update {
                LeafUpdate::Changed(value) => {
                    if value.is_empty() {
                        // Removal: remove_leaf with NoRevealProvider is atomic - returns a
                        // retriable error before any mutations (via pre_validate_reveal_chain).
                        match self.remove_leaf(&full_path, NoRevealProvider) {
                            Ok(()) => {}
                            Err(e) => {
                                if let Some(path) = Self::get_retriable_path(&e) {
                                    let (target_key, min_len) =
                                        Self::proof_target_for_path(key, &full_path, &path);
                                    proof_required_fn(target_key, min_len);
                                    #[cfg(feature = "trie-debug")]
                                    recorded_proof_targets.push((target_key, min_len));
                                    updates.insert(key, LeafUpdate::Changed(value));
                                } else {
                                    return Err(e);
                                }
                            }
                        }
                    } else {
                        // Update/insert: update_leaf is atomic - cleans up on error.
                        if let Err(e) = self.update_leaf(full_path, value.clone(), NoRevealProvider)
                        {
                            if let Some(path) = Self::get_retriable_path(&e) {
                                let (target_key, min_len) =
                                    Self::proof_target_for_path(key, &full_path, &path);
                                proof_required_fn(target_key, min_len);
                                #[cfg(feature = "trie-debug")]
                                recorded_proof_targets.push((target_key, min_len));
                                updates.insert(key, LeafUpdate::Changed(value));
                            } else {
                                return Err(e);
                            }
                        }
                    }
                }
                LeafUpdate::Touched => {
                    // Touched is read-only: check if path is accessible, request proof if blinded.
                    match self.find_leaf(&full_path, None) {
                        Err(LeafLookupError::BlindedNode { path, .. }) => {
                            let (target_key, min_len) =
                                Self::proof_target_for_path(key, &full_path, &path);
                            proof_required_fn(target_key, min_len);
                            #[cfg(feature = "trie-debug")]
                            recorded_proof_targets.push((target_key, min_len));
                            updates.insert(key, LeafUpdate::Touched);
                        }
                        // Path is fully revealed (exists or proven non-existent), no action needed.
                        Ok(_) | Err(LeafLookupError::ValueMismatch { .. }) => {}
                    }
                }
            }
        }

        #[cfg(feature = "trie-debug")]
        self.debug_recorder.record(RecordedOp::UpdateLeaves {
            updates: recorded_updates,
            proof_targets: recorded_proof_targets,
        });

        Ok(())
    }

    #[cfg(feature = "trie-debug")]
    fn take_debug_recorder(&mut self) -> TrieDebugRecorder {
        core::mem::take(&mut self.debug_recorder)
    }

    fn commit_updates(
        &mut self,
        updated: &HashMap<Nibbles, BranchNodeCompact>,
        removed: &HashSet<Nibbles>,
    ) {
        // Sync branch_node_masks with what's being committed to DB.
        // This ensures that on subsequent root() calls, the masks reflect the actual
        // DB state, which is needed for correct removal detection.
        self.branch_node_masks.reserve(updated.len());
        for (path, node) in updated {
            self.branch_node_masks.insert(
                *path,
                BranchNodeMasks { tree_mask: node.tree_mask, hash_mask: node.hash_mask },
            );
        }
        for path in removed {
            self.branch_node_masks.remove(path);
        }
    }
}

impl ParallelSparseTrie {
    /// Sets the thresholds that control when parallelism is used during operations.
    pub const fn with_parallelism_thresholds(mut self, thresholds: ParallelismThresholds) -> Self {
        self.parallelism_thresholds = thresholds;
        self
    }

    /// Returns true if retaining updates is enabled for the overall trie.
    const fn updates_enabled(&self) -> bool {
        self.updates.is_some()
    }

    /// Returns true if parallelism should be enabled for revealing the given number of nodes.
    /// Will always return false in nostd builds.
    const fn is_reveal_parallelism_enabled(&self, num_nodes: usize) -> bool {
        #[cfg(not(feature = "std"))]
        {
            let _ = num_nodes;
            return false;
        }

        #[cfg(feature = "std")]
        {
            num_nodes >= self.parallelism_thresholds.min_revealed_nodes
        }
    }

    /// Returns true if parallelism should be enabled for updating hashes with the given number
    /// of changed keys. Will always return false in nostd builds.
    const fn is_update_parallelism_enabled(&self, num_changed_keys: usize) -> bool {
        #[cfg(not(feature = "std"))]
        {
            let _ = num_changed_keys;
            return false;
        }

        #[cfg(feature = "std")]
        {
            num_changed_keys >= self.parallelism_thresholds.min_updated_nodes
        }
    }

    /// Checks if an error is retriable (`BlindedNode` or `NodeNotFoundInProvider`) and extracts
    /// the path if so.
    ///
    /// Both error types indicate that a node needs to be revealed before the operation can
    /// succeed. `BlindedNode` occurs when traversing to a Hash node, while `NodeNotFoundInProvider`
    /// occurs when `retain_updates` is enabled and an extension node's child needs revealing.
    const fn get_retriable_path(e: &SparseTrieError) -> Option<Nibbles> {
        match e.kind() {
            SparseTrieErrorKind::BlindedNode(path) |
            SparseTrieErrorKind::NodeNotFoundInProvider { path } => Some(*path),
            _ => None,
        }
    }

    /// Converts a nibbles path to a B256, right-padding with zeros to 64 nibbles.
    fn nibbles_to_padded_b256(path: &Nibbles) -> B256 {
        let mut bytes = [0u8; 32];
        path.pack_to(&mut bytes);
        B256::from(bytes)
    }

    /// Computes the proof target key and `min_len` for a blinded node error.
    ///
    /// Returns `(target_key, min_len)` where:
    /// - `target_key` is `full_key` if `path` is a prefix of `full_path`, otherwise the padded path
    /// - `min_len` is always based on `path.len()`
    fn proof_target_for_path(full_key: B256, full_path: &Nibbles, path: &Nibbles) -> (B256, u8) {
        let min_len = (path.len() as u8).min(64);
        let target_key =
            if full_path.starts_with(path) { full_key } else { Self::nibbles_to_padded_b256(path) };
        (target_key, min_len)
    }

    /// Creates a new revealed sparse trie from the given root node.
    ///
    /// This function initializes the internal structures and then reveals the root.
    /// It is a convenient method to create a trie when you already have the root node available.
    ///
    /// # Arguments
    ///
    /// * `root` - The root node of the trie
    /// * `masks` - Trie masks for root branch node
    /// * `retain_updates` - Whether to track updates
    ///
    /// # Returns
    ///
    /// Self if successful, or an error if revealing fails.
    pub fn from_root(
        root: TrieNodeV2,
        masks: Option<BranchNodeMasks>,
        retain_updates: bool,
    ) -> SparseTrieResult<Self> {
        Self::default().with_root(root, masks, retain_updates)
    }

    fn finalize_pruned_roots(&mut self, mut effective_pruned_roots: Vec<Nibbles>) -> usize {
        if effective_pruned_roots.is_empty() {
            return 0;
        }

        let nodes_converted = effective_pruned_roots.len();

        // Sort roots by subtrie type (upper first), then by path for efficient partitioning.
        effective_pruned_roots.sort_unstable_by(|path_a, path_b| {
            let subtrie_type_a = SparseSubtrieType::from_path(path_a);
            let subtrie_type_b = SparseSubtrieType::from_path(path_b);
            subtrie_type_a.cmp(&subtrie_type_b).then(path_a.cmp(path_b))
        });

        // Split off upper subtrie roots (they come first due to sorting)
        let num_upper_roots = effective_pruned_roots
            .iter()
            .position(|p| !SparseSubtrieType::path_len_is_upper(p.len()))
            .unwrap_or(effective_pruned_roots.len());

        let roots_upper = &effective_pruned_roots[..num_upper_roots];
        let roots_lower = &effective_pruned_roots[num_upper_roots..];

        debug_assert!(
            {
                let mut all_roots: Vec<_> = effective_pruned_roots.clone();
                all_roots.sort_unstable();
                all_roots.windows(2).all(|w| !w[1].starts_with(&w[0]))
            },
            "prune roots must be prefix-free"
        );

        // Upper prune roots that are prefixes of lower subtrie root paths cause the entire
        // subtrie to be cleared (preserving allocations for reuse).
        if !roots_upper.is_empty() {
            for subtrie in &mut *self.lower_subtries {
                let should_clear = subtrie.as_revealed_ref().is_some_and(|s| {
                    let search_idx = roots_upper.partition_point(|root| root <= &s.path);
                    search_idx > 0 && s.path.starts_with(&roots_upper[search_idx - 1])
                });
                if should_clear {
                    subtrie.clear();
                }
            }
        }

        // Upper subtrie: prune nodes and values
        self.upper_subtrie.nodes.retain(|p, _| !is_strict_descendant_in(roots_upper, p));
        self.upper_subtrie.inner.values.retain(|p, _| {
            !starts_with_pruned_in(roots_upper, p) && !starts_with_pruned_in(roots_lower, p)
        });

        // Process lower subtries using chunk_by to group roots by subtrie
        for roots_group in roots_lower.chunk_by(|path_a, path_b| {
            SparseSubtrieType::from_path(path_a) == SparseSubtrieType::from_path(path_b)
        }) {
            let subtrie_idx = path_subtrie_index_unchecked(&roots_group[0]);

            // Skip unrevealed/blinded subtries - nothing to prune.
            let should_clear = {
                let Some(subtrie) = self.lower_subtries[subtrie_idx].as_revealed_mut() else {
                    continue;
                };

                // Retain only nodes/values not descended from any pruned root.
                subtrie.nodes.retain(|p, _| !is_strict_descendant_in(roots_group, p));
                subtrie.inner.values.retain(|p, _| !starts_with_pruned_in(roots_group, p));

                // If prune removed the node at `subtrie.path`, the subtrie can no longer be
                // represented as revealed and must be blinded.
                !subtrie.nodes.contains_key(&subtrie.path)
            };

            if should_clear {
                self.lower_subtries[subtrie_idx].clear();
            }
        }

        // Branch node masks pruning
        self.branch_node_masks.retain(|p, _| {
            if SparseSubtrieType::path_len_is_upper(p.len()) {
                !starts_with_pruned_in(roots_upper, p)
            } else {
                !starts_with_pruned_in(roots_lower, p) && !starts_with_pruned_in(roots_upper, p)
            }
        });

        nodes_converted
    }

    /// Returns a reference to the lower `SparseSubtrie` for the given path, or None if the
    /// path belongs to the upper trie, or if the lower subtrie for the path doesn't exist or is
    /// blinded.
    fn lower_subtrie_for_path(&self, path: &Nibbles) -> Option<&SparseSubtrie> {
        match SparseSubtrieType::from_path(path) {
            SparseSubtrieType::Upper => None,
            SparseSubtrieType::Lower(idx) => self.lower_subtries[idx].as_revealed_ref(),
        }
    }

    /// Returns a mutable reference to the lower `SparseSubtrie` for the given path, or None if the
    /// path belongs to the upper trie.
    ///
    /// This method will create/reveal a new lower subtrie for the given path if one isn't already.
    /// If one does exist, but its path field is longer than the given path, then the field will be
    /// set to the given path.
    fn lower_subtrie_for_path_mut(&mut self, path: &Nibbles) -> Option<&mut SparseSubtrie> {
        match SparseSubtrieType::from_path(path) {
            SparseSubtrieType::Upper => None,
            SparseSubtrieType::Lower(idx) => {
                self.lower_subtries[idx].reveal(path);
                Some(self.lower_subtries[idx].as_revealed_mut().expect("just revealed"))
            }
        }
    }

    /// Returns a reference to either the lower or upper `SparseSubtrie` for the given path,
    /// depending on the path's length.
    ///
    /// Returns `None` if a lower subtrie does not exist for the given path.
    fn subtrie_for_path(&self, path: &Nibbles) -> Option<&SparseSubtrie> {
        if SparseSubtrieType::path_len_is_upper(path.len()) {
            Some(&self.upper_subtrie)
        } else {
            self.lower_subtrie_for_path(path)
        }
    }

    /// Returns a mutable reference to either the lower or upper `SparseSubtrie` for the given path,
    /// depending on the path's length.
    ///
    /// This method will create/reveal a new lower subtrie for the given path if one isn't already.
    /// If one does exist, but its path field is longer than the given path, then the field will be
    /// set to the given path.
    fn subtrie_for_path_mut(&mut self, path: &Nibbles) -> &mut SparseSubtrie {
        // We can't just call `lower_subtrie_for_path` and return `upper_subtrie` if it returns
        // None, because Rust complains about double mutable borrowing `self`.
        if SparseSubtrieType::path_len_is_upper(path.len()) {
            &mut self.upper_subtrie
        } else {
            self.lower_subtrie_for_path_mut(path).unwrap()
        }
    }

    /// Returns a mutable reference to a subtrie without marking it as modified.
    /// Used for internal operations like pruning that shouldn't affect heat tracking.
    fn subtrie_for_path_mut_untracked(&mut self, path: &Nibbles) -> Option<&mut SparseSubtrie> {
        if SparseSubtrieType::path_len_is_upper(path.len()) {
            Some(&mut self.upper_subtrie)
        } else {
            match SparseSubtrieType::from_path(path) {
                SparseSubtrieType::Upper => None,
                SparseSubtrieType::Lower(idx) => self.lower_subtries[idx].as_revealed_mut(),
            }
        }
    }

    /// Returns the next node in the traversal path from the given path towards the leaf for the
    /// given full leaf path, or an error if any node along the traversal path is not revealed.
    ///
    ///
    /// ## Panics
    ///
    /// If `from_path` is not a prefix of `leaf_full_path`.
    fn find_next_to_leaf(
        from_path: &Nibbles,
        from_node: &SparseNode,
        leaf_full_path: &Nibbles,
    ) -> FindNextToLeafOutcome {
        debug_assert!(leaf_full_path.len() >= from_path.len());
        debug_assert!(leaf_full_path.starts_with(from_path));

        match from_node {
            // If empty node is found it means the subtrie doesn't have any nodes in it, let alone
            // the target leaf.
            SparseNode::Empty => FindNextToLeafOutcome::NotFound,
            SparseNode::Leaf { key, .. } => {
                let mut found_full_path = *from_path;
                found_full_path.extend(key);

                if &found_full_path == leaf_full_path {
                    return FindNextToLeafOutcome::Found;
                }
                FindNextToLeafOutcome::NotFound
            }
            SparseNode::Extension { key, .. } => {
                if leaf_full_path.len() == from_path.len() {
                    return FindNextToLeafOutcome::NotFound;
                }

                let mut child_path = *from_path;
                child_path.extend(key);

                if !leaf_full_path.starts_with(&child_path) {
                    return FindNextToLeafOutcome::NotFound;
                }
                FindNextToLeafOutcome::ContinueFrom(child_path)
            }
            SparseNode::Branch { state_mask, blinded_mask, .. } => {
                if leaf_full_path.len() == from_path.len() {
                    return FindNextToLeafOutcome::NotFound;
                }

                let nibble = leaf_full_path.get_unchecked(from_path.len());
                if !state_mask.is_bit_set(nibble) {
                    return FindNextToLeafOutcome::NotFound;
                }

                let mut child_path = *from_path;
                child_path.push_unchecked(nibble);

                if blinded_mask.is_bit_set(nibble) {
                    return FindNextToLeafOutcome::BlindedNode(child_path);
                }

                FindNextToLeafOutcome::ContinueFrom(child_path)
            }
        }
    }

    /// Called when a child node has collapsed into its parent as part of `remove_leaf`. If the
    /// new parent node is a leaf, then the previous child also was, and if the previous child was
    /// on a lower subtrie while the parent is on an upper then the leaf value needs to be moved to
    /// the upper.
    fn move_value_on_leaf_removal(
        &mut self,
        parent_path: &Nibbles,
        new_parent_node: &SparseNode,
        prev_child_path: &Nibbles,
    ) {
        // If the parent path isn't in the upper then it doesn't matter what the new node is,
        // there's no situation where a leaf value needs to be moved.
        if SparseSubtrieType::from_path(parent_path).lower_index().is_some() {
            return;
        }

        if let SparseNode::Leaf { key, .. } = new_parent_node {
            let Some(prev_child_subtrie) = self.lower_subtrie_for_path_mut(prev_child_path) else {
                return;
            };

            let mut leaf_full_path = *parent_path;
            leaf_full_path.extend(key);

            let val = prev_child_subtrie.inner.values.remove(&leaf_full_path).expect("ParallelSparseTrie is in an inconsistent state, expected value on subtrie which wasn't found");
            self.upper_subtrie.inner.values.insert(leaf_full_path, val);
        }
    }

    /// Used by `remove_leaf` to ensure that when a node is removed from a lower subtrie that any
    /// externalities are handled. These can include:
    /// - Removing the lower subtrie completely, if it is now empty.
    /// - Updating the `path` field of the lower subtrie to indicate that its root node has changed.
    ///
    /// This method assumes that the caller will deal with putting all other nodes in the trie into
    /// a consistent state after the removal of this one.
    ///
    /// ## Panics
    ///
    /// - If the removed node was not a leaf or extension.
    fn remove_node(&mut self, path: &Nibbles) {
        let subtrie = self.subtrie_for_path_mut(path);
        let node = subtrie.nodes.remove(path);

        let Some(idx) = SparseSubtrieType::from_path(path).lower_index() else {
            // When removing a node from the upper trie there's nothing special we need to do to fix
            // its path field; the upper trie's path is always empty.
            return;
        };

        match node {
            Some(SparseNode::Leaf { .. }) => {
                // If the leaf was the final node in its lower subtrie then we can blind the
                // subtrie, effectively marking it as empty.
                if subtrie.nodes.is_empty() {
                    self.lower_subtries[idx].clear();
                }
            }
            Some(SparseNode::Extension { key, .. }) => {
                // If the removed extension was the root node of a lower subtrie then the lower
                // subtrie's `path` needs to be updated to be whatever node the extension used to
                // point to.
                if &subtrie.path == path {
                    subtrie.path.extend(&key);
                }
            }
            _ => panic!("Expected to remove a leaf or extension, but removed {node:?}"),
        }
    }

    /// Given the path to a parent branch node and a child node which is the sole remaining child on
    /// that branch after removing a leaf, returns a node to replace the parent branch node and a
    /// boolean indicating if the child should be deleted.
    ///
    /// ## Panics
    ///
    /// - If either parent or child node is not already revealed.
    /// - If parent's path is not a prefix of the child's path.
    fn branch_changes_on_leaf_removal(
        parent_path: &Nibbles,
        remaining_child_path: &Nibbles,
        remaining_child_node: &SparseNode,
    ) -> (SparseNode, bool) {
        debug_assert!(remaining_child_path.len() > parent_path.len());
        debug_assert!(remaining_child_path.starts_with(parent_path));

        let remaining_child_nibble = remaining_child_path.get_unchecked(parent_path.len());

        // If we swap the branch node out either an extension or leaf, depending on
        // what its remaining child is.
        match remaining_child_node {
            SparseNode::Empty => {
                panic!("remaining child must have been revealed already")
            }
            // If the only child is a leaf node, we downgrade the branch node into a
            // leaf node, prepending the nibble to the key, and delete the old
            // child.
            SparseNode::Leaf { key, .. } => {
                let mut new_key = Nibbles::from_nibbles_unchecked([remaining_child_nibble]);
                new_key.extend(key);
                (SparseNode::new_leaf(new_key), true)
            }
            // If the only child node is an extension node, we downgrade the branch
            // node into an even longer extension node, prepending the nibble to the
            // key, and delete the old child.
            SparseNode::Extension { key, .. } => {
                let mut new_key = Nibbles::from_nibbles_unchecked([remaining_child_nibble]);
                new_key.extend(key);
                (SparseNode::new_ext(new_key), true)
            }
            // If the only child is a branch node, we downgrade the current branch
            // node into a one-nibble extension node.
            SparseNode::Branch { .. } => (
                SparseNode::new_ext(Nibbles::from_nibbles_unchecked([remaining_child_nibble])),
                false,
            ),
        }
    }

    /// Given the path to a parent extension and its key, and a child node (not necessarily on this
    /// subtrie), returns an optional replacement parent node. If a replacement is returned then the
    /// child node should be deleted.
    ///
    /// ## Panics
    ///
    /// - If either parent or child node is not already revealed.
    /// - If parent's path is not a prefix of the child's path.
    fn extension_changes_on_leaf_removal(
        parent_path: &Nibbles,
        parent_key: &Nibbles,
        child_path: &Nibbles,
        child: &SparseNode,
    ) -> Option<SparseNode> {
        debug_assert!(child_path.len() > parent_path.len());
        debug_assert!(child_path.starts_with(parent_path));

        // If the parent node is an extension node, we need to look at its child to see
        // if we need to merge it.
        match child {
            SparseNode::Empty => {
                panic!("child must be revealed")
            }
            // For a leaf node, we collapse the extension node into a leaf node,
            // extending the key. While it's impossible to encounter an extension node
            // followed by a leaf node in a complete trie, it's possible here because we
            // could have downgraded the extension node's child into a leaf node from a
            // branch in a previous call to `branch_changes_on_leaf_removal`.
            SparseNode::Leaf { key, .. } => {
                let mut new_key = *parent_key;
                new_key.extend(key);
                Some(SparseNode::new_leaf(new_key))
            }
            // Similar to the leaf node, for an extension node, we collapse them into one
            // extension node, extending the key.
            SparseNode::Extension { key, .. } => {
                let mut new_key = *parent_key;
                new_key.extend(key);
                Some(SparseNode::new_ext(new_key))
            }
            // For a branch node, we just leave the extension node as-is.
            SparseNode::Branch { .. } => None,
        }
    }

    /// Drains any [`SparseTrieUpdatesAction`]s from the given subtrie, and applies each action to
    /// the given `updates` set. If the given set is None then this is a no-op.
    #[instrument(level = "trace", target = "trie::parallel_sparse", skip_all)]
    fn apply_subtrie_update_actions(
        &mut self,
        update_actions: impl Iterator<Item = SparseTrieUpdatesAction>,
    ) {
        if let Some(updates) = self.updates.as_mut() {
            let additional = update_actions.size_hint().0;
            updates.updated_nodes.reserve(additional);
            updates.removed_nodes.reserve(additional);
            for action in update_actions {
                match action {
                    SparseTrieUpdatesAction::InsertRemoved(path) => {
                        updates.updated_nodes.remove(&path);
                        updates.removed_nodes.insert(path);
                    }
                    SparseTrieUpdatesAction::RemoveUpdated(path) => {
                        updates.updated_nodes.remove(&path);
                    }
                    SparseTrieUpdatesAction::InsertUpdated(path, branch_node) => {
                        updates.updated_nodes.insert(path, branch_node);
                        updates.removed_nodes.remove(&path);
                    }
                }
            }
        };
    }

    /// Updates hashes for the upper subtrie, using nodes from both upper and lower subtries.
    #[instrument(level = "trace", target = "trie::parallel_sparse", skip_all, ret)]
    fn update_upper_subtrie_hashes(&mut self, prefix_set: &mut PrefixSet) -> RlpNode {
        trace!(target: "trie::parallel_sparse", "Updating upper subtrie hashes");

        debug_assert!(self.upper_subtrie.inner.buffers.path_stack.is_empty());
        self.upper_subtrie.inner.buffers.path_stack.push(RlpNodePathStackItem {
            path: Nibbles::default(), // Start from root
            is_in_prefix_set: None,
        });

        #[cfg(feature = "metrics")]
        let start = Instant::now();

        let mut update_actions_buf =
            self.updates_enabled().then(|| self.update_actions_buffers.pop().unwrap_or_default());

        while let Some(stack_item) = self.upper_subtrie.inner.buffers.path_stack.pop() {
            let path = stack_item.path;
            let node = if path.len() < UPPER_TRIE_MAX_DEPTH {
                self.upper_subtrie.nodes.get_mut(&path).expect("upper subtrie node must exist")
            } else {
                let index = path_subtrie_index_unchecked(&path);
                let node = self.lower_subtries[index]
                    .as_revealed_mut()
                    .expect("lower subtrie must exist")
                    .nodes
                    .get_mut(&path)
                    .expect("lower subtrie node must exist");
                // Lower subtrie root node RLP nodes must be computed before updating upper subtrie
                // hashes
                debug_assert!(
                    node.cached_rlp_node().is_some(),
                    "Lower subtrie root node {node:?} at path {path:?} has no cached RLP node"
                );
                node
            };

            // Calculate the RLP node for the current node using upper subtrie
            self.upper_subtrie.inner.rlp_node(
                prefix_set,
                &mut update_actions_buf,
                stack_item,
                node,
                &self.branch_node_masks,
            );
        }

        // If there were any branch node updates as a result of calculating the RLP node for the
        // upper trie then apply them to the top-level set.
        if let Some(mut update_actions_buf) = update_actions_buf {
            self.apply_subtrie_update_actions(
                #[expect(clippy::iter_with_drain)]
                update_actions_buf.drain(..),
            );
            self.update_actions_buffers.push(update_actions_buf);
        }

        #[cfg(feature = "metrics")]
        self.metrics.subtrie_upper_hash_latency.record(start.elapsed());

        debug_assert_eq!(self.upper_subtrie.inner.buffers.rlp_node_stack.len(), 1);
        self.upper_subtrie.inner.buffers.rlp_node_stack.pop().unwrap().rlp_node
    }

    /// Returns:
    /// 1. List of lower [subtries](SparseSubtrie) that have changed according to the provided
    ///    [prefix set](PrefixSet). See documentation of [`ChangedSubtrie`] for more details. Lower
    ///    subtries whose root node is missing a hash will also be returned; this is required to
    ///    handle cases where extensions/leafs get shortened and therefore moved from the upper to a
    ///    lower subtrie.
    /// 2. Prefix set of keys that do not belong to any lower subtrie.
    ///
    /// This method helps optimize hash recalculations by identifying which specific
    /// lower subtries need to be updated. Each lower subtrie can then be updated in parallel.
    ///
    /// IMPORTANT: The method removes the subtries from `lower_subtries`, and the caller is
    /// responsible for returning them back into the array.
    #[instrument(level = "trace", target = "trie::parallel_sparse", skip_all, fields(prefix_set_len = prefix_set.len()))]
    fn take_changed_lower_subtries(
        &mut self,
        prefix_set: &mut PrefixSet,
    ) -> (Vec<ChangedSubtrie>, PrefixSetMut) {
        // Fast-path: If the prefix set is empty then no subtries can have been changed. Just return
        // empty values.
        if prefix_set.is_empty() {
            return Default::default();
        }

        // Clone the prefix set to iterate over its keys. Cloning is cheap, it's just an Arc.
        let prefix_set_clone = prefix_set.clone();
        let mut prefix_set_iter = prefix_set_clone.into_iter().copied().peekable();
        let mut changed_subtries = Vec::new();
        let mut unchanged_prefix_set = PrefixSetMut::default();
        let updates_enabled = self.updates_enabled();

        for (index, subtrie) in self.lower_subtries.iter_mut().enumerate() {
            if let Some(subtrie) = subtrie.take_revealed_if(|subtrie| {
                prefix_set.contains(&subtrie.path) ||
                    subtrie
                        .nodes
                        .get(&subtrie.path)
                        .is_some_and(|n| n.cached_rlp_node().is_none())
            }) {
                let prefix_set = if prefix_set.all() {
                    unchanged_prefix_set = PrefixSetMut::all();
                    PrefixSetMut::all()
                } else {
                    // Take those keys from the original prefix set that start with the subtrie path
                    //
                    // Subtries are stored in the order of their paths, so we can use the same
                    // prefix set iterator.
                    let mut new_prefix_set = Vec::new();
                    while let Some(key) = prefix_set_iter.peek() {
                        if key.starts_with(&subtrie.path) {
                            // If the key starts with the subtrie path, add it to the new prefix set
                            new_prefix_set.push(prefix_set_iter.next().unwrap());
                        } else if new_prefix_set.is_empty() && key < &subtrie.path {
                            // If we didn't yet have any keys that belong to this subtrie, and the
                            // current key is still less than the subtrie path, add it to the
                            // unchanged prefix set
                            unchanged_prefix_set.insert(prefix_set_iter.next().unwrap());
                        } else {
                            // If we're past the subtrie path, we're done with this subtrie. Do not
                            // advance the iterator, the next key will be processed either by the
                            // next subtrie or inserted into the unchanged prefix set.
                            break;
                        }
                    }
                    PrefixSetMut::from(new_prefix_set)
                }
                .freeze();

                // We need the full path of root node of the lower subtrie to the unchanged prefix
                // set, so that we don't skip it when calculating hashes for the upper subtrie.
                match subtrie.nodes.get(&subtrie.path) {
                    Some(SparseNode::Extension { key, .. } | SparseNode::Leaf { key, .. }) => {
                        unchanged_prefix_set.insert(subtrie.path.join(key));
                    }
                    Some(SparseNode::Branch { .. }) => {
                        unchanged_prefix_set.insert(subtrie.path);
                    }
                    _ => {}
                }

                let update_actions_buf =
                    updates_enabled.then(|| self.update_actions_buffers.pop().unwrap_or_default());

                changed_subtries.push(ChangedSubtrie {
                    index,
                    subtrie,
                    prefix_set,
                    update_actions_buf,
                });
            }
        }

        // Extend the unchanged prefix set with the remaining keys that are not part of any subtries
        unchanged_prefix_set.extend_keys(prefix_set_iter);

        (changed_subtries, unchanged_prefix_set)
    }

    /// Reveals a trie node in the upper trie if it has not been revealed before. When revealing
    /// branch/extension nodes this may recurse into a lower trie to reveal a child.
    ///
    /// This function decodes a trie node and inserts it into the trie structure. It handles
    /// different node types (leaf, extension, branch) by appropriately adding them to the trie and
    /// recursively revealing their children.
    ///
    /// # Arguments
    ///
    /// * `path` - The path where the node should be revealed
    /// * `node` - The trie node to reveal
    /// * `masks` - Branch node masks if known
    ///
    /// # Returns
    ///
    /// `Ok(())` if successful, or an error if the node was not revealed.
    fn reveal_upper_node(
        &mut self,
        path: Nibbles,
        node: &TrieNodeV2,
        masks: Option<BranchNodeMasks>,
    ) -> SparseTrieResult<()> {
        // Only reveal nodes that can be reached given the current state of the upper trie. If they
        // can't be reached, it means that they were removed.
        if !self.is_path_reachable_from_upper(&path) {
            return Ok(());
        }

        // Exit early if the node was already revealed before.
        if !self.upper_subtrie.reveal_node(path, node, masks, None)? {
            if let TrieNodeV2::Branch(branch) = node {
                if branch.key.is_empty() {
                    return Ok(());
                }

                // We might still potentially need to reveal a child branch node in the lower
                // subtrie, even if the upper subtrie already knew about the extension node.
                if SparseSubtrieType::path_len_is_upper(path.len() + branch.key.len()) {
                    return Ok(());
                }
            } else {
                return Ok(());
            }
        }

        // The previous upper_trie.reveal_node call will not have revealed any child nodes via
        // reveal_node_or_hash if the child node would be found on a lower subtrie. We handle that
        // here by manually checking the specific cases where this could happen, and calling
        // reveal_node_or_hash for each.
        match node {
            TrieNodeV2::Branch(branch) => {
                let mut branch_path = path;
                branch_path.extend(&branch.key);

                // If only the parent extension belongs to the upper trie, we need to reveal the
                // actual branch node in the corresponding lower subtrie.
                if !SparseSubtrieType::path_len_is_upper(branch_path.len()) {
                    self.lower_subtrie_for_path_mut(&branch_path)
                        .expect("branch_path must have a lower subtrie")
                        .reveal_branch(
                            branch_path,
                            branch.state_mask,
                            &branch.stack,
                            masks,
                            branch.branch_rlp_node.clone(),
                        )?
                } else if !SparseSubtrieType::path_len_is_upper(branch_path.len() + 1) {
                    // If a branch is at the cutoff level of the trie then it will be in the upper
                    // trie, but all of its children will be in a lower trie.
                    // Check if a child node would be in the lower subtrie, and
                    // reveal accordingly.
                    for (stack_ptr, idx) in branch.state_mask.iter().enumerate() {
                        let mut child_path = branch_path;
                        child_path.push_unchecked(idx);
                        let child = &branch.stack[stack_ptr];

                        // Only reveal children that are not hashes. Hashes are stored on branch
                        // nodes directly.
                        if !child.is_hash() {
                            self.lower_subtrie_for_path_mut(&child_path)
                                .expect("child_path must have a lower subtrie")
                                .reveal_node(
                                    child_path,
                                    &TrieNodeV2::decode(&mut branch.stack[stack_ptr].as_ref())?,
                                    None,
                                    None,
                                )?;
                        }
                    }
                }
            }
            TrieNodeV2::Extension(ext) => {
                let mut child_path = path;
                child_path.extend(&ext.key);
                if let Some(subtrie) = self.lower_subtrie_for_path_mut(&child_path) {
                    subtrie.reveal_node(
                        child_path,
                        &TrieNodeV2::decode(&mut ext.child.as_ref())?,
                        None,
                        None,
                    )?;
                }
            }
            TrieNodeV2::EmptyRoot | TrieNodeV2::Leaf(_) => (),
        }

        Ok(())
    }

    /// Return updated subtries back to the trie after executing any actions required on the
    /// top-level `SparseTrieUpdates`.
    #[instrument(level = "trace", target = "trie::parallel_sparse", skip_all)]
    fn insert_changed_subtries(
        &mut self,
        changed_subtries: impl IntoIterator<Item = ChangedSubtrie>,
    ) {
        for ChangedSubtrie { index, subtrie, update_actions_buf, .. } in changed_subtries {
            if let Some(mut update_actions_buf) = update_actions_buf {
                self.apply_subtrie_update_actions(
                    #[expect(clippy::iter_with_drain)]
                    update_actions_buf.drain(..),
                );
                self.update_actions_buffers.push(update_actions_buf);
            }

            self.lower_subtries[index] = LowerSparseSubtrie::Revealed(subtrie);
        }
    }

    /// Returns a heuristic for the in-memory size of this trie in bytes.
    ///
    /// This is an approximation that accounts for:
    /// - The upper subtrie nodes and values
    /// - All revealed lower subtries nodes and values
    /// - The prefix set keys
    /// - The branch node masks map
    /// - Updates if retained
    /// - Update action buffers
    ///
    /// Note: Heap allocations for hash maps may be larger due to load factor overhead.
    pub fn memory_size(&self) -> usize {
        let mut size = core::mem::size_of::<Self>();

        // Upper subtrie
        size += self.upper_subtrie.memory_size();

        // Lower subtries (both Revealed and Blind with allocation)
        for subtrie in &*self.lower_subtries {
            size += subtrie.memory_size();
        }

        // Prefix set keys
        size += self.prefix_set.len() * core::mem::size_of::<Nibbles>();

        // Branch node masks map
        size += self.branch_node_masks.len() *
            (core::mem::size_of::<Nibbles>() + core::mem::size_of::<BranchNodeMasks>());

        // Updates if present
        if let Some(updates) = &self.updates {
            size += updates.updated_nodes.len() *
                (core::mem::size_of::<Nibbles>() + core::mem::size_of::<BranchNodeCompact>());
            size += updates.removed_nodes.len() * core::mem::size_of::<Nibbles>();
        }

        // Update actions buffers
        for buf in &self.update_actions_buffers {
            size += buf.capacity() * core::mem::size_of::<SparseTrieUpdatesAction>();
        }

        size
    }

    /// Determines if the given path can be directly reached from the upper trie.
    fn is_path_reachable_from_upper(&self, path: &Nibbles) -> bool {
        let mut current = Nibbles::default();
        while current.len() < path.len() {
            let Some(node) = self.upper_subtrie.nodes.get(&current) else { return false };
            match node {
                SparseNode::Branch { state_mask, .. } => {
                    if !state_mask.is_bit_set(path.get_unchecked(current.len())) {
                        return false;
                    }

                    current.push_unchecked(path.get_unchecked(current.len()));
                }
                SparseNode::Extension { key, .. } => {
                    if *key != path.slice(current.len()..current.len() + key.len()) {
                        return false;
                    }
                    current.extend(key);
                }
                SparseNode::Empty | SparseNode::Leaf { .. } => return false,
            }
        }
        true
    }

    /// Checks if a boundary leaf (at `path.len() == UPPER_TRIE_MAX_DEPTH`) is reachable from its
    /// parent branch in the upper subtrie.
    ///
    /// This is used for leaves that sit at the upper/lower subtrie boundary, where the leaf is
    /// in a lower subtrie but its parent branch is in the upper subtrie.
    fn is_boundary_leaf_reachable(
        upper_nodes: &HashMap<Nibbles, SparseNode>,
        path: &Nibbles,
        node: &TrieNodeV2,
    ) -> bool {
        debug_assert_eq!(path.len(), UPPER_TRIE_MAX_DEPTH);

        if !matches!(node, TrieNodeV2::Leaf(_)) {
            return true;
        }

        let parent_path = path.slice(..path.len() - 1);
        let leaf_nibble = path.get_unchecked(path.len() - 1);

        match upper_nodes.get(&parent_path) {
            Some(SparseNode::Branch { state_mask, .. }) => state_mask.is_bit_set(leaf_nibble),
            _ => false,
        }
    }

    /// Returns a bitset of all subtries that are reachable from the upper trie. If subtrie is not
    /// reachable it means that it does not exist.
    fn reachable_subtries(&self) -> SubtriesBitmap {
        let mut reachable = SubtriesBitmap::default();

        let mut stack = Vec::new();
        stack.push(Nibbles::default());

        while let Some(current) = stack.pop() {
            let Some(node) = self.upper_subtrie.nodes.get(&current) else { continue };
            match node {
                SparseNode::Branch { state_mask, .. } => {
                    for idx in state_mask.iter() {
                        let mut next = current;
                        next.push_unchecked(idx);
                        if next.len() >= UPPER_TRIE_MAX_DEPTH {
                            reachable.set(path_subtrie_index_unchecked(&next));
                        } else {
                            stack.push(next);
                        }
                    }
                }
                SparseNode::Extension { key, .. } => {
                    let mut next = current;
                    next.extend(key);
                    if next.len() >= UPPER_TRIE_MAX_DEPTH {
                        reachable.set(path_subtrie_index_unchecked(&next));
                    } else {
                        stack.push(next);
                    }
                }
                SparseNode::Empty | SparseNode::Leaf { .. } => {}
            };
        }

        reachable
    }
}

/// Bitset tracking which of the 256 lower subtries were modified in the current cycle.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct SubtriesBitmap(U256);

impl SubtriesBitmap {
    /// Marks a subtrie index.
    #[inline]
    fn set(&mut self, idx: usize) {
        debug_assert!(idx < NUM_LOWER_SUBTRIES);
        self.0.set_bit(idx, true);
    }

    /// Returns whether a subtrie index is set.
    #[inline]
    fn get(&self, idx: usize) -> bool {
        debug_assert!(idx < NUM_LOWER_SUBTRIES);
        self.0.bit(idx)
    }
}

/// This is a subtrie of the [`ParallelSparseTrie`] that contains a map from path to sparse trie
/// nodes.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SparseSubtrie {
    /// The root path of this subtrie.
    ///
    /// This is the _full_ path to this subtrie, meaning it includes the first
    /// [`UPPER_TRIE_MAX_DEPTH`] nibbles that we also use for indexing subtries in the
    /// [`ParallelSparseTrie`].
    ///
    /// There should be a node for this path in `nodes` map.
    pub(crate) path: Nibbles,
    /// The map from paths to sparse trie nodes within this subtrie.
    nodes: HashMap<Nibbles, SparseNode>,
    /// Subset of fields for mutable access while `nodes` field is also being mutably borrowed.
    inner: SparseSubtrieInner,
}

/// Returned by the `find_next_to_leaf` method to indicate either that the leaf has been found,
/// traversal should be continued from the given path, or the leaf is not in the trie.
enum FindNextToLeafOutcome {
    /// `Found` indicates that the leaf was found at the given path.
    Found,
    /// `ContinueFrom` indicates that traversal should continue from the given path.
    ContinueFrom(Nibbles),
    /// `NotFound` indicates that there is no way to traverse to the leaf, as it is not in the
    /// trie.
    NotFound,
    /// `BlindedNode` indicates that the node is blinded with the contained hash and cannot be
    /// traversed.
    BlindedNode(Nibbles),
}

impl SparseSubtrie {
    /// Creates a new empty subtrie with the specified root path.
    pub(crate) fn new(path: Nibbles) -> Self {
        Self { path, ..Default::default() }
    }

    /// Returns true if this subtrie has any nodes, false otherwise.
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Returns true if the current path and its child are both found in the same level.
    fn is_child_same_level(current_path: &Nibbles, child_path: &Nibbles) -> bool {
        let current_level = core::mem::discriminant(&SparseSubtrieType::from_path(current_path));
        let child_level = core::mem::discriminant(&SparseSubtrieType::from_path(child_path));
        current_level == child_level
    }

    /// Checks if a leaf node at the given path is reachable from its parent branch node.
    ///
    /// Returns `true` if:
    /// - The path is at the root (no parent to check)
    /// - The parent branch node has the corresponding `state_mask` bit set for this leaf
    ///
    /// Returns `false` if the parent is a branch node that doesn't have the `state_mask` bit set
    /// for this leaf's nibble, meaning the leaf is not reachable.
    fn is_leaf_reachable_from_parent(&self, path: &Nibbles) -> bool {
        if path.is_empty() {
            return true;
        }

        let parent_path = path.slice(..path.len() - 1);
        let leaf_nibble = path.get_unchecked(path.len() - 1);

        match self.nodes.get(&parent_path) {
            Some(SparseNode::Branch { state_mask, .. }) => state_mask.is_bit_set(leaf_nibble),
            _ => false,
        }
    }

    /// Updates or inserts a leaf node at the specified key path with the provided RLP-encoded
    /// value.
    ///
    /// If the leaf did not previously exist, this method adjusts the trie structure by inserting
    /// new leaf nodes, splitting branch nodes, or collapsing extension nodes as needed.
    ///
    /// # Returns
    ///
    /// Returns the path and masks of any blinded node revealed as a result of updating the leaf.
    ///
    /// If an update requires revealing a blinded node, an error is returned if the blinded
    /// provider returns an error.
    ///
    /// This method is atomic: if an error occurs during structural changes, all modifications
    /// are rolled back and the trie state is unchanged.
    pub fn update_leaf(&mut self, full_path: Nibbles, value: Vec<u8>) -> SparseTrieResult<()> {
        debug_assert!(full_path.starts_with(&self.path));

        // Check if value already exists - if so, just update it (no structural changes needed)
        if let Entry::Occupied(mut e) = self.inner.values.entry(full_path) {
            e.insert(value);
            return Ok(());
        }

        // Here we are starting at the root of the subtrie, and traversing from there.
        let mut current = Some(self.path);

        while let Some(current_path) = current.as_mut() {
            match self.update_next_node(current_path, &full_path)? {
                LeafUpdateStep::Continue => {}
                LeafUpdateStep::NodeNotFound | LeafUpdateStep::Complete { .. } => break,
            }
        }

        // Only insert the value after all structural changes succeed
        self.inner.values.insert(full_path, value);

        Ok(())
    }

    /// Processes the current node, returning what to do next in the leaf update process.
    ///
    /// This will add or update any nodes in the trie as necessary.
    ///
    /// Returns a `LeafUpdateStep` containing the next node to process (if any) and
    /// the paths of nodes that were inserted during this step.
    fn update_next_node(
        &mut self,
        current: &mut Nibbles,
        path: &Nibbles,
    ) -> SparseTrieResult<LeafUpdateStep> {
        debug_assert!(path.starts_with(&self.path));
        debug_assert!(current.starts_with(&self.path));
        debug_assert!(path.starts_with(current));
        let Some(node) = self.nodes.get_mut(current) else {
            return Ok(LeafUpdateStep::NodeNotFound);
        };

        match node {
            SparseNode::Empty => {
                // We need to insert the node with a different path and key depending on the path of
                // the subtrie.
                let path = path.slice(self.path.len()..);
                *node = SparseNode::new_leaf(path);
                Ok(LeafUpdateStep::complete_with_insertions(vec![*current]))
            }
            SparseNode::Leaf { key: current_key, .. } => {
                current.extend(current_key);

                // this leaf is being updated
                debug_assert!(current != path, "we already checked leaf presence in the beginning");

                // find the common prefix
                let common = current.common_prefix_length(path);

                // update existing node
                let new_ext_key = current.slice(current.len() - current_key.len()..common);
                *node = SparseNode::new_ext(new_ext_key);

                // create a branch node and corresponding leaves
                self.nodes.reserve(3);
                let branch_path = current.slice(..common);
                let new_leaf_path = path.slice(..=common);
                let existing_leaf_path = current.slice(..=common);

                self.nodes.insert(
                    branch_path,
                    SparseNode::new_split_branch(
                        current.get_unchecked(common),
                        path.get_unchecked(common),
                    ),
                );
                self.nodes.insert(new_leaf_path, SparseNode::new_leaf(path.slice(common + 1..)));
                self.nodes
                    .insert(existing_leaf_path, SparseNode::new_leaf(current.slice(common + 1..)));

                Ok(LeafUpdateStep::complete_with_insertions(vec![
                    branch_path,
                    new_leaf_path,
                    existing_leaf_path,
                ]))
            }
            SparseNode::Extension { key, .. } => {
                current.extend(key);

                if !path.starts_with(current) {
                    // find the common prefix
                    let common = current.common_prefix_length(path);
                    *key = current.slice(current.len() - key.len()..common);

                    // create state mask for new branch node
                    // NOTE: this might overwrite the current extension node
                    self.nodes.reserve(3);
                    let branch_path = current.slice(..common);
                    let new_leaf_path = path.slice(..=common);
                    let branch = SparseNode::new_split_branch(
                        current.get_unchecked(common),
                        path.get_unchecked(common),
                    );

                    self.nodes.insert(branch_path, branch);

                    // create new leaf
                    let new_leaf = SparseNode::new_leaf(path.slice(common + 1..));
                    self.nodes.insert(new_leaf_path, new_leaf);

                    let mut inserted_nodes = vec![branch_path, new_leaf_path];

                    // recreate extension to previous child if needed
                    let key = current.slice(common + 1..);
                    if !key.is_empty() {
                        let ext_path = current.slice(..=common);
                        self.nodes.insert(ext_path, SparseNode::new_ext(key));
                        inserted_nodes.push(ext_path);
                    }

                    return Ok(LeafUpdateStep::complete_with_insertions(inserted_nodes));
                }

                Ok(LeafUpdateStep::Continue)
            }
            SparseNode::Branch { state_mask, blinded_mask, .. } => {
                let nibble = path.get_unchecked(current.len());
                current.push_unchecked(nibble);

                if !state_mask.is_bit_set(nibble) {
                    state_mask.set_bit(nibble);
                    let new_leaf = SparseNode::new_leaf(path.slice(current.len()..));
                    self.nodes.insert(*current, new_leaf);
                    return Ok(LeafUpdateStep::complete_with_insertions(vec![*current]));
                }

                if blinded_mask.is_bit_set(nibble) {
                    return Err(SparseTrieErrorKind::BlindedNode(*current).into());
                }

                // If the nibble is set, we can continue traversing the branch.
                Ok(LeafUpdateStep::Continue)
            }
        }
    }

    /// Reveals a branch node at the given path.
    fn reveal_branch(
        &mut self,
        path: Nibbles,
        state_mask: TrieMask,
        children: &[RlpNode],
        masks: Option<BranchNodeMasks>,
        rlp_node: Option<RlpNode>,
    ) -> SparseTrieResult<()> {
        match self.nodes.entry(path) {
            Entry::Occupied(_) => {
                // Branch already revealed, do nothing
                return Ok(());
            }
            Entry::Vacant(entry) => {
                let state =
                    match rlp_node.as_ref() {
                        Some(rlp_node) => SparseNodeState::Cached {
                            rlp_node: rlp_node.clone(),
                            store_in_db_trie: Some(masks.is_some_and(|m| {
                                !m.hash_mask.is_empty() || !m.tree_mask.is_empty()
                            })),
                        },
                        None => SparseNodeState::Dirty,
                    };

                let mut blinded_mask = TrieMask::default();
                let mut blinded_hashes = Box::new([B256::ZERO; 16]);

                for (stack_ptr, idx) in state_mask.iter().enumerate() {
                    let mut child_path = path;
                    child_path.push_unchecked(idx);
                    let child = &children[stack_ptr];

                    if let Some(hash) = child.as_hash() {
                        blinded_mask.set_bit(idx);
                        blinded_hashes[idx as usize] = hash;
                    }
                }

                entry.insert(SparseNode::Branch {
                    state_mask,
                    state,
                    blinded_mask,
                    blinded_hashes,
                });
            }
        }

        // For a branch node, iterate over all children. This must happen second so leaf
        // children can check connectivity with parent branch.
        for (stack_ptr, idx) in state_mask.iter().enumerate() {
            let mut child_path = path;
            child_path.push_unchecked(idx);
            let child = &children[stack_ptr];
            if !child.is_hash() && Self::is_child_same_level(&path, &child_path) {
                // Reveal each child node or hash it has, but only if the child is on
                // the same level as the parent.
                self.reveal_node(
                    child_path,
                    &TrieNodeV2::decode(&mut child.as_ref())?,
                    None,
                    None,
                )?;
            }
        }

        Ok(())
    }

    /// Internal implementation of the method of the same name on `ParallelSparseTrie`.
    ///
    /// This accepts `hash_from_upper` to handle cases when boundary nodes revealed in lower subtrie
    /// but its blinded hash is known from the upper subtrie.
    fn reveal_node(
        &mut self,
        path: Nibbles,
        node: &TrieNodeV2,
        masks: Option<BranchNodeMasks>,
        hash_from_upper: Option<B256>,
    ) -> SparseTrieResult<bool> {
        debug_assert!(path.starts_with(&self.path));

        // If the node is already revealed, do nothing.
        if self.nodes.contains_key(&path) {
            return Ok(false);
        }

        // If the hash is provided from the upper subtrie, use it. Otherwise, find the parent branch
        // node, unset its blinded bit and use the hash.
        let hash = if let Some(hash) = hash_from_upper {
            Some(hash)
        } else if path.len() != UPPER_TRIE_MAX_DEPTH && !path.is_empty() {
            let Some(SparseNode::Branch { state_mask, blinded_mask, blinded_hashes, .. }) =
                self.nodes.get_mut(&path.slice(0..path.len() - 1))
            else {
                return Ok(false);
            };
            let nibble = path.last().unwrap();
            if !state_mask.is_bit_set(nibble) {
                return Ok(false);
            }

            blinded_mask.is_bit_set(nibble).then(|| {
                blinded_mask.unset_bit(nibble);
                blinded_hashes[nibble as usize]
            })
        } else {
            None
        };

        trace!(
            target: "trie::parallel_sparse",
            ?path,
            ?node,
            ?masks,
            "Revealing node",
        );

        match node {
            TrieNodeV2::EmptyRoot => {
                // For an empty root, ensure that we are at the root path, and at the upper subtrie.
                debug_assert!(path.is_empty());
                debug_assert!(self.path.is_empty());
                self.nodes.insert(path, SparseNode::Empty);
            }
            TrieNodeV2::Branch(branch) => {
                if branch.key.is_empty() {
                    self.reveal_branch(
                        path,
                        branch.state_mask,
                        &branch.stack,
                        masks,
                        hash.as_ref().map(RlpNode::word_rlp),
                    )?;
                    return Ok(true);
                }

                self.nodes.insert(
                    path,
                    SparseNode::Extension {
                        key: branch.key,
                        state: hash
                            .as_ref()
                            .map(|hash| SparseNodeState::Cached {
                                rlp_node: RlpNode::word_rlp(hash),
                                // Inherit `store_in_db_trie` from the child branch
                                // node masks so that the memoized hash can be used
                                // without needing to fetch the child branch.
                                store_in_db_trie: Some(masks.is_some_and(|m| {
                                    !m.hash_mask.is_empty() || !m.tree_mask.is_empty()
                                })),
                            })
                            .unwrap_or(SparseNodeState::Dirty),
                    },
                );

                let mut branch_path = path;
                branch_path.extend(&branch.key);

                // Exit early if the actual branch node does not belong to this subtrie.
                if !Self::is_child_same_level(&path, &branch_path) {
                    return Ok(true);
                }

                // Reveal the actual branch node.
                self.reveal_branch(
                    branch_path,
                    branch.state_mask,
                    &branch.stack,
                    masks,
                    branch.branch_rlp_node.clone(),
                )?;
            }
            TrieNodeV2::Extension(_) => unreachable!(),
            TrieNodeV2::Leaf(leaf) => {
                // Skip the reachability check when path.len() == UPPER_TRIE_MAX_DEPTH because
                // at that boundary the leaf is in the lower subtrie but its parent branch is in
                // the upper subtrie. The subtrie cannot check connectivity across the upper/lower
                // boundary, so that check happens in `reveal_nodes` instead.
                if path.len() != UPPER_TRIE_MAX_DEPTH && !self.is_leaf_reachable_from_parent(&path)
                {
                    trace!(
                        target: "trie::parallel_sparse",
                        ?path,
                        "Leaf not reachable from parent branch, skipping",
                    );
                    return Ok(false);
                }

                let mut full_key = path;
                full_key.extend(&leaf.key);

                match self.inner.values.entry(full_key) {
                    Entry::Occupied(_) => {
                        trace!(
                            target: "trie::parallel_sparse",
                            ?path,
                            ?full_key,
                            "Leaf full key value already present, skipping",
                        );
                        return Ok(false);
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(leaf.value.clone());
                    }
                }

                self.nodes.insert(
                    path,
                    SparseNode::Leaf {
                        key: leaf.key,
                        state: hash
                            .as_ref()
                            .map(|hash| SparseNodeState::Cached {
                                rlp_node: RlpNode::word_rlp(hash),
                                store_in_db_trie: Some(false),
                            })
                            .unwrap_or(SparseNodeState::Dirty),
                    },
                );
            }
        }

        Ok(true)
    }

    /// Recalculates and updates the RLP hashes for the changed nodes in this subtrie.
    ///
    /// The function starts from the subtrie root, traverses down to leaves, and then calculates
    /// the hashes from leaves back up to the root. It uses a stack from [`SparseSubtrieBuffers`] to
    /// track the traversal and accumulate RLP encodings.
    ///
    /// # Parameters
    ///
    /// - `prefix_set`: The set of trie paths whose nodes have changed.
    /// - `update_actions`: A buffer which `SparseTrieUpdatesAction`s will be written to in the
    ///   event that any changes to the top-level updates are required. If None then update
    ///   retention is disabled.
    /// - `branch_node_masks`: The tree and hash masks for branch nodes.
    ///
    /// # Returns
    ///
    /// A tuple containing the root node of the updated subtrie.
    ///
    /// # Panics
    ///
    /// If the node at the root path does not exist.
    #[instrument(level = "trace", target = "trie::parallel_sparse", skip_all, fields(root = ?self.path), ret)]
    fn update_hashes(
        &mut self,
        prefix_set: &mut PrefixSet,
        update_actions: &mut Option<Vec<SparseTrieUpdatesAction>>,
        branch_node_masks: &BranchNodeMasksMap,
    ) -> RlpNode {
        trace!(target: "trie::parallel_sparse", "Updating subtrie hashes");

        debug_assert!(prefix_set.iter().all(|path| path.starts_with(&self.path)));

        debug_assert!(self.inner.buffers.path_stack.is_empty());
        self.inner
            .buffers
            .path_stack
            .push(RlpNodePathStackItem { path: self.path, is_in_prefix_set: None });

        while let Some(stack_item) = self.inner.buffers.path_stack.pop() {
            let path = stack_item.path;
            let node = self
                .nodes
                .get_mut(&path)
                .unwrap_or_else(|| panic!("node at path {path:?} does not exist"));

            self.inner.rlp_node(prefix_set, update_actions, stack_item, node, branch_node_masks);
        }

        debug_assert_eq!(self.inner.buffers.rlp_node_stack.len(), 1);
        self.inner.buffers.rlp_node_stack.pop().unwrap().rlp_node
    }

    /// Removes all nodes and values from the subtrie, resetting it to a blank state
    /// with only an empty root node. This is used when a storage root is deleted.
    fn wipe(&mut self) {
        self.nodes.clear();
        self.nodes.insert(Nibbles::default(), SparseNode::Empty);
        self.inner.clear();
    }

    /// Clears the subtrie, keeping the data structures allocated.
    pub(crate) fn clear(&mut self) {
        self.nodes.clear();
        self.inner.clear();
    }

    /// Shrinks the capacity of the subtrie's node storage.
    pub(crate) fn shrink_nodes_to(&mut self, size: usize) {
        self.nodes.shrink_to(size);
    }

    /// Shrinks the capacity of the subtrie's value storage.
    pub(crate) fn shrink_values_to(&mut self, size: usize) {
        self.inner.values.shrink_to(size);
    }

    /// Returns a heuristic for the in-memory size of this subtrie in bytes.
    pub(crate) fn memory_size(&self) -> usize {
        let mut size = core::mem::size_of::<Self>();

        // Nodes map: key (Nibbles) + value (SparseNode)
        for (path, node) in &self.nodes {
            size += core::mem::size_of::<Nibbles>();
            size += path.len(); // Nibbles heap allocation
            size += node.memory_size();
        }

        // Values map: key (Nibbles) + value (Vec<u8>)
        for (path, value) in &self.inner.values {
            size += core::mem::size_of::<Nibbles>();
            size += path.len(); // Nibbles heap allocation
            size += core::mem::size_of::<Vec<u8>>() + value.capacity();
        }

        // Buffers
        size += self.inner.buffers.memory_size();

        size
    }
}

/// Helper type for [`SparseSubtrie`] to mutably access only a subset of fields from the original
/// struct.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct SparseSubtrieInner {
    /// Map from leaf key paths to their values.
    /// All values are stored here instead of directly in leaf nodes.
    values: HashMap<Nibbles, Vec<u8>>,
    /// Reusable buffers for [`SparseSubtrie::update_hashes`].
    buffers: SparseSubtrieBuffers,
}

impl SparseSubtrieInner {
    /// Computes the RLP encoding and its hash for a single (trie node)[`SparseNode`].
    ///
    /// # Deferred Processing
    ///
    /// When an extension or a branch node depends on child nodes that haven't been computed yet,
    /// the function pushes the current node back onto the path stack along with its children,
    /// then returns early. This allows the iterative algorithm to process children first before
    /// retrying the parent.
    ///
    /// # Parameters
    ///
    /// - `prefix_set`: Set of prefixes (key paths) that have been marked as updated
    /// - `update_actions`: A buffer which `SparseTrieUpdatesAction`s will be written to in the
    ///   event that any changes to the top-level updates are required. If None then update
    ///   retention is disabled.
    /// - `stack_item`: The stack item to process
    /// - `node`: The sparse node to process (will be mutated to update hash)
    /// - `branch_node_masks`: The tree and hash masks for branch nodes.
    ///
    /// # Side Effects
    ///
    /// - Updates the node's hash field after computing RLP
    /// - Pushes nodes to [`SparseSubtrieBuffers::path_stack`] to manage traversal
    /// - May push items onto the path stack for deferred processing
    ///
    /// # Exit condition
    ///
    /// Once all nodes have been processed and all RLPs and hashes calculated, pushes the root node
    /// onto the [`SparseSubtrieBuffers::rlp_node_stack`] and exits.
    fn rlp_node(
        &mut self,
        prefix_set: &mut PrefixSet,
        update_actions: &mut Option<Vec<SparseTrieUpdatesAction>>,
        mut stack_item: RlpNodePathStackItem,
        node: &mut SparseNode,
        branch_node_masks: &BranchNodeMasksMap,
    ) {
        let path = stack_item.path;
        trace!(
            target: "trie::parallel_sparse",
            ?path,
            ?node,
            "Calculating node RLP"
        );

        // Check if the path is in the prefix set.
        // First, check the cached value. If it's `None`, then check the prefix set, and update
        // the cached value.
        let mut prefix_set_contains = |path: &Nibbles| {
            *stack_item.is_in_prefix_set.get_or_insert_with(|| prefix_set.contains(path))
        };

        let (rlp_node, node_type) = match node {
            SparseNode::Empty => (RlpNode::word_rlp(&EMPTY_ROOT_HASH), SparseNodeType::Empty),
            SparseNode::Leaf { key, state } => {
                let mut path = path;
                path.extend(key);
                let value = self.values.get(&path);

                // Check if we should use cached RLP:
                // - If there's a cached RLP and the path is not in prefix_set, use cached
                // - If the value is not in this subtrie's values (e.g., lower subtrie leaf being
                //   processed via upper subtrie), we must use cached RLP
                let cached_rlp_node = state.cached_rlp_node();
                let use_cached =
                    cached_rlp_node.is_some() && (!prefix_set_contains(&path) || value.is_none());

                if let Some(rlp_node) = use_cached.then(|| cached_rlp_node.unwrap()) {
                    // Return the cached RLP
                    (rlp_node.clone(), SparseNodeType::Leaf)
                } else {
                    // Encode the leaf node and update its RlpNode
                    let value = value.expect("leaf value must exist in subtrie");
                    self.buffers.rlp_buf.clear();
                    let rlp_node = LeafNodeRef { key, value }.rlp(&mut self.buffers.rlp_buf);
                    *state = SparseNodeState::Cached {
                        rlp_node: rlp_node.clone(),
                        store_in_db_trie: Some(false),
                    };
                    trace!(
                        target: "trie::parallel_sparse",
                        ?path,
                        ?key,
                        value = %alloy_primitives::hex::encode(value),
                        ?rlp_node,
                        "Calculated leaf RLP node",
                    );
                    (rlp_node, SparseNodeType::Leaf)
                }
            }
            SparseNode::Extension { key, state } => {
                let mut child_path = path;
                child_path.extend(key);
                if let Some((rlp_node, store_in_db_trie)) = state
                    .cached_rlp_node()
                    .zip(state.store_in_db_trie())
                    .filter(|_| !prefix_set_contains(&path))
                {
                    // If the node is already computed, and the node path is not in
                    // the prefix set, return the pre-computed node
                    (
                        rlp_node.clone(),
                        SparseNodeType::Extension { store_in_db_trie: Some(store_in_db_trie) },
                    )
                } else if self.buffers.rlp_node_stack.last().is_some_and(|e| e.path == child_path) {
                    // Top of the stack has the child node, we can encode the extension node and
                    // update its hash
                    let RlpNodeStackItem { path: _, rlp_node: child, node_type: child_node_type } =
                        self.buffers.rlp_node_stack.pop().unwrap();
                    self.buffers.rlp_buf.clear();
                    let rlp_node =
                        ExtensionNodeRef::new(key, &child).rlp(&mut self.buffers.rlp_buf);

                    let store_in_db_trie_value = child_node_type.store_in_db_trie();

                    trace!(
                        target: "trie::parallel_sparse",
                        ?path,
                        ?child_path,
                        ?child_node_type,
                        "Extension node"
                    );

                    *state = SparseNodeState::Cached {
                        rlp_node: rlp_node.clone(),
                        store_in_db_trie: store_in_db_trie_value,
                    };

                    (
                        rlp_node,
                        SparseNodeType::Extension {
                            // Inherit the `store_in_db_trie` flag from the child node, which is
                            // always the branch node
                            store_in_db_trie: store_in_db_trie_value,
                        },
                    )
                } else {
                    // Need to defer processing until child is computed, on the next
                    // invocation update the node's hash.
                    self.buffers.path_stack.extend([
                        RlpNodePathStackItem {
                            path,
                            is_in_prefix_set: Some(prefix_set_contains(&path)),
                        },
                        RlpNodePathStackItem { path: child_path, is_in_prefix_set: None },
                    ]);
                    return;
                }
            }
            SparseNode::Branch { state_mask, state, blinded_mask, blinded_hashes } => {
                if let Some((rlp_node, store_in_db_trie)) = state
                    .cached_rlp_node()
                    .zip(state.store_in_db_trie())
                    .filter(|_| !prefix_set_contains(&path))
                {
                    let node_type =
                        SparseNodeType::Branch { store_in_db_trie: Some(store_in_db_trie) };

                    trace!(
                        target: "trie::parallel_sparse",
                        ?path,
                        ?node_type,
                        ?rlp_node,
                        "Adding node to RLP node stack (cached branch)"
                    );

                    // If the node hash is already computed, and the node path is not in
                    // the prefix set, return the pre-computed hash
                    self.buffers.rlp_node_stack.push(RlpNodeStackItem {
                        path,
                        rlp_node: rlp_node.clone(),
                        node_type,
                    });
                    return;
                }

                let retain_updates = update_actions.is_some() && prefix_set_contains(&path);

                self.buffers.branch_child_buf.clear();
                // Walk children in a reverse order from `f` to `0`, so we pop the `0` first
                // from the stack and keep walking in the sorted order.
                for bit in state_mask.iter().rev() {
                    let mut child = path;
                    child.push_unchecked(bit);

                    if !blinded_mask.is_bit_set(bit) {
                        self.buffers.branch_child_buf.push(child);
                    }
                }

                self.buffers.branch_value_stack_buf.resize(state_mask.len(), Default::default());

                let mut tree_mask = TrieMask::default();
                let mut hash_mask = TrieMask::default();
                let mut hashes = Vec::new();

                // Lazy lookup for branch node masks - shared across loop iterations
                let mut path_masks_storage = None;
                let mut path_masks =
                    || *path_masks_storage.get_or_insert_with(|| branch_node_masks.get(&path));

                for (i, child_nibble) in state_mask.iter().enumerate().rev() {
                    let mut child_path = path;
                    child_path.push_unchecked(child_nibble);

                    let (child, child_node_type) = if blinded_mask.is_bit_set(child_nibble) {
                        (
                            RlpNode::word_rlp(&blinded_hashes[child_nibble as usize]),
                            SparseNodeType::Hash,
                        )
                    } else if self
                        .buffers
                        .rlp_node_stack
                        .last()
                        .is_some_and(|e| e.path == child_path)
                    {
                        let RlpNodeStackItem { path: _, rlp_node, node_type } =
                            self.buffers.rlp_node_stack.pop().unwrap();

                        (rlp_node, node_type)
                    } else {
                        // Need to defer processing until children are computed, on the next
                        // invocation update the node's hash.
                        self.buffers.path_stack.push(RlpNodePathStackItem {
                            path,
                            is_in_prefix_set: Some(prefix_set_contains(&path)),
                        });
                        self.buffers.path_stack.extend(
                            self.buffers
                                .branch_child_buf
                                .drain(..)
                                .map(|path| RlpNodePathStackItem { path, is_in_prefix_set: None }),
                        );
                        return;
                    };

                    // Update the masks only if we need to retain trie updates
                    if retain_updates {
                        // Determine whether we need to set trie mask bit.
                        let should_set_tree_mask_bit =
                            if let Some(store_in_db_trie) = child_node_type.store_in_db_trie() {
                                // A branch or an extension node explicitly set the
                                // `store_in_db_trie` flag
                                store_in_db_trie
                            } else {
                                // A blinded node has the tree mask bit set
                                child_node_type.is_hash() &&
                                    path_masks().is_some_and(|masks| {
                                        masks.tree_mask.is_bit_set(child_nibble)
                                    })
                            };
                        if should_set_tree_mask_bit {
                            tree_mask.set_bit(child_nibble);
                        }
                        // Set the hash mask. If a child node is a revealed branch node OR
                        // is a blinded node that has its hash mask bit set according to the
                        // database, set the hash mask bit and save the hash.
                        let hash = child.as_hash().filter(|_| {
                            child_node_type.is_branch() ||
                                (child_node_type.is_hash() &&
                                    path_masks().is_some_and(|masks| {
                                        masks.hash_mask.is_bit_set(child_nibble)
                                    }))
                        });
                        if let Some(hash) = hash {
                            hash_mask.set_bit(child_nibble);
                            hashes.push(hash);
                        }
                    }

                    // Insert children in the resulting buffer in a normal order,
                    // because initially we iterated in reverse.
                    // SAFETY: i < len and len is never 0
                    self.buffers.branch_value_stack_buf[i] = child;
                }

                trace!(
                    target: "trie::parallel_sparse",
                    ?path,
                    ?tree_mask,
                    ?hash_mask,
                    "Branch node masks"
                );

                // Top of the stack has all children node, we can encode the branch node and
                // update its hash
                self.buffers.rlp_buf.clear();
                let branch_node_ref =
                    BranchNodeRef::new(&self.buffers.branch_value_stack_buf, *state_mask);
                let rlp_node = branch_node_ref.rlp(&mut self.buffers.rlp_buf);

                // Save a branch node update only if it's not a root node, and we need to
                // persist updates.
                let store_in_db_trie_value = if let Some(update_actions) =
                    update_actions.as_mut().filter(|_| retain_updates && !path.is_empty())
                {
                    let store_in_db_trie = !tree_mask.is_empty() || !hash_mask.is_empty();
                    if store_in_db_trie {
                        // Store in DB trie if there are either any children that are stored in
                        // the DB trie, or any children represent hashed values
                        hashes.reverse();
                        let branch_node =
                            BranchNodeCompact::new(*state_mask, tree_mask, hash_mask, hashes, None);
                        update_actions
                            .push(SparseTrieUpdatesAction::InsertUpdated(path, branch_node));
                    } else {
                        // New tree and hash masks are empty - check previous state
                        let prev_had_masks = path_masks()
                            .is_some_and(|m| !m.tree_mask.is_empty() || !m.hash_mask.is_empty());
                        if prev_had_masks {
                            // Previously had masks, now empty - mark as removed
                            update_actions.push(SparseTrieUpdatesAction::InsertRemoved(path));
                        } else {
                            // Previously empty too - just remove the update
                            update_actions.push(SparseTrieUpdatesAction::RemoveUpdated(path));
                        }
                    }

                    store_in_db_trie
                } else {
                    false
                };

                *state = SparseNodeState::Cached {
                    rlp_node: rlp_node.clone(),
                    store_in_db_trie: Some(store_in_db_trie_value),
                };

                (
                    rlp_node,
                    SparseNodeType::Branch { store_in_db_trie: Some(store_in_db_trie_value) },
                )
            }
        };

        trace!(
            target: "trie::parallel_sparse",
            ?path,
            ?node_type,
            ?rlp_node,
            "Adding node to RLP node stack"
        );
        self.buffers.rlp_node_stack.push(RlpNodeStackItem { path, rlp_node, node_type });
    }

    /// Clears the subtrie, keeping the data structures allocated.
    fn clear(&mut self) {
        self.values.clear();
        self.buffers.clear();
    }
}

/// Represents the outcome of processing a node during leaf insertion
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum LeafUpdateStep {
    /// Continue traversing to the next node
    Continue,
    /// Update is complete with nodes inserted
    Complete {
        /// The node paths that were inserted during this step
        inserted_nodes: Vec<Nibbles>,
    },
    /// The node was not found
    #[default]
    NodeNotFound,
}

impl LeafUpdateStep {
    /// Creates a step indicating completion with inserted nodes
    pub const fn complete_with_insertions(inserted_nodes: Vec<Nibbles>) -> Self {
        Self::Complete { inserted_nodes }
    }
}

/// Sparse Subtrie Type.
///
/// Used to determine the type of subtrie a certain path belongs to:
/// - Paths in the range `0x..=0xf` belong to the upper subtrie.
/// - Paths in the range `0x00..` belong to one of the lower subtries. The index of the lower
///   subtrie is determined by the first [`UPPER_TRIE_MAX_DEPTH`] nibbles of the path.
///
/// There can be at most [`NUM_LOWER_SUBTRIES`] lower subtries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SparseSubtrieType {
    /// Upper subtrie with paths in the range `0x..=0xf`
    Upper,
    /// Lower subtrie with paths in the range `0x00..`. Includes the index of the subtrie,
    /// according to the path prefix.
    Lower(usize),
}

impl SparseSubtrieType {
    /// Returns true if a node at a path of the given length would be placed in the upper subtrie.
    ///
    /// Nodes with paths shorter than [`UPPER_TRIE_MAX_DEPTH`] nibbles belong to the upper subtrie,
    /// while longer paths belong to the lower subtries.
    pub const fn path_len_is_upper(len: usize) -> bool {
        len < UPPER_TRIE_MAX_DEPTH
    }

    /// Returns the type of subtrie based on the given path.
    pub fn from_path(path: &Nibbles) -> Self {
        if Self::path_len_is_upper(path.len()) {
            Self::Upper
        } else {
            Self::Lower(path_subtrie_index_unchecked(path))
        }
    }

    /// Returns the index of the lower subtrie, if it exists.
    pub const fn lower_index(&self) -> Option<usize> {
        match self {
            Self::Upper => None,
            Self::Lower(index) => Some(*index),
        }
    }
}

impl Ord for SparseSubtrieType {
    /// Orders two [`SparseSubtrieType`]s such that `Upper` is less than `Lower(_)`, and `Lower`s
    /// are ordered by their index.
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Upper, Self::Upper) => Ordering::Equal,
            (Self::Upper, Self::Lower(_)) => Ordering::Less,
            (Self::Lower(_), Self::Upper) => Ordering::Greater,
            (Self::Lower(idx_a), Self::Lower(idx_b)) if idx_a == idx_b => Ordering::Equal,
            (Self::Lower(idx_a), Self::Lower(idx_b)) => idx_a.cmp(idx_b),
        }
    }
}

impl PartialOrd for SparseSubtrieType {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Collection of reusable buffers for calculating subtrie hashes.
///
/// These buffers reduce allocations when computing RLP representations during trie updates.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SparseSubtrieBuffers {
    /// Stack of RLP node paths
    path_stack: Vec<RlpNodePathStackItem>,
    /// Stack of RLP nodes
    rlp_node_stack: Vec<RlpNodeStackItem>,
    /// Reusable branch child path
    branch_child_buf: Vec<Nibbles>,
    /// Reusable branch value stack
    branch_value_stack_buf: Vec<RlpNode>,
    /// Reusable RLP buffer
    rlp_buf: Vec<u8>,
}

impl SparseSubtrieBuffers {
    /// Clears all buffers.
    fn clear(&mut self) {
        self.path_stack.clear();
        self.rlp_node_stack.clear();
        self.branch_child_buf.clear();
        self.branch_value_stack_buf.clear();
        self.rlp_buf.clear();
    }

    /// Returns a heuristic for the in-memory size of these buffers in bytes.
    const fn memory_size(&self) -> usize {
        let mut size = core::mem::size_of::<Self>();

        size += self.path_stack.capacity() * core::mem::size_of::<RlpNodePathStackItem>();
        size += self.rlp_node_stack.capacity() * core::mem::size_of::<RlpNodeStackItem>();
        size += self.branch_child_buf.capacity() * core::mem::size_of::<Nibbles>();
        size += self.branch_value_stack_buf.capacity() * core::mem::size_of::<RlpNode>();
        size += self.rlp_buf.capacity();

        size
    }
}

/// RLP node path stack item.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RlpNodePathStackItem {
    /// Path to the node.
    pub path: Nibbles,
    /// Whether the path is in the prefix set. If [`None`], then unknown yet.
    pub is_in_prefix_set: Option<bool>,
}

/// Changed subtrie.
#[derive(Debug)]
struct ChangedSubtrie {
    /// Lower subtrie index in the range [0, [`NUM_LOWER_SUBTRIES`]).
    index: usize,
    /// Changed subtrie
    subtrie: Box<SparseSubtrie>,
    /// Prefix set of keys that belong to the subtrie.
    prefix_set: PrefixSet,
    /// Reusable buffer for collecting [`SparseTrieUpdatesAction`]s during computations. Will be
    /// None if update retention is disabled.
    update_actions_buf: Option<Vec<SparseTrieUpdatesAction>>,
}

/// Convert first [`UPPER_TRIE_MAX_DEPTH`] nibbles of the path into a lower subtrie index in the
/// range [0, [`NUM_LOWER_SUBTRIES`]).
///
/// # Panics
///
/// If the path is shorter than [`UPPER_TRIE_MAX_DEPTH`] nibbles.
fn path_subtrie_index_unchecked(path: &Nibbles) -> usize {
    debug_assert_eq!(UPPER_TRIE_MAX_DEPTH, 2);
    let idx = path.get_byte_unchecked(0) as usize;
    // SAFETY: always true.
    unsafe { core::hint::assert_unchecked(idx < NUM_LOWER_SUBTRIES) };
    idx
}

/// Checks if `path` is a strict descendant of any root in a sorted slice.
///
/// Uses binary search to find the candidate root that could be an ancestor.
/// Returns `true` if `path` starts with a root and is longer (strict descendant).
fn is_strict_descendant_in(roots: &[Nibbles], path: &Nibbles) -> bool {
    if roots.is_empty() {
        return false;
    }
    debug_assert!(roots.windows(2).all(|w| w[0] <= w[1]), "roots must be sorted by path");
    let idx = roots.partition_point(|root| root <= path);
    if idx > 0 {
        let candidate = &roots[idx - 1];
        if path.starts_with(candidate) && path.len() > candidate.len() {
            return true;
        }
    }
    false
}

/// Returns true if any retained leaf path has `prefix` as a prefix.
///
/// The `retained` slice must be sorted.
fn has_retained_descendant(retained: &[Nibbles], prefix: &Nibbles) -> bool {
    if retained.is_empty() {
        return false;
    }
    debug_assert!(retained.windows(2).all(|w| w[0] <= w[1]), "retained must be sorted by path");
    let idx = retained.partition_point(|path| path < prefix);
    idx < retained.len() && retained[idx].starts_with(prefix)
}

/// Checks if `path` starts with any root in a sorted slice (inclusive).
///
/// Uses binary search to find the candidate root that could be a prefix.
/// Returns `true` if `path` starts with a root (including exact match).
fn starts_with_pruned_in(roots: &[Nibbles], path: &Nibbles) -> bool {
    if roots.is_empty() {
        return false;
    }
    debug_assert!(roots.windows(2).all(|w| w[0] <= w[1]), "roots must be sorted by path");
    let idx = roots.partition_point(|root| root <= path);
    if idx > 0 {
        let candidate = &roots[idx - 1];
        if path.starts_with(candidate) {
            return true;
        }
    }
    false
}

/// Used by lower subtries to communicate updates to the top-level [`SparseTrieUpdates`] set.
#[derive(Clone, Debug, Eq, PartialEq)]
enum SparseTrieUpdatesAction {
    /// Remove the path from the `updated_nodes`, if it was present, and add it to `removed_nodes`.
    InsertRemoved(Nibbles),
    /// Remove the path from the `updated_nodes`, if it was present, leaving `removed_nodes`
    /// unaffected.
    RemoveUpdated(Nibbles),
    /// Insert the branch node into `updated_nodes`.
    InsertUpdated(Nibbles, BranchNodeCompact),
}
