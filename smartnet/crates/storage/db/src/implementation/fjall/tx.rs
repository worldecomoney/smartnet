//! Fjall transaction wrapper.

use super::{
    cursor::Cursor,
    encode_value, err_info, physical_key, prefix_end, table_key,
    view::{entry_from, exists, get_effective, keys_in, View},
};
use crate::{tables, DatabaseError};
use fjall::{Database as FjallDatabase, Keyspace, PersistMode, Snapshot};
use smartnet_db_api::{
    table::{Decompress, DupSort, Encode, IntoVec, Table, TableImporter},
    transaction::{DbTx, DbTxMut},
};
use std::{
    collections::{BTreeMap, HashMap},
    fmt::{self, Debug},
    ops::Bound,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
    thread::ThreadId,
};

/// Physical keys buffered for one table, mapped to their new value.
///
/// `None` is a pending deletion; a key that is absent was not touched by the transaction.
pub(crate) type PendingTable = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// Transaction over a Fjall database.
///
/// Fjall has no multi-statement transactions, so a writable transaction buffers every write in
/// memory and reads observe those buffers on top of the stored data. Nothing is written to the
/// keyspace — and therefore nothing reaches Fjall's write-ahead log — until [`DbTx::commit`]
/// replays the whole buffer in a single atomic batch. Dropping a transaction without
/// committing discards the buffer, so an interrupted transaction leaves no trace at all: there
/// is nothing to undo, and a crash cannot resurrect half of it.
///
/// A read-only transaction pins a [`Snapshot`] instead, so every read in it observes the data
/// as it was when the transaction began even if a writer commits meanwhile.
pub struct Tx {
    db: FjallDatabase,
    keyspaces: Arc<HashMap<&'static str, Keyspace>>,
    writable: bool,
    /// Write state shared with every cursor created by this transaction.
    shared: Arc<Shared>,
    /// Snapshot pinned by a read-only transaction, or `None` for a writable one, which reads
    /// the live keyspaces (see [`super::view::View`]).
    snapshot: Option<Snapshot>,
    /// The environment's single-writer lock, held while this transaction is alive.
    ///
    /// The buffer is discarded in [`Drop for Tx`] before fields are dropped, so the lock is
    /// released only after the transaction has stopped being visible to its own cursors.
    write_guard: Option<WriteGuard>,
}

/// Write state of a transaction, shared with its cursors.
pub(crate) struct Shared {
    /// Whether the transaction has pending writes that must be flushed on commit.
    wrote: AtomicBool,
    /// Whether the transaction was committed, which disarms the discard on drop.
    committed: AtomicBool,
    /// Writes buffered per table, replayed as one atomic batch on commit.
    pending: Mutex<HashMap<&'static str, PendingTable>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            wrote: AtomicBool::new(false),
            committed: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Locks the buffered writes so that a read can merge them over the stored data.
    pub(crate) fn lock_pending(&self) -> MutexGuard<'_, HashMap<&'static str, PendingTable>> {
        self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Buffers the write of `physical` for `table`, where a `value` of `None` is a pending
    /// deletion.
    ///
    /// Must be called instead of writing the keyspace directly: the batch that [`DbTx::commit`]
    /// replays is what makes the transaction atomic, and keeping the writes out of the log is
    /// what makes dropping a transaction a complete rollback.
    pub(crate) fn set(&self, table: &'static str, physical: &[u8], value: Option<&[u8]>) {
        debug_assert!(
            !self.committed.load(Ordering::SeqCst),
            "write buffered after its transaction committed; \
             a cursor must not outlive the transaction that created it",
        );
        self.lock_pending()
            .entry(table)
            .or_default()
            .insert(physical.to_vec(), value.map(|value| value.to_vec()));
        self.wrote.store(true, Ordering::Relaxed);
    }
}

impl Debug for Tx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tx")
            .field("writable", &self.writable)
            .field("wrote", &self.shared.wrote.load(Ordering::SeqCst))
            .field("write_guard", &self.write_guard)
            .finish()
    }
}

/// Tables whose writes are pure schema control: applying them is not "data progress",
/// so a commit that writes only these must not bump the persistence watermark.
/// [`tables::Migrations`] is empty under the fresh-init-only policy but is still
/// schema control, so a future migration log write stays watermark-neutral too.
fn is_schema_control(table: &str) -> bool {
    matches!(table, <tables::SchemaInfo as Table>::NAME | <tables::Migrations as Table>::NAME)
}

