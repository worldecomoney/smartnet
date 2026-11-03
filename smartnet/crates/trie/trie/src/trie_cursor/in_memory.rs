use super::{TrieCursor, TrieCursorFactory, TrieStorageCursor};
use crate::{forward_cursor::ForwardInMemoryCursor, updates::TrieUpdatesSorted};
use alloy_primitives::B256;
use smartnet_trie_common::{BranchNodeCompact, Nibbles};
use smartnet_storage_errors::db::DatabaseError;

/// The trie cursor factory for the trie updates.
#[derive(Debug, Clone)]
pub struct InMemoryTrieCursorFactory<CF, T> {
    /// Underlying trie cursor factory.
    cursor_factory: CF,
    /// Reference to sorted trie updates.
    trie_updates: T,
}

impl<CF, T> InMemoryTrieCursorFactory<CF, T> {
    /// Create a new trie cursor factory.
    pub const fn new(cursor_factory: CF, trie_updates: T) -> Self {
        Self { cursor_factory, trie_updates }
    }
}

impl<'overlay, CF, T> TrieCursorFactory for InMemoryTrieCursorFactory<CF, &'overlay T>
where
    CF: TrieCursorFactory + 'overlay,
    T: AsRef<TrieUpdatesSorted>,
{
    type AccountTrieCursor<'cursor>
        = InMemoryTrieCursor<'overlay, CF::AccountTrieCursor<'cursor>>
    where
        Self: 'cursor;

    type StorageTrieCursor<'cursor>
        = InMemoryTrieCursor<'overlay, CF::StorageTrieCursor<'cursor>>
    where
        Self: 'cursor;

    fn account_trie_cursor(&self) -> Result<Self::AccountTrieCursor<'_>, DatabaseError> {
        let cursor = self.cursor_factory.account_trie_cursor()?;
        Ok(InMemoryTrieCursor::new_account(cursor, self.trie_updates.as_ref()))
    }

    fn storage_trie_cursor(
        &self,
        hashed_address: B256,
    ) -> Result<Self::StorageTrieCursor<'_>, DatabaseError> {
        let trie_updates = self.trie_updates.as_ref();
        let cursor = self.cursor_factory.storage_trie_cursor(hashed_address)?;
        Ok(InMemoryTrieCursor::new_storage(cursor, trie_updates, hashed_address))
    }
}

/// A cursor to iterate over trie updates and corresponding database entries.
/// It will always give precedence to the data from the trie updates.
#[derive(Debug)]
pub struct InMemoryTrieCursor<'a, C> {
    /// The underlying cursor.
    cursor: C,
    /// Whether the underlying cursor should be ignored (when storage trie was wiped).
    cursor_wiped: bool,
    /// Entry that `cursor` is currently pointing to.
    cursor_entry: Option<(Nibbles, BranchNodeCompact)>,
    /// Forward-only in-memory cursor over storage trie nodes.
    in_memory_cursor: ForwardInMemoryCursor<'a, Nibbles, Option<BranchNodeCompact>>,
    /// The key most recently returned from the Cursor.
    last_key: Option<Nibbles>,
    /// Whether an initial seek was called.
    seeked: bool,
    /// Reference to the full trie updates.
    trie_updates: &'a TrieUpdatesSorted,
}

impl<'a, C: TrieCursor> InMemoryTrieCursor<'a, C> {
    /// Create new account trie cursor which combines a DB cursor and the trie updates.
    pub fn new_account(cursor: C, trie_updates: &'a TrieUpdatesSorted) -> Self {
        let in_memory_cursor = ForwardInMemoryCursor::new(trie_updates.account_nodes_ref());
        Self {
            cursor,
            cursor_wiped: false,
            cursor_entry: None,
            in_memory_cursor,
            last_key: None,
            seeked: false,
            trie_updates,
        }
    }

    /// Create new storage trie cursor with full trie updates reference.
    /// This allows the cursor to switch between storage tries when `set_hashed_address` is called.
    pub fn new_storage(
        cursor: C,
        trie_updates: &'a TrieUpdatesSorted,
        hashed_address: B256,
    ) -> Self {
        let (in_memory_cursor, cursor_wiped) =
            Self::get_storage_overlay(trie_updates, hashed_address);
        Self {
            cursor,
            cursor_wiped,
            cursor_entry: None,
            in_memory_cursor,
            last_key: None,
            seeked: false,
            trie_updates,
        }
    }

    /// Returns the storage overlay for `hashed_address` and whether it was deleted.
    fn get_storage_overlay(
        trie_updates: &'a TrieUpdatesSorted,
        hashed_address: B256,
    ) -> (ForwardInMemoryCursor<'a, Nibbles, Option<BranchNodeCompact>>, bool) {
        let storage_trie_updates = trie_updates.storage_tries_ref().get(&hashed_address);
        let cursor_wiped = storage_trie_updates.is_some_and(|u| u.is_deleted());
        let storage_nodes = storage_trie_updates.map(|u| u.storage_nodes_ref()).unwrap_or(&[]);

        (ForwardInMemoryCursor::new(storage_nodes), cursor_wiped)
    }

    /// Returns a mutable reference to the underlying cursor if it's not wiped, None otherwise.
    fn get_cursor_mut(&mut self) -> Option<&mut C> {
        (!self.cursor_wiped).then_some(&mut self.cursor)
    }

    /// Asserts that the next entry to be returned from the cursor is not previous to the last entry
    /// returned.
    fn set_last_key(&mut self, next_entry: &Option<(Nibbles, BranchNodeCompact)>) {
        let next_key = next_entry.as_ref().map(|e| e.0);
        debug_assert!(
            self.last_key.is_none_or(|last| next_key.is_none_or(|next| next >= last)),
            "Cannot return entry {:?} previous to the last returned entry at {:?}",
            next_key,
            self.last_key,
        );
        self.last_key = next_key;
    }

