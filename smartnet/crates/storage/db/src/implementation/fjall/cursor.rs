//! Fjall cursor wrapper.

use super::{
    decode_pair, encode_value, err_info, physical_key, prefix_end, successor, table_key,
    tx::Shared,
    view::{entry_before, entry_from, exists, get_effective, keys_in, Entry, View},
    write_error,
};
use crate::DatabaseError;
use smartnet_db_api::{
    common::{PairResult, ValueOnlyResult},
    cursor::{
        DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW, DupWalker, RangeWalker,
        ReverseWalker, Walker,
    },
    table::{Decompress, DupSort, Encode, IntoVec, Table},
};
use smartnet_storage_errors::db::DatabaseWriteOperation;
use std::{
    collections::Bound,
    fmt::{self, Debug},
    marker::PhantomData,
    ops::RangeBounds,
    sync::Arc,
};

/// Cursor over a single Fjall keyspace.
///
/// Fjall iterators are one-shot, so the cursor keeps the physical key of the entry it points at
/// and re-seeks on every movement. For `DupSort` tables the physical key is
/// `encoded key ++ encoded value`, which keeps duplicates of a key adjacent and ordered.
///
/// Every movement merges the owning transaction's buffered writes over the stored data, so a
/// cursor observes read-your-writes like the rest of the transaction does. Writes go into that
/// same buffer rather than the keyspace, which is what makes them invisible to other
/// transactions, absent from Fjall's log until commit, and gone entirely if the transaction is
/// dropped.
pub struct Cursor<T: Table> {
    /// Stored data this cursor reads, pinned to the transaction's snapshot when it has one.
    view: View,
    /// Write state of the transaction that created the cursor, shared with it so that writes
    /// made through the cursor are buffered and flushed together with the transaction's own
    /// writes.
    shared: Arc<Shared>,
    /// Whether the owning transaction accepts writes: only cursors created by
    /// [`DbTxMut::cursor_write`] do, even on a writable transaction.
    writable: bool,
    /// Physical key of the entry the cursor points at, if any.
    current: Option<Vec<u8>>,
    /// Whether a miss cleared the position past the last entry (`first`/`last`/`seek` finding
    /// nothing). A fresh cursor has no position either, but it sits *before* the first entry,
    /// so its `next` yields that entry; from past the end `next` must yield nothing instead of
    /// silently restarting at the beginning.
    past_end: bool,
    _phantom: PhantomData<T>,
}

impl<T: Table> Debug for Cursor<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cursor").field("current", &self.current).finish()
    }
}

impl<T: Table> Cursor<T> {
    /// Creates a cursor positioned before the first entry of the keyspace.
    pub(crate) const fn new(view: View, shared: Arc<Shared>, writable: bool) -> Self {
        Self { view, shared, writable, current: None, past_end: false, _phantom: PhantomData }
    }

    /// Fails the operation when the cursor belongs to a read-only transaction.
    fn ensure_writable(&self) -> Result<(), DatabaseError> {
        if self.writable {
            Ok(())
        } else {
            Err(DatabaseError::Other("cursor belongs to a read-only transaction".to_string()))
        }
    }

    /// The first visible entry at or after `start`, with the transaction's buffer laid over
    /// the stored data and pending deletions skipped.
    fn first_at_or_after(&self, start: &[u8]) -> Result<Option<Entry>, DatabaseError> {
        let pending = self.shared.lock_pending();
        entry_from(&self.view, pending.get(T::NAME), start)
    }

    /// The last visible entry before `end`, or the last one at all when `end` is `None`.
    fn last_before(&self, end: Option<&[u8]>) -> Result<Option<Entry>, DatabaseError> {
        let pending = self.shared.lock_pending();
        entry_before(&self.view, pending.get(T::NAME), end)
    }

    /// The value at `physical` once the transaction's buffer is applied.
    fn value_at(&self, physical: &[u8]) -> Result<Option<Vec<u8>>, DatabaseError> {
        let pending = self.shared.lock_pending();
        get_effective(&self.view, pending.get(T::NAME), physical)
    }

    /// Whether `physical` exists once the transaction's buffer is applied.
    fn is_present(&self, physical: &[u8]) -> Result<bool, DatabaseError> {
        let pending = self.shared.lock_pending();
        exists(&self.view, pending.get(T::NAME), physical)
    }