/// Whether every row a commit writes belongs to a table whose loss a crash may take.
///
/// Regenerable tables hold progress markers, diagnostics, and conservative bounds —
/// never chain data. Production bundles real stage checkpoints with the data commits
/// that produced them (one transaction), so those rows keep the full `SyncData`
/// guarantee together with their data; only a commit that touches *regenerable tables
/// exclusively* skips the fsync, and its rows are exactly ones the pipeline
/// re-derives by re-running idempotent work after a restart.
///
/// `Metadata` is deliberately **not** regenerable: it carries the `SHUTDOWN_CLEAN`
/// marker, whose durability gates whether the next boot runs crash recovery (a lost
/// marker means a spurious recovery that unwinds committed blocks). Metadata writes
/// are rare (boot clear, init, shutdown set, floor-in-data-commit), so the extra
/// fsync costs nothing on the hot path.
fn is_regenerable(table: &str) -> bool {
    matches!(
        table,
        <tables::StageCheckpoints as Table>::NAME |
            <tables::StageCheckpointProgresses as Table>::NAME |
            <tables::PruneCheckpoints as Table>::NAME |
            <tables::ChainState as Table>::NAME |
            <tables::ClientVersions as Table>::NAME |
            <tables::PersistenceProgress as Table>::NAME |
            <tables::SchemaInfo as Table>::NAME |
            <tables::Migrations as Table>::NAME
    )
}

/// The environment's single-writer lock, matching MDBX semantics: at most one writable
/// [`Tx`] may exist per database environment at a time.
///
/// Acquired in `DatabaseEnv::tx_mut` and held by the transaction for its whole lifetime —
/// through commit and through the discard in [`Drop for Tx`] — so a second writer on
/// another thread blocks until the first has completely finished with the keyspace.
#[derive(Debug)]
pub struct WriteLock {
    /// Whether a writer currently holds the lock, and which thread owns it.
    state: Mutex<LockState>,
    /// Signalled when a writer releases the lock.
    free: Condvar,
}

/// Current holder of the [`WriteLock`].
#[derive(Debug)]
struct LockState {
    /// Whether the lock is currently held.
    held: bool,
    /// Thread that acquired the lock, used to reject same-thread nesting.
    owner: Option<ThreadId>,
}

/// Held by a writable [`Tx`] for its lifetime; releases the [`WriteLock`] when dropped.
#[derive(Debug)]
pub struct WriteGuard {
    lock: Arc<WriteLock>,
}

impl WriteLock {
    /// Creates an unlocked write lock.
    pub(crate) const fn new() -> Self {
        Self { state: Mutex::new(LockState { held: false, owner: None }), free: Condvar::new() }
    }

    /// Acquires the lock, blocking until the current writer commits or drops its
    /// transaction.
    ///
    /// Acquisition from the thread that already holds the lock fails with
    /// [`DatabaseError::InitTx`] instead of deadlocking: two live writable transactions on
    /// one thread would interleave independent buffers over the same keyspace.
    pub(crate) fn acquire(self: &Arc<Self>) -> Result<WriteGuard, DatabaseError> {
        let thread = std::thread::current().id();
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        if state.owner.as_ref() == Some(&thread) {
            return Err(DatabaseError::InitTx(err_info(
                "nested write transaction on the same thread",
            )));
        }
        while state.held {
            state = self.free.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
        }

        state.held = true;
        state.owner = Some(thread);
        Ok(WriteGuard { lock: self.clone() })
    }
}

impl WriteGuard {
    /// Releases the lock, handing it to the next blocked writer.
    fn release(&self) {
        let mut state = self.lock.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.held = false;
        state.owner = None;
        self.lock.free.notify_one();
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        self.release();
    }
}

impl Tx {
    pub(crate) fn new(
        db: FjallDatabase,
        keyspaces: Arc<HashMap<&'static str, Keyspace>>,
        writable: bool,
        write_guard: Option<WriteGuard>,
        snapshot: Option<Snapshot>,
    ) -> Self {
        debug_assert_eq!(
            writable,
            write_guard.is_some(),
            "a writable transaction must hold the single-writer lock"
        );
        debug_assert_eq!(
            writable,
            snapshot.is_none(),
            "a writable transaction reads the live keyspaces, not a pinned snapshot"
        );
        Self { db, keyspaces, writable, shared: Arc::new(Shared::new()), snapshot, write_guard }
    }

    /// The stored data of table `T`, with this transaction's buffer laid over it by the reads.
    fn view<T: Table>(&self) -> Result<View, DatabaseError> {
        Ok(View::new(self.keyspace::<T>()?.clone(), self.snapshot.clone()))
    }

    /// Returns the keyspace backing table `T`.
    fn keyspace<T: Table>(&self) -> Result<&Keyspace, DatabaseError> {
        self.keyspaces.get(T::NAME).ok_or_else(|| {
            DatabaseError::Other(format!("keyspace for table `{}` is not open", T::NAME))
        })
    }