    /// Seeks the `cursor_entry` field of the struct using the cursor.
    fn cursor_seek(&mut self, key: Nibbles) -> Result<(), DatabaseError> {
        // Only seek if:
        // 1. We have a cursor entry and need to seek forward (entry.0 < key), OR
        // 2. We have no cursor entry and haven't seeked yet (!self.seeked)
        let should_seek = match self.cursor_entry.as_ref() {
            Some(entry) => entry.0 < key,
            None => !self.seeked,
        };

        if should_seek {
            self.cursor_entry = self.get_cursor_mut().map(|c| c.seek(key)).transpose()?.flatten();
        }

        Ok(())
    }

    /// Seeks the `cursor_entry` field of the struct to the subsequent entry using the cursor.
    fn cursor_next(&mut self) -> Result<(), DatabaseError> {
        debug_assert!(self.seeked);

        // If the previous entry is `None`, and we've done a seek previously, then the cursor is
        // exhausted and we shouldn't call `next` again.
        if self.cursor_entry.is_some() {
            self.cursor_entry = self.get_cursor_mut().map(|c| c.next()).transpose()?.flatten();
        }

        Ok(())
    }

    /// Compares the current in-memory entry with the current entry of the cursor, and applies the
    /// in-memory entry to the cursor entry as an overlay.
    //
    /// This may consume and move forward the current entries when the overlay indicates a removed
    /// node.
    fn choose_next_entry(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        loop {
            match (self.in_memory_cursor.current().cloned(), &self.cursor_entry) {
                (Some((mem_key, None)), _)
                    if self.cursor_entry.as_ref().is_none_or(|(db_key, _)| &mem_key < db_key) =>
                {
                    // If overlay has a removed node but DB cursor is exhausted or ahead of the
                    // in-memory cursor then move ahead in-memory, as there might be further
                    // non-removed overlay nodes.
                    self.in_memory_cursor.first_after(&mem_key);
                }
                (Some((mem_key, None)), Some((db_key, _))) if &mem_key == db_key => {
                    // If overlay has a removed node which is returned from DB then move both
                    // cursors ahead to the next key.
                    self.in_memory_cursor.first_after(&mem_key);
                    self.cursor_next()?;
                }
                (Some((mem_key, Some(node))), _)
                    if self.cursor_entry.as_ref().is_none_or(|(db_key, _)| &mem_key <= db_key) =>
                {
                    // If overlay returns a node prior to the DB's node, or the DB is exhausted,
                    // then we return the overlay's node.
                    return Ok(Some((mem_key, node)));
                }
                // All other cases:
                // - mem_key > db_key
                // - overlay is exhausted
                // Return the db_entry. If DB is also exhausted then this returns None.
                _ => return Ok(self.cursor_entry.clone()),
            }
        }
    }
}

impl<C: TrieCursor> TrieCursor for InMemoryTrieCursor<'_, C> {
    fn seek_exact(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.cursor_seek(key)?;
        let mem_entry = self.in_memory_cursor.seek(&key);

        self.seeked = true;

        let entry = match (mem_entry, &self.cursor_entry) {
            (Some((mem_key, entry_inner)), _) if *mem_key == key => {
                entry_inner.clone().map(|node| (key, node))
            }
            (_, Some((db_key, node))) if db_key == &key => Some((key, node.clone())),
            _ => None,
        };

        self.set_last_key(&entry);
        Ok(entry)
    }

    fn seek(
        &mut self,
        key: Nibbles,
    ) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        self.cursor_seek(key)?;
        self.in_memory_cursor.seek(&key);

        self.seeked = true;

        let entry = self.choose_next_entry()?;
        self.set_last_key(&entry);
        Ok(entry)
    }

    fn next(&mut self) -> Result<Option<(Nibbles, BranchNodeCompact)>, DatabaseError> {
        debug_assert!(self.seeked, "Cursor must be seek'd before next is called");

        // A `last_key` of `None` indicates that the cursor is exhausted.
        let Some(last_key) = self.last_key else {
            return Ok(None);
        };

        // If either cursor is currently pointing to the last entry which was returned then consume
        // that entry so that `choose_next_entry` is looking at the subsequent one.
        if let Some((key, _)) = self.in_memory_cursor.current() &&
            key == &last_key
        {
            self.in_memory_cursor.first_after(&last_key);
        }

        if let Some((key, _)) = &self.cursor_entry &&
            key == &last_key
        {
            self.cursor_next()?;
        }

        let entry = self.choose_next_entry()?;
        self.set_last_key(&entry);
        Ok(entry)
    }

    fn current(&mut self) -> Result<Option<Nibbles>, DatabaseError> {
        match &self.last_key {
            Some(key) => Ok(Some(*key)),
            None => Ok(self.get_cursor_mut().map(|c| c.current()).transpose()?.flatten()),
        }
    }

    fn reset(&mut self) {
        let Self {
            cursor,
            cursor_wiped,
            cursor_entry,
            in_memory_cursor,
            last_key,
            seeked,
            trie_updates: _,
        } = self;

        cursor.reset();
        in_memory_cursor.reset();

        *cursor_wiped = false;
        *cursor_entry = None;
        *last_key = None;
        *seeked = false;
    }
}

impl<C: TrieStorageCursor> TrieStorageCursor for InMemoryTrieCursor<'_, C> {
    fn set_hashed_address(&mut self, hashed_address: B256) {
        self.reset();
        self.cursor.set_hashed_address(hashed_address);
        (self.in_memory_cursor, self.cursor_wiped) =
            Self::get_storage_overlay(self.trie_updates, hashed_address);
    }
}