    /// Moves onto `entry` and decodes it as the current entry, keeping the position when
    /// there is no entry to move onto.
    fn land(&mut self, entry: Option<(Vec<u8>, Vec<u8>)>) -> PairResult<T> {
        match entry {
            Some((stored_key, stored_value)) => {
                self.current = Some(stored_key.clone());
                self.past_end = false;
                decode_pair::<T>(&stored_key, &stored_value).map(Some)
            }
            None => Ok(None),
        }
    }

    /// Like [`Cursor::land`], but a miss also clears the position, as seeks and the
    /// first/last movements require.
    fn land_or_clear(&mut self, entry: Option<(Vec<u8>, Vec<u8>)>) -> PairResult<T> {
        if entry.is_none() {
            self.current = None;
            self.past_end = true;
        }
        self.land(entry)
    }

    /// Returns the encoded table key of the entry at `physical`.
    ///
    /// For `DupSort` tables the physical key carries the value as a suffix, so the stored value
    /// is needed to split it.
    fn primary_key(&self, physical: &[u8]) -> Result<Option<Vec<u8>>, DatabaseError> {
        match self.value_at(physical)? {
            Some(value) => Ok(Some(table_key::<T>(physical, &value).to_vec())),
            None => Ok(None),
        }
    }

    /// Returns the encoded bytes of `key`.
    fn encoded<K: Encode>(key: K) -> Vec<u8> {
        key.encode().into_vec()
    }

    /// Buffers `(key, value)` and moves the cursor onto it.
    fn write_entry(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        self.ensure_writable()?;
        let encoded_key = Self::encoded(key);
        let encoded_value = encode_value(value);
        let physical = physical_key::<T>(&encoded_key, &encoded_value);

        self.shared.set(T::NAME, &physical, Some(&encoded_value));
        self.current = Some(physical);
        self.past_end = false;

        Ok(())
    }
}

impl<T: Table> DbCursorRO<T> for Cursor<T> {
    fn first(&mut self) -> PairResult<T> {
        let entry = self.first_at_or_after(&[])?;
        self.land_or_clear(entry)
    }

    fn seek_exact(&mut self, key: T::Key) -> PairResult<T> {
        let encoded = Self::encoded(key);

        let entry = if T::DUPSORT {
            // Duplicates live under `encoded key ++ encoded value`, so the first entry with the
            // encoded key as prefix is the first duplicate.
            self.first_at_or_after(&encoded)?
                .filter(|(physical, value)| table_key::<T>(physical, value) == encoded.as_slice())
        } else {
            self.value_at(&encoded)?.map(|value| (encoded.clone(), value))
        };

        self.land_or_clear(entry)
    }

    fn seek(&mut self, key: T::Key) -> PairResult<T> {
        let encoded = Self::encoded(key);
        let entry = self.first_at_or_after(&encoded)?;
        self.land_or_clear(entry)
    }

    fn next(&mut self) -> PairResult<T> {
        // Past the end there is no successor: a cleared position must not restart at the
        // first entry (only a fresh cursor, which sits before it, may).
        if self.current.is_none() && self.past_end {
            return Ok(None);
        }

        let start = match &self.current {
            Some(current) => successor(current),
            None => Vec::new(),
        };

        let entry = self.first_at_or_after(&start)?;
        self.land(entry)
    }

    fn prev(&mut self) -> PairResult<T> {
        let end = self.current.clone();
        let entry = self.last_before(end.as_deref())?;
        self.land(entry)
    }

    fn last(&mut self) -> PairResult<T> {
        let entry = self.last_before(None)?;
        self.land_or_clear(entry)
    }

    fn current(&mut self) -> PairResult<T> {
        let Some(physical) = self.current.clone() else { return Ok(None) };
        match self.value_at(&physical)? {
            Some(value) => decode_pair::<T>(&physical, &value).map(Some),
            None => Ok(None),
        }
    }

    fn walk(&mut self, start_key: Option<T::Key>) -> Result<Walker<'_, T, Self>, DatabaseError> {
        let start = match start_key {
            Some(start_key) => self.seek(start_key).transpose(),
            None => self.first().transpose(),
        };

