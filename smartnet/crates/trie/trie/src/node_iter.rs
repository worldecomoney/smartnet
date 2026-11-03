use crate::{
    hashed_cursor::HashedCursor, trie_cursor::TrieCursor, walker::TrieWalker, Nibbles, TrieType,
};
use alloy_primitives::B256;
use alloy_trie::proof::AddedRemovedKeys;
use smartnet_storage_errors::db::DatabaseError;
use tracing::{instrument, trace};

/// Represents a branch node in the trie.
#[derive(Debug)]
pub struct TrieBranchNode {
    /// The key associated with the node.
    pub key: Nibbles,
    /// The value associated with the node.
    pub value: B256,
    /// Indicates whether children are in the trie.
    pub children_are_in_trie: bool,
}

impl TrieBranchNode {
    /// Creates a new `TrieBranchNode`.
    pub const fn new(key: Nibbles, value: B256, children_are_in_trie: bool) -> Self {
        Self { key, value, children_are_in_trie }
    }
}

/// Represents variants of trie nodes returned by the iteration.
#[derive(Debug)]
pub enum TrieElement<Value> {
    /// Branch node.
    Branch(TrieBranchNode),
    /// Leaf node.
    Leaf(B256, Value),
}

/// Result of calling [`HashedCursor::seek`].
#[derive(Debug)]
struct SeekedHashedEntry<V> {
    /// The key that was seeked.
    seeked_key: B256,
    /// The result of the seek.

    /// If no entry was found for the provided key, this will be [`None`].
    result: Option<(B256, V)>,
}

/// Iterates over trie nodes for hash building.
///
/// This iterator depends on the ordering guarantees of [`TrieCursor`],
/// and additionally uses hashed cursor lookups when operating on storage tries.
#[derive(Debug)]
pub struct TrieNodeIter<C, H: HashedCursor, K> {
    /// The walker over intermediate nodes.
    pub walker: TrieWalker<C, K>,
    /// The cursor for the hashed entries.
    pub hashed_cursor: H,
    /// The type of the trie.
    trie_type: TrieType,
    /// The previous hashed key. If the iteration was previously interrupted, this value can be
    /// used to resume iterating from the last returned leaf node.
    previous_hashed_key: Option<B256>,

    /// Current hashed  entry.
    current_hashed_entry: Option<(B256, H::Value)>,
    /// Flag indicating whether we should check the current walker key.
    should_check_walker_key: bool,

    /// The last seeked hashed entry.
    ///
    /// We use it to not seek the same hashed entry twice, and instead reuse it.
    last_seeked_hashed_entry: Option<SeekedHashedEntry<H::Value>>,

    #[cfg(feature = "metrics")]
    metrics: crate::metrics::TrieNodeIterMetrics,
    /// Stores the result of the last successful [`Self::next_hashed_entry`], used to avoid a
    /// redundant [`Self::seek_hashed_entry`] call if the walker points to the same key that
    /// was just returned by `next()`.
    last_next_result: Option<(B256, H::Value)>,
}

impl<C, H: HashedCursor, K> TrieNodeIter<C, H, K>
where
    H::Value: Copy,
    K: AsRef<AddedRemovedKeys>,
{
    /// Creates a new [`TrieNodeIter`] for the state trie.
    pub fn state_trie(walker: TrieWalker<C, K>, hashed_cursor: H) -> Self {
        Self::new(walker, hashed_cursor, TrieType::State)
    }

    /// Creates a new [`TrieNodeIter`] for the storage trie.
    pub fn storage_trie(walker: TrieWalker<C, K>, hashed_cursor: H) -> Self {
        Self::new(walker, hashed_cursor, TrieType::Storage)
    }

    /// Creates a new [`TrieNodeIter`].
    fn new(walker: TrieWalker<C, K>, hashed_cursor: H, trie_type: TrieType) -> Self {
        Self {
            walker,
            hashed_cursor,
            trie_type,
            previous_hashed_key: None,
            current_hashed_entry: None,
            should_check_walker_key: false,
            last_seeked_hashed_entry: None,
            #[cfg(feature = "metrics")]
            metrics: crate::metrics::TrieNodeIterMetrics::new(trie_type),
            last_next_result: None,
        }
    }

    /// Sets the last iterated hashed key and returns the modified [`TrieNodeIter`].
    /// This is used to resume iteration from the last checkpoint.
    pub const fn with_last_hashed_key(mut self, previous_hashed_key: B256) -> Self {
        self.previous_hashed_key = Some(previous_hashed_key);
        self
    }

    /// Seeks the hashed cursor to the given key.
    ///
    /// If the key is the same as the last seeked key, the result of the last seek is returned.
    ///
    /// If `metrics` feature is enabled, it also updates the metrics.
    fn seek_hashed_entry(&mut self, key: B256) -> Result<Option<(B256, H::Value)>, DatabaseError> {
        if let Some((last_key, last_value)) = self.last_next_result &&
            last_key == key
        {
            trace!(target: "trie::node_iter", seek_key = ?key, "reusing result from last next() call instead of seeking");
            self.last_next_result = None; // Consume the cached value

            let result = Some((last_key, last_value));
            self.last_seeked_hashed_entry = Some(SeekedHashedEntry { seeked_key: key, result });

            return Ok(result);
        }

        if let Some(entry) = self
            .last_seeked_hashed_entry
            .as_ref()
            .filter(|entry| entry.seeked_key == key)
            .map(|entry| entry.result)
        {
            #[cfg(feature = "metrics")]
            self.metrics.inc_leaf_nodes_same_seeked();
            return Ok(entry);
        }

        trace!(target: "trie::node_iter", ?key, "performing hashed cursor seek");
        let result = self.hashed_cursor.seek(key)?;
        self.last_seeked_hashed_entry = Some(SeekedHashedEntry { seeked_key: key, result });

        #[cfg(feature = "metrics")]
        {
            self.metrics.inc_leaf_nodes_seeked();
        }
        Ok(result)
    }

    /// Advances the hashed cursor to the next entry.
    ///
    /// If `metrics` feature is enabled, it also updates the metrics.
    fn next_hashed_entry(&mut self) -> Result<Option<(B256, H::Value)>, DatabaseError> {
        let next = self.hashed_cursor.next()?;

        self.last_next_result = next;

        #[cfg(feature = "metrics")]
        {
            self.metrics.inc_leaf_nodes_advanced();
        }
        Ok(next)
    }
}

