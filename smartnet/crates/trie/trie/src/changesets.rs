//! Trie changeset computation.
//!
//! This module provides functionality to compute trie changesets from trie updates.
//! Changesets represent the old values of trie nodes before a block was applied,
//! enabling reorgs by reverting blocks to their previous state.
//!
//! ## Overview
//!
//! When a block is executed, the trie is updated with new node values. To support
//! chain reorganizations, we need to preserve the old values that existed before
//! the block was applied. These old values are called "changesets".
//!
//! ## Usage
//!
//! The primary function is `compute_trie_changesets`, which takes:
//! - A `TrieCursorFactory` for reading current trie state
//! - `TrieUpdatesSorted` containing the new node values
//!
//! And returns `TrieUpdatesSorted` containing the old node values.

use crate::trie_cursor::TrieCursorIter;
use alloy_primitives::{map::B256Map, B256};
use itertools::{merge_join_by, EitherOrBoth};
use smartnet_trie_common::{
    updates::{StorageTrieUpdatesSorted, TrieUpdatesSorted},
    BranchNodeCompact, Nibbles,
};
use smartnet_storage_errors::db::DatabaseError;
use std::cmp::Ordering;

use crate::trie_cursor::{TrieCursor, TrieCursorFactory, TrieStorageCursor};

/// Result type for changeset operations.
pub type ChangesetResult<T> = Result<T, DatabaseError>;

/// Computes trie changesets by looking up current node values from the trie.
///
/// Takes the new trie updates and queries the trie for the old values of
/// changed nodes. Returns changesets representing the state before the block
/// was applied, suitable for reorg operations.
///
/// # Arguments
///
/// * `factory` - Trie cursor factory for reading current trie state
/// * `trie_updates` - New trie node values produced by state root computation
///
/// # Returns
///
/// `TrieUpdatesSorted` containing old node values (before this block)
pub fn compute_trie_changesets<Factory>(
    factory: &Factory,
    trie_updates: &TrieUpdatesSorted,
) -> ChangesetResult<TrieUpdatesSorted>
where
    Factory: TrieCursorFactory,
{
    // Compute account trie changesets
    let account_nodes = compute_account_changesets(factory, trie_updates)?;

    // Compute storage trie changesets
    let mut storage_tries = B256Map::default();

    // Create storage cursor once and reuse it for all addresses
    let mut storage_cursor = factory.storage_trie_cursor(B256::default())?;

    for (hashed_address, storage_updates) in trie_updates.storage_tries_ref() {
        storage_cursor.set_hashed_address(*hashed_address);

        let storage_changesets = if storage_updates.is_deleted() {
            // Handle wiped storage
            compute_wiped_storage_changesets(&mut storage_cursor, storage_updates)?
        } else {
            // Handle normal storage updates
            compute_storage_changesets(&mut storage_cursor, storage_updates)?
        };

        if !storage_changesets.is_empty() {
            storage_tries.insert(
                *hashed_address,
                StorageTrieUpdatesSorted {
                    is_deleted: storage_updates.is_deleted(),
                    storage_nodes: storage_changesets,
                },
            );
        }
    }

    // Build and return the result
    Ok(TrieUpdatesSorted::new(account_nodes, storage_tries))
}

/// Computes account trie changesets.
///
/// Looks up the current value for each changed account node path and returns
/// a vector of (path, `old_node`) pairs. The result is already sorted since
/// `trie_updates.account_nodes_ref()` is sorted.
fn compute_account_changesets<Factory>(
    factory: &Factory,
    trie_updates: &TrieUpdatesSorted,
) -> ChangesetResult<Vec<(Nibbles, Option<BranchNodeCompact>)>>
where
    Factory: TrieCursorFactory,
{
    let mut cursor = factory.account_trie_cursor()?;
    let mut account_changesets = Vec::with_capacity(trie_updates.account_nodes_ref().len());

    // For each changed account node, look up its current value
    // The input is already sorted, so the output will be sorted
    for (path, _new_node) in trie_updates.account_nodes_ref() {
        let old_node = cursor.seek_exact(*path)?.map(|(_path, node)| node);
        account_changesets.push((*path, old_node));
    }

    Ok(account_changesets)
}