        Ok(Walker::new(self, start))
    }

    fn walk_range(
        &mut self,
        range: impl RangeBounds<T::Key>,
    ) -> Result<RangeWalker<'_, T, Self>, DatabaseError> {
        let start = match range.start_bound().cloned() {
            Bound::Included(key) => self.seek(key).transpose(),
            Bound::Excluded(_key) => {
                unreachable!("Rust doesn't allow for Bound::Excluded in starting bounds")
            }
            Bound::Unbounded => self.first().transpose(),
        };

        Ok(RangeWalker::new(self, start, range.end_bound().cloned()))
    }

    fn walk_back(
        &mut self,
        start_key: Option<T::Key>,
    ) -> Result<ReverseWalker<'_, T, Self>, DatabaseError> {
        let start = match start_key {
            Some(start_key) => self.seek(start_key).transpose(),
            None => self.last().transpose(),
        };

        Ok(ReverseWalker::new(self, start))
    }
}

impl<T: DupSort> DbDupCursorRO<T> for Cursor<T> {
    fn prev_dup(&mut self) -> PairResult<T> {
        let Some(physical) = self.current.clone() else { return Ok(None) };
        let Some(primary) = self.primary_key(&physical)? else { return Ok(None) };

        let entry = self.last_before(Some(&physical))?;
        match entry {
            Some((stored_key, stored_value))
                if table_key::<T>(&stored_key, &stored_value) == primary.as_slice() =>
            {
                self.current = Some(stored_key.clone());
                decode_pair::<T>(&stored_key, &stored_value).map(Some)
            }
            _ => Ok(None),
        }
    }

    fn next_dup(&mut self) -> PairResult<T> {
        let Some(physical) = self.current.clone() else { return Ok(None) };
        let Some(primary) = self.primary_key(&physical)? else { return Ok(None) };

        // Duplicates of a key are contiguous, so only the immediate successor can be a duplicate.
        let entry = self.first_at_or_after(&successor(&physical))?;
        match entry {
            Some((stored_key, stored_value))
                if table_key::<T>(&stored_key, &stored_value) == primary.as_slice() =>
            {
                self.current = Some(stored_key.clone());
                decode_pair::<T>(&stored_key, &stored_value).map(Some)
            }
            _ => Ok(None),
        }
    }

    fn last_dup(&mut self) -> ValueOnlyResult<T> {
        let Some(physical) = self.current.clone() else { return Ok(None) };
        let Some(primary) = self.primary_key(&physical)? else { return Ok(None) };

        // Every duplicate of `primary` sorts below the exclusive end of its prefix, so the
        // last entry below that is the last duplicate.
        let upper = prefix_end(&primary);
        let entry = self.last_before(upper.as_deref())?;
        match entry {
            Some((stored_key, stored_value))
                if table_key::<T>(&stored_key, &stored_value) == primary.as_slice() =>
            {
                self.current = Some(stored_key);
                Ok(Some(Decompress::decompress(&stored_value)?))
            }
            _ => Ok(None),
        }
    }

    fn next_no_dup(&mut self) -> PairResult<T> {
        let Some(physical) = self.current.clone() else { return self.first() };
        let Some(primary) = self.primary_key(&physical)? else { return Ok(None) };

        let mut start = successor(&physical);
        loop {
            let Some((stored_key, stored_value)) = self.first_at_or_after(&start)? else {
                return Ok(None);
            };
            if table_key::<T>(&stored_key, &stored_value) != primary.as_slice() {
                self.current = Some(stored_key.clone());
                return decode_pair::<T>(&stored_key, &stored_value).map(Some);
            }
            start = successor(&stored_key);
        }
    }

    fn next_dup_val(&mut self) -> ValueOnlyResult<T> {
        Ok(self.next_dup()?.map(|(_, value)| value))
    }

    fn seek_by_key_subkey(&mut self, key: T::Key, subkey: T::SubKey) -> ValueOnlyResult<T> {
        Ok(self.seek_pair_by_key_subkey(key, subkey)?.map(|(_, value)| value))
    }

