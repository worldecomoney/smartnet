use super::TrieCursor;
use crate::{BranchNodeCompact, Nibbles};
use smartnet_storage_errors::db::DatabaseError;
use std::{cmp::Ordering, iter::FusedIterator};
use tracing::trace;

/// Compares two Nibbles in depth-first order.
///
/// See [`smartnet_trie_common::depth_first_cmp`] for details.
pub fn cmp(a: &Nibbles, b: &Nibbles) -> Ordering {
    smartnet_trie_common::depth_first_cmp(a, b)
}

/// An iterator that traverses trie nodes in depth-first post-order.
///
/// This iterator yields nodes in post-order traversal (children before parents),
/// which matches the `cmp` comparison function where descendants
/// come before their ancestors.
#[derive(Debug)]
pub struct DepthFirstTrieIterator<C: TrieCursor> {
    /// The underlying trie cursor.
    cursor: C,
    /// Set to true once the trie cursor has done its initial seek to the root node.
    initialized: bool,
    /// Stack of nodes which have been fetched. Each node's path is a prefix of the next's.
    stack: Vec<(Nibbles, BranchNodeCompact)>,
    /// Nodes which are ready to be yielded from `next`.
    next: Vec<(Nibbles, BranchNodeCompact)>,
    /// Set to true once the cursor has been exhausted.
    complete: bool,
}

impl<C: TrieCursor> DepthFirstTrieIterator<C> {
    /// Create a new depth-first iterator from a trie cursor.
    pub fn new(cursor: C) -> Self {
        Self {
            cursor,
            initialized: false,
            stack: Default::default(),
            next: Default::default(),
            complete: false,
        }
    }

    fn push(&mut self, path: Nibbles, node: BranchNodeCompact) {
        loop {
            match self.stack.last() {
                None => {
                    // If the stack is empty then we push this node onto it, as it may have child
                    // nodes which need to be yielded first.
                    self.stack.push((path, node));
                    break;
                }
                Some((top_path, _)) if path.starts_with(top_path) => {
                    // If the top of the stack is a prefix of this node, it means this node is a
                    // child of the top of the stack (and all other nodes on the stack). Push this
                    // node onto the stack, as future nodes may be children of it.
                    self.stack.push((path, node));
                    break;
                }
                Some((_, _)) => {
                    // The top of the stack is not a prefix of this node, therefore it is not a
                    // parent of this node. Yield the top of the stack, and loop back to see if this
                    // node is a child of the new top-of-stack.
                    self.next.push(self.stack.pop().expect("stack is not empty"));
                }
            }
        }

        // We will have popped off the top of the stack in the order we want to yield nodes, but
        // `next` is itself popped off so it needs to be reversed.
        self.next.reverse();
    }

    fn fill_next(&mut self) -> Result<(), DatabaseError> {
        debug_assert!(self.next.is_empty());

        loop {
            let Some((path, node)) = (if self.initialized {
                self.cursor.next()?
            } else {
                self.initialized = true;
                self.cursor.seek(Nibbles::new())?
            }) else {
                // Record that the cursor is empty and yield the stack. The stack is in reverse
                // order of what we want to yield, but `next` is popped from, so we don't have to
                // reverse it.
                self.complete = true;
                self.next = core::mem::take(&mut self.stack);
                return Ok(());
            };

            trace!(
                target: "trie::trie_cursor::depth_first",
                ?path,
                "Iterated from cursor",
            );

            self.push(path, node);
            if !self.next.is_empty() {
                return Ok(());
            }
        }
    }
}

impl<C: TrieCursor> Iterator for DepthFirstTrieIterator<C> {
    type Item = Result<(Nibbles, BranchNodeCompact), DatabaseError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(next) = self.next.pop() {
                return Some(Ok(next));
            }

            if self.complete {
                return None;
            }

            if let Err(err) = self.fill_next() {
                self.complete = true;
                return Some(Err(err));
            }
        }
    }
}

impl<C: TrieCursor> FusedIterator for DepthFirstTrieIterator<C> {}