    /// Fails the operation if the database was opened in read-only mode.
    fn ensure_writable(&self) -> Result<(), DatabaseError> {
        if self.writable {
            Ok(())
        } else {
            Err(DatabaseError::Other("database was opened in read-only mode".to_string()))
        }
    }
}

impl Drop for Tx {
    fn drop(&mut self) {
        if self.writable && !self.shared.committed.load(Ordering::SeqCst) {
            // Discarding the buffer is the whole rollback: the writes never reached Fjall's
            // log, so not even a crash can bring them back.
            self.shared.lock_pending().clear();
        }
    }
}

impl TableImporter for Tx {}

impl DbTx for Tx {
    type Cursor<T: Table> = Cursor<T>;
    type DupCursor<T: DupSort> = Cursor<T>;

    fn get<T: Table>(&self, key: T::Key) -> Result<Option<T::Value>, DatabaseError> {
        self.get_by_encoded_key::<T>(&key.encode())
    }

    fn get_by_encoded_key<T: Table>(
        &self,
        key: &<T::Key as Encode>::Encoded,
    ) -> Result<Option<T::Value>, DatabaseError> {
        let view = self.view::<T>()?;
        let encoded = key.as_ref();

        let stored = {
            let pending = self.shared.lock_pending();
            let pending = pending.get(T::NAME);

            if T::DUPSORT {
                // Duplicates of a key are stored under `encoded key ++ encoded value`, so the first
                // entry with the encoded key as prefix sorts first, giving the "first value of this
                // key" behaviour expected by the callers.
                match entry_from(&view, pending, encoded)? {
                    Some((physical, value)) if table_key::<T>(&physical, &value) == encoded => {
                        Some(value)
                    }
                    _ => None,
                }
            } else {
                get_effective(&view, pending, encoded)?
            }
        };

        match stored {
            Some(value) => Ok(Some(Decompress::decompress(&value)?)),
            None => Ok(None),
        }
    }

    fn commit(self) -> Result<(), DatabaseError> {
        if self.writable && self.shared.wrote.load(Ordering::Relaxed) {
            // The watermark row joins the same atomic batch as the buffered writes:
            // `bump_persistence_progress` reads through the buffer and writes back into
            // it, so a crash keeps the watermark and its data together or not at all.
            //
            // The bump fires on every commit except one that wrote only the
            // schema-control tables (schema info, migrations): a fresh database's
            // version-init commit must leave the watermark absent, so `last_txnid`
            // stays "unknown" until the first real commit.
            let bumps_watermark = {
                let pending = self.shared.lock_pending();
                pending.keys().any(|table| !is_schema_control(table))
            };
            crate::persistence::bump_persistence_progress(&self, bumps_watermark)?;

            let pending = std::mem::take(&mut *self.shared.lock_pending());

            // Replay the whole buffer in one batch: every write of every table reaches
            // Fjall's log in a single append, so a crash either keeps all of it or none.
            let mut batch = self.db.batch();
            for (table, entries) in &pending {
                let keyspace = self.keyspaces.get(table).ok_or_else(|| {
                    DatabaseError::Other(format!("keyspace for table `{table}` is not open"))
                })?;
                for (physical, value) in entries {
                    match value {
                        Some(value) => {
                            batch.insert(keyspace, physical.as_slice(), value.as_slice())
                        }
                        None => batch.remove(keyspace, physical.as_slice()),
                    }
                }
            }
            batch.commit().map_err(|error| DatabaseError::Commit(err_info(error)))?;

            // Durability classification (F1): the batch reached Fjall's journal either way
            // (atomic either way), but only commits carrying chain data — or the watermark
            // written alongside it — pay the fdatasync. A commit of regenerable rows alone
            // (progress markers, diagnostics) returns with its rows still in the journal's
            // write buffer: a crash may lose them, and every one of them is re-derived by
            // re-running idempotent work. The database persists the journal on clean
            // shutdown, so only an unclean exit can lose such a tail.
            let carries_data = pending.keys().any(|table| !is_regenerable(table));
            if carries_data {
                self.db
                    .persist(PersistMode::SyncData)
                    .map_err(|error| DatabaseError::Commit(err_info(error)))?;
            }
        }

        self.shared.committed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn abort(self) {
        // Dropping the transaction discards its buffered writes (see `impl Drop for Tx`).
    }

    fn cursor_read<T: Table>(&self) -> Result<Self::Cursor<T>, DatabaseError> {
        Ok(Cursor::new(self.view::<T>()?, self.shared.clone(), false))
    }

    fn cursor_dup_read<T: DupSort>(&self) -> Result<Self::DupCursor<T>, DatabaseError> {
        Ok(Cursor::new(self.view::<T>()?, self.shared.clone(), false))
    }

    fn entries<T: Table>(&self) -> Result<usize, DatabaseError> {
        let view = self.view::<T>()?;
        let pending = self.shared.lock_pending();
        let pending = pending.get(T::NAME);

        let mut count = view.len()?;
        if let Some(table) = pending {
            for (physical, value) in table {
                // A buffered write adds an entry that is not stored yet, and a buffered
                // deletion removes one that still is.
                match (value.is_some(), view.contains_key(physical)?) {
                    (true, false) => count += 1,
                    (false, true) => count -= 1,
                    _ => {}
                }
            }
        }
        Ok(count)
    }

    fn disable_long_read_transaction_safety(&mut self) {}
}

impl DbTxMut for Tx {
    type CursorMut<T: Table> = Cursor<T>;
    type DupCursorMut<T: DupSort> = Cursor<T>;