    fn walk_dup(
        &mut self,
        key: Option<T::Key>,
        subkey: Option<T::SubKey>,
    ) -> Result<DupWalker<'_, T, Self>, DatabaseError> {
        let start = match (key, subkey) {
            (Some(key), Some(subkey)) => self.seek_pair_by_key_subkey(key, subkey).transpose(),
            (Some(key), None) => self.seek_exact(key).transpose(),
            (None, Some(subkey)) => match self.first()? {
                Some((first_key, _)) => self.seek_pair_by_key_subkey(first_key, subkey).transpose(),
                None => Some(Err(DatabaseError::Read(err_info("table is empty")))),
            },
            (None, None) => self.first().transpose(),
        };

        Ok(DupWalker { cursor: self, start })
    }
}

impl<T: DupSort> Cursor<T> {
    /// Seeks to the first entry at or after `(key, subkey)` that still belongs to `key`,
    /// returning the whole pair.
    ///
    /// Duplicates are stored under `encoded key ++ encoded value`, and the subkey is a prefix of
    /// the encoded value, so seeking to `encoded key ++ encoded subkey` lands on the first
    /// duplicate at or after the subkey.
    pub(crate) fn seek_pair_by_key_subkey(
        &mut self,
        key: T::Key,
        subkey: T::SubKey,
    ) -> PairResult<T> {
        let primary = Self::encoded(key);
        let mut target = primary.clone();
        target.extend_from_slice(&Self::encoded(subkey)[..]);

        match self.first_at_or_after(&target)? {
            Some((stored_key, stored_value))
                if table_key::<T>(&stored_key, &stored_value) == primary.as_slice() =>
            {
                self.current = Some(stored_key.clone());
                decode_pair::<T>(&stored_key, &stored_value).map(Some)
            }
            _ => {
                self.current = None;
                Ok(None)
            }
        }
    }
}

impl<T: Table> DbCursorRW<T> for Cursor<T> {
    fn upsert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        self.write_entry(key, value)
    }

    fn insert(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        self.ensure_writable()?;
        let encoded_key = Self::encoded(key);
        let encoded_value = encode_value(value);
        let physical = physical_key::<T>(&encoded_key, &encoded_value);

        if self.is_present(&physical)? {
            return Err(write_error::<T>(
                DatabaseWriteOperation::CursorInsert,
                physical,
                "key already exists",
            ));
        }

        self.shared.set(T::NAME, &physical, Some(&encoded_value));
        self.current = Some(physical);

        Ok(())
    }

    fn append(&mut self, key: T::Key, value: &T::Value) -> Result<(), DatabaseError> {
        // Fjall has no append mode; the LSM-tree does not require ordered appends.
        self.write_entry(key, value)
    }

    fn delete_current(&mut self) -> Result<(), DatabaseError> {
        self.ensure_writable()?;
        let Some(physical) = self.current.clone() else {
            return Err(DatabaseError::Delete(err_info("cursor is not positioned")));
        };

        if !self.is_present(&physical)? {
            // Deleting a missing entry is a no-op.
            return Ok(());
        }

        self.shared.set(T::NAME, &physical, None);

        // The position is kept so that `next` and `prev` continue from the deleted entry.
        Ok(())
    }
}

impl<T: DupSort> DbDupCursorRW<T> for Cursor<T> {
    fn delete_current_duplicates(&mut self) -> Result<(), DatabaseError> {
        self.ensure_writable()?;
        let Some(physical) = self.current.clone() else {
            return Err(DatabaseError::Delete(err_info("cursor is not positioned")));
        };
        let Some(primary) = self.primary_key(&physical)? else {
            return Err(DatabaseError::Delete(err_info("no such entry")));
        };

        // Every duplicate of `primary` sorts below the exclusive end of its prefix, which
        // bounds them exactly.
        let upper = prefix_end(&primary).map_or(Bound::Unbounded, Bound::Excluded);
        let bounds = (Bound::Included(primary.clone()), upper);
        let duplicates = {
            let pending = self.shared.lock_pending();
            keys_in(&self.view, pending.get(T::NAME), bounds)?
        };

        if duplicates.is_empty() {
            // Deleting when there is nothing to delete is a no-op.
            return Ok(());
        }

        for duplicate in &duplicates {
            self.shared.set(T::NAME, duplicate, None);
        }

        // Park the cursor on the now empty key: `next` then returns the first entry of the
        // following key and `prev` the last entry of the preceding one.
        self.current = Some(primary);

        Ok(())
    }

    fn append_dup(&mut self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        self.write_entry(key, &value)
    }
}
