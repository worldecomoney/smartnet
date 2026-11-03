//! Stored data views, and how a transaction's buffered writes overlay them.
//!
//! A [`View`] is the data a transaction reads *before* its own writes are applied. Read-only
//! transactions pin a [`Snapshot`] so that every read in the transaction observes the data as
//! it was when the transaction began (MDBX repeatable-read parity). Writable transactions read
//! the live keyspaces instead: the environment's single-writer lock guarantees nothing else
//! can change them while the transaction runs.
//!
//! The overlay itself lives in [`super::tx::Shared`] as a [`PendingTable`] per table. These
//! helpers merge such a table over a view, so every read in the transaction observes
//! read-your-writes semantics without the writes ever reaching Fjall's log.

use super::{read_error, tx::PendingTable};
use crate::DatabaseError;
use fjall::{Iter, Keyspace, Readable, Snapshot};
use std::ops::Bound;

/// Inclusive/exclusive key bounds understood by both Fjall's iterators and [`PendingTable`].
pub(crate) type Bounds = (Bound<Vec<u8>>, Bound<Vec<u8>>);

/// A physical key together with the value it holds once pending writes are applied.
pub(crate) type Entry = (Vec<u8>, Vec<u8>);

/// The stored data of one keyspace, optionally pinned to a snapshot.
pub(crate) struct View {
    keyspace: Keyspace,
    snapshot: Option<Snapshot>,
}

impl View {
    /// Views `keyspace`, pinned to `snapshot` when one is given.
    pub(crate) const fn new(keyspace: Keyspace, snapshot: Option<Snapshot>) -> Self {
        Self { keyspace, snapshot }
    }

    /// The stored value at `physical`, ignoring pending writes.
    pub(crate) fn get(&self, physical: &[u8]) -> Result<Option<Vec<u8>>, DatabaseError> {
        let value = match &self.snapshot {
            Some(snapshot) => snapshot.get(&self.keyspace, physical),
            None => self.keyspace.get(physical),
        };
        value.map_err(read_error).map(|value| value.map(|value| value.to_vec()))
    }

    /// Whether the stored data holds `physical`, ignoring pending writes.
    pub(crate) fn contains_key(&self, physical: &[u8]) -> Result<bool, DatabaseError> {
        match &self.snapshot {
            Some(snapshot) => snapshot.contains_key(&self.keyspace, physical).map_err(read_error),
            None => self.keyspace.contains_key(physical).map_err(read_error),
        }
    }

    /// Iterates the stored data over `bounds`.
    pub(crate) fn range(&self, bounds: Bounds) -> Iter {
        match &self.snapshot {
            Some(snapshot) => snapshot.range(&self.keyspace, bounds),
            None => self.keyspace.range(bounds),
        }
    }

    /// The number of stored entries (a full scan, as in Fjall itself).
    pub(crate) fn len(&self) -> Result<usize, DatabaseError> {
        match &self.snapshot {
            Some(snapshot) => snapshot.len(&self.keyspace).map_err(read_error),
            None => self.keyspace.len().map_err(read_error),
        }
    }
}

/// The value stored at `physical` once `pending` is applied.
pub(crate) fn get_effective(
    view: &View,
    pending: Option<&PendingTable>,
    physical: &[u8],
) -> Result<Option<Vec<u8>>, DatabaseError> {
    match pending.and_then(|table| table.get(physical)) {
        Some(Some(value)) => Ok(Some(value.clone())),
        // Written then deleted by this transaction: absent.
        Some(None) => Ok(None),
        None => view.get(physical),
    }
}

/// Whether `physical` exists once `pending` is applied.
pub(crate) fn exists(
    view: &View,
    pending: Option<&PendingTable>,
    physical: &[u8],
) -> Result<bool, DatabaseError> {
    match pending.and_then(|table| table.get(physical)) {
        Some(Some(_)) => Ok(true),
        Some(None) => Ok(false),
        None => view.contains_key(physical),
    }
}