/// Computes storage trie changesets for a single account.
///
/// Looks up the current value for each changed storage node path and returns
/// a vector of (path, `old_node`) pairs. The result is already sorted since
/// `storage_updates.storage_nodes` is sorted.
///
/// # Arguments
///
/// * `cursor` - Reusable storage trie cursor. The hashed address will be set before use.
/// * `hashed_address` - The hashed address of the account
/// * `storage_updates` - Storage trie updates for this account
fn compute_storage_changesets(
    cursor: &mut impl TrieStorageCursor,
    storage_updates: &StorageTrieUpdatesSorted,
) -> ChangesetResult<Vec<(Nibbles, Option<BranchNodeCompact>)>> {
    let mut storage_changesets = Vec::with_capacity(storage_updates.storage_nodes.len());

    // For each changed storage node, look up its current value
    // The input is already sorted, so the output will be sorted
    for (path, _new_node) in &storage_updates.storage_nodes {
        let old_node = cursor.seek_exact(*path)?.map(|(_path, node)| node);
        storage_changesets.push((*path, old_node));
    }

    Ok(storage_changesets)
}

/// Handles wiped storage trie changeset computation.
///
/// When an account's storage is completely wiped (e.g., account is destroyed),
/// we need to capture not just the changed nodes, but ALL existing nodes in
/// the storage trie, since they all will be deleted.
///
/// This uses an iterator-based approach to avoid allocating an intermediate Vec.
/// It merges two sorted iterators:
/// - Current values of changed paths
/// - All existing nodes in the storage trie
///
/// # Arguments
///
/// * `changed_cursor` - Cursor for looking up changed node values
/// * `wiped_cursor` - Cursor for iterating all nodes in the storage trie
/// * `hashed_address` - The hashed address of the account
/// * `storage_updates` - Storage trie updates for this account
fn compute_wiped_storage_changesets(
    cursor: &mut impl TrieStorageCursor,
    storage_updates: &StorageTrieUpdatesSorted,
) -> ChangesetResult<Vec<(Nibbles, Option<BranchNodeCompact>)>> {
    // Set the hashed address for this account's storage trie
    // Create an iterator that yields all nodes in the storage trie
    let all_nodes = TrieCursorIter::new(cursor);

    // Merge the two sorted iterators
    let merged = storage_trie_wiped_changeset_iter(
        storage_updates.storage_nodes.iter().map(|e| e.0),
        all_nodes,
    )?;

    // Collect into a Vec
    let mut storage_changesets = Vec::new();
    for result in merged {
        storage_changesets.push(result?);
    }

    Ok(storage_changesets)
}

/// Returns an iterator which produces the changeset values for an account whose storage was wiped
/// during a block.
///
/// ## Arguments
///
/// - `curr_values_of_changed` is an iterator over the current values of all trie nodes modified by
///   the block, ordered by path.
/// - `all_nodes` is an iterator over all existing trie nodes for the account, ordered by path.
///
/// ## Returns
///
/// An iterator of trie node paths and a `Some(node)` (indicating the node was wiped) or a `None`
/// (indicating the node was modified in the block but didn't previously exist). The iterator's
/// results will be ordered by path.
pub fn storage_trie_wiped_changeset_iter(
    changed_paths: impl Iterator<Item = Nibbles>,
    all_nodes: impl Iterator<Item = Result<(Nibbles, BranchNodeCompact), DatabaseError>>,
) -> Result<
    impl Iterator<Item = Result<(Nibbles, Option<BranchNodeCompact>), DatabaseError>>,
    DatabaseError,
> {
    let all_nodes = all_nodes.map(|e| e.map(|(nibbles, node)| (nibbles, Some(node))));

    let merged = merge_join_by(changed_paths, all_nodes, |a, b| match (a, b) {
        (_, Err(_)) => Ordering::Greater,
        (a, Ok(b)) => a.cmp(&b.0),
    });

    Ok(merged.map(|either_or| match either_or {
        EitherOrBoth::Left(changed) => {
            // A path of a changed node which was not found in the database. The current value of
            // this path must be None, otherwise it would have also been returned by the `all_nodes`
            // iter.
            Ok((changed, None))
        }
        EitherOrBoth::Right(wiped) => {
            // A node was found in the db (indicating it was wiped) but was not a changed node.
            // Return it as-is.
            wiped
        }
        EitherOrBoth::Both(_changed, wiped) => {
            // A path of a changed node was found with a previous value in the database. If the
            // changed node had no previous value (None) it wouldn't be returned by `all_nodes` and
            // so would be in the Left branch.
            //
            // Due to the ordering closure passed to `merge_join_by` it's not possible for wrapped
            // to be an error here.
            debug_assert!(wiped.is_ok(), "unreachable error condition: {wiped:?}");
            wiped
        }
    }))
}