    fn put<T: Table>(&self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        self.ensure_writable()?;
        self.keyspace::<T>()?;

        let encoded_key = key.encode().into_vec();
        let encoded_value = encode_value(&value);
        let physical = physical_key::<T>(&encoded_key, &encoded_value);

        self.shared.set(T::NAME, &physical, Some(&encoded_value));
        Ok(())
    }

    fn append<T: Table>(&self, key: T::Key, value: T::Value) -> Result<(), DatabaseError> {
        // Fjall has no append mode, so this is an upsert like `put`.
        self.put::<T>(key, value)
    }

    fn delete<T: Table>(
        &self,
        key: T::Key,
        value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        self.ensure_writable()?;
        let view = self.view::<T>()?;
        let encoded_key = key.encode().into_vec();

        // Work out what to delete against the buffered state first, then release the buffer
        // lock before recording the deletions, which locks it again.
        let doomed = {
            let pending = self.shared.lock_pending();
            let pending = pending.get(T::NAME);

            if T::DUPSORT {
                match value {
                    // Delete a single duplicate, or report a no-op when it does not exist.
                    Some(value) => {
                        let encoded_value = encode_value(&value);
                        let physical = physical_key::<T>(&encoded_key, &encoded_value);
                        if exists(&view, pending, &physical)? {
                            vec![physical]
                        } else {
                            Vec::new()
                        }
                    }
                    // Without a value every duplicate of the key is deleted. They all sort
                    // below the exclusive end of the key's prefix.
                    None => {
                        let upper =
                            prefix_end(&encoded_key).map_or(Bound::Unbounded, Bound::Excluded);
                        let bounds = (Bound::Included(encoded_key), upper);
                        keys_in(&view, pending, bounds)?
                    }
                }
            } else {
                match get_effective(&view, pending, &encoded_key)? {
                    // Deleting a missing key is a no-op, matching MDBX semantics.
                    None => Vec::new(),
                    Some(stored) => {
                        if let Some(value) = value {
                            let encoded_value = encode_value(&value);
                            if stored != encoded_value.as_slice() {
                                return Err(DatabaseError::Delete(err_info("value mismatch")));
                            }
                        }
                        vec![encoded_key]
                    }
                }
            }
        };

        // Deleting missing entries is a no-op, matching MDBX semantics.
        if doomed.is_empty() {
            return Ok(false);
        }

        for physical in &doomed {
            self.shared.set(T::NAME, physical, None);
        }
        Ok(true)
    }

    fn clear<T: Table>(&self) -> Result<(), DatabaseError> {
        self.ensure_writable()?;
        let view = self.view::<T>()?;

        // Buffer a deletion for every visible entry so that dropping the transaction leaves
        // the table untouched and committing removes all of it in one batch.
        let entries = {
            let pending = self.shared.lock_pending();
            keys_in(&view, pending.get(T::NAME), (Bound::Unbounded, Bound::Unbounded))?
        };
        for entry in &entries {
            self.shared.set(T::NAME, entry, None);
        }

        Ok(())
    }

    fn cursor_write<T: Table>(&self) -> Result<Self::CursorMut<T>, DatabaseError> {
        self.ensure_writable()?;
        Ok(Cursor::new(self.view::<T>()?, self.shared.clone(), true))
    }

    fn cursor_dup_write<T: DupSort>(&self) -> Result<Self::DupCursorMut<T>, DatabaseError> {
        self.ensure_writable()?;
        Ok(Cursor::new(self.view::<T>()?, self.shared.clone(), true))
    }
}