impl<C, H, K> TrieNodeIter<C, H, K>
where
    C: TrieCursor,
    H: HashedCursor,
    H::Value: Copy,
    K: AsRef<AddedRemovedKeys>,
{
    /// Return the next trie node to be added to the hash builder.
    ///
    /// Returns the nodes using this algorithm:
    /// 1. Return the current intermediate branch node if it hasn't been updated.
    /// 2. Advance the trie walker to the next intermediate branch node and retrieve next
    ///    unprocessed key.
    /// 3. Reposition the hashed cursor on the next unprocessed key.
    /// 4. Return every hashed entry up to the key of the current intermediate branch node.
    /// 5. Repeat.
    ///
    /// NOTE: The iteration will start from the key of the previous hashed entry if it was supplied.
    #[instrument(
        level = "trace",
        target = "trie::node_iter",
        skip_all,
        fields(trie_type = ?self.trie_type),
        ret
    )]
    pub fn try_next(
        &mut self,
    ) -> Result<Option<TrieElement<<H as HashedCursor>::Value>>, DatabaseError> {
        loop {
            // If the walker has a key...
            if let Some(key) = self.walker.key() {
                // Ensure that the current walker key shouldn't be checked and there's no previous
                // hashed key
                if !self.should_check_walker_key && self.previous_hashed_key.is_none() {
                    // Make sure we check the next walker key, because we only know we can skip the
                    // current one.
                    self.should_check_walker_key = true;
                    // If it's possible to skip the current node in the walker, return a branch node
                    if self.walker.can_skip_current_node {
                        #[cfg(feature = "metrics")]
                        self.metrics.inc_branch_nodes_returned();
                        return Ok(Some(TrieElement::Branch(TrieBranchNode::new(
                            *key,
                            self.walker.hash().unwrap(),
                            self.walker.children_are_in_trie(),
                        ))));
                    }
                }
            }

            // If there's a hashed entry...
            if let Some((hashed_key, value)) = self.current_hashed_entry.take() {
                // Check if the walker's key is less than the key of the current hashed entry
                if self.walker.key().is_some_and(|key| key < &Nibbles::unpack(hashed_key)) {
                    self.should_check_walker_key = false;
                    continue;
                }

                // Set the next hashed entry as a leaf node and return
                trace!(target: "trie::node_iter", ?hashed_key, "next hashed entry");
                self.current_hashed_entry = self.next_hashed_entry()?;

                #[cfg(feature = "metrics")]
                self.metrics.inc_leaf_nodes_returned();
                return Ok(Some(TrieElement::Leaf(hashed_key, value)));
            }

            // Handle seeking and advancing based on the previous hashed key
            match self.previous_hashed_key.take() {
                Some(hashed_key) => {
                    trace!(target: "trie::node_iter", ?hashed_key, "seeking to the previous hashed entry");
                    // Seek to the previous hashed key and get the next hashed entry
                    self.seek_hashed_entry(hashed_key)?;
                    self.current_hashed_entry = self.next_hashed_entry()?;
                }
                None => {
                    // Get the seek key and set the current hashed entry based on walker's next
                    // unprocessed key
                    let (seek_key, seek_prefix) = match self.walker.next_unprocessed_key() {
                        Some(key) => key,
                        None => break, // no more keys
                    };

                    trace!(
                        target: "trie::node_iter",
                        ?seek_key,
                        can_skip_current_node = self.walker.can_skip_current_node,
                        last = ?self.walker.stack.last(),
                        "seeking to the next unprocessed hashed entry"
                    );
                    let can_skip_node = self.walker.can_skip_current_node;
                    self.walker.advance()?;
                    trace!(
                        target: "trie::node_iter",
                        last = ?self.walker.stack.last(),
                        "advanced walker"
                    );

                    // We should get the iterator to return a branch node if we can skip the
                    // current node and the tree flag for the current node is set.
                    //
                    // `can_skip_node` is already set when the hash flag is set, so we don't need
                    // to check for the hash flag explicitly.
                    //
                    // It is possible that the branch node at the key `seek_key` is not stored in
                    // the database, so the walker will advance to the branch node after it. Because
                    // of this, we need to check that the current walker key has a prefix of the key
                    // that we seeked to.
                    if can_skip_node &&
                        self.walker.key().is_some_and(|key| key.starts_with(&seek_prefix)) &&
                        self.walker.children_are_in_trie()
                    {
                        trace!(
                            target: "trie::node_iter",
                            ?seek_key,
                            walker_hash = ?self.walker.maybe_hash(),
                            "skipping hashed seek"
                        );

                        self.should_check_walker_key = false;
                        continue;
                    }

                    self.current_hashed_entry = self.seek_hashed_entry(seek_key)?;
                }
            }
        }

        Ok(None)
    }
}