/// The first visible `(physical key, value)` at or after `start`.
pub(crate) fn entry_from(
    view: &View,
    pending: Option<&PendingTable>,
    start: &[u8],
) -> Result<Option<Entry>, DatabaseError> {
    let bounds = (Bound::Included(start.to_vec()), Bound::Unbounded);
    let mut stored = view.range(bounds.clone());
    let mut buffered = pending.map(|table| table.range(bounds));

    let mut stored_item = next_stored(&mut stored, Ordering::Forward)?;
    let mut pending_item = buffered.as_mut().and_then(|range| range.next());

    loop {
        if pending_wins(&pending_item, &stored_item, Ordering::Forward) {
            let (key, value) = pending_item.take().expect("pending entry was present");
            let key = key.clone();
            if stored_item.as_ref().is_some_and(|(stored_key, _)| *stored_key == key) {
                stored_item = next_stored(&mut stored, Ordering::Forward)?;
            }
            pending_item = buffered.as_mut().and_then(|range| range.next());
            if let Some(value) = value {
                return Ok(Some((key, value.clone())));
            }
            // Deleted by this transaction: skip it and its stored copy.
            continue;
        }

        return Ok(stored_item.take());
    }
}

/// The last visible `(physical key, value)` before `end`, or the last one when `end` is `None`.
pub(crate) fn entry_before(
    view: &View,
    pending: Option<&PendingTable>,
    end: Option<&[u8]>,
) -> Result<Option<Entry>, DatabaseError> {
    let bounds =
        (Bound::Unbounded, end.map_or(Bound::Unbounded, |end| Bound::Excluded(end.to_vec())));
    let mut stored = view.range(bounds.clone());
    let mut buffered = pending.map(|table| table.range(bounds));

    let mut stored_item = next_stored(&mut stored, Ordering::Back)?;
    let mut pending_item = buffered.as_mut().and_then(|range| range.next_back());

    loop {
        if pending_wins(&pending_item, &stored_item, Ordering::Back) {
            let (key, value) = pending_item.take().expect("pending entry was present");
            let key = key.clone();
            if stored_item.as_ref().is_some_and(|(stored_key, _)| *stored_key == key) {
                stored_item = next_stored(&mut stored, Ordering::Back)?;
            }
            pending_item = buffered.as_mut().and_then(|range| range.next_back());
            if let Some(value) = value {
                return Ok(Some((key, value.clone())));
            }
            continue;
        }

        return Ok(stored_item.take());
    }
}

/// Every visible physical key over `bounds`, in ascending order.
///
/// Used by the operations that have to touch a whole key range at once: clearing a table and
/// deleting all duplicates of a `DupSort` key.
pub(crate) fn keys_in(
    view: &View,
    pending: Option<&PendingTable>,
    bounds: Bounds,
) -> Result<Vec<Vec<u8>>, DatabaseError> {
    let mut keys = std::collections::BTreeSet::new();

    for guard in view.range(bounds.clone()) {
        let (key, _) = guard.into_inner().map_err(read_error)?;
        keys.insert(key.to_vec());
    }

    if let Some(table) = pending {
        for (key, value) in table.range(bounds) {
            match value {
                Some(_) => {
                    keys.insert(key.clone());
                }
                // A pending deletion removes the key, whether or not it is stored.
                None => {
                    keys.remove(key);
                }
            }
        }
    }

    Ok(keys.into_iter().collect())
}

/// Which side of the merge comes first when walking in `order`.
fn pending_wins(
    pending: &Option<(&Vec<u8>, &Option<Vec<u8>>)>,
    stored: &Option<(Vec<u8>, Vec<u8>)>,
    order: Ordering,
) -> bool {
    match (pending, stored) {
        (Some((pending_key, _)), Some((stored_key, _))) => match order {
            Ordering::Forward => *pending_key <= stored_key,
            Ordering::Back => *pending_key >= stored_key,
        },
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Pulls the next stored entry in `order`, decoding it into owned bytes.
fn next_stored(stored: &mut Iter, order: Ordering) -> Result<Option<Entry>, DatabaseError> {
    let guard = match order {
        Ordering::Forward => stored.next(),
        Ordering::Back => stored.next_back(),
    };

    match guard {
        Some(guard) => {
            let (key, value) = guard.into_inner().map_err(read_error)?;
            Ok(Some((key.to_vec(), value.to_vec())))
        }
        None => Ok(None),
    }
}

/// Direction a merge walks in.
#[derive(Clone, Copy)]
enum Ordering {
    Forward,
    Back,
}
